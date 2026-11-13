//! External vendor RAW inventories and signed post-acquisition verification claims.
//! These types describe claims; signer authorization is a separate policy check.
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::{self, Read},
    path::{Path, PathBuf},
};

use serde::{Deserialize, Deserializer, Serialize};
use sha2::{Digest, Sha256};

use super::manifest::{
    CoverageReport, ManifestError, canonical_json, invalid, required_option, validate_digest,
    validate_integer, validate_path, validate_transport_path,
};

pub(crate) fn text(value: &str, label: &str) -> Result<(), ManifestError> {
    if value.is_empty() || value.len() > 128 || !value.bytes().all(|b| (32..=126).contains(&b)) {
        return Err(invalid(format!("invalid {label}")));
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceFile {
    pub path: String,
    pub size: u64,
    pub sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceManifest {
    pub version: u64,
    pub format: String,
    pub files: Vec<SourceFile>,
}
impl SourceManifest {
    pub fn validate(&self) -> Result<(), ManifestError> {
        if self.version != 1 || self.files.is_empty() {
            return Err(invalid("unsupported or empty source inventory"));
        }
        text(&self.format, "source format")?;
        let mut names = BTreeSet::new();
        for file in &self.files {
            validate_transport_path(&file.path)?;
            validate_integer(file.size, "source file size")?;
            validate_digest(&file.sha256)?;
            if !names.insert(file.path.to_ascii_lowercase()) {
                return Err(invalid("duplicate or case-colliding source path"));
            }
        }
        Ok(())
    }
    pub fn canonical_bytes(&self) -> Result<Vec<u8>, ManifestError> {
        self.validate()?;
        let mut normalized = self.clone();
        normalized.files.sort_by(|a, b| a.path.cmp(&b.path));
        canonical_json(&normalized)
    }
    pub fn digest(&self) -> Result<String, ManifestError> {
        Ok(hex::encode(Sha256::digest(self.canonical_bytes()?)))
    }
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, ManifestError> {
        let value: Self = serde_json::from_slice(bytes)?;
        value.validate()?;
        Ok(value)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReportReference {
    pub path: String,
    pub sha256: String,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum VerificationResult {
    Passed,
    Failed,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VendorVerification {
    pub vendor: String,
    pub method: String,
    pub verifier_name: String,
    pub verifier_version: String,
    pub source_sha256: String,
    pub result: VerificationResult,
    #[serde(deserialize_with = "required_option")]
    pub report: Option<ReportReference>,
}
impl VendorVerification {
    pub fn validate(&self) -> Result<(), ManifestError> {
        for (value, label) in [
            (&self.vendor, "vendor"),
            (&self.method, "verification method"),
            (&self.verifier_name, "verifier name"),
            (&self.verifier_version, "verifier version"),
        ] {
            text(value, label)?;
        }
        validate_digest(&self.source_sha256)?;
        if let Some(report) = &self.report {
            validate_path(&report.path)?;
            validate_digest(&report.sha256)?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConversionClaim {
    pub source: SourceManifest,
    pub verification: VendorVerification,
    #[serde(deserialize_with = "required_option")]
    pub claimed_acquisition_time: Option<u64>,
}
impl ConversionClaim {
    pub fn validate(&self) -> Result<(), ManifestError> {
        self.source.validate()?;
        self.verification.validate()?;
        if self.verification.source_sha256 != self.source.digest()? {
            return Err(invalid(
                "vendor verification refers to a different source inventory",
            ));
        }
        if let Some(time) = self.claimed_acquisition_time {
            validate_integer(time, "claimed acquisition time")?;
        }
        Ok(())
    }
}

// Unlike an absent optional field, an explicitly supplied null conversion is
// always invalid. This preserves strict v1 parsing without changing signed bytes.
pub(crate) fn present_conversion<'de, D: Deserializer<'de>>(
    d: D,
) -> Result<Option<ConversionClaim>, D::Error> {
    ConversionClaim::deserialize(d).map(Some)
}

fn inventory(root: &Path) -> Result<BTreeMap<String, PathBuf>, ManifestError> {
    fn walk(
        root: &Path,
        current: &Path,
        files: &mut BTreeMap<String, PathBuf>,
        names: &mut BTreeSet<String>,
    ) -> Result<(), ManifestError> {
        let metadata = fs::symlink_metadata(current)?;
        if linked(&metadata) {
            return Err(invalid("symbolic links are not RAW source content"));
        }
        // Validate directory names too, including empty directories. Otherwise
        // Rust could accept a dataset the Python capture rejects as nonportable.
        if current != root {
            let relative = current
                .strip_prefix(root)
                .map_err(|_| invalid("source path is outside its root"))?;
            let parts = relative
                .components()
                .map(|part| match part {
                    std::path::Component::Normal(value) => value
                        .to_str()
                        .ok_or_else(|| invalid("source paths must be portable ASCII")),
                    _ => Err(invalid("unsafe source path")),
                })
                .collect::<Result<Vec<_>, _>>()?;
            validate_transport_path(&parts.join("/"))?;
        }
        if metadata.is_dir() {
            for entry in fs::read_dir(current)? {
                walk(root, &entry?.path(), files, names)?;
            }
        } else if metadata.is_file() {
            let relative = current
                .strip_prefix(root)
                .map_err(|_| invalid("source path is outside its root"))?;
            let mut components = Vec::new();
            for part in relative.components() {
                let std::path::Component::Normal(value) = part else {
                    return Err(invalid("unsafe source path"));
                };
                components.push(
                    value
                        .to_str()
                        .ok_or_else(|| invalid("source paths must be portable ASCII"))?,
                );
            }
            let name = components.join("/");
            validate_transport_path(&name)?;
            if !names.insert(name.to_ascii_lowercase()) {
                return Err(invalid("case-colliding source paths"));
            }
            files.insert(name, current.to_owned());
        } else {
            return Err(invalid(
                "RAW source must contain only regular files and directories",
            ));
        }
        Ok(())
    }
    let metadata = fs::symlink_metadata(root)?;
    if linked(&metadata) {
        return Err(invalid("symbolic links are not RAW source content"));
    }
    let mut files = BTreeMap::new();
    if metadata.is_file() {
        let name = root
            .file_name()
            .and_then(|s| s.to_str())
            .ok_or_else(|| invalid("source file needs a portable name"))?;
        validate_transport_path(name)?;
        files.insert(name.to_owned(), root.to_owned());
    } else if metadata.is_dir() {
        walk(root, root, &mut files, &mut BTreeSet::new())?;
    } else {
        return Err(invalid("RAW source must be a regular file or directory"));
    }
    Ok(files)
}

#[cfg(windows)]
fn linked(metadata: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    metadata.file_type().is_symlink() || metadata.file_attributes() & 0x400 != 0
}
#[cfg(not(windows))]
fn linked(metadata: &fs::Metadata) -> bool {
    metadata.file_type().is_symlink()
}

#[cfg(unix)]
fn same_file(a: &fs::Metadata, b: &fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    a.dev() == b.dev()
        && a.ino() == b.ino()
        && a.ctime() == b.ctime()
        && a.ctime_nsec() == b.ctime_nsec()
}
#[cfg(not(unix))]
fn same_file(a: &fs::Metadata, b: &fs::Metadata) -> bool {
    // This preliminary metadata comparison is supplemented below with the
    // actual open handle's volume and 128-bit file identity on Windows.
    a.created().ok() == b.created().ok()
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
    // File owns a live handle; the buffer matches FileIdInfo's documented
    // layout and is read only after a successful API call.
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
        "source file identity unavailable",
    ))
}
fn matches_snapshot(
    path: &Path,
    before: &fs::Metadata,
    identity: FileIdentity,
) -> io::Result<bool> {
    let after = fs::symlink_metadata(path)?;
    if !after.is_file()
        || linked(&after)
        || !same_file(before, &after)
        || before.len() != after.len()
        || before.modified()? != after.modified()?
    {
        return Ok(false);
    }
    let file = fs::File::open(path)?;
    let opened = file.metadata()?;
    Ok(file_identity(&file)? == identity
        && same_file(before, &opened)
        && opened.len() == before.len()
        && opened.modified()? == before.modified()?)
}
fn hash_file(
    name: String,
    path: &Path,
    after_read: &mut impl FnMut(&Path),
) -> Result<(SourceFile, fs::Metadata, FileIdentity), ManifestError> {
    let before = fs::symlink_metadata(path)?;
    if !before.is_file() || linked(&before) {
        return Err(invalid("RAW source changed during capture"));
    }
    let mut file = fs::File::open(path)?;
    let opened = file.metadata()?;
    let identity = file_identity(&file)?;
    if !same_file(&before, &opened)
        || opened.len() != before.len()
        || opened.modified()? != before.modified()?
    {
        return Err(invalid("RAW source changed during capture"));
    }
    validate_integer(opened.len(), "source file size")?;
    let mut hash = Sha256::new();
    let mut read = 0_u64;
    let mut buffer = [0_u8; 65536];
    loop {
        let count = match file.read(&mut buffer) {
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            other => other?,
        };
        if count == 0 {
            break;
        }
        read = read
            .checked_add(count as u64)
            .ok_or_else(|| invalid("source length overflow"))?;
        hash.update(&buffer[..count]);
        after_read(path);
    }
    let after = fs::symlink_metadata(path)?;
    let retained = file.metadata()?;
    if !after.is_file()
        || linked(&after)
        || !same_file(&opened, &after)
        || !same_file(&opened, &retained)
        || read != opened.len()
        || after.len() != opened.len()
        || retained.len() != opened.len()
        || after.modified()? != opened.modified()?
        || retained.modified()? != opened.modified()?
        || !matches_snapshot(path, &opened, identity)?
    {
        return Err(invalid("RAW source changed during capture"));
    }
    Ok((
        SourceFile {
            path: name,
            size: read,
            sha256: hex::encode(hash.finalize()),
        },
        retained,
        identity,
    ))
}
fn capture_files(path: &Path) -> Result<Vec<SourceFile>, ManifestError> {
    capture_files_with_read_observer(path, &mut |_| {})
}
// The no-op observer above optimizes away; tests can deterministically rewrite
// a source during hashing without depending on concurrent filesystem timing.
fn capture_files_with_read_observer(
    path: &Path,
    after_read: &mut impl FnMut(&Path),
) -> Result<Vec<SourceFile>, ManifestError> {
    let paths = inventory(path)?;
    let mut files = Vec::new();
    let mut snapshots = Vec::new();
    for (name, full) in &paths {
        let (file, metadata, identity) = hash_file(name.clone(), full, after_read)?;
        files.push(file);
        snapshots.push((full, metadata, identity));
    }
    if inventory(path)? != paths {
        return Err(invalid("RAW source inventory changed during capture"));
    }
    for (full, before, identity) in snapshots {
        if !matches_snapshot(full, &before, identity)? {
            return Err(invalid("RAW source changed during capture"));
        }
    }
    Ok(files)
}

/// Hash an immutable caller-supplied RAW file/dataset; never run vendor software.
pub fn capture_source(
    path: impl AsRef<Path>,
    format: &str,
) -> Result<SourceManifest, ManifestError> {
    let result = SourceManifest {
        version: 1,
        format: format.to_owned(),
        files: capture_files(path.as_ref())?,
    };
    result.validate()?;
    Ok(result)
}
/// Recheck an explicitly supplied local source, independently of vendor claims.
pub fn verify_source(
    path: impl AsRef<Path>,
    manifest: &SourceManifest,
) -> Result<CoverageReport, ManifestError> {
    manifest.validate()?;
    let actual: BTreeMap<_, _> = capture_files(path.as_ref())?
        .into_iter()
        .map(|f| (f.path.clone(), f))
        .collect();
    let expected: BTreeMap<_, _> = manifest.files.iter().map(|f| (f.path.clone(), f)).collect();
    Ok(CoverageReport {
        missing: expected
            .keys()
            .filter(|p| !actual.contains_key(*p))
            .cloned()
            .collect(),
        uncovered: actual
            .keys()
            .filter(|p| !expected.contains_key(*p))
            .cloned()
            .collect(),
        changed: expected
            .iter()
            .filter(|(p, e)| {
                actual
                    .get(*p)
                    .is_some_and(|a| a.size != e.size || a.sha256 != e.sha256)
            })
            .map(|(p, _)| p.clone())
            .collect(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn directory_inventory_detects_changes_missing_and_extra_files() {
        let temp = tempfile::tempdir().unwrap();
        fs::create_dir(temp.path().join("nested")).unwrap();
        fs::write(temp.path().join("a.raw"), b"a").unwrap();
        fs::write(temp.path().join("nested/b.raw"), b"b").unwrap();
        let source = capture_source(temp.path(), "vendor.raw").unwrap();
        assert!(verify_source(temp.path(), &source).unwrap().complete());
        fs::write(temp.path().join("a.raw"), b"modified").unwrap();
        fs::remove_file(temp.path().join("nested/b.raw")).unwrap();
        fs::write(temp.path().join("extra.raw"), b"extra").unwrap();
        let report = verify_source(temp.path(), &source).unwrap();
        assert_eq!(report.changed, ["a.raw"]);
        assert_eq!(report.missing, ["nested/b.raw"]);
        assert_eq!(report.uncovered, ["extra.raw"]);
    }
    #[test]
    fn replaced_sources_do_not_match_even_with_equal_size_and_mtime() {
        let directory = tempfile::tempdir().unwrap();
        let original = directory.path().join("source.raw");
        let replacement = directory.path().join("replacement.raw");
        fs::write(&original, b"raw").unwrap();
        fs::write(&replacement, b"raw").unwrap();
        let source = fs::File::open(&original).unwrap();
        let metadata = source.metadata().unwrap();
        let identity = file_identity(&source).unwrap();
        let other = fs::File::options().write(true).open(&replacement).unwrap();
        other
            .set_times(fs::FileTimes::new().set_modified(metadata.modified().unwrap()))
            .unwrap();
        assert_ne!(identity, file_identity(&other).unwrap());
        assert!(!matches_snapshot(&replacement, &metadata, identity).unwrap());
        assert!(matches_snapshot(&original, &metadata, identity).unwrap());
    }

    #[cfg(unix)]
    fn rewrite_preserving_size_mtime_and_identity(path: &Path, before: &fs::Metadata) {
        use std::os::unix::fs::MetadataExt;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        loop {
            fs::write(path, b"new").unwrap();
            fs::File::options()
                .write(true)
                .open(path)
                .unwrap()
                .set_times(fs::FileTimes::new().set_modified(before.modified().unwrap()))
                .unwrap();
            let after = fs::symlink_metadata(path).unwrap();
            assert_eq!(after.len(), before.len());
            assert_eq!(after.modified().unwrap(), before.modified().unwrap());
            assert_eq!((after.dev(), after.ino()), (before.dev(), before.ino()));
            if (after.ctime(), after.ctime_nsec()) != (before.ctime(), before.ctime_nsec()) {
                break;
            }
            // Accommodate coarse filesystem timestamps without a racing writer.
            assert!(std::time::Instant::now() < deadline);
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
    }

    #[cfg(unix)]
    #[test]
    fn source_rewrite_during_hash_is_rejected_with_restored_mtime() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("source.raw");
        fs::write(&path, b"raw").unwrap();
        let opened = fs::File::open(&path).unwrap();
        let before = opened.metadata().unwrap();
        let identity = file_identity(&opened).unwrap();
        let mut rewritten = false;
        let result = capture_files_with_read_observer(&path, &mut |_| {
            if !rewritten {
                rewrite_preserving_size_mtime_and_identity(&path, &before);
                rewritten = true;
            }
        });
        assert!(rewritten);
        assert!(
            result.is_err(),
            "accepted a source rewritten during its hash"
        );
        assert!(!same_file(&before, &opened.metadata().unwrap()));
        assert!(!matches_snapshot(&path, &before, identity).unwrap());
    }

    #[cfg(unix)]
    #[test]
    fn earlier_source_rewrite_during_later_hash_is_rejected_with_restored_mtime() {
        let directory = tempfile::tempdir().unwrap();
        let earlier = directory.path().join("a.raw");
        let later = directory.path().join("b.raw");
        fs::write(&earlier, b"raw").unwrap();
        fs::write(&later, b"raw").unwrap();
        let expected = capture_source(directory.path(), "vendor.raw").unwrap();
        let opened = fs::File::open(&earlier).unwrap();
        let before = opened.metadata().unwrap();
        let identity = file_identity(&opened).unwrap();
        let mut rewritten = false;
        let result = capture_files_with_read_observer(directory.path(), &mut |path| {
            if path == later && !rewritten {
                rewrite_preserving_size_mtime_and_identity(&earlier, &before);
                rewritten = true;
            }
        });
        assert!(rewritten);
        assert!(result.is_err(), "accepted an earlier source's stale digest");
        assert!(!same_file(&before, &opened.metadata().unwrap()));
        assert!(!matches_snapshot(&earlier, &before, identity).unwrap());
        let actual = capture_source(directory.path(), "vendor.raw").unwrap();
        assert_ne!(actual.files[0].sha256, expected.files[0].sha256);
        let report = verify_source(directory.path(), &expected).unwrap();
        assert_eq!(report.changed, ["a.raw"]);
    }

    #[test]
    fn canonical_inventory_order_and_strict_schema() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(temp.path().join("a.raw"), b"a").unwrap();
        fs::write(temp.path().join("b.raw"), b"b").unwrap();
        let mut source = capture_source(temp.path(), "vendor.raw").unwrap();
        let bytes = source.canonical_bytes().unwrap();
        source.files.reverse();
        assert_eq!(bytes, source.canonical_bytes().unwrap());
        source.files.push(source.files[0].clone());
        assert!(source.validate().is_err());
        assert!(
            SourceManifest::from_bytes(br#"{"version":1,"version":1,"format":"raw","files":[]}"#)
                .is_err()
        );
        assert!(capture_source(tempfile::tempdir().unwrap().path(), "raw").is_err());
    }
    #[cfg(unix)]
    #[test]
    fn source_symlinks_are_rejected() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(temp.path().join("a.raw"), b"a").unwrap();
        std::os::unix::fs::symlink("a.raw", temp.path().join("b.raw")).unwrap();
        assert!(capture_source(temp.path(), "raw").is_err());
    }
}

#[cfg(all(test, feature = "verification"))]
mod proof_tests {
    use super::*;
    use crate::provenance::{
        evidence::{EvidenceBundle, TsaPolicy},
        historical::{HistoricalPolicy, verify_historical_history},
        signing::{OpenSslBackend, SignaturePolicy},
        stages::{Stage, StageKind},
        trust::{CheckStatus, SignerRole, TrustPolicy},
        verification::{
            ConversionPolicy, HistoryPolicy, SignedRelease, SignedStage, verify_history,
        },
    };
    use serde::de::DeserializeOwned;

    fn read(name: &str) -> Vec<u8> {
        fs::read(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("test/data/provenance/conversion")
                .join(name),
        )
        .unwrap()
    }
    fn json<T: DeserializeOwned>(name: &str) -> T {
        serde_json::from_slice(&read(name)).unwrap()
    }
    struct Fixture {
        records: Vec<SignedStage>,
        releases: BTreeMap<String, SignedRelease>,
        files: BTreeMap<String, Vec<u8>>,
        trust: TrustPolicy,
        conversion: ConversionPolicy,
        identities: BTreeMap<String, String>,
        publishers: BTreeMap<String, String>,
        heads: Vec<String>,
        raw: tempfile::TempDir,
        tsa: TsaPolicy,
        stage_evidence: BTreeMap<String, EvidenceBundle>,
        release_evidence: BTreeMap<String, EvidenceBundle>,
        metadata: serde_json::Value,
    }
    impl Fixture {
        fn new() -> Self {
            let raw = tempfile::tempdir().unwrap();
            for (name, bytes) in json::<BTreeMap<String, String>>("raw-files.json") {
                let path = raw.path().join(name);
                fs::create_dir_all(path.parent().unwrap()).unwrap();
                fs::write(path, hex::decode(bytes).unwrap()).unwrap();
            }
            let mut conversion: ConversionPolicy = json("conversion-policy.json");
            conversion
                .sources
                .insert("conversion".into(), raw.path().to_owned());
            Self {
                records: json("records.json"),
                releases: json("releases.json"),
                files: json::<BTreeMap<String, String>>("files.json")
                    .into_iter()
                    .map(|(k, v)| (k, hex::decode(v).unwrap()))
                    .collect(),
                trust: json("trust.json"),
                conversion,
                identities: json("identities.json"),
                publishers: json("publishers.json"),
                heads: json("heads.json"),
                raw,
                tsa: json("tsa-policy.json"),
                stage_evidence: json("stage-evidence.json"),
                release_evidence: json("release-evidence.json"),
                metadata: json("metadata.json"),
            }
        }
        fn policy(&self, allow: bool) -> HistoryPolicy<'_> {
            HistoryPolicy {
                trust: &self.trust,
                required_signatures: SignaturePolicy::Ecdsa,
                identities: &self.identities,
                publishers: &self.publishers,
                expected_heads: Some(&self.heads),
                conversion: allow.then_some(&self.conversion),
            }
        }
        fn current(&self, allow: bool) -> crate::provenance::verification::HistoryReport {
            verify_history(
                &self.records,
                &self.files,
                &self.policy(allow),
                &self.releases,
                &OpenSslBackend::default(),
            )
            .unwrap()
        }
        fn historical(&self) -> crate::provenance::historical::HistoricalHistoryReport {
            let policy_bytes = read("policy.json");
            let evidence = HistoricalPolicy {
                tsa: &self.tsa,
                policy_id: self.metadata["policy_id"].as_str().unwrap(),
                policy_bytes: &policy_bytes,
                stage_evidence: &self.stage_evidence,
                release_evidence: &self.release_evidence,
            };
            verify_historical_history(
                &self.records,
                &self.files,
                &self.policy(true),
                &self.releases,
                &OpenSslBackend::default(),
                &evidence,
            )
            .unwrap()
        }
    }
    #[test]
    fn shared_conversion_bytes_and_reordered_external_inventory() {
        let vectors: serde_json::Value = json("canonical-vectors.json");
        let source: SourceManifest = json("source.json");
        assert_eq!(
            source.digest().unwrap(),
            vectors["source_sha256"].as_str().unwrap()
        );
        assert_eq!(
            hex::encode(source.canonical_bytes().unwrap()),
            vectors["source_hex"].as_str().unwrap()
        );
        let mut stage: Stage = json("stage.json");
        assert_eq!(
            stage.digest().unwrap(),
            vectors["conversion_sha256"].as_str().unwrap()
        );
        assert_eq!(
            hex::encode(stage.canonical_bytes().unwrap()),
            vectors["conversion_hex"].as_str().unwrap()
        );
        stage.conversion.as_mut().unwrap().source.files.reverse();
        assert_eq!(
            stage.digest().unwrap(),
            vectors["conversion_sha256"].as_str().unwrap()
        );
        let records: Vec<SignedStage> = json("records.json");
        assert_eq!(
            hex::encode(records[1].stage.canonical_bytes().unwrap()),
            vectors["processing_hex"].as_str().unwrap()
        );
    }
    #[test]
    fn conversion_root_and_v1_processing_have_separate_source_assurance() {
        let mut fixture = Fixture::new();
        let report = fixture.current(true);
        assert!(report.accepted(), "{report:?}");
        assert!(report.lineage.acquisitions.is_empty());
        assert_eq!(report.lineage.conversions.len(), 1);
        assert_eq!(report.stages[0].kind, StageKind::Conversion);
        assert_eq!(report.stages[1].kind, StageKind::Processing);
        assert_eq!(
            report.stages[0]
                .conversion
                .as_ref()
                .unwrap()
                .source_integrity,
            CheckStatus::Valid
        );
        assert!(!fixture.current(false).accepted());
        fixture.conversion.sources.clear();
        let absent = fixture.current(true);
        assert!(absent.accepted());
        assert_eq!(
            absent.stages[0]
                .conversion
                .as_ref()
                .unwrap()
                .source_integrity,
            CheckStatus::Unknown
        );
        fixture.conversion.require_source_bytes = true;
        assert!(!fixture.current(true).accepted());
        fixture
            .conversion
            .sources
            .insert("conversion".into(), fixture.raw.path().to_owned());
        fixture.conversion.require_source_bytes = false;
        fs::write(fixture.raw.path().join("header.bin"), b"altered RAW").unwrap();
        let mismatch = fixture.current(true);
        assert!(!mismatch.accepted());
        assert_eq!(
            mismatch.stages[0]
                .conversion
                .as_ref()
                .unwrap()
                .source_integrity,
            CheckStatus::Invalid
        );
    }
    #[test]
    fn supplied_source_mappings_cannot_be_silently_ignored() {
        for key in ["misspelled", "processing"] {
            let mut fixture = Fixture::new();
            fixture.conversion.sources.clear();
            fixture
                .conversion
                .sources
                .insert(key.into(), fixture.raw.path().join("missing"));
            let report = verify_history(
                &fixture.records,
                &fixture.files,
                &fixture.policy(true),
                &fixture.releases,
                &OpenSslBackend::default(),
            );
            assert!(
                report.is_err(),
                "unused supplied source key was silently ignored: {key}"
            );
        }
    }

    #[test]
    fn signed_vendor_claim_requires_correct_role_method_and_intact_report() {
        let mut fixture = Fixture::new();
        fixture
            .trust
            .authorizations
            .iter_mut()
            .find(|g| g.role == SignerRole::VendorConversion)
            .unwrap()
            .role = SignerRole::Processing;
        let wrong = fixture.current(true);
        assert!(!wrong.accepted());
        assert_eq!(
            wrong.stages[0].certificates[0].authorization,
            CheckStatus::Invalid
        );
        let mut fixture = Fixture::new();
        fixture.conversion.allowed_verifications[0].method = "different".into();
        assert!(!fixture.current(true).accepted());
        let mut fixture = Fixture::new();
        fixture.files.insert(
            "validation/vendor-report.json".into(),
            b"tampered report".to_vec(),
        );
        let report = fixture.current(true);
        assert!(!report.accepted());
        assert_eq!(report.coverage.changed, ["validation/vendor-report.json"]);
        let mut fixture = Fixture::new();
        fixture.records[0]
            .stage
            .conversion
            .as_mut()
            .unwrap()
            .claimed_acquisition_time = Some(0);
        // Isolate the changed root so lineage rejection cannot mask the
        // cryptographic failure of its otherwise well-formed statement.
        fixture.records.truncate(1);
        fixture.heads = vec![fixture.records[0].stage.digest().unwrap()];
        let changed = fixture.current(true);
        assert_eq!(changed.stages[0].signature, CheckStatus::Invalid);
        assert!(!changed.accepted());
    }
    #[test]
    fn historical_vendor_conversion_uses_attestation_time_and_preserves_external_grants() {
        let mut fixture = Fixture::new();
        fixture.trust.validation_time = fixture.metadata["historical_validation_time"]
            .as_i64()
            .unwrap();
        assert!(
            !fixture.current(true).accepted(),
            "claimed acquisition date must not rescue expired current certificates"
        );
        let historical = fixture.historical();
        assert!(historical.accepted(), "{historical:?}");
        let claimed = fixture.metadata["claimed_acquisition_time"]
            .as_i64()
            .unwrap();
        for (stage, record) in historical.history.stages.iter().zip(&fixture.records) {
            let time = historical.stage_timestamps[&record.stage.manifest.stage_id]
                .trusted_time
                .unwrap();
            assert!(time > claimed && time < fixture.trust.validation_time);
            assert!(
                stage
                    .certificates
                    .iter()
                    .all(|certificate| certificate.validation_time == time)
            );
        }
        fixture
            .trust
            .authorizations
            .iter_mut()
            .find(|g| g.role == SignerRole::VendorConversion)
            .unwrap()
            .role = SignerRole::Processing;
        assert!(!fixture.historical().accepted());
        let mut fixture = Fixture::new();
        fixture.trust.validation_time = fixture.metadata["historical_validation_time"]
            .as_i64()
            .unwrap();
        fixture.release_evidence.clear();
        assert!(!fixture.historical().accepted());
        fixture.stage_evidence.insert(
            "conversion".into(),
            fixture.stage_evidence["processing"].clone(),
        );
        assert!(!fixture.historical().accepted());
    }
}
