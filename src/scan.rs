use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use jwalk::{Parallelism, WalkDirGeneric};

pub const VOLUME_METADATA: &[&str] = &[
    ".Spotlight-V100",
    ".Trashes",
    ".fseventsd",
    ".TemporaryItems",
    ".DocumentRevisions-V100",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Entry {
    File { size: u64, mtime: SystemTime },
    Dir,
    Symlink { link: PathBuf },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScanError {
    pub path: PathBuf,
    pub message: String,
}

/// Comparison key -> on-disk relative path, only for entries where they differ.
pub type DiskNames = HashMap<PathBuf, PathBuf>;

#[derive(Debug, Default)]
pub struct Scan {
    pub entries: HashMap<PathBuf, Entry>,
    pub disk_names: DiskNames,
    pub errors: Vec<ScanError>,
    pub skipped_special: Vec<PathBuf>,
}

pub fn disk_path<'a>(names: &'a DiskNames, key: &'a Path) -> &'a Path {
    names.get(key).map_or(key, PathBuf::as_path)
}

/// macOS file systems match names regardless of Unicode normalization, but
/// list them in different forms (exFAT and HFS+ return decomposed NFD names,
/// APFS keeps whatever was written). Comparing NFC keys stops the same file
/// from looking deleted on one side and new on the other.
#[cfg(target_os = "macos")]
pub fn name_key(relative: &Path) -> PathBuf {
    use unicode_normalization::{UnicodeNormalization, is_nfc};
    match relative.to_str() {
        Some(text) if !is_nfc(text) => PathBuf::from(text.nfc().collect::<String>()),
        _ => relative.to_path_buf(),
    }
}

#[cfg(not(target_os = "macos"))]
pub fn name_key(relative: &Path) -> PathBuf {
    relative.to_path_buf()
}

#[derive(Debug, Default)]
enum Probe {
    #[default]
    Pending,
    Found(Entry),
    Special,
    Failed(String),
}

type Walker = WalkDirGeneric<((), Probe)>;

pub fn scan(root: &Path, threads: usize) -> Scan {
    let walk_root = root.to_path_buf();
    let walker = Walker::new(root)
        .parallelism(Parallelism::RayonNewPool(threads))
        .skip_hidden(false)
        .follow_links(false)
        .sort(false)
        .process_read_dir(move |_, dir, _, children| {
            if dir == walk_root {
                children.retain(|child| {
                    child.as_ref().map_or(true, |entry| {
                        !VOLUME_METADATA.iter().any(|name| entry.file_name == *name)
                    })
                });
            }
            for entry in children.iter_mut().flatten() {
                entry.client_state = probe(&entry.path());
            }
        });

    let mut scan = Scan::default();
    for item in walker {
        match item {
            Ok(entry) => {
                let path = entry.path();
                if let Some(error) = entry.read_children.as_ref().and_then(|c| c.error()) {
                    scan.errors.push(ScanError {
                        path: path.clone(),
                        message: error.to_string(),
                    });
                }
                if entry.depth == 0 {
                    continue;
                }
                let relative = path
                    .strip_prefix(root)
                    .expect("walked path is under its root")
                    .to_path_buf();
                match entry.client_state {
                    Probe::Found(found) => {
                        let key = name_key(&relative);
                        if key != relative {
                            scan.disk_names.insert(key.clone(), relative);
                        }
                        scan.entries.insert(key, found);
                    }
                    Probe::Special => scan.skipped_special.push(relative),
                    Probe::Failed(message) => scan.errors.push(ScanError { path, message }),
                    Probe::Pending => scan.errors.push(ScanError {
                        path,
                        message: "entry was not inspected".into(),
                    }),
                }
            }
            Err(error) => scan.errors.push(ScanError {
                path: error
                    .path()
                    .map_or_else(|| root.to_path_buf(), Path::to_path_buf),
                message: error.to_string(),
            }),
        }
    }
    scan
}

fn probe(path: &Path) -> Probe {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) => return Probe::Failed(error.to_string()),
    };
    let file_type = metadata.file_type();
    if file_type.is_symlink() {
        return match fs::read_link(path) {
            Ok(link) => Probe::Found(Entry::Symlink { link }),
            Err(error) => Probe::Failed(error.to_string()),
        };
    }
    if file_type.is_dir() {
        return Probe::Found(Entry::Dir);
    }
    if !file_type.is_file() {
        return Probe::Special;
    }
    match metadata.modified() {
        Ok(mtime) => Probe::Found(Entry::File {
            size: metadata.len(),
            mtime,
        }),
        Err(error) => Probe::Failed(error.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::{PermissionsExt, symlink};
    use std::os::unix::net::UnixListener;

    use super::*;

    fn write(root: &Path, relative: &str, content: &str) {
        let path = root.join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }

    #[test]
    fn scans_nested_tree_with_relative_paths() {
        // given
        let root = tempfile::tempdir().unwrap();
        write(root.path(), "a.txt", "hello");
        write(root.path(), "docs/deep/b.txt", "hi");
        fs::create_dir(root.path().join("empty")).unwrap();

        // when
        let scan = scan(root.path(), 4);

        // then
        assert!(scan.errors.is_empty());
        assert_eq!(scan.entries.len(), 5);
        assert!(matches!(
            scan.entries[Path::new("a.txt")],
            Entry::File { size: 5, .. }
        ));
        assert_eq!(scan.entries[Path::new("docs")], Entry::Dir);
        assert_eq!(scan.entries[Path::new("docs/deep")], Entry::Dir);
        assert!(scan.entries.contains_key(Path::new("docs/deep/b.txt")));
        assert_eq!(scan.entries[Path::new("empty")], Entry::Dir);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn keys_entries_by_composed_name_and_remembers_disk_name() {
        // given
        let root = tempfile::tempdir().unwrap();
        write(root.path(), "cafe\u{301}/menu.txt", "x");

        // when
        let scan = scan(root.path(), 4);

        // then
        let key = Path::new("caf\u{e9}/menu.txt");
        assert!(scan.entries.contains_key(key));
        assert_eq!(
            disk_path(&scan.disk_names, key),
            Path::new("cafe\u{301}/menu.txt")
        );
        assert_eq!(
            disk_path(&scan.disk_names, Path::new("plain")),
            Path::new("plain")
        );
    }

    #[test]
    fn includes_hidden_files() {
        // given
        let root = tempfile::tempdir().unwrap();
        write(root.path(), ".hidden/.env", "x");

        // when
        let scan = scan(root.path(), 4);

        // then
        assert!(scan.entries.contains_key(Path::new(".hidden/.env")));
    }

    #[test]
    fn records_symlinks_without_following_them() {
        // given
        let root = tempfile::tempdir().unwrap();
        write(root.path(), "real/file.txt", "x");
        symlink("real", root.path().join("link")).unwrap();

        // when
        let scan = scan(root.path(), 4);

        // then
        assert_eq!(
            scan.entries[Path::new("link")],
            Entry::Symlink {
                link: PathBuf::from("real")
            }
        );
        assert!(!scan.entries.contains_key(Path::new("link/file.txt")));
    }

    #[test]
    fn skips_volume_metadata_only_at_root() {
        // given
        let root = tempfile::tempdir().unwrap();
        write(root.path(), ".Spotlight-V100/store", "x");
        write(root.path(), ".Trashes/501/old", "x");
        write(root.path(), "project/.Trashes/kept", "x");

        // when
        let scan = scan(root.path(), 4);

        // then
        assert!(
            !scan
                .entries
                .keys()
                .any(|p| p.starts_with(".Spotlight-V100"))
        );
        assert!(!scan.entries.keys().any(|p| p.starts_with(".Trashes")));
        assert!(
            scan.entries
                .contains_key(Path::new("project/.Trashes/kept"))
        );
    }

    #[test]
    fn skips_special_files() {
        // given
        let root = tempfile::tempdir().unwrap();
        let _listener = UnixListener::bind(root.path().join("sock")).unwrap();

        // when
        let scan = scan(root.path(), 4);

        // then
        assert_eq!(scan.skipped_special, vec![PathBuf::from("sock")]);
        assert!(scan.entries.is_empty());
    }

    #[test]
    fn reports_unreadable_directory_as_error() {
        // given
        let root = tempfile::tempdir().unwrap();
        write(root.path(), "locked/secret.txt", "x");
        let locked = root.path().join("locked");
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();

        // when
        let scan = scan(root.path(), 4);
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();

        // then
        assert!(!scan.errors.is_empty());
        assert!(!scan.entries.contains_key(Path::new("locked/secret.txt")));
    }

    #[test]
    fn reports_unreadable_root_as_error() {
        // given
        let parent = tempfile::tempdir().unwrap();
        let root = parent.path().join("root");
        write(&root, "file.txt", "x");
        fs::set_permissions(&root, fs::Permissions::from_mode(0o000)).unwrap();

        // when
        let scan = scan(&root, 4);
        fs::set_permissions(&root, fs::Permissions::from_mode(0o755)).unwrap();

        // then
        assert!(!scan.errors.is_empty());
        assert!(scan.entries.is_empty());
    }
}
