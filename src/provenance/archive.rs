//! ZIP adapters: content identity covers decompressed member bytes, not ZIP
//! transport headers. All names are checked before excluding provenance storage.
use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use sha2::{Digest, Sha256};
use zip::ZipArchive;

use super::manifest::{
    CoverageReport, FileDigest, Manifest, ManifestError, invalid, sha256_stream, validate_integer,
    validate_path, validate_transport_path,
};

fn zip_error(error: zip::result::ZipError) -> ManifestError {
    invalid(format!("invalid ZIP archive: {error}"))
}

// zip 4.x stores entries in a name-indexed map and silently collapses exact
// duplicates. Inspect central-directory records before relying on that map. The
// library still performs decompression, CRC and ZIP64 handling; this small guard
// only preserves inventory/name identity that would otherwise be lost.
fn open_archive(path: impl AsRef<Path>) -> Result<ZipArchive<File>, ManifestError> {
    let mut file = File::open(path)?;
    let mut archive = ZipArchive::new(file.try_clone()?).map_err(zip_error)?;
    let file_size = file.metadata()?.len();
    file.seek(SeekFrom::Start(archive.central_directory_start()))?;
    let mut entries = BTreeMap::new();
    loop {
        let position = file.stream_position()?;
        let mut signature = [0_u8; 4];
        file.read_exact(&mut signature)?;
        if matches!(
            signature,
            [0x50, 0x4b, 0x05, 0x06] | [0x50, 0x4b, 0x06, 0x06] | [0x50, 0x4b, 0x05, 0x05]
        ) {
            break;
        }
        if signature != [0x50, 0x4b, 0x01, 0x02] {
            return Err(invalid("unexpected ZIP central-directory record"));
        }
        let mut header = [0_u8; 42];
        file.read_exact(&mut header)?;
        let name_length = u16::from_le_bytes([header[24], header[25]]) as usize;
        let extra_length = u16::from_le_bytes([header[26], header[27]]) as u64;
        let comment_length = u16::from_le_bytes([header[28], header[29]]) as u64;
        let mut name = vec![0; name_length];
        file.read_exact(&mut name)?;
        let next = file
            .stream_position()?
            .checked_add(extra_length + comment_length)
            .ok_or_else(|| invalid("ZIP directory length overflow"))?;
        if next > file_size {
            return Err(invalid("truncated ZIP directory"));
        }
        entries.insert(position, name);
        file.seek(SeekFrom::Start(next))?;
    }
    if entries.len() != archive.len() {
        return Err(invalid("duplicate ZIP member name"));
    }
    for index in 0..archive.len() {
        let item = archive.by_index(index).map_err(zip_error)?;
        let central_name = entries
            .get(&item.central_header_start())
            .ok_or_else(|| invalid("inconsistent ZIP central-directory position"))?;
        if central_name != item.name().as_bytes() {
            return Err(invalid("alternate ZIP filename encoding is unsupported"));
        }
        file.seek(SeekFrom::Start(item.header_start()))?;
        let mut header = [0_u8; 30];
        file.read_exact(&mut header)?;
        if header[..4] != [0x50, 0x4b, 0x03, 0x04] {
            return Err(invalid("invalid ZIP local header"));
        }
        let name_length = u16::from_le_bytes([header[26], header[27]]) as usize;
        let mut local_name = vec![0; name_length];
        file.read_exact(&mut local_name)?;
        if &local_name != central_name {
            return Err(invalid("local and central ZIP names differ"));
        }
    }
    Ok(archive)
}

fn inventory<R: Read + Seek>(
    archive: &mut ZipArchive<R>,
) -> Result<BTreeMap<String, (usize, u64)>, ManifestError> {
    let mut names = BTreeSet::new();
    let mut content = BTreeMap::new();
    for index in 0..archive.len() {
        let item = archive.by_index(index).map_err(zip_error)?;
        let name = item.name();
        let normalized = if item.is_dir() {
            name.strip_suffix('/').unwrap_or(name)
        } else {
            name
        };
        validate_transport_path(normalized)?;
        if !names.insert(normalized.to_ascii_lowercase()) {
            return Err(invalid("duplicate or case-colliding ZIP member name"));
        }
        let kind = item.unix_mode().unwrap_or(0) & 0o170000;
        if !matches!(kind, 0 | 0o100000 | 0o040000) {
            return Err(invalid("ZIP links and special files are not content files"));
        }
        if kind == 0o040000 && !item.is_dir() || kind == 0o100000 && item.is_dir() {
            return Err(invalid("inconsistent ZIP directory type"));
        }
        if item.is_dir() {
            continue;
        }
        if name.to_ascii_lowercase().starts_with("provenance/")
            || name.eq_ignore_ascii_case("provenance.parquet")
        {
            continue;
        }
        validate_path(name)?;
        validate_integer(item.size(), "ZIP member size")?;
        content.insert(name.to_owned(), (index, item.size()));
    }
    Ok(content)
}

/// Capture all content, or an explicitly selected stage output inventory. This
/// never changes archive bytes, inserts metadata, or generates signing keys.
pub fn capture_archive(
    path: impl AsRef<Path>,
    run_id: &str,
    stage_id: &str,
    selected_paths: Option<&[String]>,
) -> Result<Manifest, ManifestError> {
    let mut archive = open_archive(path)?;
    let content = inventory(&mut archive)?;
    let selected: BTreeSet<String> = match selected_paths {
        None => content.keys().cloned().collect(),
        Some(paths) => {
            let set: BTreeSet<_> = paths.iter().cloned().collect();
            if set.len() != paths.len() {
                return Err(invalid("duplicate selected path"));
            }
            if set.iter().any(|p| !content.contains_key(p)) {
                return Err(invalid("selected path is missing from archive content"));
            }
            set
        }
    };
    let mut files = Vec::new();
    for name in selected {
        let (index, size) = content[&name];
        let mut item = archive.by_index(index).map_err(zip_error)?;
        let digest = sha256_stream(&mut item, Some(size))?;
        if item.read(&mut [0])? != 0 {
            return Err(invalid("ZIP member exceeds its declared size"));
        }
        files.push(FileDigest {
            path: name,
            size,
            sha256: digest,
            chunks: Vec::new(),
        });
    }
    let manifest = Manifest {
        version: 1,
        run_id: run_id.into(),
        stage_id: stage_id.into(),
        files,
    };
    manifest.validate()?;
    Ok(manifest)
}

fn verify_member<R: Read>(stream: &mut R, descriptor: &FileDigest) -> Result<bool, ManifestError> {
    let mut digest = Sha256::new();
    let mut chunks: Vec<_> = descriptor.chunks.iter().collect();
    chunks.sort_by_key(|c| (c.offset, c.length));
    let mut chunk_digests: Vec<_> = chunks.iter().map(|_| Sha256::new()).collect();
    let mut buffer = [0_u8; 65536];
    let mut offset = 0;
    let mut remaining = descriptor.size;
    let mut chunk_cursor = 0;
    while remaining > 0 {
        let wanted = remaining.min(buffer.len() as u64) as usize;
        let count = match stream.read(&mut buffer[..wanted]) {
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            other => other?,
        };
        if count == 0 {
            return Ok(false);
        }
        digest.update(&buffer[..count]);
        let end = offset + count as u64;
        for index in chunk_cursor..chunks.len() {
            let chunk = chunks[index];
            if chunk.offset >= end {
                break;
            }
            let start = offset.max(chunk.offset);
            let stop = end.min(chunk.offset + chunk.length);
            if start < stop {
                chunk_digests[index]
                    .update(&buffer[(start - offset) as usize..(stop - offset) as usize]);
            }
            if chunk.offset + chunk.length <= end {
                chunk_cursor = index + 1;
            }
        }
        offset = end;
        remaining -= count as u64;
    }
    if stream.read(&mut [0])? != 0 {
        return Ok(false);
    }
    Ok(hex::encode(digest.finalize()) == descriptor.sha256
        && chunks
            .iter()
            .zip(chunk_digests)
            .all(|(c, d)| hex::encode(d.finalize()) == c.sha256))
}

/// Verify full content coverage. Malformed names, duplicate entries, links,
/// corrupt ZIP streams and read failures return errors rather than a valid result.
pub fn verify_archive(
    path: impl AsRef<Path>,
    manifest: &Manifest,
) -> Result<CoverageReport, ManifestError> {
    manifest.validate()?;
    let mut archive = open_archive(path)?;
    let content = inventory(&mut archive)?;
    let expected: BTreeMap<_, _> = manifest.files.iter().map(|f| (&f.path, f)).collect();
    let missing = expected
        .keys()
        .filter(|p| !content.contains_key(**p))
        .map(|p| (*p).clone())
        .collect();
    let uncovered = content
        .keys()
        .filter(|p| !expected.contains_key(*p))
        .cloned()
        .collect();
    let mut changed = Vec::new();
    for (name, descriptor) in expected {
        if let Some((index, size)) = content.get(name) {
            if *size != descriptor.size {
                changed.push(name.clone());
                continue;
            }
            let mut item = archive.by_index(*index).map_err(zip_error)?;
            if !verify_member(&mut item, descriptor)? {
                changed.push(name.clone());
            }
        }
    }
    Ok(CoverageReport {
        missing,
        changed,
        uncovered,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use zip::write::SimpleFileOptions;

    fn fixture(path: &Path, members: &[(&str, &[u8])]) {
        let mut writer = zip::ZipWriter::new(File::create(path).unwrap());
        for (name, bytes) in members {
            writer
                .start_file(*name, SimpleFileOptions::default())
                .unwrap();
            writer.write_all(bytes).unwrap();
        }
        writer.finish().unwrap();
    }

    #[test]
    fn capture_and_verify_complete_archive() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("data.mzpeak");
        fixture(
            &path,
            &[
                ("mzpeak_index.json", b"{}"),
                ("spectra.parquet", b"abcdef"),
                ("provenance/envelope.json", b"untrusted"),
            ],
        );
        let manifest = capture_archive(&path, "run-1", "stage-1", None).unwrap();
        assert_eq!(manifest.files.len(), 2);
        assert!(verify_archive(&path, &manifest).unwrap().complete());
        fixture(
            &path,
            &[("mzpeak_index.json", b"[]"), ("extra.json", b"{}")],
        );
        let report = verify_archive(&path, &manifest).unwrap();
        assert_eq!(report.changed, ["mzpeak_index.json"]);
        assert_eq!(report.missing, ["spectra.parquet"]);
        assert_eq!(report.uncovered, ["extra.json"]);
    }

    #[test]
    fn selective_capture_never_claims_complete_archive() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("data.mzpeak");
        fixture(&path, &[("a", b"a"), ("b", b"b")]);
        let manifest = capture_archive(&path, "run-1", "stage-1", Some(&["a".into()])).unwrap();
        assert_eq!(verify_archive(&path, &manifest).unwrap().uncovered, ["b"]);
        assert!(capture_archive(&path, "run-1", "stage-1", Some(&["missing".into()])).is_err());
    }

    #[test]
    fn exact_duplicate_and_local_name_mismatch_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("data.mzpeak");
        fixture(&path, &[("a", b"one"), ("b", b"two")]);
        let mut bytes = std::fs::read(&path).unwrap();
        // Build an exact-duplicate fixture independently of ZipWriter, which
        // correctly refuses to create duplicates itself.
        for index in 0..bytes.len().saturating_sub(47) {
            let name_offset = if bytes[index..index + 4] == [0x50, 0x4b, 0x03, 0x04] {
                Some(30)
            } else if bytes[index..index + 4] == [0x50, 0x4b, 0x01, 0x02] {
                Some(46)
            } else {
                None
            };
            if let Some(offset) = name_offset {
                if bytes[index + offset] == b'b' {
                    bytes[index + offset] = b'a';
                }
            }
        }
        std::fs::write(&path, bytes).unwrap();
        assert!(capture_archive(&path, "run-1", "stage-1", None).is_err());
        fixture(&path, &[("a", b"data")]);
        let mut bytes = std::fs::read(&path).unwrap();
        bytes[30] = b'b';
        std::fs::write(&path, bytes).unwrap();
        assert!(capture_archive(&path, "run-1", "stage-1", None).is_err());
    }

    #[test]
    fn selected_ranges_are_hashed_in_one_pass() {
        use super::super::manifest::ChunkDigest;
        let data = vec![b'a'; 65541];
        let mut descriptor =
            FileDigest::from_stream("spectra.parquet", &mut std::io::Cursor::new(&data)).unwrap();
        descriptor.chunks = vec![
            ChunkDigest {
                offset: 0,
                length: 0,
                sha256: hex::encode(Sha256::digest(b"")),
                row_group: None,
                column: None,
                column_path: None,
            },
            ChunkDigest {
                offset: 65534,
                length: 7,
                sha256: hex::encode(Sha256::digest(b"aaaaaaa")),
                row_group: None,
                column: None,
                column_path: None,
            },
        ];
        assert!(verify_member(&mut std::io::Cursor::new(&data), &descriptor).unwrap());
        descriptor.chunks[1].sha256 = "a".repeat(64);
        assert!(!verify_member(&mut std::io::Cursor::new(&data), &descriptor).unwrap());
    }

    #[test]
    fn unsafe_and_case_alias_members_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("data.mzpeak");
        fixture(&path, &[("a", b"a"), ("A", b"a")]);
        assert!(capture_archive(&path, "run-1", "stage-1", None).is_err());
        fixture(&path, &[("../escape", b"a")]);
        assert!(capture_archive(&path, "run-1", "stage-1", None).is_err());
        let mut writer = zip::ZipWriter::new(File::create(&path).unwrap());
        writer
            .add_symlink("link", "target", SimpleFileOptions::default())
            .unwrap();
        writer.finish().unwrap();
        assert!(capture_archive(&path, "run-1", "stage-1", None).is_err());
    }
}
