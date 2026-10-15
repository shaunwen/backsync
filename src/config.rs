use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::Deserialize;

pub const SOURCE_ENV: &str = "BACKSYNC_SOURCE";
pub const TARGET_ENV: &str = "BACKSYNC_TARGET";
const DEFAULT_MODIFY_WINDOW_SECS: u64 = 2;

#[derive(Debug, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileConfig {
    pub source: Option<String>,
    pub target: Option<String>,
    pub modify_window_secs: Option<u64>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Config {
    pub source: PathBuf,
    pub target: PathBuf,
    pub modify_window: Duration,
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

pub fn resolve(file: FileConfig, env: impl Fn(&str) -> Option<String>) -> Result<Config> {
    let home = non_empty(env("HOME"));
    let pick = |env_name: &str, file_value: Option<String>, field: &str| -> Result<PathBuf> {
        let raw = non_empty(env(env_name))
            .or_else(|| non_empty(file_value))
            .with_context(|| {
                format!("{field} folder is not set: set {env_name} or `{field}` in the config file")
            })?;
        Ok(expand_tilde(&raw, home.as_deref()))
    };
    Ok(Config {
        source: pick(SOURCE_ENV, file.source, "source")?,
        target: pick(TARGET_ENV, file.target, "target")?,
        modify_window: Duration::from_secs(
            file.modify_window_secs
                .unwrap_or(DEFAULT_MODIFY_WINDOW_SECS),
        ),
    })
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
            modify_window_secs: None,
        }
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
