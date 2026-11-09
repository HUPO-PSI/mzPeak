//! Portable public proofs stored inside the ZIP. Trust policy stays external.
use super::{
    archive::{inventory, open_archive, open_archive_file},
    evidence::EvidenceBundle,
    manifest::{ManifestError, validate_digest},
    verification::{SignedRelease, SignedStage},
};
use serde::{
    Deserialize, Serialize,
    de::{self, MapAccess, Visitor},
};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::{self, Read, Write},
    path::Path,
};

pub const BUNDLE_PATH: &str = "provenance/bundle-v1.json";

#[derive(Debug, thiserror::Error)]
pub enum EmbeddedError {
    #[error("invalid embedded provenance: {0}")]
    Invalid(String),
    #[error(transparent)]
    Manifest(#[from] ManifestError),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("archive was replaced but parent directory synchronization failed: {0}")]
    PostCommitSync(#[source] std::io::Error),
    #[error(transparent)]
    Zip(#[from] zip::result::ZipError),
}
fn invalid(message: impl ToString) -> EmbeddedError {
    EmbeddedError::Invalid(message.to_string())
}

fn unique_map<'de, D, T>(deserializer: D) -> Result<BTreeMap<String, T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    struct Unique<T>(std::marker::PhantomData<T>);
    impl<'de, T: Deserialize<'de>> Visitor<'de> for Unique<T> {
        type Value = BTreeMap<String, T>;
        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            f.write_str("an object with unique keys")
        }
        fn visit_map<M: MapAccess<'de>>(self, mut map: M) -> Result<Self::Value, M::Error> {
            let mut result = BTreeMap::new();
            while let Some((key, value)) = map.next_entry::<String, T>()? {
                if result.insert(key, value).is_some() {
                    return Err(de::Error::custom("duplicate bundle map key"));
                }
            }
            Ok(result)
        }
    }
    deserializer.deserialize_map(Unique(std::marker::PhantomData))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProvenanceBundle {
    pub version: u32,
    pub records: Vec<SignedStage>,
    #[serde(deserialize_with = "unique_map")]
    pub releases: BTreeMap<String, SignedRelease>,
    #[serde(deserialize_with = "unique_map")]
    pub stage_evidence: BTreeMap<String, EvidenceBundle>,
    #[serde(deserialize_with = "unique_map")]
    pub release_evidence: BTreeMap<String, EvidenceBundle>,
}
impl Default for ProvenanceBundle {
    fn default() -> Self {
        Self {
            version: 1,
            records: vec![],
            releases: BTreeMap::new(),
            stage_evidence: BTreeMap::new(),
            release_evidence: BTreeMap::new(),
        }
    }
}
impl ProvenanceBundle {
    pub fn validate(&self) -> Result<(), EmbeddedError> {
        if self.version != 1 || self.records.is_empty() {
            return Err(invalid("unsupported or empty bundle"));
        }
        let mut stages = BTreeMap::new();
        let mut runs = BTreeSet::new();
        for record in &self.records {
            record.stage.validate()?;
            if record.envelope.version != 2 {
                return Err(invalid("unsupported signature envelope version"));
            }
            record.envelope.canonical_bytes().map_err(invalid)?;
            if stages
                .insert(record.stage.manifest.stage_id.clone(), record)
                .is_some()
            {
                return Err(invalid("duplicate embedded stage ID"));
            }
            runs.insert(&record.stage.manifest.run_id);
        }
        if runs.len() != 1 {
            return Err(invalid("embedded stages must belong to one run"));
        }
        for (key, release) in &self.releases {
            release.statement.canonical_bytes()?;
            if release.envelope.version != 2 {
                return Err(invalid("unsupported signature envelope version"));
            }
            release.envelope.canonical_bytes().map_err(invalid)?;
            if key != &release.statement.release_sha256 {
                return Err(invalid("release key differs from statement"));
            }
        }
        fn binding(
            bundle: &EvidenceBundle,
            payload: &[u8],
            envelope: &[u8],
        ) -> Result<(), EmbeddedError> {
            if bundle.version != 1
                || bundle.policy_id.is_empty()
                || bundle.manifest_sha256 != hex::encode(Sha256::digest(payload))
                || bundle.envelope_sha256 != hex::encode(Sha256::digest(envelope))
            {
                return Err(invalid("evidence is not bound to its signed record"));
            }
            validate_digest(&bundle.policy_sha256)?;
            if bundle.to_json().map_err(invalid)?.len() > 16 * 1024 * 1024 {
                return Err(invalid("evidence bundle too large"));
            }
            Ok(())
        }
        for (key, evidence) in &self.stage_evidence {
            let record = stages
                .get(key)
                .ok_or_else(|| invalid("evidence refers to unknown stage"))?;
            binding(
                evidence,
                &record.stage.canonical_bytes()?,
                &record.envelope.canonical_bytes().map_err(invalid)?,
            )?;
        }
        for (key, evidence) in &self.release_evidence {
            let release = self
                .releases
                .get(key)
                .ok_or_else(|| invalid("evidence refers to unknown release"))?;
            binding(
                evidence,
                &release.statement.canonical_bytes()?,
                &release.envelope.canonical_bytes().map_err(invalid)?,
            )?;
        }
        Ok(())
    }
    pub fn from_json(bytes: &[u8]) -> Result<Self, EmbeddedError> {
        let bundle: Self = serde_json::from_slice(bytes)?;
        bundle.validate()?;
        Ok(bundle)
    }
    pub fn to_json(&self) -> Result<Vec<u8>, EmbeddedError> {
        self.validate()?;
        let bytes = serde_json::to_vec(self)?;
        Ok(bytes)
    }
    fn merge(mut self, additions: &Self) -> Result<Self, EmbeddedError> {
        if additions.version != 1 {
            return Err(invalid("unsupported bundle version"));
        }
        let mut positions = self
            .records
            .iter()
            .enumerate()
            .map(|(index, record)| (record.stage.manifest.stage_id.clone(), index))
            .collect::<BTreeMap<_, _>>();
        let mut seen = BTreeSet::new();
        for record in &additions.records {
            let key = &record.stage.manifest.stage_id;
            if !seen.insert(key) {
                return Err(invalid("duplicate added stage"));
            }
            if let Some(&index) = positions.get(key) {
                let old = &self.records[index];
                if old.stage.canonical_bytes()? != record.stage.canonical_bytes()?
                    || old.envelope.canonical_bytes().map_err(invalid)?
                        != record.envelope.canonical_bytes().map_err(invalid)?
                {
                    return Err(invalid("cannot replace embedded stage"));
                }
            } else {
                positions.insert(key.clone(), self.records.len());
                self.records.push(record.clone());
            }
        }
        fn extend<T: Clone + Serialize>(
            old: &mut BTreeMap<String, T>,
            new: &BTreeMap<String, T>,
        ) -> Result<(), EmbeddedError> {
            for (key, value) in new {
                if let Some(previous) = old.get(key) {
                    if serde_json::to_value(previous)? != serde_json::to_value(value)? {
                        return Err(invalid("cannot replace embedded approval or evidence"));
                    }
                } else {
                    old.insert(key.clone(), value.clone());
                }
            }
            Ok(())
        }
        extend(&mut self.releases, &additions.releases)?;
        extend(&mut self.stage_evidence, &additions.stage_evidence)?;
        extend(&mut self.release_evidence, &additions.release_evidence)?;
        self.validate()?;
        Ok(self)
    }
}

fn read_bundle(
    archive: &mut zip::ZipArchive<fs::File>,
    max_bytes: Option<u64>,
) -> Result<Option<ProvenanceBundle>, EmbeddedError> {
    inventory(archive)?;
    for name in archive.file_names() {
        if name.trim_end_matches('/').eq_ignore_ascii_case(BUNDLE_PATH) && name != BUNDLE_PATH {
            return Err(invalid("noncanonical bundle member name"));
        }
    }
    let mut member = match archive.by_name(BUNDLE_PATH) {
        Ok(member) => member,
        Err(zip::result::ZipError::FileNotFound) => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    if max_bytes.is_some_and(|limit| member.size() > limit) {
        return Err(invalid("bundle exceeds caller byte budget"));
    }
    let mut bytes = vec![];
    member
        .by_ref()
        .take(max_bytes.map(|n| n.saturating_add(1)).unwrap_or(u64::MAX))
        .read_to_end(&mut bytes)?;
    if max_bytes.is_some_and(|limit| bytes.len() as u64 > limit) {
        return Err(invalid("bundle exceeds caller byte budget"));
    }
    Ok(Some(ProvenanceBundle::from_json(&bytes)?))
}

pub fn read_embedded_provenance(path: impl AsRef<Path>) -> Result<ProvenanceBundle, EmbeddedError> {
    read_bundle(&mut open_archive(path)?, None)?
        .ok_or_else(|| invalid("archive has no embedded provenance bundle"))
}

/// Read with a caller-selected uncompressed byte budget (including ZIP metadata checks).
pub fn read_embedded_provenance_with_limit(
    path: impl AsRef<Path>,
    max_bytes: u64,
) -> Result<ProvenanceBundle, EmbeddedError> {
    read_bundle(&mut open_archive(path)?, Some(max_bytes))?
        .ok_or_else(|| invalid("archive has no embedded provenance bundle"))
}

/// Atomically merge public proofs while preserving all existing records/content.
/// Requires exclusive writer access. Packaging never establishes trust.
pub fn embed_provenance(
    path: impl AsRef<Path>,
    additions: &ProvenanceBundle,
) -> Result<ProvenanceBundle, EmbeddedError> {
    embed_with_budget(path.as_ref(), additions, None)
}

/// Merge while bounding both the old and resulting uncompressed bundle size.
pub fn embed_provenance_with_limit(
    path: impl AsRef<Path>,
    additions: &ProvenanceBundle,
    max_bytes: u64,
) -> Result<ProvenanceBundle, EmbeddedError> {
    embed_with_budget(path.as_ref(), additions, Some(max_bytes))
}

fn embed_with_budget(
    path: &Path,
    additions: &ProvenanceBundle,
    max_bytes: Option<u64>,
) -> Result<ProvenanceBundle, EmbeddedError> {
    let initial_path = fs::symlink_metadata(path)?;
    if initial_path.file_type().is_symlink() {
        return Err(invalid("refusing to rewrite through a symbolic link"));
    }
    if !initial_path.is_file() {
        return Err(invalid("archive must be a regular file"));
    }
    // Keep the actual source open until the identity check. Reopening the path
    // independently for the reader would leave a gap between stat and reading.
    let source = fs::File::open(path)?;
    let initial = source.metadata()?;
    let identity = file_identity(&source)?;
    let mut archive = open_archive_file(source.try_clone()?)?;
    let bundle = read_bundle(&mut archive, max_bytes)?
        .unwrap_or_default()
        .merge(additions)?;
    let encoded = bundle.to_json()?;
    if max_bytes.is_some_and(|limit| encoded.len() as u64 > limit) {
        return Err(invalid("bundle exceeds caller byte budget"));
    }
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
    {
        let mut output = zip::ZipWriter::new(temporary.as_file_mut());
        output.set_raw_comment(archive.comment().to_vec().into());
        for index in 0..archive.len() {
            let mut item = archive.by_index(index)?;
            if item.name() == BUNDLE_PATH {
                continue;
            }
            // Read through EOF to validate CRC before preserving compressed bytes.
            std::io::copy(&mut item, &mut std::io::sink())?;
            drop(item);
            output.raw_copy_file(archive.by_index(index)?)?;
        }
        output.start_file(
            BUNDLE_PATH,
            zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Stored)
                .large_file(true),
        )?;
        output.write_all(&encoded)?;
        output.finish()?;
    }
    drop(archive);
    persist_archive(temporary, source, &identity, &initial, path, parent)?;
    Ok(bundle)
}

#[derive(Debug, PartialEq, Eq)]
struct FileIdentity {
    volume: u64,
    id: u128,
}

#[cfg(unix)]
fn file_identity(file: &fs::File) -> io::Result<FileIdentity> {
    use std::os::unix::fs::MetadataExt;
    let metadata = file.metadata()?;
    Ok(FileIdentity {
        volume: metadata.dev(),
        id: metadata.ino().into(),
    })
}

#[cfg(windows)]
fn file_identity(file: &fs::File) -> io::Result<FileIdentity> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_ID_INFO, FileIdInfo, GetFileInformationByHandleEx,
    };
    let mut info = std::mem::MaybeUninit::<FILE_ID_INFO>::uninit();
    // The live File owns this valid handle; the buffer has the exact requested
    // layout/size and is read only after Windows reports successful completion.
    let success = unsafe {
        GetFileInformationByHandleEx(
            file.as_raw_handle(),
            FileIdInfo,
            info.as_mut_ptr().cast(),
            std::mem::size_of::<FILE_ID_INFO>() as u32,
        )
    };
    if success == 0 {
        return Err(io::Error::last_os_error());
    }
    let info = unsafe { info.assume_init() };
    Ok(FileIdentity {
        volume: info.VolumeSerialNumber,
        id: u128::from_le_bytes(info.FileId.Identifier),
    })
}

#[cfg(not(any(unix, windows)))]
fn file_identity(_file: &fs::File) -> io::Result<FileIdentity> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "archive file identity unavailable",
    ))
}

fn persist_archive(
    temporary: tempfile::NamedTempFile,
    source: fs::File,
    identity: &FileIdentity,
    initial: &fs::Metadata,
    path: &Path,
    parent: &Path,
) -> Result<(), EmbeddedError> {
    let current = fs::symlink_metadata(path)?;
    if current.file_type().is_symlink()
        || current.len() != initial.len()
        || current.modified()? != initial.modified()?
        || file_identity(&fs::File::open(path)?)? != *identity
    {
        return Err(invalid("archive changed while embedding provenance"));
    }
    // The handle keeps the original file ID live through the check. It must be
    // closed before replacement on Windows. This check does not replace the
    // documented exclusive-writer requirement.
    drop(source);
    fs::set_permissions(temporary.path(), initial.permissions())?;
    temporary.as_file().sync_all()?;
    #[cfg(unix)]
    let directory = fs::File::open(parent)?;
    #[cfg(not(unix))]
    let _ = parent; // Windows has no portable directory-fsync operation.
    temporary.persist(path).map_err(|error| error.error)?;
    #[cfg(unix)]
    directory
        .sync_all()
        .map_err(EmbeddedError::PostCommitSync)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::{
        evidence::TsaPolicy,
        historical::{HistoricalPolicy, verify_historical_archive_history},
        signing::{OpenSslBackend, SignaturePolicy},
        trust::TrustPolicy,
        verification::HistoryPolicy,
    };
    use super::*;

    fn read(name: &str) -> Vec<u8> {
        fs::read(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("test/data/provenance/timestamps/history")
                .join(name),
        )
        .unwrap()
    }
    fn json<T: serde::de::DeserializeOwned>(name: &str) -> T {
        serde_json::from_slice(&read(name)).unwrap()
    }
    fn bundle() -> ProvenanceBundle {
        ProvenanceBundle {
            records: json("records.json"),
            releases: json("releases.json"),
            stage_evidence: json("stage-evidence.json"),
            release_evidence: json("release-evidence.json"),
            ..Default::default()
        }
    }
    fn archive(path: &Path) {
        let mut zip = zip::ZipWriter::new(fs::File::create(path).unwrap());
        for (name, bytes) in json::<BTreeMap<String, String>>("files.json") {
            zip.start_file(name, zip::write::SimpleFileOptions::default())
                .unwrap();
            zip.write_all(&hex::decode(bytes).unwrap()).unwrap();
        }
        zip.finish().unwrap();
    }

    #[test]
    fn replacement_with_identical_size_and_time_has_a_distinct_file_identity() {
        let directory = tempfile::tempdir().unwrap();
        let first = directory.path().join("first.mzpeak");
        let replacement = directory.path().join("replacement.mzpeak");
        archive(&first);
        fs::copy(&first, &replacement).unwrap();
        let source = fs::File::open(&first).unwrap();
        let initial = source.metadata().unwrap();
        let other = fs::File::options().write(true).open(&replacement).unwrap();
        other
            .set_times(fs::FileTimes::new().set_modified(initial.modified().unwrap()))
            .unwrap();
        let other_metadata = other.metadata().unwrap();
        assert_eq!(initial.len(), other_metadata.len());
        assert_eq!(
            initial.modified().unwrap(),
            other_metadata.modified().unwrap()
        );
        let identity = file_identity(&source).unwrap();
        assert_ne!(identity, file_identity(&other).unwrap());
        drop(other);
        let before = fs::read(&replacement).unwrap();
        let mut temporary = tempfile::NamedTempFile::new_in(directory.path()).unwrap();
        temporary
            .write_all(b"must not replace a different file")
            .unwrap();
        let temporary_path = temporary.path().to_path_buf();
        let error = persist_archive(
            temporary,
            source,
            &identity,
            &initial,
            &replacement,
            directory.path(),
        )
        .unwrap_err();
        assert!(error.to_string().contains("archive changed"));
        assert_eq!(fs::read(replacement).unwrap(), before);
        assert!(!temporary_path.exists());
    }

    #[test]
    fn embedded_bundle_uses_zip64_local_header() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("zip64.mzpeak");
        archive(&path);
        let full = bundle();
        let expected_size = full.to_json().unwrap().len() as u64;
        embed_provenance(&path, &full).unwrap();
        let header_start = {
            let mut zip = open_archive(&path).unwrap();
            let member = zip.by_name(BUNDLE_PATH).unwrap();
            assert_eq!(member.compression(), zip::CompressionMethod::Stored);
            assert_eq!(member.size(), expected_size);
            member.header_start() as usize
        };
        let bytes = fs::read(&path).unwrap();
        let header = &bytes[header_start..];
        assert_eq!(&header[..4], b"PK\x03\x04");
        // Both 32-bit size slots signal that the actual sizes are in ZIP64.
        assert_eq!(&header[18..26], &[0xff; 8]);
        let name_len = u16::from_le_bytes(header[26..28].try_into().unwrap()) as usize;
        let extra_len = u16::from_le_bytes(header[28..30].try_into().unwrap()) as usize;
        assert_eq!(&header[30..30 + name_len], BUNDLE_PATH.as_bytes());
        let mut extra = &header[30 + name_len..30 + name_len + extra_len];
        let mut sizes = None;
        while !extra.is_empty() {
            let tag = u16::from_le_bytes(extra[..2].try_into().unwrap());
            let length = u16::from_le_bytes(extra[2..4].try_into().unwrap()) as usize;
            if tag == 1 {
                assert!(length >= 16);
                sizes = Some((
                    u64::from_le_bytes(extra[4..12].try_into().unwrap()),
                    u64::from_le_bytes(extra[12..20].try_into().unwrap()),
                ));
            }
            extra = &extra[4 + length..];
        }
        assert_eq!(sizes, Some((expected_size, expected_size)));
        assert_eq!(
            read_embedded_provenance(&path).unwrap().to_json().unwrap(),
            full.to_json().unwrap()
        );
    }

    #[test]
    fn stage_batch_merge_preserves_order_idempotence_and_conflict_checks() {
        let seed = bundle().records.remove(0);
        let make = |index: usize| {
            let mut record = seed.clone();
            record.stage.manifest.stage_id = format!("stage-{index:04}");
            record
        };
        let original = ProvenanceBundle {
            records: (0..128).map(make).collect(),
            ..Default::default()
        };
        let additions = ProvenanceBundle {
            records: (64..256).rev().map(make).collect(),
            ..Default::default()
        };
        let merged = original.clone().merge(&additions).unwrap();
        let expected = (0..128)
            .chain((128..256).rev())
            .map(|i| format!("stage-{i:04}"))
            .collect::<Vec<_>>();
        assert_eq!(
            merged
                .records
                .iter()
                .map(|record| record.stage.manifest.stage_id.clone())
                .collect::<Vec<_>>(),
            expected
        );
        assert_eq!(
            merged.clone().merge(&additions).unwrap().to_json().unwrap(),
            merged.to_json().unwrap()
        );
        for index in [0, 256] {
            let duplicate = ProvenanceBundle {
                records: vec![make(index), make(index)],
                ..Default::default()
            };
            assert!(
                merged
                    .clone()
                    .merge(&duplicate)
                    .unwrap_err()
                    .to_string()
                    .contains("duplicate added stage")
            );
        }
        let mut conflict = make(0);
        conflict.envelope.signatures[0].cms_der_hex.push_str("00");
        let additions = ProvenanceBundle {
            records: vec![make(256), conflict],
            ..Default::default()
        };
        assert!(
            merged
                .clone()
                .merge(&additions)
                .unwrap_err()
                .to_string()
                .contains("cannot replace embedded stage")
        );
        let initial_batch = ProvenanceBundle::default().merge(&original).unwrap();
        assert_eq!(
            initial_batch.to_json().unwrap(),
            original.to_json().unwrap()
        );
    }

    #[test]
    fn moved_archive_carries_complete_historical_proof() {
        let temp = tempfile::tempdir().unwrap();
        let original = temp.path().join("original.mzpeak");
        archive(&original);
        let before =
            super::super::archive::capture_archive(&original, "run", "stage", None).unwrap();
        let full = bundle();
        let mut unsigned_evidence = full.clone();
        unsigned_evidence.stage_evidence.clear();
        unsigned_evidence.release_evidence.clear();
        embed_provenance(&original, &unsigned_evidence).unwrap();
        embed_provenance(
            &original,
            &ProvenanceBundle {
                stage_evidence: full.stage_evidence.clone(),
                release_evidence: full.release_evidence.clone(),
                ..Default::default()
            },
        )
        .unwrap();
        embed_provenance(&original, &full).unwrap();
        let moved = temp.path().join("moved.mzpeak");
        fs::rename(&original, &moved).unwrap();
        let loaded = read_embedded_provenance(&moved).unwrap();
        assert_eq!(loaded.to_json().unwrap(), full.to_json().unwrap());
        assert_eq!(
            super::super::archive::capture_archive(&moved, "run", "stage", None).unwrap(),
            before
        );
        let trust: TrustPolicy = json("trust.json");
        let tsa: TsaPolicy = json("tsa-policy.json");
        let identities = BTreeMap::from([
            ("acquisition".into(), "instrument-1".into()),
            ("processing".into(), "installation-1".into()),
        ]);
        let publishers = loaded
            .releases
            .keys()
            .map(|k| (k.clone(), "publisher-1".into()))
            .collect();
        let heads: Vec<String> = json("heads.json");
        let policy = HistoryPolicy {
            trust: &trust,
            required_signatures: SignaturePolicy::Ecdsa,
            identities: &identities,
            publishers: &publishers,
            expected_heads: Some(&heads),
            conversion: None,
        };
        let historical = HistoricalPolicy {
            tsa: &tsa,
            policy_id: "historical-demo-v1",
            policy_bytes: &read("policy.json"),
            stage_evidence: &loaded.stage_evidence,
            release_evidence: &loaded.release_evidence,
        };
        let report = verify_historical_archive_history(
            &moved,
            &loaded.records,
            &policy,
            &loaded.releases,
            &OpenSslBackend::default(),
            &historical,
        )
        .unwrap();
        assert!(report.accepted(), "{report:?}");
    }
    #[test]
    fn conflicting_proofs_do_not_replace_the_archive() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("data.mzpeak");
        archive(&path);
        assert!(read_embedded_provenance(&path).is_err());
        embed_provenance(&path, &bundle()).unwrap();
        let before = fs::read(&path).unwrap();
        let mut conflict = bundle();
        conflict.records[0].stage.manifest.run_id = "different".into();
        assert!(embed_provenance(&path, &conflict).is_err());
        assert_eq!(fs::read(&path).unwrap(), before);
    }
    #[test]
    fn ambiguous_json_and_unbound_evidence_are_rejected() {
        let full = bundle();
        let mut value = serde_json::to_value(&full).unwrap();
        value["trust"] = serde_json::json!({});
        assert!(ProvenanceBundle::from_json(&serde_json::to_vec(&value).unwrap()).is_err());
        let encoded = String::from_utf8(full.to_json().unwrap()).unwrap();
        assert!(
            ProvenanceBundle::from_json(format!("{{\"version\":1,{}", &encoded[1..]).as_bytes())
                .is_err()
        );
        let (key, evidence) = full.stage_evidence.first_key_value().unwrap();
        let repeated = format!("\"{key}\":{},", serde_json::to_string(evidence).unwrap());
        let duplicate = encoded.replacen(
            "\"stage_evidence\":{",
            &format!("\"stage_evidence\":{{{repeated}"),
            1,
        );
        assert!(ProvenanceBundle::from_json(duplicate.as_bytes()).is_err());
        let mut invalid = full;
        invalid
            .stage_evidence
            .get_mut("acquisition")
            .unwrap()
            .envelope_sha256 = "0".repeat(64);
        assert!(invalid.to_json().is_err());
    }
    #[test]
    fn large_bundle_inputs_use_caller_selected_budget() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("large.mzpeak");
        let full = bundle();
        let mut zip = zip::ZipWriter::new(fs::File::create(&path).unwrap());
        zip.start_file(
            BUNDLE_PATH,
            zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Deflated),
        )
        .unwrap();
        let padding = vec![b' '; 1024 * 1024];
        for _ in 0..65 {
            zip.write_all(&padding).unwrap();
        }
        zip.write_all(&full.to_json().unwrap()).unwrap();
        zip.finish().unwrap();
        assert!(
            read_embedded_provenance_with_limit(&path, 64 * 1024 * 1024)
                .unwrap_err()
                .to_string()
                .contains("caller byte budget")
        );
        assert_eq!(
            read_embedded_provenance(&path).unwrap().records.len(),
            full.records.len()
        );
        let before = fs::read(&path).unwrap();
        assert!(embed_provenance_with_limit(&path, &full, 32).is_err());
        assert_eq!(fs::read(&path).unwrap(), before);
        embed_provenance(&path, &full).unwrap();
        assert!(
            read_embedded_provenance_with_limit(&path, full.to_json().unwrap().len() as u64)
                .is_ok()
        );
    }

    #[test]
    fn aliased_bundle_and_oversized_member_are_rejected() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("bad.mzpeak");
        let mut zip = zip::ZipWriter::new(fs::File::create(&path).unwrap());
        zip.start_file(
            "Provenance/bundle-v1.json",
            zip::write::SimpleFileOptions::default(),
        )
        .unwrap();
        zip.write_all(&bundle().to_json().unwrap()).unwrap();
        zip.finish().unwrap();
        assert!(read_embedded_provenance(&path).is_err());
        let before = fs::read(&path).unwrap();
        assert!(embed_provenance(&path, &bundle()).is_err());
        assert_eq!(fs::read(&path).unwrap(), before);
        let mut zip = zip::ZipWriter::new(fs::File::create(&path).unwrap());
        zip.add_directory(
            format!("{BUNDLE_PATH}/"),
            zip::write::SimpleFileOptions::default(),
        )
        .unwrap();
        zip.finish().unwrap();
        let before = fs::read(&path).unwrap();
        assert!(embed_provenance(&path, &bundle()).is_err());
        assert_eq!(fs::read(&path).unwrap(), before);
        let mut zip = zip::ZipWriter::new(fs::File::create(&path).unwrap());
        zip.start_file(BUNDLE_PATH, zip::write::SimpleFileOptions::default())
            .unwrap();
        zip.write_all(b"{}").unwrap();
        zip.finish().unwrap();
        let mut bytes = fs::read(&path).unwrap();
        let offset = bytes.windows(4).position(|b| b == b"PK\x01\x02").unwrap();
        bytes[offset + 24..offset + 28]
            .copy_from_slice(&((64 * 1024 * 1024 + 1) as u32).to_le_bytes());
        fs::write(&path, bytes).unwrap();
        assert!(read_embedded_provenance(&path).is_err());
    }
}
