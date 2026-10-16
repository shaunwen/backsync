use std::env;
use std::path::PathBuf;
use std::process::ExitCode;
use std::thread;
use std::time::Instant;

use anyhow::{Context, Result, bail};
use backsync::compare::same_content;
use backsync::config::{self, Backup, Config};
use backsync::plan::{self, Compare, Removals};
use backsync::report;
use backsync::scan::{Scan, disk_path, scan};
use backsync::sync::{self, Names, TargetRoot};
use clap::Parser;

/**
Mirror source folders into backup target folders.

Copies new and changed files, removes files deleted from the source
(unless `mirror_deletions = false`), and never modifies the source.
Backups come from [backup.<name>] sections in the config file, or a single
pair from `source` / `target` there or BACKSYNC_SOURCE / BACKSYNC_TARGET
(env wins).
*/
#[derive(Parser)]
#[command(version)]
struct Cli {
    /// Backups to run, by their [backup.<name>] in the config file [default: all]
    #[arg(value_name = "NAME")]
    names: Vec<String>,

    /// Config file [default: $XDG_CONFIG_HOME or ~/.config, then backsync/config.toml]
    #[arg(long, value_name = "PATH")]
    config: Option<PathBuf>,

    /// Show what would change without touching the target
    #[arg(short = 'n', long)]
    dry_run: bool,

    /// Print every action as it happens
    #[arg(short, long)]
    verbose: bool,

    /// Compare file contents instead of size and modification time
    #[arg(long)]
    checksum: bool,

    /// Allow syncing an empty source, which empties the target
    #[arg(long)]
    allow_empty_source: bool,

    /// Parallel copy jobs; lower it for spinning disks [default: `jobs` in config, else 4]
    #[arg(short, long, value_name = "N", value_parser = clap::value_parser!(u16).range(1..))]
    jobs: Option<u16>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Status {
    Clean,
    Partial,
    Refused,
}

impl Status {
    fn label(self) -> &'static str {
        match self {
            Status::Clean => "ok",
            Status::Partial => "finished with errors",
            Status::Refused => "not run",
        }
    }

    fn exit_code(self) -> ExitCode {
        match self {
            Status::Clean => ExitCode::SUCCESS,
            Status::Partial => ExitCode::from(1),
            Status::Refused => ExitCode::from(2),
        }
    }
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let backups = match load_backups(&cli) {
        Ok(backups) => backups,
        Err(error) => {
            eprintln!("backsync: {error:#}");
            return Status::Refused.exit_code();
        }
    };

    let mut results = Vec::with_capacity(backups.len());
    for (index, backup) in backups.iter().enumerate() {
        if let Some(name) = &backup.name {
            if index > 0 {
                println!();
            }
            println!(
                "[{name}] {} -> {}",
                backup.config.source.display(),
                backup.config.target.display()
            );
        }
        let status = run_backup(&cli, &backup.config).unwrap_or_else(|error| {
            let tag = backup
                .name
                .as_ref()
                .map_or(String::new(), |name| format!("[{name}] "));
            eprintln!("backsync: {tag}{error:#}");
            Status::Refused
        });
        results.push((backup.name.as_deref(), status));
    }

    if results.len() > 1 {
        let tally: Vec<String> = results
            .iter()
            .map(|(name, status)| format!("{} {}", name.unwrap_or("backup"), status.label()))
            .collect();
        println!("\n{}", tally.join(", "));
    }
    results
        .iter()
        .map(|(_, status)| *status)
        .max()
        .unwrap_or(Status::Clean)
        .exit_code()
}

fn run_backup(cli: &Cli, config: &Config) -> Result<Status> {
    let config = config::validate(config.clone()).context("invalid source or target")?;
    let started = Instant::now();

    let threads = thread::available_parallelism().map_or(4, |n| n.get());
    let (source, target) = thread::scope(|s| {
        let target = s.spawn(|| scan(&config.target, threads));
        let source = scan(&config.source, threads);
        (source, target.join().expect("target scan thread panicked"))
    });
    if config.mirror_deletions
        && source.entries.is_empty()
        && !target.entries.is_empty()
        && !cli.allow_empty_source
    {
        bail!(
            "source {} is empty but the backup is not; refusing to empty the backup \
             (is the source drive mounted? use --allow-empty-source if this is intended)",
            config.source.display()
        );
    }
    warn_scan_problems("source", &source);
    warn_scan_problems("target", &target);

    let compare = if cli.checksum {
        Compare::Content
    } else {
        Compare::Metadata {
            modify_window: config.modify_window,
        }
    };
    let plan = plan::diff(&source.entries, &target.entries, compare, |key| {
        same_content(
            &config.source.join(disk_path(&source.disk_names, key)),
            &config.target.join(disk_path(&target.disk_names, key)),
        )
    });
    let removals = if !source.errors.is_empty() {
        Removals::None
    } else if config.mirror_deletions {
        Removals::All
    } else {
        Removals::KeepDeleted
    };
    let scans_clean = source.errors.is_empty() && target.errors.is_empty();

    if cli.dry_run {
        for line in report::plan_lines(&plan, removals) {
            println!("{line}");
        }
        println!("{}", report::dry_run_summary(&plan, removals));
        return Ok(status(scans_clean));
    }

    let verbose = cli.verbose;
    let outcome = sync::execute(
        &plan,
        &config.source,
        &TargetRoot::new(config.target.clone()),
        &Names {
            source: source.disk_names,
            target: target.disk_names,
        },
        &sync::Options {
            removals,
            jobs: cli.jobs.map_or(config.jobs, usize::from),
        },
        &|event| {
            if verbose {
                println!("{}", report::event_line(event));
            }
        },
    );
    for error in &outcome.errors {
        eprintln!(
            "backsync: {} {}: {}",
            error.action,
            error.path.display(),
            error.message
        );
    }
    println!(
        "{}",
        report::summary(&outcome, plan.unchanged, started.elapsed())
    );
    Ok(status(scans_clean && outcome.errors.is_empty()))
}

fn status(clean: bool) -> Status {
    if clean {
        Status::Clean
    } else {
        Status::Partial
    }
}

fn load_backups(cli: &Cli) -> Result<Vec<Backup>> {
    let lookup = |key: &str| env::var(key).ok();
    let file = match &cli.config {
        Some(path) => config::load_file(path, true)?,
        None => match config::default_config_path(lookup) {
            Some(path) => config::load_file(&path, false)?,
            None => config::FileConfig::default(),
        },
    };
    config::resolve_backups(file, lookup, &cli.names)
}

fn warn_scan_problems(side: &str, scan: &Scan) {
    for error in &scan.errors {
        eprintln!(
            "backsync: cannot read {side} {}: {}",
            error.path.display(),
            error.message
        );
    }
    if side == "source" && !scan.errors.is_empty() {
        eprintln!(
            "backsync: source was not fully read, so nothing will be removed from the backup"
        );
    }
    for path in &scan.skipped_special {
        eprintln!(
            "backsync: skipping special file in {side}: {}",
            path.display()
        );
    }
}
