use std::collections::HashSet;
use std::fs;
use std::io;
use std::os::unix::fs::symlink;
use std::path::{Component, Path, PathBuf};
use std::process;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use filetime::FileTime;
use rayon::prelude::*;

use crate::plan::{CopyAction, CopyReason, Plan};
use crate::scan::Entry;

const TEMP_PREFIX: &str = ".backsync-tmp";

pub struct TargetRoot {
    root: PathBuf,
}

impl TargetRoot {
    pub fn new(canonical_root: PathBuf) -> Self {
        Self {
            root: canonical_root,
        }
    }

    pub fn guard(&self, relative: &Path) -> io::Result<PathBuf> {
        let inside = !relative.as_os_str().is_empty()
            && relative
                .components()
                .all(|c| matches!(c, Component::Normal(_)));
        if !inside {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!("refusing to touch a path outside the target: {relative:?}"),
            ));
        }
        Ok(self.root.join(relative))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event<'a> {
    Removed(&'a Path),
    CreatedDir(&'a Path),
    Copied(&'a Path, CopyReason),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncError {
    pub path: PathBuf,
    pub action: &'static str,
    pub message: String,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Outcome {
    pub removed: usize,
    pub removals_skipped: usize,
    pub not_attempted: usize,
    pub created_dirs: usize,
    pub copied_new: usize,
    pub updated: usize,
    pub bytes: u64,
    pub errors: Vec<SyncError>,
}

pub struct Options {
    pub allow_delete: bool,
    pub jobs: usize,
}

type Copier<'a> = dyn Fn(&CopyAction) -> io::Result<u64> + Sync + 'a;

enum CopyResult {
    Done(CopyReason, u64),
    Failed(SyncError),
    NotAttempted,
}

pub fn execute(
    plan: &Plan,
    source_root: &Path,
    target: &TargetRoot,
    options: &Options,
    on_event: &(dyn Fn(Event) + Sync),
) -> Outcome {
    let copier = |action: &CopyAction| copy_entry(source_root, target, &action.path, &action.entry);
    let run = || execute_with(plan, target, options.allow_delete, on_event, &copier);
    match rayon::ThreadPoolBuilder::new()
        .num_threads(options.jobs.max(1))
        .build()
    {
        Ok(pool) => pool.install(run),
        Err(_) => run(),
    }
}

fn execute_with(
    plan: &Plan,
    target: &TargetRoot,
    allow_delete: bool,
    on_event: &(dyn Fn(Event) + Sync),
    copier: &Copier,
) -> Outcome {
    let mut outcome = Outcome::default();
    let mut blocked: HashSet<&Path> = HashSet::new();

    if allow_delete {
        let failures: Vec<(&Path, SyncError)> = plan
            .remove
            .par_iter()
            .filter_map(|path| match remove(target, path) {
                Ok(()) => {
                    on_event(Event::Removed(path));
                    None
                }
                Err(error) => Some((path.as_path(), failure(path, "remove", &error))),
            })
            .collect();
        outcome.removed = plan.remove.len() - failures.len();
        for (path, error) in failures {
            blocked.insert(path);
            outcome.errors.push(error);
        }
    } else {
        outcome.removals_skipped = plan.remove.len();
        blocked.extend(plan.remove.iter().map(PathBuf::as_path));
    }

    for path in &plan.mkdir {
        if let Some(error) = blocked_error(&blocked, path, "create directory") {
            outcome.errors.push(error);
            continue;
        }
        match target.guard(path).and_then(fs::create_dir_all) {
            Ok(()) => {
                outcome.created_dirs += 1;
                on_event(Event::CreatedDir(path));
            }
            Err(error) => {
                outcome
                    .errors
                    .push(failure(path, "create directory", &error));
                blocked.insert(path);
            }
        }
    }

    let target_full = AtomicBool::new(false);
    let results: Vec<CopyResult> = plan
        .copy
        .par_iter()
        .map(|action| {
            if target_full.load(Ordering::Relaxed) {
                return CopyResult::NotAttempted;
            }
            if let Some(error) = blocked_error(&blocked, &action.path, "copy") {
                return CopyResult::Failed(error);
            }
            match copier(action) {
                Ok(bytes) => {
                    on_event(Event::Copied(&action.path, action.reason));
                    CopyResult::Done(action.reason, bytes)
                }
                Err(error) => {
                    if is_out_of_space(&error) {
                        target_full.store(true, Ordering::Relaxed);
                    }
                    CopyResult::Failed(failure(&action.path, "copy", &error))
                }
            }
        })
        .collect();
    for result in results {
        match result {
            CopyResult::Done(CopyReason::New, bytes) => {
                outcome.copied_new += 1;
                outcome.bytes += bytes;
            }
            CopyResult::Done(CopyReason::Changed, bytes) => {
                outcome.updated += 1;
                outcome.bytes += bytes;
            }
            CopyResult::Failed(error) => outcome.errors.push(error),
            CopyResult::NotAttempted => outcome.not_attempted += 1,
        }
    }
    outcome
}

fn is_out_of_space(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::StorageFull | io::ErrorKind::QuotaExceeded
    )
}

const REMOVE_DIR_ATTEMPTS: usize = 10;

fn remove(target: &TargetRoot, relative: &Path) -> io::Result<()> {
    let path = target.guard(relative)?;
    let result = match fs::symlink_metadata(&path) {
        Ok(metadata) if metadata.is_dir() => remove_dir_tree(&path),
        Ok(_) => fs::remove_file(&path),
        Err(error) => Err(error),
    };
    match result {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        other => other,
    }
}

/// exFAT directory listings shift while entries are deleted (and macOS deletes
/// each file's `._` sidecar alongside it), so a single `remove_dir_all` pass can
/// skip entries and fail with "directory not empty". Each pass removes most of
/// what remains, so repeating converges.
fn remove_dir_tree(path: &Path) -> io::Result<()> {
    let mut attempt = 1;
    loop {
        match fs::remove_dir_all(path) {
            Err(error)
                if error.kind() == io::ErrorKind::DirectoryNotEmpty
                    && attempt < REMOVE_DIR_ATTEMPTS =>
            {
                attempt += 1;
            }
            result => return result,
        }
    }
}

fn copy_entry(
    source_root: &Path,
    target: &TargetRoot,
    relative: &Path,
    entry: &Entry,
) -> io::Result<u64> {
    let destination = target.guard(relative)?;
    let temp = temp_path(&destination);
    let result = match entry {
        Entry::File { .. } => copy_file(&source_root.join(relative), &temp),
        Entry::Symlink { link } => symlink(link, &temp).map(|()| 0),
        Entry::Dir => Err(io::Error::other("directories are created, not copied")),
    }
    .and_then(|bytes| fs::rename(&temp, &destination).map(|()| bytes));
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

fn copy_file(source: &Path, temp: &Path) -> io::Result<u64> {
    let mtime = FileTime::from_last_modification_time(&fs::metadata(source)?);
    let bytes = fs::copy(source, temp)?;
    filetime::set_file_mtime(temp, mtime)?;
    Ok(bytes)
}

fn temp_path(destination: &Path) -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    destination.with_file_name(format!("{TEMP_PREFIX}.{}.{n}", process::id()))
}

fn blocked_error(blocked: &HashSet<&Path>, path: &Path, action: &'static str) -> Option<SyncError> {
    let blocker = path.ancestors().find(|a| blocked.contains(a))?;
    Some(SyncError {
        path: path.to_path_buf(),
        action,
        message: format!(
            "skipped because conflicting target entry {} was not removed",
            blocker.display()
        ),
    })
}

fn failure(path: &Path, action: &'static str, error: &io::Error) -> SyncError {
    SyncError {
        path: path.to_path_buf(),
        action,
        message: error.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;
    use std::sync::Mutex;
    use std::time::{Duration, SystemTime};

    use super::*;

    struct Fixture {
        _dir: tempfile::TempDir,
        source: PathBuf,
        target: PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let source = dir.path().join("src");
            let target = dir.path().join("dst");
            fs::create_dir_all(&source).unwrap();
            fs::create_dir_all(&target).unwrap();
            Self {
                source: source.canonicalize().unwrap(),
                target: target.canonicalize().unwrap(),
                _dir: dir,
            }
        }

        fn run(&self, plan: &Plan, allow_delete: bool) -> Outcome {
            execute(
                plan,
                &self.source,
                &TargetRoot::new(self.target.clone()),
                &Options {
                    allow_delete,
                    jobs: 2,
                },
                &|_| {},
            )
        }
    }

    fn write(root: &Path, relative: &str, content: &str) {
        let path = root.join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }

    fn file_action(path: &str, reason: CopyReason) -> CopyAction {
        CopyAction {
            path: path.into(),
            entry: Entry::File {
                size: 0,
                mtime: SystemTime::UNIX_EPOCH,
            },
            reason,
        }
    }

    fn mtime(path: &Path) -> FileTime {
        FileTime::from_last_modification_time(&fs::metadata(path).unwrap())
    }

    fn no_temp_files(root: &Path) -> bool {
        walk(root).iter().all(|p| {
            !p.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with(TEMP_PREFIX)
        })
    }

    fn walk(root: &Path) -> Vec<PathBuf> {
        let mut out = Vec::new();
        for entry in fs::read_dir(root).unwrap() {
            let path = entry.unwrap().path();
            if fs::symlink_metadata(&path).unwrap().is_dir() {
                out.extend(walk(&path));
            }
            out.push(path);
        }
        out
    }

    #[test]
    fn guard_rejects_paths_escaping_the_target() {
        // given
        let target = TargetRoot::new("/backup".into());

        // when
        let rejected = ["", "..", "../x", "a/../../x", "/etc/passwd", "./a"]
            .map(|p| target.guard(Path::new(p)).is_err());
        let accepted = target.guard(Path::new("a/b.txt")).unwrap();

        // then
        assert_eq!(rejected, [true; 6]);
        assert_eq!(accepted, PathBuf::from("/backup/a/b.txt"));
    }

    #[test]
    fn copies_new_file_with_content_and_mtime() {
        // given
        let fx = Fixture::new();
        write(&fx.source, "d/a.txt", "hello");
        let old = FileTime::from_system_time(SystemTime::now() - Duration::from_secs(86_400));
        filetime::set_file_mtime(fx.source.join("d/a.txt"), old).unwrap();
        let plan = Plan {
            mkdir: vec!["d".into()],
            copy: vec![file_action("d/a.txt", CopyReason::New)],
            ..Plan::default()
        };

        // when
        let outcome = fx.run(&plan, true);

        // then
        assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
        assert_eq!(
            fs::read_to_string(fx.target.join("d/a.txt")).unwrap(),
            "hello"
        );
        assert_eq!(mtime(&fx.target.join("d/a.txt")), old);
        assert_eq!(
            (outcome.copied_new, outcome.created_dirs, outcome.bytes),
            (1, 1, 5)
        );
        assert!(no_temp_files(&fx.target));
    }

    #[test]
    fn overwrites_changed_file() {
        // given
        let fx = Fixture::new();
        write(&fx.source, "a.txt", "new content");
        write(&fx.target, "a.txt", "old");
        let plan = Plan {
            copy: vec![file_action("a.txt", CopyReason::Changed)],
            ..Plan::default()
        };

        // when
        let outcome = fx.run(&plan, true);

        // then
        assert_eq!(outcome.updated, 1);
        assert_eq!(
            fs::read_to_string(fx.target.join("a.txt")).unwrap(),
            "new content"
        );
    }

    #[test]
    fn overwrites_read_only_backup_file() {
        // given
        let fx = Fixture::new();
        write(&fx.source, "ro.txt", "v2");
        write(&fx.target, "ro.txt", "v1");
        fs::set_permissions(fx.target.join("ro.txt"), fs::Permissions::from_mode(0o444)).unwrap();
        let plan = Plan {
            copy: vec![file_action("ro.txt", CopyReason::Changed)],
            ..Plan::default()
        };

        // when
        let outcome = fx.run(&plan, true);

        // then
        assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
        assert_eq!(fs::read_to_string(fx.target.join("ro.txt")).unwrap(), "v2");
    }

    #[test]
    fn creates_symlink_as_link() {
        // given
        let fx = Fixture::new();
        let plan = Plan {
            copy: vec![CopyAction {
                path: "l".into(),
                entry: Entry::Symlink {
                    link: "some/where".into(),
                },
                reason: CopyReason::New,
            }],
            ..Plan::default()
        };

        // when
        let outcome = fx.run(&plan, true);

        // then
        assert!(outcome.errors.is_empty());
        assert_eq!(
            fs::read_link(fx.target.join("l")).unwrap(),
            PathBuf::from("some/where")
        );
    }

    #[test]
    fn removes_files_dirs_and_links_without_following_links() {
        // given
        let fx = Fixture::new();
        let outside = tempfile::tempdir().unwrap();
        write(outside.path(), "precious.txt", "keep me");
        write(&fx.target, "gone.txt", "x");
        write(&fx.target, "old/deep/x.txt", "x");
        symlink(outside.path(), fx.target.join("link")).unwrap();
        let plan = Plan {
            remove: vec!["gone.txt".into(), "link".into(), "old".into()],
            ..Plan::default()
        };

        // when
        let outcome = fx.run(&plan, true);

        // then
        assert_eq!(outcome.removed, 3);
        assert!(fs::read_dir(&fx.target).unwrap().next().is_none());
        assert_eq!(
            fs::read_to_string(outside.path().join("precious.txt")).unwrap(),
            "keep me"
        );
    }

    #[test]
    fn deletion_disabled_keeps_target_and_blocks_conflicting_writes() {
        // given
        let fx = Fixture::new();
        write(&fx.source, "x", "file now");
        write(&fx.target, "x/old.txt", "was a dir");
        write(&fx.target, "extra.txt", "x");
        let plan = Plan {
            remove: vec!["extra.txt".into(), "x".into()],
            copy: vec![file_action("x", CopyReason::Changed)],
            ..Plan::default()
        };

        // when
        let outcome = fx.run(&plan, false);

        // then
        assert_eq!(outcome.removals_skipped, 2);
        assert!(fx.target.join("extra.txt").exists());
        assert!(fx.target.join("x/old.txt").exists());
        assert_eq!(outcome.errors.len(), 1);
        assert_eq!(outcome.errors[0].path, PathBuf::from("x"));
    }

    #[test]
    fn removing_an_entry_that_is_already_gone_succeeds() {
        // given
        let fx = Fixture::new();
        let plan = Plan {
            remove: vec!["._vanished-with-its-owner".into()],
            ..Plan::default()
        };

        // when
        let outcome = fx.run(&plan, true);

        // then
        assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
        assert_eq!(outcome.removed, 1);
    }

    #[test]
    fn full_target_stops_further_copies() {
        // given
        let fx = Fixture::new();
        let plan = Plan {
            copy: ["a", "b", "c", "d"]
                .map(|p| file_action(p, CopyReason::New))
                .to_vec(),
            ..Plan::default()
        };
        let attempts = Mutex::new(Vec::new());
        let copier = |action: &CopyAction| {
            attempts.lock().unwrap().push(action.path.clone());
            if action.path == Path::new("a") {
                Ok(1)
            } else {
                Err(io::Error::from(io::ErrorKind::StorageFull))
            }
        };
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(1)
            .build()
            .unwrap();

        // when
        let outcome = pool.install(|| {
            execute_with(
                &plan,
                &TargetRoot::new(fx.target.clone()),
                true,
                &|_| {},
                &copier,
            )
        });

        // then
        assert_eq!(outcome.copied_new, 1);
        assert_eq!(outcome.errors.len(), 1);
        assert_eq!(outcome.not_attempted, 2);
        assert_eq!(attempts.into_inner().unwrap().len(), 2);
    }

    #[test]
    fn copy_errors_are_collected_and_other_copies_continue() {
        // given
        let fx = Fixture::new();
        write(&fx.source, "ok.txt", "x");
        let plan = Plan {
            copy: vec![
                file_action("missing.txt", CopyReason::New),
                file_action("ok.txt", CopyReason::New),
            ],
            ..Plan::default()
        };

        // when
        let outcome = fx.run(&plan, true);

        // then
        assert_eq!(outcome.copied_new, 1);
        assert_eq!(outcome.errors.len(), 1);
        assert_eq!(outcome.errors[0].path, PathBuf::from("missing.txt"));
        assert!(no_temp_files(&fx.target));
    }

    #[test]
    fn source_is_never_written() {
        // given
        let fx = Fixture::new();
        write(&fx.source, "a.txt", "hello");
        fs::set_permissions(&fx.source, fs::Permissions::from_mode(0o555)).unwrap();
        let plan = Plan {
            copy: vec![file_action("a.txt", CopyReason::New)],
            ..Plan::default()
        };

        // when
        let outcome = fx.run(&plan, true);
        fs::set_permissions(&fx.source, fs::Permissions::from_mode(0o755)).unwrap();

        // then
        assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
        assert_eq!(fs::read_dir(&fx.source).unwrap().count(), 1);
    }

    #[test]
    fn reports_each_completed_action() {
        // given
        let fx = Fixture::new();
        write(&fx.source, "d/a", "x");
        write(&fx.target, "old", "x");
        let plan = Plan {
            remove: vec!["old".into()],
            mkdir: vec!["d".into()],
            copy: vec![file_action("d/a", CopyReason::New)],
            ..Plan::default()
        };
        let events = Mutex::new(Vec::new());

        // when
        execute(
            &plan,
            &fx.source,
            &TargetRoot::new(fx.target.clone()),
            &Options {
                allow_delete: true,
                jobs: 2,
            },
            &|event| events.lock().unwrap().push(format!("{event:?}")),
        );

        // then
        assert_eq!(
            events.into_inner().unwrap(),
            vec![
                "Removed(\"old\")",
                "CreatedDir(\"d\")",
                "Copied(\"d/a\", New)"
            ]
        );
    }
}
