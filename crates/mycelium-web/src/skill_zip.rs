//! Deterministic zip writer for skill bundles (Task 8). The web layer's
//! only job on top of the assembler (`mycelium_store::skill_bundle`,
//! which produces plain files, never zip): turn those files into a
//! Claude-Code-shaped archive with byte-identical output across calls.

use std::io::{Cursor, Write};

use mycelium_store::SkillBundleFile;
use zip::result::ZipError;
use zip::{CompressionMethod, DateTime, ZipWriter, write::SimpleFileOptions};

/// Zip the assembled files deterministically: entries in the order
/// given (the assembler's fixed order — `SKILL.md`, companions sorted,
/// scripts sorted), fixed mtime 1980-01-01 00:00:00, fixed unix perms
/// 0644, deflate. Two calls with the same files produce byte-identical
/// archives — no clock, no host, no randomness.
///
/// The fixed datetime is [`DateTime::DEFAULT`], the zip crate's own
/// 1980-01-01 00:00:00 epoch value (`from_date_and_time(1980, 1, 1, 0,
/// 0, 0)` yields the identical datepart/timepart; the brief's sketched
/// `DateTime::from_parts` does not exist in the resolved zip 8.6 API —
/// verified against the vendored source). It is set EXPLICITLY because
/// the crate's default would stamp the current time.
pub fn zip_files(files: &[SkillBundleFile]) -> Result<Vec<u8>, ZipError> {
    let mut buf = Cursor::new(Vec::new());
    let mut zw = ZipWriter::new(&mut buf);
    let opts = SimpleFileOptions::default()
        .compression_method(CompressionMethod::Deflated)
        .unix_permissions(0o644)
        .last_modified_time(DateTime::DEFAULT);
    for f in files {
        zw.start_file(f.rel_path.as_str(), opts)?;
        zw.write_all(&f.bytes)?;
    }
    // Consumes the writer, finalizes the central directory. The
    // returned `&mut Cursor` borrow ends here, freeing `buf`.
    zw.finish()?;
    Ok(buf.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    fn mk(rel: &str, bytes: &[u8]) -> SkillBundleFile {
        SkillBundleFile {
            rel_path: rel.into(),
            bytes: bytes.to_vec(),
        }
    }

    /// Same input → byte-identical archives (fixed entry order, mtime,
    /// perms, compression level — nothing clock- or host-derived).
    #[test]
    fn zip_is_deterministic() {
        let files = vec![mk("SKILL.md", b"# x"), mk("scripts/a.py", b"y")];
        assert_eq!(
            zip_files(&files).unwrap(),
            zip_files(&files).unwrap(),
            "two calls must be byte-identical"
        );
    }

    /// Round-trip: every entry's name and exact bytes survive, in the
    /// given order, with the fixed contract metadata (deflate, 0644
    /// regular file, mtime 1980-01-01 — zip's DOS epoch).
    #[test]
    fn zip_roundtrips_entry_bytes() {
        let files = vec![
            mk("SKILL.md", b"---\ntype: Skill\n---\n\nbody"),
            mk("scripts/a.py", b"print('a')\n"),
            mk("scripts/b.sh", b"#!/bin/sh\n"),
        ];
        let bytes = zip_files(&files).unwrap();
        let mut za = zip::ZipArchive::new(Cursor::new(bytes)).unwrap();
        assert_eq!(za.len(), 3);
        // Entry order is the order given (central-directory order).
        let names: Vec<String> = za.file_names().map(str::to_string).collect();
        assert_eq!(names, ["SKILL.md", "scripts/a.py", "scripts/b.sh"]);
        for f in &files {
            let mut entry = za.by_name(&f.rel_path).unwrap();
            assert_eq!(entry.name(), f.rel_path);
            let mut got = Vec::new();
            entry.read_to_end(&mut got).unwrap();
            assert_eq!(got, f.bytes, "entry {}", f.rel_path);
            assert_eq!(entry.compression(), CompressionMethod::Deflated);
            // The writer ORs in S_IFREG (regular file) next to 0644.
            assert_eq!(entry.unix_mode(), Some(0o100_644));
            assert_eq!(entry.last_modified(), Some(DateTime::DEFAULT));
        }
    }
}
