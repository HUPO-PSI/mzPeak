//! Deterministic, versioned content manifests. Hash matches establish integrity,
//! never certificate trust or instrument/software authorization.
use std::collections::{BTreeMap, BTreeSet};
use std::io::{self, Read, Seek, SeekFrom};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub const MAX_INTEGER: u64 = (1_u64 << 53) - 1;

#[derive(Debug, thiserror::Error)]
pub enum ManifestError {
    #[error("invalid provenance manifest: {0}")]
    Invalid(String),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Io(#[from] io::Error),
}

pub(crate) fn invalid(message: impl Into<String>) -> ManifestError {
    ManifestError::Invalid(message.into())
}

pub(crate) fn validate_integer(value: u64, label: &str) -> Result<(), ManifestError> {
    if value > MAX_INTEGER {
        return Err(invalid(format!("{label} exceeds 2^53-1")));
    }
    Ok(())
}

pub(crate) fn validate_identifier(value: &str, label: &str) -> Result<(), ManifestError> {
    let first = value.as_bytes().first().copied();
    if value.len() > 128
        || !first.is_some_and(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
        || !value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b))
    {
        return Err(invalid(format!("invalid {label}")));
    }
    Ok(())
}

pub(crate) fn validate_digest(value: &str) -> Result<(), ManifestError> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err(invalid("expected a lowercase SHA-256 digest"));
    }
    Ok(())
}

pub(crate) fn validate_transport_path(value: &str) -> Result<(), ManifestError> {
    if value.is_empty() || value.len() > 1024 {
        return Err(invalid("invalid content path"));
    }
    for part in value.split('/') {
        if part.is_empty()
            || part == "."
            || part == ".."
            || part.ends_with([' ', '.'])
            || !part.as_bytes()[0].is_ascii_alphanumeric() && !b"_.-".contains(&part.as_bytes()[0])
            || !part
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"_. -".contains(&b))
        {
            return Err(invalid("unsafe or nonportable content path"));
        }
    }
    Ok(())
}

pub(crate) fn validate_path(value: &str) -> Result<(), ManifestError> {
    validate_transport_path(value)?;
    if value
        .split('/')
        .next()
        .is_some_and(|p| p.eq_ignore_ascii_case("provenance"))
        || value.eq_ignore_ascii_case("provenance.parquet")
    {
        return Err(invalid("provenance records cannot cover themselves"));
    }
    Ok(())
}

pub(crate) fn required_option<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer)
}

/// Serialize the constrained schema as UTF-8 JSON with lexically sorted keys.
/// This deliberately supports no floating point values or arbitrary extensions.
pub(crate) fn canonical_json<T: Serialize>(item: &T) -> Result<Vec<u8>, ManifestError> {
    fn write(value: &serde_json::Value, output: &mut Vec<u8>) -> Result<(), ManifestError> {
        match value {
            serde_json::Value::Object(fields) => {
                output.push(b'{');
                let sorted: BTreeMap<_, _> = fields.iter().collect();
                for (i, (key, value)) in sorted.iter().enumerate() {
                    if i > 0 {
                        output.push(b',');
                    }
                    output.extend(serde_json::to_vec(key)?);
                    output.push(b':');
                    write(value, output)?;
                }
                output.push(b'}');
            }
            serde_json::Value::Array(items) => {
                output.push(b'[');
                for (i, value) in items.iter().enumerate() {
                    if i > 0 {
                        output.push(b',');
                    }
                    write(value, output)?;
                }
                output.push(b']');
            }
            serde_json::Value::Number(number) => {
                let value = number
                    .as_u64()
                    .ok_or_else(|| invalid("only nonnegative integers are supported"))?;
                validate_integer(value, "number")?;
                output.extend(value.to_string().bytes());
            }
            _ => output.extend(serde_json::to_vec(value)?),
        }
        Ok(())
    }
    let mut result = Vec::new();
    write(&serde_json::to_value(item)?, &mut result)?;
    Ok(result)
}

pub fn sha256_stream<R: Read>(stream: &mut R, length: Option<u64>) -> io::Result<String> {
    if length.is_some_and(|n| n > MAX_INTEGER) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "length exceeds 2^53-1",
        ));
    }
    let mut digest = Sha256::new();
    let mut buffer = [0_u8; 65536];
    let mut remaining = length;
    while remaining != Some(0) {
        let wanted = remaining.map_or(buffer.len(), |n| n.min(buffer.len() as u64) as usize);
        let count = match stream.read(&mut buffer[..wanted]) {
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            other => other?,
        };
        if count == 0 {
            if remaining.is_some_and(|n| n != 0) {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "content is shorter than its signed length",
                ));
            }
            break;
        }
        digest.update(&buffer[..count]);
        if let Some(ref mut n) = remaining {
            *n -= count as u64;
        }
    }
    Ok(hex::encode(digest.finalize()))
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChunkDigest {
    pub offset: u64,
    pub length: u64,
    pub sha256: String,
    #[serde(deserialize_with = "required_option")]
    pub row_group: Option<u64>,
    #[serde(deserialize_with = "required_option")]
    pub column: Option<u64>,
    #[serde(deserialize_with = "required_option")]
    pub column_path: Option<String>,
}

impl ChunkDigest {
    pub fn validate(&self, file_size: u64) -> Result<(), ManifestError> {
        validate_integer(self.offset, "offset")?;
        validate_integer(self.length, "length")?;
        validate_digest(&self.sha256)?;
        if self
            .offset
            .checked_add(self.length)
            .is_none_or(|n| n > file_size)
        {
            return Err(invalid("chunk extends beyond its file"));
        }
        match (&self.row_group, &self.column, &self.column_path) {
            (None, None, None) => {}
            (Some(row), Some(col), Some(path)) => {
                validate_integer(*row, "row_group")?;
                validate_integer(*col, "column")?;
                if path.is_empty() || !path.bytes().all(|b| (32..=126).contains(&b)) {
                    return Err(invalid("invalid column_path"));
                }
            }
            _ => {
                return Err(invalid(
                    "Parquet chunk coordinates must be supplied together",
                ));
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileDigest {
    pub path: String,
    pub size: u64,
    pub sha256: String,
    pub chunks: Vec<ChunkDigest>,
}

impl FileDigest {
    pub fn validate(&self) -> Result<(), ManifestError> {
        validate_path(&self.path)?;
        validate_integer(self.size, "size")?;
        validate_digest(&self.sha256)?;
        let mut chunks: Vec<_> = self.chunks.iter().collect();
        chunks.sort_by_key(|c| (c.offset, c.length));
        let mut intervals = BTreeSet::new();
        let mut coordinates = BTreeSet::new();
        let mut end = 0;
        for chunk in chunks {
            chunk.validate(self.size)?;
            if !intervals.insert((chunk.offset, chunk.length)) || chunk.offset < end {
                return Err(invalid("duplicate or overlapping chunks"));
            }
            end = chunk.offset + chunk.length;
            if let (Some(row), Some(col)) = (chunk.row_group, chunk.column) {
                if !coordinates.insert((row, col)) {
                    return Err(invalid("duplicate Parquet chunk coordinates"));
                }
            }
        }
        Ok(())
    }

    pub fn from_stream<R: Read + Seek>(
        path: impl Into<String>,
        stream: &mut R,
    ) -> Result<Self, ManifestError> {
        let size = stream.seek(SeekFrom::End(0))?;
        validate_integer(size, "size")?;
        stream.seek(SeekFrom::Start(0))?;
        let item = Self {
            path: path.into(),
            size,
            sha256: sha256_stream(stream, Some(size))?,
            chunks: Vec::new(),
        };
        item.validate()?;
        Ok(item)
    }

    pub fn verify_stream<R: Read + Seek>(&self, stream: &mut R) -> Result<bool, ManifestError> {
        self.validate()?;
        if stream.seek(SeekFrom::End(0))? != self.size {
            return Ok(false);
        }
        stream.seek(SeekFrom::Start(0))?;
        if !check_digest(stream, self.size, &self.sha256)? {
            return Ok(false);
        }
        for chunk in &self.chunks {
            stream.seek(SeekFrom::Start(chunk.offset))?;
            if !check_digest(stream, chunk.length, &chunk.sha256)? {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// Check only the selected range, without claiming whole-file coverage.
    pub fn verify_chunk<R: Read + Seek>(
        &self,
        stream: &mut R,
        index: usize,
    ) -> Result<bool, ManifestError> {
        self.validate()?;
        let mut chunks: Vec<_> = self.chunks.iter().collect();
        chunks.sort_by_key(|c| (c.offset, c.length));
        let chunk = chunks
            .get(index)
            .ok_or_else(|| invalid("chunk index out of range"))?;
        stream.seek(SeekFrom::Start(chunk.offset))?;
        Ok(check_digest(stream, chunk.length, &chunk.sha256)?)
    }
}

fn check_digest<R: Read>(stream: &mut R, size: u64, expected: &str) -> io::Result<bool> {
    match sha256_stream(stream, Some(size)) {
        Ok(actual) => Ok(actual == expected),
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => Ok(false),
        Err(e) => Err(e),
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CoverageReport {
    pub missing: Vec<String>,
    pub changed: Vec<String>,
    pub uncovered: Vec<String>,
}

impl CoverageReport {
    pub fn complete(&self) -> bool {
        self.missing.is_empty() && self.changed.is_empty() && self.uncovered.is_empty()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub version: u64,
    pub run_id: String,
    pub stage_id: String,
    pub files: Vec<FileDigest>,
}

impl Manifest {
    pub fn validate(&self) -> Result<(), ManifestError> {
        if self.version != 1 {
            return Err(invalid("unsupported manifest version"));
        }
        validate_identifier(&self.run_id, "run_id")?;
        validate_identifier(&self.stage_id, "stage_id")?;
        if self.files.is_empty() {
            return Err(invalid("a manifest must cover at least one file"));
        }
        let mut paths = BTreeSet::new();
        for file in &self.files {
            file.validate()?;
            if !paths.insert(file.path.to_ascii_lowercase()) {
                return Err(invalid("duplicate or case-colliding content path"));
            }
        }
        Ok(())
    }

    pub fn canonical_bytes(&self) -> Result<Vec<u8>, ManifestError> {
        self.validate()?;
        let mut normalized = self.clone();
        normalized.files.sort_by(|a, b| a.path.cmp(&b.path));
        for file in &mut normalized.files {
            file.chunks.sort_by_key(|c| (c.offset, c.length));
        }
        canonical_json(&normalized)
    }

    pub fn digest(&self) -> Result<String, ManifestError> {
        Ok(hex::encode(Sha256::digest(self.canonical_bytes()?)))
    }

    pub fn from_bytes(data: &[u8]) -> Result<Self, ManifestError> {
        // Struct deserialization rejects duplicate fields as well as extensions.
        let item: Self = serde_json::from_slice(data)?;
        item.validate()?;
        Ok(item)
    }

    /// Inventory must contain every content entry, excluding detached provenance
    /// storage. Duplicate ZIP names must be rejected by the archive adapter.
    pub fn verify_files(
        &self,
        files: &BTreeMap<String, Vec<u8>>,
    ) -> Result<CoverageReport, ManifestError> {
        self.validate()?;
        let expected: BTreeMap<_, _> = self.files.iter().map(|f| (f.path.clone(), f)).collect();
        let missing = expected
            .keys()
            .filter(|p| !files.contains_key(*p))
            .cloned()
            .collect();
        let uncovered = files
            .keys()
            .filter(|p| !expected.contains_key(*p))
            .cloned()
            .collect();
        let mut changed = Vec::new();
        for (path, file) in &expected {
            if let Some(bytes) = files.get(path) {
                if !file.verify_stream(&mut io::Cursor::new(bytes))? {
                    changed.push(path.clone());
                }
            }
        }
        Ok(CoverageReport {
            missing,
            changed,
            uncovered,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn sample() -> Manifest {
        Manifest {
            version: 1,
            run_id: "run-1".into(),
            stage_id: "acquisition-1".into(),
            files: vec![
                FileDigest::from_stream("spectra.parquet", &mut Cursor::new(b"abcdef")).unwrap(),
                FileDigest::from_stream("index.json", &mut Cursor::new(b"{}")).unwrap(),
            ],
        }
    }

    #[test]
    fn shared_vector_and_ordering() {
        let m = Manifest::from_bytes(include_bytes!(
            "../../test/data/provenance/manifest-v1.json"
        ))
        .unwrap();
        assert_eq!(
            m.canonical_bytes().unwrap(),
            include_bytes!("../../test/data/provenance/manifest-v1.json")
        );
        assert_eq!(
            m.digest().unwrap(),
            include_str!("../../test/data/provenance/manifest-v1.sha256").trim()
        );
        let mut reversed = m.clone();
        reversed.files.reverse();
        assert_eq!(
            m.canonical_bytes().unwrap(),
            reversed.canonical_bytes().unwrap()
        );
    }

    #[test]
    fn coverage_and_tampering() {
        let m = sample();
        let mut files = BTreeMap::from([
            ("spectra.parquet".into(), b"abcdef".to_vec()),
            ("index.json".into(), b"{}".to_vec()),
        ]);
        assert!(m.verify_files(&files).unwrap().complete());
        files.insert("spectra.parquet".into(), b"abcdeg".to_vec());
        files.remove("index.json");
        files.insert("extra.json".into(), b"{}".to_vec());
        let r = m.verify_files(&files).unwrap();
        assert_eq!(r.changed, ["spectra.parquet"]);
        assert_eq!(r.missing, ["index.json"]);
        assert_eq!(r.uncovered, ["extra.json"]);
    }

    #[test]
    fn explicit_zero_and_exact_ranges() {
        assert_eq!(
            sha256_stream(&mut Cursor::new(b"abc"), Some(0)).unwrap(),
            hex::encode(Sha256::digest(b""))
        );
        assert_eq!(
            sha256_stream(&mut Cursor::new(b"abc"), Some(4))
                .unwrap_err()
                .kind(),
            io::ErrorKind::UnexpectedEof
        );
        let mut f =
            FileDigest::from_stream("spectra.parquet", &mut Cursor::new(b"abcdef")).unwrap();
        f.chunks.push(ChunkDigest {
            offset: 0,
            length: 3,
            sha256: hex::encode(Sha256::digest(b"abc")),
            row_group: Some(0),
            column: Some(1),
            column_path: Some("intensity".into()),
        });
        let mut data = Cursor::new(b"abcXXX");
        data.set_position(4);
        assert!(f.verify_chunk(&mut data, 0).unwrap());
        assert!(!f.verify_stream(&mut data).unwrap());
    }

    #[test]
    fn strict_schema_and_paths() {
        for path in [
            "../escape",
            "/absolute",
            "C:/drive",
            "a\\b",
            "a//b",
            "provenance/stage.json",
            "provenance.parquet",
            "a/./b",
            "a.",
        ] {
            let mut m = sample();
            m.files[0].path = path.into();
            assert!(m.validate().is_err(), "{path}");
        }
        let mut m = sample();
        m.files.push(m.files[0].clone());
        assert!(m.validate().is_err());
        assert!(
            Manifest::from_bytes(
                br#"{"version":1,"version":1,"run_id":"x","stage_id":"y","files":[]}"#
            )
            .is_err()
        );
        let mut m = sample();
        m.files[0].size = MAX_INTEGER + 1;
        assert!(m.validate().is_err());
        let mut m = sample();
        m.version = 2;
        assert!(m.validate().is_err());
    }
}
