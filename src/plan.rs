use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
use std::mem::discriminant;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use rayon::prelude::*;

use crate::scan::Entry;

pub type Entries = HashMap<PathBuf, Entry>;

pub const TEMP_PREFIX: &str = ".backsync-tmp";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Compare {
    Metadata { modify_window: Duration },
    Content,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CopyReason {
    New,
    Changed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CopyAction {
    pub path: PathBuf,
    pub entry: Entry,
    pub reason: CopyReason,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Removals {
    All,
    KeepDeleted,
    None,
}

/**
`remove` holds target entries deleted from the source. `replace` holds
entries that must go even when deleted files are kept: entries whose type
changed in the source, and temp files left by an interrupted run.
*/
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Plan {
    pub remove: Vec<PathBuf>,
    pub replace: Vec<PathBuf>,
    pub mkdir: Vec<PathBuf>,
    pub copy: Vec<CopyAction>,
    pub unchanged: usize,
}

impl Plan {
    pub fn is_empty(&self) -> bool {
        self.remove.is_empty()
            && self.replace.is_empty()
            && self.mkdir.is_empty()
            && self.copy.is_empty()
    }

    pub fn removals(&self, mode: Removals) -> Vec<&Path> {
        let removed: HashSet<&Path> = self.remove.iter().map(PathBuf::as_path).collect();
        let mut paths: Vec<&Path> = match mode {
            Removals::All => self
                .remove
                .iter()
                .chain(
                    self.replace
                        .iter()
                        .filter(|path| !path.ancestors().any(|a| removed.contains(a))),
                )
                .map(PathBuf::as_path)
                .collect(),
            Removals::KeepDeleted => self.replace.iter().map(PathBuf::as_path).collect(),
            Removals::None => Vec::new(),
        };
        paths.sort();
        paths
    }
}

enum Verdict {
    Unchanged,
    Copy(CopyAction),
    Mkdir(PathBuf),
    DirExists,
}

pub fn diff(
    source: &Entries,
    target: &Entries,
    compare: Compare,
    same_content: impl Fn(&Path) -> bool + Sync,
) -> Plan {
    let verdicts: Vec<Verdict> = source
        .par_iter()
        .map(|(path, entry)| judge(path, entry, target.get(path), compare, &same_content))
        .collect();

    let mut deleted = Vec::new();
    let mut replaced = Vec::new();
    for (path, entry) in target {
        match source.get(path) {
            Some(src) if discriminant(src) != discriminant(entry) => replaced.push(path.clone()),
            Some(_) => {}
            None if is_temp_file(path) => replaced.push(path.clone()),
            None if !is_sidecar_of_mirrored_entry(path, source) => deleted.push(path.clone()),
            None => {}
        }
    }
    let replace = topmost(replaced);
    let replaced: HashSet<&Path> = replace.iter().map(PathBuf::as_path).collect();
    deleted.retain(|path| !path.ancestors().any(|a| replaced.contains(a)));
    let mut plan = Plan {
        remove: topmost(deleted),
        replace,
        ..Plan::default()
    };
    for verdict in verdicts {
        match verdict {
            Verdict::Unchanged => plan.unchanged += 1,
            Verdict::Copy(action) => plan.copy.push(action),
            Verdict::Mkdir(path) => plan.mkdir.push(path),
            Verdict::DirExists => {}
        }
    }
    plan.remove.sort();
    plan.replace.sort();
    plan.mkdir.sort();
    plan.copy.sort_by(|a, b| a.path.cmp(&b.path));
    plan
}

fn judge(
    path: &Path,
    entry: &Entry,
    existing: Option<&Entry>,
    compare: Compare,
    same_content: &(impl Fn(&Path) -> bool + Sync),
) -> Verdict {
    let copy = |reason| {
        Verdict::Copy(CopyAction {
            path: path.to_path_buf(),
            entry: entry.clone(),
            reason,
        })
    };
    match (entry, existing) {
        (Entry::Dir, Some(Entry::Dir)) => Verdict::DirExists,
        (Entry::Dir, _) => Verdict::Mkdir(path.to_path_buf()),
        (_, None) => copy(CopyReason::New),
        (
            Entry::Symlink { link },
            Some(Entry::Symlink {
                link: existing_link,
            }),
        ) => {
            if link == existing_link {
                Verdict::Unchanged
            } else {
                copy(CopyReason::Changed)
            }
        }
        (
            Entry::File { size, mtime },
            Some(Entry::File {
                size: existing_size,
                mtime: existing_mtime,
            }),
        ) => {
            let same = size == existing_size
                && match compare {
                    Compare::Metadata { modify_window } => {
                        within(*mtime, *existing_mtime, modify_window)
                    }
                    Compare::Content => same_content(path),
                };
            if same {
                Verdict::Unchanged
            } else {
                copy(CopyReason::Changed)
            }
        }
        _ => copy(CopyReason::Changed),
    }
}

fn within(a: SystemTime, b: SystemTime, window: Duration) -> bool {
    let gap = a.duration_since(b).unwrap_or_else(|e| e.duration());
    gap <= window
}

/// On file systems without native extended attributes (exFAT, FAT32), macOS
/// stores a file's attributes in a `._<name>` AppleDouble sidecar. Copying
/// creates it, and deleting or renaming the owner removes it, so it must not be
/// treated as an extraneous file while its owner is part of the mirror.
fn is_sidecar_of_mirrored_entry(path: &Path, source: &Entries) -> bool {
    let Some(name) = path.file_name() else {
        return false;
    };
    match name.as_bytes().strip_prefix(b"._") {
        Some(owner) if !owner.is_empty() => {
            source.contains_key(&path.with_file_name(OsStr::from_bytes(owner)))
        }
        _ => false,
    }
}

fn is_temp_file(path: &Path) -> bool {
    path.file_name()
        .is_some_and(|name| name.as_bytes().starts_with(TEMP_PREFIX.as_bytes()))
}

fn topmost(candidates: Vec<PathBuf>) -> Vec<PathBuf> {
    let set: HashSet<&Path> = candidates.iter().map(PathBuf::as_path).collect();
    candidates
        .iter()
        .filter(|path| !path.ancestors().skip(1).any(|a| set.contains(a)))
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const WINDOW: Compare = Compare::Metadata {
        modify_window: Duration::from_secs(2),
    };

    fn at(secs: u64) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(secs)
    }

    fn file(size: u64, secs: u64) -> Entry {
        Entry::File {
            size,
            mtime: at(secs),
        }
    }

    fn link(to: &str) -> Entry {
        Entry::Symlink { link: to.into() }
    }

    fn entries(items: &[(&str, Entry)]) -> Entries {
        items
            .iter()
            .map(|(path, entry)| (PathBuf::from(path), entry.clone()))
            .collect()
    }

    fn paths(items: &[&str]) -> Vec<PathBuf> {
        items.iter().map(PathBuf::from).collect()
    }

    fn copied(plan: &Plan) -> Vec<(PathBuf, CopyReason)> {
        plan.copy
            .iter()
            .map(|c| (c.path.clone(), c.reason))
            .collect()
    }

    fn never_called(_: &Path) -> bool {
        panic!("content comparison must not run")
    }

    #[test]
    fn identical_trees_produce_an_empty_plan() {
        // given
        let tree = entries(&[("d", Entry::Dir), ("d/a", file(3, 100)), ("l", link("d"))]);

        // when
        let plan = diff(&tree, &tree, WINDOW, never_called);

        // then
        assert!(plan.is_empty());
        assert_eq!(plan.unchanged, 2);
    }

    #[test]
    fn missing_files_and_dirs_are_created() {
        // given
        let source = entries(&[
            ("d", Entry::Dir),
            ("d/e", Entry::Dir),
            ("d/e/a", file(1, 1)),
        ]);

        // when
        let plan = diff(&source, &Entries::new(), WINDOW, never_called);

        // then
        assert_eq!(plan.mkdir, paths(&["d", "d/e"]));
        assert_eq!(copied(&plan), vec![("d/e/a".into(), CopyReason::New)]);
        assert!(plan.remove.is_empty());
    }

    #[test]
    fn size_change_is_copied() {
        // given
        let source = entries(&[("a", file(2, 100))]);
        let target = entries(&[("a", file(1, 100))]);

        // when
        let plan = diff(&source, &target, WINDOW, never_called);

        // then
        assert_eq!(copied(&plan), vec![("a".into(), CopyReason::Changed)]);
    }

    #[test]
    fn mtime_change_beyond_window_is_copied_and_within_window_is_not() {
        // given
        let source = entries(&[("old", file(1, 100)), ("new", file(1, 100))]);
        let target = entries(&[("old", file(1, 98)), ("new", file(1, 97))]);

        // when
        let plan = diff(&source, &target, WINDOW, never_called);

        // then
        assert_eq!(copied(&plan), vec![("new".into(), CopyReason::Changed)]);
        assert_eq!(plan.unchanged, 1);
    }

    #[test]
    fn target_newer_than_source_is_still_overwritten() {
        // given
        let source = entries(&[("a", file(1, 100))]);
        let target = entries(&[("a", file(1, 500))]);

        // when
        let plan = diff(&source, &target, WINDOW, never_called);

        // then
        assert_eq!(copied(&plan), vec![("a".into(), CopyReason::Changed)]);
    }

    #[test]
    fn content_mode_uses_comparator_only_for_same_size_files() {
        // given
        let source = entries(&[
            ("same", file(1, 1)),
            ("edited", file(1, 1)),
            ("grown", file(9, 1)),
        ]);
        let target = entries(&[
            ("same", file(1, 1)),
            ("edited", file(1, 1)),
            ("grown", file(1, 1)),
        ]);

        // when
        let plan = diff(&source, &target, Compare::Content, |path| {
            assert_ne!(path, Path::new("grown"));
            path == Path::new("same")
        });

        // then
        assert_eq!(
            copied(&plan),
            vec![
                ("edited".into(), CopyReason::Changed),
                ("grown".into(), CopyReason::Changed)
            ]
        );
        assert_eq!(plan.unchanged, 1);
    }

    #[test]
    fn extraneous_target_entries_are_removed_topmost_only() {
        // given
        let source = entries(&[("keep", file(1, 1))]);
        let target = entries(&[
            ("keep", file(1, 1)),
            ("gone.txt", file(1, 1)),
            ("old", Entry::Dir),
            ("old/inner", Entry::Dir),
            ("old/inner/x", file(1, 1)),
        ]);

        // when
        let plan = diff(&source, &target, WINDOW, never_called);

        // then
        assert_eq!(plan.remove, paths(&["gone.txt", "old"]));
    }

    #[test]
    fn type_change_removes_then_recreates() {
        // given
        let source = entries(&[
            ("was_dir", file(1, 1)),
            ("was_file", Entry::Dir),
            ("was_link", file(1, 1)),
        ]);
        let target = entries(&[
            ("was_dir", Entry::Dir),
            ("was_dir/child", file(1, 1)),
            ("was_file", file(1, 1)),
            ("was_link", link("elsewhere")),
        ]);

        // when
        let plan = diff(&source, &target, WINDOW, never_called);

        // then
        assert_eq!(plan.replace, paths(&["was_dir", "was_file", "was_link"]));
        assert!(plan.remove.is_empty());
        assert_eq!(plan.mkdir, paths(&["was_file"]));
        assert_eq!(
            copied(&plan),
            vec![
                ("was_dir".into(), CopyReason::Changed),
                ("was_link".into(), CopyReason::Changed)
            ]
        );
    }

    #[test]
    fn temp_files_are_replaced_while_deleted_entries_are_removed() {
        // given
        let source = entries(&[("d", Entry::Dir)]);
        let target = entries(&[
            ("d", Entry::Dir),
            ("d/.backsync-tmp.9.0", file(1, 1)),
            ("gone", Entry::Dir),
            ("gone/.backsync-tmp.9.1", file(1, 1)),
        ]);

        // when
        let plan = diff(&source, &target, WINDOW, never_called);

        // then
        assert_eq!(plan.remove, paths(&["gone"]));
        assert_eq!(
            plan.replace,
            paths(&["d/.backsync-tmp.9.0", "gone/.backsync-tmp.9.1"])
        );
    }

    #[test]
    fn removals_depend_on_the_mode() {
        // given
        let plan = Plan {
            remove: paths(&["gone"]),
            replace: paths(&["gone/.backsync-tmp.9.1", "x"]),
            ..Plan::default()
        };

        // when
        let all = plan.removals(Removals::All);
        let keep_deleted = plan.removals(Removals::KeepDeleted);
        let none = plan.removals(Removals::None);

        // then
        assert_eq!(all, [Path::new("gone"), Path::new("x")]);
        assert_eq!(
            keep_deleted,
            [Path::new("gone/.backsync-tmp.9.1"), Path::new("x")]
        );
        assert!(none.is_empty());
    }

    #[test]
    fn retargeted_symlink_is_replaced_without_removal() {
        // given
        let source = entries(&[("l", link("new"))]);
        let target = entries(&[("l", link("old"))]);

        // when
        let plan = diff(&source, &target, WINDOW, never_called);

        // then
        assert!(plan.remove.is_empty());
        assert_eq!(copied(&plan), vec![("l".into(), CopyReason::Changed)]);
    }

    #[test]
    fn apple_double_sidecars_of_mirrored_files_are_kept() {
        // given
        let source = entries(&[("d", Entry::Dir), ("d/photo.jpg", file(1, 1))]);
        let target = entries(&[
            ("d", Entry::Dir),
            ("d/photo.jpg", file(1, 1)),
            ("d/._photo.jpg", file(4096, 1)),
            ("._d", file(4096, 1)),
            ("d/._deleted.jpg", file(4096, 1)),
            ("._", file(1, 1)),
        ]);

        // when
        let plan = diff(&source, &target, WINDOW, never_called);

        // then
        assert_eq!(plan.remove, paths(&["._", "d/._deleted.jpg"]));
    }

    #[test]
    fn apple_double_files_that_exist_in_source_are_mirrored_normally() {
        // given
        let source = entries(&[("a", file(1, 1)), ("._a", file(2, 100))]);
        let target = entries(&[("a", file(1, 1)), ("._a", file(9, 1))]);

        // when
        let plan = diff(&source, &target, WINDOW, never_called);

        // then
        assert_eq!(copied(&plan), vec![("._a".into(), CopyReason::Changed)]);
    }
}
