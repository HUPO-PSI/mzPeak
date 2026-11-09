//! Compose content coverage, stage signatures, certificate policy and publisher approval.
//! Acceptance covers supplied history; external expected heads protect against rollback.
use super::{
    manifest::{CoverageReport, Manifest, ManifestError, canonical_json, validate_digest},
    signing::{OpenSslBackend, SignatureEnvelope, SignaturePolicy},
    stages::{LineageReport, Stage, StageKind, verify_lineage},
    trust::{
        CertificateReport, CheckStatus, SignerRole, TrustError, TrustPolicy,
        verify_certificate_with,
    },
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VendorVerificationGrant {
    pub vendor: String,
    pub format: String,
    pub method: String,
    pub verifier_name: String,
    pub verifier_version: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConversionPolicy {
    pub allowed_verifications: Vec<VendorVerificationGrant>,
    pub require_source_bytes: bool,
    #[serde(deserialize_with = "unique_sources")]
    pub sources: BTreeMap<String, PathBuf>,
}
fn unique_sources<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<BTreeMap<String, PathBuf>, D::Error> {
    struct Unique;
    impl<'de> serde::de::Visitor<'de> for Unique {
        type Value = BTreeMap<String, PathBuf>;
        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            f.write_str("unique source stage IDs")
        }
        fn visit_map<A: serde::de::MapAccess<'de>>(
            self,
            mut map: A,
        ) -> Result<Self::Value, A::Error> {
            let mut sources = BTreeMap::new();
            while let Some((key, value)) = map.next_entry::<String, PathBuf>()? {
                if sources.insert(key, value).is_some() {
                    return Err(serde::de::Error::custom("duplicate source stage ID"));
                }
            }
            Ok(sources)
        }
    }
    deserializer.deserialize_map(Unique)
}
impl ConversionPolicy {
    pub fn validate(&self) -> Result<(), ManifestError> {
        let mut unique = std::collections::BTreeSet::new();
        for grant in &self.allowed_verifications {
            for (value, label) in [
                (&grant.vendor, "vendor"),
                (&grant.format, "source format"),
                (&grant.method, "verification method"),
                (&grant.verifier_name, "verifier name"),
                (&grant.verifier_version, "verifier version"),
            ] {
                super::conversion::text(value, label)?;
            }
            if !unique.insert((
                &grant.vendor,
                &grant.format,
                &grant.method,
                &grant.verifier_name,
                &grant.verifier_version,
            )) {
                return Err(ManifestError::Invalid(
                    "duplicate vendor verification grant".into(),
                ));
            }
        }
        for (stage, path) in &self.sources {
            super::manifest::validate_identifier(stage, "source stage ID")?;
            if path
                .to_str()
                .is_none_or(|value| value.is_empty() || value.contains('\0'))
            {
                return Err(ManifestError::Invalid("empty local source path".into()));
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConversionReport {
    pub vendor_attestation: CheckStatus,
    pub source_integrity: CheckStatus,
    pub source_required: bool,
    pub source_manifest_sha256: String,
    pub claimed_acquisition_time: Option<u64>,
    pub details: Vec<String>,
}
impl ConversionReport {
    pub fn accepted(&self) -> bool {
        self.vendor_attestation == CheckStatus::Valid
            && (self.source_integrity == CheckStatus::Valid
                || self.source_integrity == CheckStatus::Unknown && !self.source_required)
    }
}

fn conversion_report(
    stage: &Stage,
    identity: Option<&str>,
    policy: Option<&ConversionPolicy>,
    check_source: bool,
) -> Result<ConversionReport, ManifestError> {
    let claim = stage
        .conversion
        .as_ref()
        .ok_or_else(|| ManifestError::Invalid("conversion claim missing".into()))?;
    let mut report = ConversionReport {
        vendor_attestation: CheckStatus::Invalid,
        source_integrity: CheckStatus::Unknown,
        source_required: policy.is_some_and(|p| p.require_source_bytes),
        source_manifest_sha256: claim.source.digest()?,
        claimed_acquisition_time: claim.claimed_acquisition_time,
        details: vec![],
    };
    let Some(policy) = policy else {
        report
            .details
            .push("vendor conversion is not enabled by recipient policy".into());
        return Ok(report);
    };
    let verification = &claim.verification;
    let allowed = policy.allowed_verifications.iter().any(|grant| {
        grant.vendor == verification.vendor
            && grant.format == claim.source.format
            && grant.method == verification.method
            && grant.verifier_name == verification.verifier_name
            && grant.verifier_version == verification.verifier_version
    });
    if identity == Some(verification.vendor.as_str())
        && allowed
        && verification.result == super::conversion::VerificationResult::Passed
        && check_source
    {
        report.vendor_attestation = CheckStatus::Valid;
    } else {
        report.details.push("vendor identity, verification method, result or cryptographic stage signature did not qualify".into());
    }
    if !check_source {
        return Ok(report);
    }
    match policy.sources.get(&stage.manifest.stage_id) {
        None => report
            .details
            .push("original RAW source bytes were not supplied for independent checking".into()),
        Some(path) => match super::conversion::verify_source(path, &claim.source) {
            Ok(coverage) if coverage.complete() => report.source_integrity = CheckStatus::Valid,
            Ok(_) => {
                report.source_integrity = CheckStatus::Invalid;
                report
                    .details
                    .push("source inventory differs from supplied RAW bytes".into());
            }
            Err(error) => {
                report.source_integrity = CheckStatus::Invalid;
                report.details.push(format!(
                    "supplied RAW source could not be verified: {error}"
                ));
            }
        },
    }
    Ok(report)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReleaseStatement {
    pub name: String,
    pub version: String,
    pub release_sha256: String,
}
impl ReleaseStatement {
    pub fn canonical_bytes(&self) -> Result<Vec<u8>, ManifestError> {
        validate_digest(&self.release_sha256)?;
        for text in [&self.name, &self.version] {
            if text.is_empty() || text.len() > 128 || !text.bytes().all(|b| (32..=126).contains(&b))
            {
                return Err(ManifestError::Invalid(
                    "invalid release name/version".into(),
                ));
            }
        }
        canonical_json(
            &serde_json::json!({"type":"mzpeak-software-release","version":1,
            "name":self.name,"software_version":self.version,"release_sha256":self.release_sha256}),
        )
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SignedRelease {
    pub statement: ReleaseStatement,
    pub envelope: SignatureEnvelope,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SignedStage {
    pub stage: Stage,
    pub envelope: SignatureEnvelope,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StageReport {
    pub stage_id: String,
    pub kind: StageKind,
    pub conversion: Option<ConversionReport>,
    pub signature: CheckStatus,
    pub certificates: Vec<CertificateReport>,
    pub software_release: Option<CheckStatus>,
    pub release_certificates: Vec<CertificateReport>,
    pub details: Vec<String>,
}
impl StageReport {
    pub fn accepted(&self) -> bool {
        self.signature == CheckStatus::Valid
            && !self.certificates.is_empty()
            && self.certificates.iter().all(|c| c.accepted())
            && self
                .software_release
                .is_none_or(|s| s == CheckStatus::Valid)
            && self
                .conversion
                .as_ref()
                .is_none_or(ConversionReport::accepted)
            && (self.kind != StageKind::Conversion || self.conversion.is_some())
    }
}
#[derive(Debug, Clone, Serialize)]
pub struct HistoryReport {
    pub coverage: CoverageReport,
    pub lineage: LineageReport,
    pub stages: Vec<StageReport>,
}
impl HistoryReport {
    pub fn accepted(&self) -> bool {
        self.coverage.complete()
            && !self.stages.is_empty()
            && self.stages.iter().all(|s| s.accepted())
    }
}
/// All values here are relying-party configuration, never implicitly trusted archive metadata.
pub struct HistoryPolicy<'a> {
    pub trust: &'a TrustPolicy,
    pub required_signatures: SignaturePolicy,
    pub identities: &'a BTreeMap<String, String>,
    pub publishers: &'a BTreeMap<String, String>,
    pub expected_heads: Option<&'a [String]>,
    pub conversion: Option<&'a ConversionPolicy>,
}
#[derive(Debug, thiserror::Error)]
pub enum HistoryError {
    #[error(transparent)]
    Manifest(#[from] ManifestError),
    #[error(transparent)]
    Trust(#[from] TrustError),
}
fn union_manifest(records: &[SignedStage]) -> Result<Manifest, ManifestError> {
    let first = records
        .first()
        .ok_or_else(|| ManifestError::Invalid("empty history".into()))?;
    let mut files = BTreeMap::new();
    for record in records {
        for file in &record.stage.manifest.files {
            // Match the chunk order used by manifest/stage canonicalization.
            let mut normalized = file.clone();
            normalized
                .chunks
                .sort_by_key(|chunk| (chunk.offset, chunk.length));
            if files
                .get(&file.path)
                .is_some_and(|previous| previous != &normalized)
            {
                return Err(ManifestError::Invalid(
                    "inconsistent repeated file descriptor".into(),
                ));
            }
            files.insert(file.path.clone(), normalized);
        }
    }
    Ok(Manifest {
        version: 1,
        run_id: first.stage.manifest.run_id.clone(),
        stage_id: "verified-history".into(),
        files: files.into_values().collect(),
    })
}
fn check_stages(
    records: &[SignedStage],
    policy: &HistoryPolicy<'_>,
    releases: &BTreeMap<String, SignedRelease>,
    backend: &OpenSslBackend,
) -> Result<Vec<StageReport>, HistoryError> {
    let mut reports = Vec::new();
    for record in records {
        let stage = &record.stage;
        let mut report = StageReport {
            stage_id: stage.manifest.stage_id.clone(),
            kind: stage.kind.clone(),
            conversion: if stage.kind == StageKind::Conversion {
                Some(conversion_report(stage, None, policy.conversion, false)?)
            } else {
                None
            },
            signature: CheckStatus::Invalid,
            certificates: vec![],
            software_release: None,
            release_certificates: vec![],
            details: vec![],
        };
        let Some(identity) = policy
            .identities
            .get(&stage.manifest.stage_id)
            .filter(|v| !v.is_empty())
        else {
            report
                .details
                .push("no externally configured stage identity".into());
            reports.push(report);
            continue;
        };
        let signers = match backend.verify(
            &stage.canonical_bytes()?,
            &record.envelope,
            policy.required_signatures,
        ) {
            Ok(signers) => signers,
            Err(e) => {
                report.details.push(e.to_string());
                reports.push(report);
                continue;
            }
        };
        report.signature = CheckStatus::Valid;
        if stage.kind == StageKind::Conversion {
            report.conversion = Some(conversion_report(
                stage,
                Some(identity),
                policy.conversion,
                true,
            )?);
        }
        let role = SignerRole::for_stage_kind(&stage.kind);
        for signer in signers {
            report.certificates.push(verify_certificate_with(
                &backend.executable,
                &signer.certificate_pem,
                &signer.intermediates_pem,
                policy.trust,
                role,
                identity,
            )?);
        }
        if let Some(software) = &stage.software {
            report.software_release = Some(CheckStatus::Invalid);
            match (
                releases.get(&software.release_sha256),
                policy.publishers.get(&software.release_sha256),
            ) {
                (Some(release), Some(publisher))
                    if release.statement.name == software.name
                        && release.statement.version == software.version
                        && release.statement.release_sha256 == software.release_sha256 =>
                {
                    match backend.verify(
                        &release.statement.canonical_bytes()?,
                        &release.envelope,
                        policy.required_signatures,
                    ) {
                        Ok(signers) => {
                            for signer in signers {
                                report.release_certificates.push(verify_certificate_with(
                                    &backend.executable,
                                    &signer.certificate_pem,
                                    &signer.intermediates_pem,
                                    policy.trust,
                                    SignerRole::SoftwareRelease,
                                    publisher,
                                )?);
                            }
                            if !report.release_certificates.is_empty()
                                && report.release_certificates.iter().all(|r| r.accepted())
                            {
                                report.software_release = Some(CheckStatus::Valid);
                            }
                        }
                        Err(e) => report.details.push(e.to_string()),
                    }
                }
                _ => report.details.push(
                    "matching publisher approval for this software release is missing".into(),
                ),
            }
        }
        reports.push(report);
    }
    Ok(reports)
}

fn validate_conversion_sources(
    records: &[SignedStage],
    policy: &HistoryPolicy<'_>,
) -> Result<(), ManifestError> {
    if let Some(conversion) = policy.conversion {
        conversion.validate()?;
        for key in conversion.sources.keys() {
            if !records.iter().any(|record| {
                record.stage.manifest.stage_id == *key && record.stage.kind == StageKind::Conversion
            }) {
                return Err(ManifestError::Invalid(
                    "source mapping must reference a conversion stage in this history".into(),
                ));
            }
        }
    }
    Ok(())
}

pub fn verify_history(
    records: &[SignedStage],
    files: &BTreeMap<String, Vec<u8>>,
    policy: &HistoryPolicy<'_>,
    releases: &BTreeMap<String, SignedRelease>,
    backend: &OpenSslBackend,
) -> Result<HistoryReport, HistoryError> {
    validate_conversion_sources(records, policy)?;
    let stages: Vec<_> = records.iter().map(|r| r.stage.clone()).collect();
    let lineage = verify_lineage(&stages, policy.expected_heads)?;
    let coverage = union_manifest(records)?.verify_files(files)?;
    let reports = check_stages(records, policy, releases, backend)?;
    Ok(HistoryReport {
        coverage,
        lineage,
        stages: reports,
    })
}

pub fn verify_archive_history(
    archive: impl AsRef<std::path::Path>,
    records: &[SignedStage],
    policy: &HistoryPolicy<'_>,
    releases: &BTreeMap<String, SignedRelease>,
    backend: &OpenSslBackend,
) -> Result<HistoryReport, HistoryError> {
    validate_conversion_sources(records, policy)?;
    let stages: Vec<_> = records.iter().map(|r| r.stage.clone()).collect();
    let lineage = verify_lineage(&stages, policy.expected_heads)?;
    let coverage = super::archive::verify_archive(archive, &union_manifest(records)?)?;
    let reports = check_stages(records, policy, releases, backend)?;
    Ok(HistoryReport {
        coverage,
        lineage,
        stages: reports,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    fn read(name: &str) -> Vec<u8> {
        std::fs::read(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("test/data/provenance/history")
                .join(name),
        )
        .unwrap()
    }
    fn conversion_policy() -> ConversionPolicy {
        ConversionPolicy {
            allowed_verifications: vec![VendorVerificationGrant {
                vendor: "vendor-1".into(),
                format: "vendor.raw".into(),
                method: "vendor-check-v1".into(),
                verifier_name: "Verifier".into(),
                verifier_version: "1.0".into(),
            }],
            require_source_bytes: false,
            sources: BTreeMap::new(),
        }
    }

    #[test]
    fn conversion_policy_separates_vendor_claim_and_original_source_bytes() {
        let mut stage = super::super::stages::tests::conversion_stage();
        let mut policy = conversion_policy();
        let denied = conversion_report(&stage, Some("vendor-1"), None, true).unwrap();
        assert!(!denied.accepted());
        let absent = conversion_report(&stage, Some("vendor-1"), Some(&policy), true).unwrap();
        assert!(absent.accepted());
        assert_eq!(absent.source_integrity, CheckStatus::Unknown);
        policy.require_source_bytes = true;
        assert!(
            !conversion_report(&stage, Some("vendor-1"), Some(&policy), true)
                .unwrap()
                .accepted()
        );
        let source = tempfile::tempdir().unwrap();
        std::fs::write(source.path().join("sample.raw"), b"raw").unwrap();
        std::fs::write(source.path().join("b.raw"), b"b").unwrap();
        policy
            .sources
            .insert(stage.manifest.stage_id.clone(), source.path().to_owned());
        let present = conversion_report(&stage, Some("vendor-1"), Some(&policy), true).unwrap();
        assert!(present.accepted());
        assert_eq!(present.source_integrity, CheckStatus::Valid);
        assert_eq!(present.claimed_acquisition_time, Some(1));
        assert!(
            !conversion_report(&stage, Some("different-vendor"), Some(&policy), true)
                .unwrap()
                .accepted()
        );
        stage.conversion.as_mut().unwrap().verification.result =
            super::super::conversion::VerificationResult::Failed;
        assert!(
            !conversion_report(&stage, Some("vendor-1"), Some(&policy), true)
                .unwrap()
                .accepted()
        );
        stage.conversion.as_mut().unwrap().verification.result =
            super::super::conversion::VerificationResult::Passed;
        policy.allowed_verifications[0].verifier_version = "unsupported".into();
        assert!(
            !conversion_report(&stage, Some("vendor-1"), Some(&policy), true)
                .unwrap()
                .accepted()
        );
        policy.allowed_verifications[0].verifier_version = "1.0".into();
        policy.require_source_bytes = false;
        std::fs::write(source.path().join("sample.raw"), b"tampered").unwrap();
        let changed = conversion_report(&stage, Some("vendor-1"), Some(&policy), true).unwrap();
        assert_eq!(changed.source_integrity, CheckStatus::Invalid);
        assert!(!changed.accepted());
        policy.sources.insert(
            stage.manifest.stage_id.clone(),
            source.path().join("not-present"),
        );
        assert_eq!(
            conversion_report(&stage, Some("vendor-1"), Some(&policy), true)
                .unwrap()
                .source_integrity,
            CheckStatus::Invalid
        );
        assert!(serde_json::from_str::<ConversionPolicy>(r#"{"allowed_verifications":[],"require_source_bytes":false,"sources":{"a":"x","a":"y"}}"#).is_err());
    }

    #[test]
    fn acquisition_grant_does_not_authorize_vendor_conversion() {
        let records: Vec<SignedStage> = serde_json::from_slice(&read("records.json")).unwrap();
        let mut trust: TrustPolicy = serde_json::from_slice(&read("trust.json")).unwrap();
        let signer = &records[0].envelope.signatures[0];
        let check = |trust: &TrustPolicy| {
            super::super::trust::verify_certificate(
                &signer.certificate_pem,
                &signer.intermediates_pem,
                trust,
                SignerRole::VendorConversion,
                "vendor-1",
            )
            .unwrap()
        };
        assert!(!check(&trust).accepted());
        let fingerprint =
            super::super::trust::certificate_fingerprint(&signer.certificate_pem).unwrap();
        let grant = trust
            .authorizations
            .iter_mut()
            .find(|g| g.certificate_sha256 == fingerprint)
            .unwrap();
        grant.role = SignerRole::VendorConversion;
        grant.identity = "vendor-1".into();
        assert!(check(&trust).accepted());
        assert_eq!(
            SignerRole::for_stage_kind(&StageKind::Conversion),
            SignerRole::VendorConversion
        );
    }

    #[test]
    fn shared_full_history_and_security_failures() {
        let mut records: Vec<SignedStage> = serde_json::from_slice(&read("records.json")).unwrap();
        let releases: BTreeMap<String, SignedRelease> =
            serde_json::from_slice(&read("releases.json")).unwrap();
        let mut trust: TrustPolicy = serde_json::from_slice(&read("trust.json")).unwrap();
        let encoded: BTreeMap<String, String> =
            serde_json::from_slice(&read("files.json")).unwrap();
        let mut files: BTreeMap<_, _> = encoded
            .into_iter()
            .map(|(k, v)| (k, hex::decode(v).unwrap()))
            .collect();
        let heads: Vec<String> = serde_json::from_slice(&read("heads.json")).unwrap();
        let identities = BTreeMap::from([
            ("acquisition".into(), "instrument-1".into()),
            ("processing".into(), "installation-1".into()),
        ]);
        let publishers = releases
            .keys()
            .map(|k| (k.clone(), "publisher-1".into()))
            .collect();
        let p = HistoryPolicy {
            trust: &trust,
            required_signatures: SignaturePolicy::Ecdsa,
            identities: &identities,
            publishers: &publishers,
            expected_heads: Some(&heads),
            conversion: None,
        };
        let backend = OpenSslBackend::default();
        let report = verify_history(&records, &files, &p, &releases, &backend).unwrap();
        assert!(report.accepted(), "{report:?}");
        assert!(report.lineage.checkpoint_checked);
        assert!(verify_history(&records[..1], &files, &p, &releases, &backend).is_err());
        assert!(
            !verify_history(&records, &files, &p, &BTreeMap::new(), &backend)
                .unwrap()
                .accepted()
        );
        files.get_mut("raw.bin").unwrap().push(0);
        assert!(
            !verify_history(&records, &files, &p, &releases, &backend)
                .unwrap()
                .accepted()
        );
        files.get_mut("raw.bin").unwrap().pop();
        records[0].envelope = records[1].envelope.clone();
        assert!(
            !verify_history(&records, &files, &p, &releases, &backend)
                .unwrap()
                .accepted()
        );
        records = serde_json::from_slice(&read("records.json")).unwrap();
        trust.crls_pem.clear();
        let p = HistoryPolicy {
            trust: &trust,
            required_signatures: SignaturePolicy::Ecdsa,
            identities: &identities,
            publishers: &publishers,
            expected_heads: Some(&heads),
            conversion: None,
        };
        assert!(
            !verify_history(&records, &files, &p, &releases, &backend)
                .unwrap()
                .accepted()
        );
    }

    #[test]
    fn repeated_chunk_order_preserves_signed_history_and_checkpoint() {
        let mut records: Vec<SignedStage> =
            serde_json::from_slice(&read("chunk-order/records.json")).unwrap();
        let releases: BTreeMap<String, SignedRelease> =
            serde_json::from_slice(&read("chunk-order/releases.json")).unwrap();
        let trust: TrustPolicy = serde_json::from_slice(&read("chunk-order/trust.json")).unwrap();
        let encoded: BTreeMap<String, String> =
            serde_json::from_slice(&read("chunk-order/files.json")).unwrap();
        let files: BTreeMap<_, _> = encoded
            .into_iter()
            .map(|(name, value)| (name, hex::decode(value).unwrap()))
            .collect();
        let heads: Vec<String> = serde_json::from_slice(&read("chunk-order/heads.json")).unwrap();
        let identities = BTreeMap::from([
            ("acquisition".into(), "instrument-1".into()),
            ("processing".into(), "installation-1".into()),
        ]);
        let publishers = releases
            .keys()
            .map(|key| (key.clone(), "publisher-1".into()))
            .collect();
        let mut policy = HistoryPolicy {
            trust: &trust,
            required_signatures: SignaturePolicy::Ecdsa,
            identities: &identities,
            publishers: &publishers,
            expected_heads: Some(&heads),
            conversion: None,
        };
        let backend = OpenSslBackend::default();
        let original = verify_history(&records, &files, &policy, &releases, &backend).unwrap();
        assert!(original.accepted(), "{original:?}");

        let signed_bytes = records[1].stage.canonical_bytes().unwrap();
        let repeated = records[1]
            .stage
            .manifest
            .files
            .iter_mut()
            .find(|file| file.path == "raw.bin")
            .unwrap();
        assert_eq!(repeated.chunks.len(), 2);
        repeated.chunks.reverse();
        assert_eq!(records[1].stage.canonical_bytes().unwrap(), signed_bytes);
        assert_eq!(records[1].stage.digest().unwrap(), heads[0]);
        let reordered = verify_history(&records, &files, &policy, &releases, &backend).unwrap();
        assert!(reordered.accepted(), "{reordered:?}");
        assert!(reordered.lineage.checkpoint_checked);

        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("reordered.mzpeak");
        let mut writer = zip::ZipWriter::new(std::fs::File::create(&path).unwrap());
        for (name, data) in &files {
            writer
                .start_file(name, zip::write::SimpleFileOptions::default())
                .unwrap();
            writer.write_all(data).unwrap();
        }
        writer.finish().unwrap();
        let archived =
            verify_archive_history(&path, &records, &policy, &releases, &backend).unwrap();
        assert!(archived.accepted(), "{archived:?}");
        assert!(archived.lineage.checkpoint_checked);

        // Even without an external checkpoint, the union must reject changed
        // chunk hashes or coordinates rather than select one signed claim.
        policy.expected_heads = None;
        for change_coordinates in [false, true] {
            let mut conflicting = records.clone();
            let chunk = &mut conflicting[1]
                .stage
                .manifest
                .files
                .iter_mut()
                .find(|file| file.path == "raw.bin")
                .unwrap()
                .chunks[0];
            if change_coordinates {
                chunk.column_path = Some("different.column".into());
            } else {
                chunk.sha256 = "0".repeat(64);
            }
            assert_ne!(conflicting[1].stage.digest().unwrap(), heads[0]);
            let error =
                verify_history(&conflicting, &files, &policy, &releases, &backend).unwrap_err();
            assert!(
                error
                    .to_string()
                    .contains("inconsistent repeated file descriptor")
            );
        }
    }
}
