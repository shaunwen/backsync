use std::fs::File;
use std::io::{self, Read};
use std::path::Path;

const CHUNK: usize = 256 * 1024;

pub fn same_content(a: &Path, b: &Path) -> bool {
    files_equal(a, b).unwrap_or(false)
}

fn files_equal(a: &Path, b: &Path) -> io::Result<bool> {
    let (mut a, mut b) = (File::open(a)?, File::open(b)?);
    if a.metadata()?.len() != b.metadata()?.len() {
        return Ok(false);
    }
    let (mut buf_a, mut buf_b) = (vec![0u8; CHUNK], vec![0u8; CHUNK]);
    loop {
        let n = fill(&mut a, &mut buf_a)?;
        let m = fill(&mut b, &mut buf_b)?;
        if n != m || buf_a[..n] != buf_b[..m] {
            return Ok(false);
        }
        if n == 0 {
            return Ok(true);
        }
    }
}

fn fill(reader: &mut impl Read, buf: &mut [u8]) -> io::Result<usize> {
    let mut filled = 0;
    while filled < buf.len() {
        match reader.read(&mut buf[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
    Ok(filled)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    #[test]
    fn equal_large_files_are_the_same() {
        // given
        let dir = tempfile::tempdir().unwrap();
        let data: Vec<u8> = (0..CHUNK * 3 + 17).map(|i| (i % 251) as u8).collect();
        fs::write(dir.path().join("a"), &data).unwrap();
        fs::write(dir.path().join("b"), &data).unwrap();

        // when
        let same = same_content(&dir.path().join("a"), &dir.path().join("b"));

        // then
        assert!(same);
    }

    #[test]
    fn one_byte_difference_in_last_chunk_is_detected() {
        // given
        let dir = tempfile::tempdir().unwrap();
        let data: Vec<u8> = vec![7; CHUNK * 2 + 5];
        let mut changed = data.clone();
        *changed.last_mut().unwrap() = 8;
        fs::write(dir.path().join("a"), &data).unwrap();
        fs::write(dir.path().join("b"), &changed).unwrap();

        // when
        let same = same_content(&dir.path().join("a"), &dir.path().join("b"));

        // then
        assert!(!same);
    }

    #[test]
    fn unreadable_file_counts_as_different() {
        // given
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a"), "x").unwrap();

        // when
        let same = same_content(&dir.path().join("a"), &dir.path().join("missing"));

        // then
        assert!(!same);
    }
}
