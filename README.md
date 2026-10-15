# backsync

Mirror a folder to a backup folder, for example a portable drive. New and changed files are copied, files deleted from the source are deleted from the backup, and unchanged files are left alone. The source is never modified.

```text
$ backsync
12 new, 3 updated, 2 removed, 48211 unchanged; 1.4 GiB copied in 6.20s
```

## Install

```bash
make install            # lint, test, release build, copy to ~/workspace/bin
make install BIN_DIR=~/bin
```

## Configure

Set the source and target in `~/.config/backsync/config.toml` (or `$XDG_CONFIG_HOME/backsync/config.toml`):

```toml
source = "~/Documents"
target = "/Volumes/Backup/Documents"

# optional
modify_window_secs = 2   # mtime difference still treated as unchanged
jobs = 4                 # parallel copies; use 1-2 for spinning disks
```

Or with environment variables, which take priority over the file field by field:

```bash
BACKSYNC_SOURCE=~/Documents BACKSYNC_TARGET=/Volumes/Backup/Documents backsync
```

## Use

```bash
backsync                 # sync
backsync -n              # dry run: list what would change
backsync -v              # print each action
backsync --checksum      # compare contents, not just size and mtime
backsync -j 2            # fewer parallel copies
backsync --config PATH   # another config file
```

| Exit code | Meaning |
|-----------|---------|
| 0 | backup matches the source |
| 1 | finished, but some files could not be read, copied or removed (listed on stderr) |
| 2 | nothing was changed: configuration problem or a safety check refused to run |

## How it decides

- A file is unchanged when its size matches and its modification time is within `modify_window_secs` (default 2 s, which covers FAT32 and exFAT timestamp precision). `--checksum` compares contents instead.
- Copied files get the source's modification time, so the next run sees them as unchanged.
- Files are written to a temporary name and renamed into place, so an interrupted run never leaves a half-written file under the real name. Leftover temp files are removed on the next run.
- Symlinks are copied as links, never followed. Sockets and other special files are skipped with a warning.

## Safety

- The source is only read. Every write and delete is checked to be inside the target folder.
- Source and target must exist and must not be the same folder or inside each other.
- The target folder is never created. If the drive is not mounted, the run stops instead of filling the internal disk.
- An empty source with a non-empty backup is refused (it usually means the source drive is missing). Pass `--allow-empty-source` if you really want to empty the backup.
- If any part of the source cannot be read, nothing is removed from the backup in that run.
- macOS volume metadata at the target root (`.Spotlight-V100`, `.Trashes`, `.fseventsd`, ...) is left alone.

## Portable drives (exFAT / FAT32)

- macOS stores extended attributes on these drives in hidden `._<name>` files. backsync keeps them next to the files they belong to and removes orphans.
- These drives list names in a different Unicode form than APFS (for example `Sönke`). backsync compares names in a normalized form, so such files are not copied again on every run.
- The drive needs room for each file's full allocation unit; thousands of small files take far more space on exFAT than on APFS. If the drive fills up, backsync stops copying and reports how many files were not attempted.

## Performance

Measured on an Apple Silicon Mac with 50,000 files (1.1 GiB), backing up to a separate APFS volume:

| | backsync | rsync -a --delete |
|-|----------|-------------------|
| first backup | 8.1 s | 17.0 s |
| nothing changed | 0.1 s | 2.8 s |

Both folders are scanned in parallel, and copies run on 4 parallel jobs by default. More jobs were slower in testing.

## Development

```bash
make lint   # rustfmt + clippy -D warnings
make test   # unit tests and end-to-end tests
```

The end-to-end tests run the real binary against temporary folders. On macOS, `tests/exfat.rs` also mounts a small exFAT disk image with `hdiutil` to check behaviour on a real portable-drive file system.

The design and the reasoning behind it are in [docs/plan.md](docs/plan.md).
