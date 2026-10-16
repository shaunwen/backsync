use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use indexmap::IndexMap;
use serde::Deserialize;

pub const SOURCE_ENV: &str = "BACKSYNC_SOURCE";
pub const TARGET_ENV: &str = "BACKSYNC_TARGET";
const DEFAULT_MODIFY_WINDOW_SECS: u64 = 2;
const DEFAULT_JOBS: usize = 4;

#[derive(Debug, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileConfig {
    pub source: Option<String>,
    pub target: Option<String>,
    pub modify_window_secs: Option<u64>,
    pub jobs: Option<usize>,
    pub mirror_deletions: Option<bool>,
    #[serde(default)]
    pub backup: IndexMap<String, BackupEntry>,
}

#[derive(Debug, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BackupEntry {
    pub source: String,
    pub target: String,
    pub modify_window_secs: Option<u64>,
    pub jobs: Option<usize>,
    pub mirror_deletions: Option<bool>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Config {
    pub source: PathBuf,
    pub target: PathBuf,
    pub modify_window: Duration,
    pub jobs: usize,
    pub mirror_deletions: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Backup {
    pub name: Option<String>,
    pub config: Config,
}

pub fn default_config_path(env: impl Fn(&str) -> Option<String>) -> Option<PathBuf> {
    let base = non_empty(env("XDG_CONFIG_HOME"))
        .map(PathBuf::from)
        .or_else(|| non_empty(env("HOME")).map(|home| Path::new(&home).join(".config")))?;
    Some(base.join("backsync").join("config.toml"))
}

pub fn load_file(path: &Path, must_exist: bool) -> Result<FileConfig> {
    if !must_exist && !path.exists() {
        return Ok(FileConfig::default());
    }
    let text = fs::read_to_string(path)
        .with_context(|| format!("cannot read config file {}", path.display()))?;
    toml::from_str(&text).with_context(|| format!("invalid config file {}", path.display()))
}

pub fn resolve_backups(
    file: FileConfig,
    env: impl Fn(&str) -> Option<String>,
    names: &[String],
) -> Result<Vec<Backup>> {
    let home = non_empty(env("HOME"));
    let env_source = non_empty(env(SOURCE_ENV));
    let env_target = non_empty(env(TARGET_ENV));
    let defaults = file_tuning(&file);

    if env_source.is_some() || env_target.is_some() {
        if !names.is_empty() {
            bail!(
                "backup names cannot be combined with {SOURCE_ENV} / {TARGET_ENV}; \
                 unset them to run named backups"
            );
        }
        let pair = (
            env_source.or_else(|| non_empty(file.source)),
            env_target.or_else(|| non_empty(file.target)),
        );
        return Ok(vec![single(pair, &defaults, home.as_deref())?]);
    }

    if file.backup.is_empty() {
        if !names.is_empty() {
            bail!("the config file has no [backup.<name>] sections to select from");
        }
        let pair = (non_empty(file.source), non_empty(file.target));
        return Ok(vec![single(pair, &defaults, home.as_deref())?]);
    }

    if non_empty(file.source.clone()).is_some() || non_empty(file.target.clone()).is_some() {
        bail!(
            "the config file has both top-level `source` / `target` and [backup.<name>] \
             sections; move the top-level pair into a named section"
        );
    }
    if let Some(name) = file.backup.keys().find(|name| name.trim().is_empty()) {
        bail!("backup name {name:?} is empty");
    }

    selected_names(&file.backup, names)?
        .into_iter()
        .map(|name| {
            let entry = &file.backup[name];
            let tuning = Tuning {
                modify_window_secs: entry.modify_window_secs.or(defaults.modify_window_secs),
                jobs: entry.jobs.or(defaults.jobs),
                mirror_deletions: entry.mirror_deletions.or(defaults.mirror_deletions),
            };
            let config = build(&entry.source, &entry.target, &tuning, home.as_deref())
                .with_context(|| format!("backup '{name}'"))?;
            Ok(Backup {
                name: Some(name.clone()),
                config,
            })
        })
        .collect()
}

struct Tuning {
    modify_window_secs: Option<u64>,
    jobs: Option<usize>,
    mirror_deletions: Option<bool>,
}

fn file_tuning(file: &FileConfig) -> Tuning {
    Tuning {
        modify_window_secs: file.modify_window_secs,
        jobs: file.jobs,
        mirror_deletions: file.mirror_deletions,
    }
}

fn single(
    (source, target): (Option<String>, Option<String>),
    tuning: &Tuning,
    home: Option<&str>,
) -> Result<Backup> {
    let missing = |env_name: &str, field: &str| {
        format!(
            "{field} folder is not set: set {env_name}, `{field}` in the config file, \
             or add [backup.<name>] sections"
        )
    };
    let source = source.with_context(|| missing(SOURCE_ENV, "source"))?;
    let target = target.with_context(|| missing(TARGET_ENV, "target"))?;
    Ok(Backup {
        name: None,
        config: build(&source, &target, tuning, home)?,
    })
}

fn build(source: &str, target: &str, tuning: &Tuning, home: Option<&str>) -> Result<Config> {
    let jobs = tuning.jobs.unwrap_or(DEFAULT_JOBS);
    if jobs == 0 {
        bail!("`jobs` must be at least 1");
    }
    Ok(Config {
        source: expand_tilde(source, home),
        target: expand_tilde(target, home),
        modify_window: Duration::from_secs(
            tuning
                .modify_window_secs
                .unwrap_or(DEFAULT_MODIFY_WINDOW_SECS),
        ),
        jobs,
        mirror_deletions: tuning.mirror_deletions.unwrap_or(true),
    })
}

fn selected_names<'a>(
    backups: &'a IndexMap<String, BackupEntry>,
    names: &'a [String],
) -> Result<Vec<&'a String>> {
    if names.is_empty() {
        return Ok(backups.keys().collect());
    }
    let unknown: Vec<&str> = names
        .iter()
        .filter(|name| !backups.contains_key(*name))
        .map(String::as_str)
        .collect();
    if !unknown.is_empty() {
        let available: Vec<&str> = backups.keys().map(String::as_str).collect();
        bail!(
            "unknown backup {}; available: {}",
            unknown.join(", "),
            available.join(", ")
        );
    }
    let mut selected: Vec<&String> = Vec::new();
    for name in names {
        if !selected.contains(&name) {
            selected.push(name);
        }
    }
    Ok(selected)
}

pub fn validate(config: Config) -> Result<Config> {
    let source = existing_dir(&config.source, "source")?;
    let target = existing_dir(&config.target, "target")?;
    if source == target {
        bail!(
            "source and target are the same folder: {}",
            source.display()
        );
    }
    if source.starts_with(&target) {
        bail!(
            "source {} is inside target {}; syncing would delete the source",
            source.display(),
            target.display()
        );
    }
    if target.starts_with(&source) {
        bail!(
            "target {} is inside source {}; the backup would copy itself",
            target.display(),
            source.display()
        );
    }
    Ok(Config {
        source,
        target,
        ..config
    })
}

fn existing_dir(path: &Path, field: &str) -> Result<PathBuf> {
    let canonical = path.canonicalize().with_context(|| {
        format!(
            "{field} folder {} does not exist (is the drive mounted?)",
            path.display()
        )
    })?;
    if !canonical.is_dir() {
        bail!("{field} {} is not a folder", canonical.display());
    }
    Ok(canonical)
}

fn expand_tilde(raw: &str, home: Option<&str>) -> PathBuf {
    match (raw, home) {
        ("~", Some(home)) => PathBuf::from(home),
        (_, Some(home)) if raw.starts_with("~/") => Path::new(home).join(&raw[2..]),
        _ => PathBuf::from(raw),
    }
}

fn non_empty(value: Option<String>) -> Option<String> {
    value.filter(|v| !v.trim().is_empty())
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    fn env_of(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |key| map.get(key).cloned()
    }

    fn file_config(source: &str, target: &str) -> FileConfig {
        FileConfig {
            source: Some(source.into()),
            target: Some(target.into()),
            ..FileConfig::default()
        }
    }

    fn resolve(file: FileConfig, env: impl Fn(&str) -> Option<String>) -> Result<Config> {
        let mut backups = resolve_backups(file, env, &[])?;
        assert_eq!(backups.len(), 1);
        Ok(backups.remove(0).config)
    }

    fn parse(text: &str) -> FileConfig {
        toml::from_str(text).unwrap()
    }

    fn names(list: &[&str]) -> Vec<String> {
        list.iter().map(|n| n.to_string()).collect()
    }

    fn summary(backups: &[Backup]) -> Vec<(Option<&str>, &Path, &Path)> {
        backups
            .iter()
            .map(|b| {
                (
                    b.name.as_deref(),
                    b.config.source.as_path(),
                    b.config.target.as_path(),
                )
            })
            .collect()
    }

    const TWO_BACKUPS: &str = r#"
        [backup.zeta]
        source = "~/z"
        target = "/Volumes/D/z"

        [backup.alpha]
        source = "/a"
        target = "/Volumes/D/a"
    "#;

    #[test]
    fn named_backups_all_run_in_file_order() {
        // given
        let file = parse(TWO_BACKUPS);
        let env = env_of(&[("HOME", "/Users/me")]);

        // when
        let backups = resolve_backups(file, env, &[]).unwrap();

        // then
        assert_eq!(
            summary(&backups),
            vec![
                (
                    Some("zeta"),
                    Path::new("/Users/me/z"),
                    Path::new("/Volumes/D/z")
                ),
                (Some("alpha"), Path::new("/a"), Path::new("/Volumes/D/a")),
            ]
        );
    }

    #[test]
    fn selected_backups_run_in_the_given_order_once_each() {
        // given
        let file = parse(TWO_BACKUPS);

        // when
        let backups =
            resolve_backups(file, env_of(&[]), &names(&["alpha", "zeta", "alpha"])).unwrap();

        // then
        let order: Vec<_> = backups.iter().map(|b| b.name.as_deref()).collect();
        assert_eq!(order, vec![Some("alpha"), Some("zeta")]);
    }

    #[test]
    fn unknown_backup_name_lists_the_available_ones() {
        // given
        let file = parse(TWO_BACKUPS);

        // when
        let error = resolve_backups(file, env_of(&[]), &names(&["alpah"])).unwrap_err();

        // then
        assert_eq!(
            error.to_string(),
            "unknown backup alpah; available: zeta, alpha"
        );
    }

    #[test]
    fn per_backup_settings_override_top_level_defaults() {
        // given
        let file = parse(
            r#"
            jobs = 8
            modify_window_secs = 5

            [backup.fast]
            source = "/a"
            target = "/b"

            [backup.slow]
            source = "/c"
            target = "/d"
            jobs = 1
            modify_window_secs = 0
        "#,
        );

        // when
        let backups = resolve_backups(file, env_of(&[]), &[]).unwrap();

        // then
        let settings: Vec<_> = backups
            .iter()
            .map(|b| (b.config.jobs, b.config.modify_window))
            .collect();
        assert_eq!(
            settings,
            vec![(8, Duration::from_secs(5)), (1, Duration::ZERO)]
        );
    }

    #[test]
    fn env_pair_runs_alone_and_ignores_named_backups() {
        // given
        let file = parse(TWO_BACKUPS);
        let env = env_of(&[(SOURCE_ENV, "/env/src"), (TARGET_ENV, "/env/dst")]);

        // when
        let backups = resolve_backups(file, env, &[]).unwrap();

        // then
        assert_eq!(
            summary(&backups),
            vec![(None, Path::new("/env/src"), Path::new("/env/dst"))]
        );
    }

    #[test]
    fn backup_names_cannot_be_combined_with_env_pair() {
        // given
        let file = parse(TWO_BACKUPS);
        let env = env_of(&[(SOURCE_ENV, "/env/src")]);

        // when
        let error = resolve_backups(file, env, &names(&["alpha"])).unwrap_err();

        // then
        assert!(error.to_string().contains("cannot be combined"));
    }

    #[test]
    fn backup_names_need_named_sections() {
        // given
        let file = file_config("/a", "/b");

        // when
        let error = resolve_backups(file, env_of(&[]), &names(&["books"])).unwrap_err();

        // then
        assert!(error.to_string().contains("no [backup.<name>] sections"));
    }

    #[test]
    fn top_level_pair_and_named_sections_together_are_ambiguous() {
        // given
        let file = parse(
            r#"
            source = "/a"
            target = "/b"

            [backup.books]
            source = "/c"
            target = "/d"
        "#,
        );

        // when
        let error = resolve_backups(file, env_of(&[]), &[]).unwrap_err();

        // then
        assert!(error.to_string().contains("both top-level"));
    }

    #[test]
    fn invalid_backup_sections_are_rejected() {
        // given
        let missing_target = "[backup.books]\nsource = \"/a\"\n";
        let unknown_key = "[backup.books]\nsource = \"/a\"\ntarget = \"/b\"\nexclude = []\n";
        let empty_name = "[backup.\"\"]\nsource = \"/a\"\ntarget = \"/b\"\n";
        let zero_jobs = "[backup.books]\nsource = \"/a\"\ntarget = \"/b\"\njobs = 0\n";

        // when
        let parse_errors =
            [missing_target, unknown_key].map(|t| toml::from_str::<FileConfig>(t).is_err());
        let empty = resolve_backups(parse(empty_name), env_of(&[]), &[]);
        let zero = resolve_backups(parse(zero_jobs), env_of(&[]), &[]);

        // then
        assert_eq!(parse_errors, [true, true]);
        assert!(empty.is_err());
        assert_eq!(
            format!("{:#}", zero.unwrap_err()),
            "backup 'books': `jobs` must be at least 1"
        );
    }

    #[test]
    fn env_overrides_config_file() {
        // given
        let file = file_config("/file/src", "/file/dst");
        let env = env_of(&[(SOURCE_ENV, "/env/src"), (TARGET_ENV, "/env/dst")]);

        // when
        let config = resolve(file, env).unwrap();

        // then
        assert_eq!(config.source, PathBuf::from("/env/src"));
        assert_eq!(config.target, PathBuf::from("/env/dst"));
    }

    #[test]
    fn each_field_falls_back_to_config_file_independently() {
        // given
        let file = file_config("/file/src", "/file/dst");
        let env = env_of(&[(TARGET_ENV, "/env/dst")]);

        // when
        let config = resolve(file, env).unwrap();

        // then
        assert_eq!(config.source, PathBuf::from("/file/src"));
        assert_eq!(config.target, PathBuf::from("/env/dst"));
    }

    #[test]
    fn empty_env_value_is_ignored() {
        // given
        let file = file_config("/file/src", "/file/dst");
        let env = env_of(&[(SOURCE_ENV, "  ")]);

        // when
        let config = resolve(file, env).unwrap();

        // then
        assert_eq!(config.source, PathBuf::from("/file/src"));
    }

    #[test]
    fn missing_source_is_an_error_naming_the_env_var() {
        // given
        let file = FileConfig {
            target: Some("/dst".into()),
            ..FileConfig::default()
        };

        // when
        let error = resolve(file, env_of(&[])).unwrap_err();

        // then
        assert!(error.to_string().contains(SOURCE_ENV));
    }

    #[test]
    fn tilde_expands_to_home() {
        // given
        let file = file_config("~/Documents", "~");
        let env = env_of(&[("HOME", "/Users/me")]);

        // when
        let config = resolve(file, env).unwrap();

        // then
        assert_eq!(config.source, PathBuf::from("/Users/me/Documents"));
        assert_eq!(config.target, PathBuf::from("/Users/me"));
    }

    #[test]
    fn modify_window_defaults_to_two_seconds_and_can_be_set() {
        // given
        let default_file = file_config("/a", "/b");
        let custom_file = FileConfig {
            modify_window_secs: Some(0),
            ..file_config("/a", "/b")
        };

        // when
        let default_config = resolve(default_file, env_of(&[])).unwrap();
        let custom_config = resolve(custom_file, env_of(&[])).unwrap();

        // then
        assert_eq!(default_config.modify_window, Duration::from_secs(2));
        assert_eq!(custom_config.modify_window, Duration::ZERO);
    }

    #[test]
    fn jobs_default_to_four_and_zero_is_rejected() {
        // given
        let default_file = file_config("/a", "/b");
        let zero_file = FileConfig {
            jobs: Some(0),
            ..file_config("/a", "/b")
        };

        // when
        let default_config = resolve(default_file, env_of(&[])).unwrap();
        let zero = resolve(zero_file, env_of(&[]));

        // then
        assert_eq!(default_config.jobs, 4);
        assert!(zero.is_err());
    }

    #[test]
    fn mirroring_deletions_is_on_by_default_and_can_be_turned_off_per_backup() {
        // given
        let file = parse(
            r#"
            [backup.mirror]
            source = "/a"
            target = "/b"

            [backup.archive]
            source = "/c"
            target = "/d"
            mirror_deletions = false
        "#,
        );

        // when
        let backups = resolve_backups(file, env_of(&[]), &[]).unwrap();

        // then
        let mirrored: Vec<bool> = backups.iter().map(|b| b.config.mirror_deletions).collect();
        assert_eq!(mirrored, vec![true, false]);
    }

    #[test]
    fn top_level_mirror_deletions_applies_to_every_backup_unless_overridden() {
        // given
        let file = parse(
            r#"
            mirror_deletions = false

            [backup.archive]
            source = "/a"
            target = "/b"

            [backup.mirror]
            source = "/c"
            target = "/d"
            mirror_deletions = true
        "#,
        );

        // when
        let backups = resolve_backups(file, env_of(&[]), &[]).unwrap();

        // then
        let mirrored: Vec<bool> = backups.iter().map(|b| b.config.mirror_deletions).collect();
        assert_eq!(mirrored, vec![false, true]);
    }

    #[test]
    fn default_config_path_prefers_xdg_config_home() {
        // given
        let env = env_of(&[("XDG_CONFIG_HOME", "/xdg"), ("HOME", "/Users/me")]);

        // when
        let path = default_config_path(env);

        // then
        assert_eq!(path, Some(PathBuf::from("/xdg/backsync/config.toml")));
    }

    #[test]
    fn default_config_path_falls_back_to_home_dot_config() {
        // given
        let env = env_of(&[("HOME", "/Users/me")]);

        // when
        let path = default_config_path(env);

        // then
        assert_eq!(
            path,
            Some(PathBuf::from("/Users/me/.config/backsync/config.toml"))
        );
    }

    #[test]
    fn missing_optional_config_file_is_empty() {
        // given
        let dir = tempfile::tempdir().unwrap();

        // when
        let file = load_file(&dir.path().join("absent.toml"), false).unwrap();

        // then
        assert_eq!(file, FileConfig::default());
    }

    #[test]
    fn missing_required_config_file_is_an_error() {
        // given
        let dir = tempfile::tempdir().unwrap();

        // when
        let result = load_file(&dir.path().join("absent.toml"), true);

        // then
        assert!(result.is_err());
    }

    #[test]
    fn config_file_is_parsed_and_unknown_keys_rejected() {
        // given
        let dir = tempfile::tempdir().unwrap();
        let good = dir.path().join("good.toml");
        let bad = dir.path().join("bad.toml");
        fs::write(
            &good,
            "source = \"/a\"\ntarget = \"/b\"\nmodify_window_secs = 1\n",
        )
        .unwrap();
        fs::write(&bad, "sorce = \"/a\"\n").unwrap();

        // when
        let parsed = load_file(&good, true).unwrap();
        let rejected = load_file(&bad, true);

        // then
        assert_eq!(parsed.modify_window_secs, Some(1));
        assert!(rejected.is_err());
    }

    fn config_for(source: &Path, target: &Path) -> Config {
        Config {
            source: source.to_path_buf(),
            target: target.to_path_buf(),
            modify_window: Duration::from_secs(2),
            jobs: 4,
            mirror_deletions: true,
        }
    }

    #[test]
    fn validate_canonicalizes_existing_folders() {
        // given
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("src");
        let target = root.path().join("dst");
        fs::create_dir_all(&source).unwrap();
        fs::create_dir_all(&target).unwrap();

        // when
        let config = validate(config_for(&source.join("."), &target)).unwrap();

        // then
        assert_eq!(config.source, source.canonicalize().unwrap());
        assert_eq!(config.target, target.canonicalize().unwrap());
    }

    #[test]
    fn validate_rejects_missing_target_instead_of_creating_it() {
        // given
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("src");
        fs::create_dir_all(&source).unwrap();
        let target = root.path().join("unmounted");

        // when
        let error = validate(config_for(&source, &target)).unwrap_err();

        // then
        assert!(error.to_string().contains("does not exist"));
        assert!(!target.exists());
    }

    #[test]
    fn validate_rejects_same_and_nested_folders() {
        // given
        let root = tempfile::tempdir().unwrap();
        let outer = root.path().join("outer");
        let inner = outer.join("inner");
        fs::create_dir_all(&inner).unwrap();

        // when
        let same = validate(config_for(&outer, &outer));
        let source_inside_target = validate(config_for(&inner, &outer));
        let target_inside_source = validate(config_for(&outer, &inner));

        // then
        assert!(same.is_err());
        assert!(source_inside_target.is_err());
        assert!(target_inside_source.is_err());
    }

    #[test]
    fn validate_rejects_a_file_as_folder() {
        // given
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("src");
        fs::create_dir_all(&source).unwrap();
        let file = root.path().join("file.txt");
        fs::write(&file, "x").unwrap();

        // when
        let result = validate(config_for(&source, &file));

        // then
        assert!(result.is_err());
    }
}
