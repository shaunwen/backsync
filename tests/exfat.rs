#![cfg(target_os = "macos")]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command as Process;

use assert_cmd::Command;
use predicates::str::contains;

struct ExfatVolume {
    image: PathBuf,
    mount: PathBuf,
}

impl ExfatVolume {
    fn create(dir: &Path) -> Self {
        let image = dir.join("backup.sparseimage");
        let mount = dir.join("mnt");
        fs::create_dir_all(&mount).unwrap();
        run(Process::new("hdiutil")
            .args(["create", "-size", "64m", "-fs", "ExFAT"])
            .args(["-volname", "BSTEST", "-type", "SPARSE", "-quiet"])
            .arg(&image));
        run(Process::new("hdiutil")
            .arg("attach")
            .arg(&image)
            .arg("-mountpoint")
            .arg(&mount)
            .args(["-nobrowse", "-quiet"]));
        Self { image, mount }
    }
}

impl Drop for ExfatVolume {
    fn drop(&mut self) {
        let _ = Process::new("hdiutil")
            .args(["detach", "-force", "-quiet"])
            .arg(&self.mount)
            .status();
        let _ = fs::remove_file(&self.image);
    }
}

fn run(command: &mut Process) {
    let status = command.status().unwrap();
    assert!(status.success(), "{command:?} failed");
}

fn folder_with_xattr_files(root: &Path, name: &str) {
    let dir = root.join(name);
    fs::create_dir_all(&dir).unwrap();
    let files: Vec<PathBuf> = (0..100)
        .map(|i| {
            let file = dir.join(format!("f{i}.txt"));
            fs::write(&file, format!("{name}-{i}")).unwrap();
            file
        })
        .collect();
    run(Process::new("xattr")
        .args(["-w", "com.backsync.test", "1"])
        .args(&files));
}

#[test]
fn exfat_backup_resyncs_cleanly_and_removes_deleted_folders() {
    // given
    let dir = tempfile::tempdir().unwrap();
    let volume = ExfatVolume::create(dir.path());
    let source = dir.path().join("source");
    let target = volume.mount.join("backup");
    fs::create_dir_all(&target).unwrap();
    for name in ["a", "b", "c", "d", "e"] {
        folder_with_xattr_files(&source, name);
    }
    fs::write(source.join("e/S\u{f6}nke caf\u{e9}.txt"), "accented").unwrap();
    let sync = || {
        let mut cmd = Command::cargo_bin("backsync").unwrap();
        cmd.env("XDG_CONFIG_HOME", dir.path())
            .env("BACKSYNC_SOURCE", &source)
            .env("BACKSYNC_TARGET", &target);
        cmd.assert()
    };
    sync().success().stdout(contains("501 new"));

    // when
    let resync = sync();
    for name in ["a", "b", "c", "d"] {
        fs::remove_dir_all(source.join(name)).unwrap();
    }
    let after_delete = sync();

    // then
    resync
        .success()
        .stdout(contains("0 new, 0 updated, 0 removed, 501 unchanged"));
    after_delete.success();
    for name in ["a", "b", "c", "d"] {
        assert!(!target.join(name).exists(), "{name} was not removed");
    }
    assert_eq!(fs::read_to_string(target.join("e/f7.txt")).unwrap(), "e-7");
}
