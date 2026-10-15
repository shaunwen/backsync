use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use assert_cmd::Command;
use filetime::FileTime;
use predicates::str::contains;
use tempfile::TempDir;

#[derive(Debug, Clone, PartialEq, Eq)]
enum Node {
    Dir,
    File(Vec<u8>),
    Link(PathBuf),
}

struct Env {
    dir: TempDir,
}

impl Env {
    fn new() -> Self {
        let env = Self {
            dir: tempfile::tempdir().unwrap(),
        };
        fs::create_dir_all(env.source()).unwrap();
        fs::create_dir_all(env.target()).unwrap();
        fs::create_dir_all(env.dir.path().join("xdg")).unwrap();
        env
    }

    fn source(&self) -> PathBuf {
        self.dir.path().join("source")
    }

    fn target(&self) -> PathBuf {
        self.dir.path().join("target")
    }

    fn command(&self) -> Command {
        let mut cmd = Command::cargo_bin("backsync").unwrap();
        cmd.env("XDG_CONFIG_HOME", self.dir.path().join("xdg"))
            .env("BACKSYNC_SOURCE", self.source())
            .env("BACKSYNC_TARGET", self.target());
        cmd
    }

    fn sync(&self) -> assert_cmd::assert::Assert {
        self.command().assert()
    }
}

fn write(root: &Path, relative: &str, content: &str) {
    let path = root.join(relative);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, content).unwrap();
}

fn set_mtime(path: &Path, secs_ago: u64) {
    let time = SystemTime::now() - Duration::from_secs(secs_ago);
    filetime::set_symlink_file_times(
        path,
        FileTime::from_system_time(time),
        FileTime::from_system_time(time),
    )
    .unwrap();
}

fn snapshot(root: &Path) -> BTreeMap<PathBuf, Node> {
    let mut nodes = BTreeMap::new();
    collect(root, root, &mut nodes);
    nodes
}

fn collect(root: &Path, dir: &Path, nodes: &mut BTreeMap<PathBuf, Node>) {
    for entry in fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        let relative = path.strip_prefix(root).unwrap().to_path_buf();
        let meta = fs::symlink_metadata(&path).unwrap();
        if meta.file_type().is_symlink() {
            nodes.insert(relative, Node::Link(fs::read_link(&path).unwrap()));
        } else if meta.is_dir() {
            nodes.insert(relative, Node::Dir);
            collect(root, &path, nodes);
        } else {
            nodes.insert(relative, Node::File(fs::read(&path).unwrap()));
        }
    }
}

fn sample_tree(root: &Path) {
    write(root, "top.txt", "top");
    write(root, "docs/a.txt", "alpha");
    write(root, "docs/deep/b.txt", "bravo");
    write(root, ".hidden/.env", "secret");
    fs::create_dir_all(root.join("empty")).unwrap();
}

#[test]
fn first_sync_mirrors_the_whole_tree() {
    // given
    let env = Env::new();
    sample_tree(&env.source());

    // when
    let assert = env.sync();

    // then
    assert
        .success()
        .stdout(contains("4 new, 0 updated, 0 removed"));
    assert_eq!(snapshot(&env.target()), snapshot(&env.source()));
}

#[test]
fn second_sync_leaves_identical_files_untouched() {
    // given
    let env = Env::new();
    sample_tree(&env.source());
    env.sync().success();
    let backup_file = env.target().join("docs/a.txt");
    let before = fs::metadata(&backup_file).unwrap();

    // when
    let assert = env.sync();

    // then
    assert
        .success()
        .stdout(contains("0 new, 0 updated, 0 removed, 4 unchanged"));
    let after = fs::metadata(&backup_file).unwrap();
    assert_eq!(after.ino(), before.ino());
    assert_eq!(after.mtime(), before.mtime());
}

#[test]
fn updated_file_overwrites_backup_in_place_of_tree() {
    // given
    let env = Env::new();
    sample_tree(&env.source());
    set_mtime(&env.source().join("docs/deep/b.txt"), 3600);
    env.sync().success();
    write(&env.source(), "docs/deep/b.txt", "bravo v2");

    // when
    let assert = env.sync();

    // then
    assert.success().stdout(contains("0 new, 1 updated"));
    assert_eq!(
        fs::read_to_string(env.target().join("docs/deep/b.txt")).unwrap(),
        "bravo v2"
    );
    assert_eq!(snapshot(&env.target()), snapshot(&env.source()));
}

#[test]
fn files_and_folders_deleted_from_source_are_removed_from_backup() {
    // given
    let env = Env::new();
    sample_tree(&env.source());
    env.sync().success();
    fs::remove_file(env.source().join("top.txt")).unwrap();
    fs::remove_dir_all(env.source().join("docs")).unwrap();

    // when
    let assert = env.sync();

    // then
    assert.success().stdout(contains("2 removed"));
    assert_eq!(snapshot(&env.target()), snapshot(&env.source()));
}

#[test]
fn source_is_never_modified() {
    // given
    let env = Env::new();
    sample_tree(&env.source());
    symlink("docs", env.source().join("link")).unwrap();
    write(&env.target(), "extra/stale.txt", "only in backup");
    write(&env.target(), "top.txt", "different");
    let before = snapshot(&env.source());

    // when
    env.sync().success();
    env.command().arg("--checksum").assert().success();

    // then
    assert_eq!(snapshot(&env.source()), before);
    assert_eq!(snapshot(&env.target()), before);
}

#[test]
fn type_changes_are_mirrored() {
    // given
    let env = Env::new();
    write(&env.source(), "flip", "now a file");
    write(&env.source(), "flop/inner.txt", "now a dir");
    write(&env.target(), "flip/old.txt", "was a dir");
    write(&env.target(), "flop", "was a file");

    // when
    let assert = env.sync();

    // then
    assert.success();
    assert_eq!(snapshot(&env.target()), snapshot(&env.source()));
}

#[test]
fn symlinks_are_copied_as_links() {
    // given
    let env = Env::new();
    let outside = tempfile::tempdir().unwrap();
    write(outside.path(), "big.bin", "not copied");
    symlink(outside.path(), env.source().join("external")).unwrap();

    // when
    env.sync().success();

    // then
    assert_eq!(
        fs::read_link(env.target().join("external")).unwrap(),
        outside.path()
    );
    assert_eq!(snapshot(&env.target()).len(), 1);
}

#[test]
fn checksum_detects_change_hidden_from_size_and_mtime() {
    // given
    let env = Env::new();
    write(&env.source(), "a.txt", "aaaa");
    env.sync().success();
    let mtime =
        FileTime::from_last_modification_time(&fs::metadata(env.source().join("a.txt")).unwrap());
    write(&env.source(), "a.txt", "bbbb");
    filetime::set_file_mtime(env.source().join("a.txt"), mtime).unwrap();

    // when
    let plain = env.sync();
    let checked = env.command().arg("--checksum").assert();

    // then
    plain.success().stdout(contains("0 updated"));
    checked.success().stdout(contains("1 updated"));
    assert_eq!(
        fs::read_to_string(env.target().join("a.txt")).unwrap(),
        "bbbb"
    );
}

#[test]
fn dry_run_lists_actions_and_changes_nothing() {
    // given
    let env = Env::new();
    write(&env.source(), "new.txt", "n");
    write(&env.target(), "stale.txt", "s");
    let before = snapshot(&env.target());

    // when
    let assert = env.command().arg("--dry-run").assert();

    // then
    assert
        .success()
        .stdout(contains("remove stale.txt"))
        .stdout(contains("new    new.txt"))
        .stdout(contains("would copy 1 new, update 0, remove 1"));
    assert_eq!(snapshot(&env.target()), before);
}

#[test]
fn stale_temp_files_from_interrupted_runs_are_cleaned_up() {
    // given
    let env = Env::new();
    write(&env.source(), "a.txt", "a");
    write(&env.target(), ".backsync-tmp.123.0", "half written");

    // when
    env.sync().success();

    // then
    assert_eq!(snapshot(&env.target()), snapshot(&env.source()));
}

#[test]
fn macos_volume_metadata_on_target_root_is_left_alone() {
    // given
    let env = Env::new();
    write(&env.source(), "a.txt", "a");
    write(&env.target(), ".Spotlight-V100/store.db", "index");
    write(&env.target(), ".Trashes/501/x", "trash");

    // when
    env.sync().success();

    // then
    assert!(env.target().join(".Spotlight-V100/store.db").exists());
    assert!(env.target().join(".Trashes/501/x").exists());
}

#[test]
fn unreadable_source_folder_prevents_all_deletions() {
    // given
    let env = Env::new();
    write(&env.source(), "private/doc.txt", "p");
    write(&env.source(), "ok.txt", "ok");
    env.sync().success();
    fs::remove_file(env.source().join("ok.txt")).unwrap();
    let private = env.source().join("private");
    fs::set_permissions(&private, fs::Permissions::from_mode(0o000)).unwrap();

    // when
    let assert = env.sync();
    fs::set_permissions(&private, fs::Permissions::from_mode(0o755)).unwrap();

    // then
    assert
        .code(1)
        .stderr(contains("nothing will be removed"))
        .stdout(contains("removals skipped"));
    assert!(env.target().join("private/doc.txt").exists());
    assert!(env.target().join("ok.txt").exists());
}

#[test]
fn env_overrides_config_file() {
    // given
    let env = Env::new();
    write(&env.source(), "from-env.txt", "e");
    let config_source = env.dir.path().join("config-source");
    write(&config_source, "from-config.txt", "c");
    let config = env.dir.path().join("config.toml");
    fs::write(
        &config,
        format!(
            "source = {:?}\ntarget = {:?}\n",
            config_source,
            env.dir.path().join("unused")
        ),
    )
    .unwrap();

    // when
    let assert = env.command().arg("--config").arg(&config).assert();

    // then
    assert.success();
    assert!(env.target().join("from-env.txt").exists());
    assert!(!env.target().join("from-config.txt").exists());
}

#[test]
fn default_config_file_is_used_when_env_is_not_set() {
    // given
    let env = Env::new();
    write(&env.source(), "a.txt", "a");
    let config_dir = env.dir.path().join("xdg/backsync");
    fs::create_dir_all(&config_dir).unwrap();
    fs::write(
        config_dir.join("config.toml"),
        format!("source = {:?}\ntarget = {:?}\n", env.source(), env.target()),
    )
    .unwrap();

    // when
    let assert = env
        .command()
        .env_remove("BACKSYNC_SOURCE")
        .env_remove("BACKSYNC_TARGET")
        .assert();

    // then
    assert.success();
    assert!(env.target().join("a.txt").exists());
}

#[test]
fn missing_configuration_explains_how_to_set_it() {
    // given
    let env = Env::new();

    // when
    let assert = env
        .command()
        .env_remove("BACKSYNC_SOURCE")
        .env_remove("BACKSYNC_TARGET")
        .assert();

    // then
    assert.code(2).stderr(contains("BACKSYNC_SOURCE"));
}

#[test]
fn unmounted_target_is_refused_and_not_created() {
    // given
    let env = Env::new();
    write(&env.source(), "a.txt", "a");
    let unmounted = env.dir.path().join("Volumes/Backup");

    // when
    let assert = env.command().env("BACKSYNC_TARGET", &unmounted).assert();

    // then
    assert.code(2).stderr(contains("is the drive mounted?"));
    assert!(!unmounted.exists());
}

#[test]
fn nested_source_and_target_are_refused() {
    // given
    let env = Env::new();
    write(&env.source(), "a.txt", "a");
    let inside_source = env.source().join("backup");
    fs::create_dir_all(&inside_source).unwrap();

    // when
    let target_in_source = env
        .command()
        .env("BACKSYNC_TARGET", &inside_source)
        .assert();
    let source_in_target = env
        .command()
        .env("BACKSYNC_SOURCE", &inside_source)
        .env("BACKSYNC_TARGET", env.source())
        .assert();

    // then
    target_in_source.code(2).stderr(contains("inside source"));
    source_in_target.code(2).stderr(contains("inside target"));
    assert!(env.source().join("a.txt").exists());
}

#[test]
fn empty_source_is_refused_unless_allowed() {
    // given
    let env = Env::new();
    write(&env.target(), "precious.txt", "backup");

    // when
    let refused = env.sync();
    let allowed = env.command().arg("--allow-empty-source").assert();

    // then
    refused.code(2).stderr(contains("--allow-empty-source"));
    allowed.success().stdout(contains("1 removed"));
    assert!(snapshot(&env.target()).is_empty());
}

#[test]
fn many_files_sync_and_resync() {
    // given
    let env = Env::new();
    for dir in 0..20 {
        for file in 0..50 {
            write(
                &env.source(),
                &format!("d{dir}/f{file}.txt"),
                &format!("{dir}-{file}"),
            );
        }
    }

    // when
    let first = env.sync();
    let second = env.sync();

    // then
    first.success().stdout(contains("1000 new"));
    second.success().stdout(contains("1000 unchanged"));
    assert_eq!(snapshot(&env.target()), snapshot(&env.source()));
}

#[test]
fn works_when_the_global_thread_pool_is_tiny() {
    // given
    let env = Env::new();
    sample_tree(&env.source());

    // when
    let assert = env.command().env("RAYON_NUM_THREADS", "1").assert();

    // then
    assert.success().stdout(contains("4 new"));
    assert_eq!(snapshot(&env.target()), snapshot(&env.source()));
}
