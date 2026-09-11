//! Full data-directory backup as a tar.gz archive (admin portal
//! button + CLI `backup` command share this).

use std::path::Path;

/// Tar+gzip the data directory in memory. Suitable for the expected
/// data sizes (documents + SQLite); the caller streams the bytes out.
///
/// The SQLite database is copied file-wise (WAL sidecar included when
/// present) — a live database's main file may be mid-checkpoint, so
/// the archive is a best-effort snapshot. For a guaranteed-consistent
/// backup, stop the server first (the CLI documents this).
pub fn backup_tar_gz(data_dir: &Path) -> Result<Vec<u8>, std::io::Error> {
    let mut builder = tar::Builder::new(Vec::new());
    for entry in walkdir::WalkDir::new(data_dir)
        .follow_links(false)
        .into_iter()
        .filter_map(|e| e.ok())
    {
        let path = entry.path();
        let rel = path.strip_prefix(data_dir).unwrap_or(path);
        if rel.as_os_str().is_empty() {
            continue;
        }
        if path.is_file() {
            // Never archive backup archives: a backup written inside
            // the data dir must not be recursively embedded in the
            // next backup (unbounded growth).
            if path.extension().is_some_and(|e| e == "gz")
                && path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.ends_with(".tar.gz"))
            {
                continue;
            }
            let bytes = std::fs::read(path)?;
            let mut header = tar::Header::new_gnu();
            header.set_size(bytes.len() as u64);
            header.set_mode(0o600);
            header.set_cksum();
            builder
                .append_data(&mut header, rel, bytes.as_slice())
                .map_err(std::io::Error::other)?;
        }
    }
    let tar = builder.into_inner().map_err(std::io::Error::other)?;
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    std::io::Write::write_all(&mut encoder, &tar)?;
    encoder.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backup_contains_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), b"hello").unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("sub/b.txt"), b"world").unwrap();
        let bytes = backup_tar_gz(dir.path()).unwrap();
        assert!(!bytes.is_empty());
        // Decompresses and lists both entries.
        let mut tar_bytes = Vec::new();
        let mut decoder = flate2::read::GzDecoder::new(&bytes[..]);
        std::io::Read::read_to_end(&mut decoder, &mut tar_bytes).unwrap();
        let mut archive = tar::Archive::new(&tar_bytes[..]);
        let mut names = Vec::new();
        for entry in archive.entries().unwrap() {
            names.push(entry.unwrap().path().unwrap().display().to_string());
        }
        assert!(names.iter().any(|n| n.contains("a.txt")));
        assert!(
            names
                .iter()
                .any(|n| n.contains("sub/b.txt") || n.contains("sub\\b.txt"))
        );
    }

    #[test]
    fn backup_excludes_tar_gz_archives() {
        // A backup written inside the data dir must not be embedded
        // in the next backup (recursive growth).
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("data.txt"), b"real data").unwrap();
        std::fs::write(dir.path().join("old.tar.gz"), b"pretend archive").unwrap();
        std::fs::write(
            dir.path().join("not-a-tar.gz"),
            b"kept: only .tar.gz skipped",
        )
        .unwrap();
        let bytes = backup_tar_gz(dir.path()).unwrap();
        let mut tar_bytes = Vec::new();
        let mut decoder = flate2::read::GzDecoder::new(&bytes[..]);
        std::io::Read::read_to_end(&mut decoder, &mut tar_bytes).unwrap();
        let mut archive = tar::Archive::new(&tar_bytes[..]);
        let mut names = Vec::new();
        for entry in archive.entries().unwrap() {
            names.push(entry.unwrap().path().unwrap().display().to_string());
        }
        assert!(names.iter().any(|n| n.contains("data.txt")));
        assert!(names.iter().any(|n| n.contains("not-a-tar.gz")));
        assert!(
            !names.iter().any(|n| n.contains("old.tar.gz")),
            "tar.gz archives must be excluded: {names:?}"
        );
    }
}
