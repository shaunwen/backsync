use std::env;
use std::path::PathBuf;
use std::process::ExitCode;
use std::thread;
use std::time::Instant;

use anyhow::{Context, Result, bail};
use backsync::compare::same_content;
use backsync::config::{self, Config};
use backsync::plan::{self, Compare};
use backsync::report;
use backsync::scan::{Scan, scan};
use backsync::sync::{self, TargetRoot};
use clap::Parser;

/// Mirror a source folder into a backup target folder.
///
/// Copies new and changed files, removes files deleted from the source,
/// and never modifies the source. Source and target come from
/// BACKSYNC_SOURCE / BACKSYNC_TARGET or the config file (env wins).
#[derive(Parser)]
#[command(version)]
struct Cli {
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
}

enum Status {
    Clean,
    Partial,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(&cli) {
        Ok(Status::Clean) => ExitCode::SUCCESS,
        Ok(Status::Partial) => ExitCode::from(1),
        Err(error) => {
            eprintln!("backsync: {error:#}");
            ExitCode::from(2)
        }
    }
}

fn run(cli: &Cli) -> Result<Status> {
    let config = load_config(cli)?;
    let started = Instant::now();

    let threads = thread::available_parallelism().map_or(4, |n| n.get());
    let (source, target) = thread::scope(|s| {
        let target = s.spawn(|| scan(&config.target, threads));
        let source = scan(&config.source, threads);
        (source, target.join().expect("target scan thread panicked"))
    });
    if source.entries.is_empty() && !target.entries.is_empty() && !cli.allow_empty_source {
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
    let plan = plan::diff(&source.entries, &target.entries, compare, |relative| {
        same_content(&config.source.join(relative), &config.target.join(relative))
    });
    let allow_delete = source.errors.is_empty();
    let scans_clean = source.errors.is_empty() && target.errors.is_empty();

    if cli.dry_run {
        for line in report::plan_lines(&plan, allow_delete) {
            println!("{line}");
        }
        println!("{}", report::dry_run_summary(&plan, allow_delete));
        return Ok(status(scans_clean));
    }

    let verbose = cli.verbose;
    let outcome = sync::execute(
        &plan,
        &config.source,
        &TargetRoot::new(config.target.clone()),
        &sync::Options { allow_delete },
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

fn load_config(cli: &Cli) -> Result<Config> {
    let lookup = |key: &str| env::var(key).ok();
    let file = match &cli.config {
        Some(path) => config::load_file(path, true)?,
        None => match config::default_config_path(lookup) {
            Some(path) => config::load_file(&path, false)?,
            None => config::FileConfig::default(),
        },
    };
    config::validate(config::resolve(file, lookup)?).context("invalid source or target")
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
