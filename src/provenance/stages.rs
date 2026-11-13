//! Immutable acquisition/processing lineage. Graph integrity is independent of
//! signature trust, role authorization and approval of the declared software.
use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::manifest::{
    Manifest, ManifestError, canonical_json, invalid, required_option, validate_digest,
    validate_path,
};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StageKind {
    Acquisition,
    Processing,
    Conversion,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SoftwareIdentity {
    pub name: String,
    pub version: String,
    pub release_sha256: String,
    pub parameters_sha256: String,
}

impl SoftwareIdentity {
    pub fn validate(&self) -> Result<(), ManifestError> {
        for value in [&self.name, &self.version] {
            if value.is_empty()
                || value.len() > 128
                || !value.bytes().all(|b| (32..=126).contains(&b))
            {
                return Err(invalid("invalid software name or version"));
            }
        }
        validate_digest(&self.release_sha256)?;
        validate_digest(&self.parameters_sha256)?;
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InputReference {
    pub stage_sha256: String,
    pub path: String,
    pub sha256: String,
}

impl InputReference {
    pub fn validate(&self) -> Result<(), ManifestError> {
        validate_digest(&self.stage_sha256)?;
        validate_path(&self.path)?;
        validate_digest(&self.sha256)?;
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Stage {
    pub version: u64,
    pub kind: StageKind,
    pub manifest: Manifest,
    pub parents: Vec<String>,
    pub inputs: Vec<InputReference>,
    #[serde(deserialize_with = "required_option")]
    pub software: Option<SoftwareIdentity>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "super::conversion::present_conversion"
    )]
    pub conversion: Option<super::conversion::ConversionClaim>,
}

impl Stage {
    pub fn validate(&self) -> Result<(), ManifestError> {
        if !matches!(
            (&self.kind, self.version, &self.conversion),
            (StageKind::Acquisition | StageKind::Processing, 1, None)
                | (StageKind::Conversion, 2, Some(_))
        ) {
            return Err(invalid("unsupported stage version or conversion shape"));
        }
        self.manifest.validate()?;
        let mut parents = BTreeSet::new();
        for parent in &self.parents {
            validate_digest(parent)?;
            if !parents.insert(parent) {
                return Err(invalid("duplicate parents"));
            }
        }
        let mut references = BTreeSet::new();
        let mut used_parents = BTreeSet::new();
        for input in &self.inputs {
            input.validate()?;
            if !references.insert((&input.stage_sha256, &input.path)) {
                return Err(invalid("duplicate input reference"));
            }
            used_parents.insert(&input.stage_sha256);
        }
        match self.kind {
            StageKind::Acquisition => {
                if !self.parents.is_empty() || !self.inputs.is_empty() {
                    return Err(invalid(
                        "an acquisition cannot claim parent processing stages",
                    ));
                }
            }
            StageKind::Conversion => {
                if !self.parents.is_empty() || !self.inputs.is_empty() || self.software.is_none() {
                    return Err(invalid(
                        "conversion requires an external source, software identity and no internal parents",
                    ));
                }
                let claim = self.conversion.as_ref().unwrap();
                claim.validate()?;
                if let Some(report) = &claim.verification.report {
                    if !self
                        .manifest
                        .files
                        .iter()
                        .any(|file| file.path == report.path && file.sha256 == report.sha256)
                    {
                        return Err(invalid(
                            "vendor report must match this conversion stage output inventory",
                        ));
                    }
                }
            }
            StageKind::Processing => {
                if self.parents.is_empty() || self.inputs.is_empty() || self.software.is_none() {
                    return Err(invalid(
                        "processing requires parents, exact inputs and a software identity",
                    ));
                }
                if used_parents != parents {
                    return Err(invalid(
                        "inputs must reference every and only declared parent",
                    ));
                }
            }
        }
        if let Some(software) = &self.software {
            software.validate()?;
        }
        Ok(())
    }

    pub fn canonical_bytes(&self) -> Result<Vec<u8>, ManifestError> {
        self.validate()?;
        let mut normalized = self.clone();
        normalized.parents.sort();
        normalized
            .inputs
            .sort_by(|a, b| (&a.stage_sha256, &a.path).cmp(&(&b.stage_sha256, &b.path)));
        // Reuse the manifest normalizer rather than maintaining a second set of
        // file/chunk ordering rules here.
        normalized.manifest = Manifest::from_bytes(&self.manifest.canonical_bytes()?)?;
        if let Some(claim) = normalized.conversion.as_mut() {
            claim.source.files.sort_by(|a, b| a.path.cmp(&b.path));
        }
        canonical_json(&normalized)
    }

    pub fn digest(&self) -> Result<String, ManifestError> {
        Ok(hex::encode(Sha256::digest(self.canonical_bytes()?)))
    }

    pub fn from_bytes(data: &[u8]) -> Result<Self, ManifestError> {
        let item: Self = serde_json::from_slice(data)?;
        item.validate()?;
        Ok(item)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LineageReport {
    pub heads: Vec<String>,
    pub acquisitions: Vec<String>,
    #[serde(default)]
    pub conversions: Vec<String>,
    pub stage_count: usize,
    pub checkpoint_checked: bool,
}

/// Check all supplied history, with optional trusted head digests to detect
/// removal of an entire later stage. Signatures and roles are checked separately.
pub fn verify_lineage(
    stages: &[Stage],
    expected_heads: Option<&[String]>,
) -> Result<LineageReport, ManifestError> {
    let mut records = BTreeMap::new();
    let mut identifiers = BTreeSet::new();
    let mut paths = BTreeMap::new();
    let mut run_id = None;
    for stage in stages {
        stage.validate()?;
        let digest = stage.digest()?;
        if records.insert(digest, stage).is_some() || !identifiers.insert(&stage.manifest.stage_id)
        {
            return Err(invalid("duplicate stage or stage identifier"));
        }
        if run_id.is_some_and(|id| id != &stage.manifest.run_id) {
            return Err(invalid("stages belong to different runs"));
        }
        run_id = Some(&stage.manifest.run_id);
        for file in &stage.manifest.files {
            let identity = (&file.path, file.size, &file.sha256);
            if let Some(previous) = paths.insert(file.path.to_ascii_lowercase(), identity) {
                if previous != identity {
                    return Err(invalid(
                        "a later stage overwrites previously covered content",
                    ));
                }
            }
        }
    }
    if records.is_empty() {
        return Err(invalid("empty lineage"));
    }
    let mut referenced = BTreeSet::new();
    let mut acquisitions = Vec::new();
    let mut conversions = Vec::new();
    for (digest, stage) in &records {
        if stage.kind == StageKind::Acquisition {
            acquisitions.push(digest.clone());
        }
        if stage.kind == StageKind::Conversion {
            conversions.push(digest.clone());
        }
        for parent in &stage.parents {
            if !records.contains_key(parent) {
                return Err(invalid("missing parent stage"));
            }
            referenced.insert(parent.clone());
        }
        for input in &stage.inputs {
            let parent = records
                .get(&input.stage_sha256)
                .ok_or_else(|| invalid("missing input stage"))?;
            let descriptor = parent.manifest.files.iter().find(|f| f.path == input.path);
            if descriptor.is_none_or(|f| f.sha256 != input.sha256) {
                return Err(invalid(
                    "input does not match the parent stage's covered output",
                ));
            }
        }
    }
    // Iterative topological traversal avoids a stack overflow on deep histories.
    let mut pending: BTreeMap<_, _> = records
        .iter()
        .map(|(d, s)| (d.clone(), s.parents.len()))
        .collect();
    let mut children: BTreeMap<String, Vec<String>> =
        records.keys().map(|d| (d.clone(), Vec::new())).collect();
    for (digest, stage) in &records {
        for parent in &stage.parents {
            children
                .get_mut(parent)
                .ok_or_else(|| invalid("missing parent stage"))?
                .push(digest.clone());
        }
    }
    let mut ready: Vec<_> = pending
        .iter()
        .filter(|(_, n)| **n == 0)
        .map(|(d, _)| d.clone())
        .collect();
    let mut visited = 0;
    while let Some(digest) = ready.pop() {
        visited += 1;
        for child in &children[&digest] {
            let n = pending
                .get_mut(child)
                .ok_or_else(|| invalid("missing child stage"))?;
            *n -= 1;
            if *n == 0 {
                ready.push(child.clone());
            }
        }
    }
    if visited != records.len() {
        return Err(invalid("cyclic lineage"));
    }
    let heads: Vec<String> = records
        .keys()
        .filter(|d| !referenced.contains(*d))
        .cloned()
        .collect();
    if acquisitions.is_empty() && conversions.is_empty() {
        return Err(invalid(
            "lineage contains no acquisition or conversion root",
        ));
    }
    if let Some(expected) = expected_heads {
        let mut set = BTreeSet::new();
        for digest in expected {
            validate_digest(digest)?;
            if !set.insert(digest.clone()) {
                return Err(invalid("duplicate external head checkpoint"));
            }
        }
        if set.is_empty() || set != heads.iter().cloned().collect() {
            return Err(invalid(
                "lineage does not match the external head checkpoint",
            ));
        }
    }
    Ok(LineageReport {
        heads,
        acquisitions,
        conversions,
        stage_count: records.len(),
        checkpoint_checked: expected_heads.is_some(),
    })
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::provenance::manifest::FileDigest;
    use std::io::Cursor;

    fn chain() -> (Stage, Stage) {
        let first = Stage {
            version: 1,
            kind: StageKind::Acquisition,
            manifest: Manifest {
                version: 1,
                run_id: "run-1".into(),
                stage_id: "acquisition-1".into(),
                files: vec![
                    FileDigest::from_stream("raw.parquet", &mut Cursor::new(b"raw")).unwrap(),
                ],
            },
            parents: vec![],
            inputs: vec![],
            conversion: None,
            software: None,
        };
        let parent = first.digest().unwrap();
        let second = Stage {
            version: 1,
            kind: StageKind::Processing,
            manifest: Manifest {
                version: 1,
                run_id: "run-1".into(),
                stage_id: "processing-1".into(),
                files: vec![
                    FileDigest::from_stream("processed.parquet", &mut Cursor::new(b"processed"))
                        .unwrap(),
                ],
            },
            parents: vec![parent.clone()],
            inputs: vec![InputReference {
                stage_sha256: parent,
                path: "raw.parquet".into(),
                sha256: first.manifest.files[0].sha256.clone(),
            }],
            conversion: None,
            software: Some(SoftwareIdentity {
                name: "processor".into(),
                version: "1.2.3".into(),
                release_sha256: "a".repeat(64),
                parameters_sha256: "b".repeat(64),
            }),
        };
        (first, second)
    }

    pub(crate) fn conversion_stage() -> Stage {
        use crate::provenance::conversion::*;
        let (mut stage, processing) = chain();
        let source = SourceManifest {
            version: 1,
            format: "vendor.raw".into(),
            files: vec![
                SourceFile {
                    path: "sample.raw".into(),
                    size: 3,
                    sha256: hex::encode(Sha256::digest(b"raw")),
                },
                SourceFile {
                    path: "b.raw".into(),
                    size: 1,
                    sha256: hex::encode(Sha256::digest(b"b")),
                },
            ],
        };
        let verification = VendorVerification {
            vendor: "vendor-1".into(),
            method: "vendor-check-v1".into(),
            verifier_name: "Verifier".into(),
            verifier_version: "1.0".into(),
            source_sha256: source.digest().unwrap(),
            result: VerificationResult::Passed,
            report: None,
        };
        stage.version = 2;
        stage.kind = StageKind::Conversion;
        stage.software = processing.software;
        stage.conversion = Some(ConversionClaim {
            source,
            verification,
            claimed_acquisition_time: Some(1),
        });
        stage
    }

    #[test]
    fn conversion_root_and_processing_child_preserve_lineage() {
        let mut conversion = conversion_stage();
        let (_, mut child) = chain();
        child.parents = vec![conversion.digest().unwrap()];
        child.inputs[0].stage_sha256 = conversion.digest().unwrap();
        let report = verify_lineage(&[conversion.clone(), child], None).unwrap();
        assert!(report.acquisitions.is_empty());
        assert_eq!(report.conversions, [conversion.digest().unwrap()]);
        let bytes = conversion.canonical_bytes().unwrap();
        conversion
            .conversion
            .as_mut()
            .unwrap()
            .source
            .files
            .reverse();
        assert_eq!(bytes, conversion.canonical_bytes().unwrap());
        let claim = conversion.conversion.as_mut().unwrap();
        claim.verification.source_sha256 = "0".repeat(64);
        assert!(conversion.validate().is_err());
    }

    #[test]
    fn stage_versions_have_strict_distinct_shapes_and_bound_reports() {
        let (old, _) = chain();
        let mut value: serde_json::Value =
            serde_json::from_slice(&old.canonical_bytes().unwrap()).unwrap();
        value["conversion"] = serde_json::Value::Null;
        assert!(Stage::from_bytes(&serde_json::to_vec(&value).unwrap()).is_err());
        let mut conversion = conversion_stage();
        let bytes = conversion.canonical_bytes().unwrap();
        assert_eq!(
            Stage::from_bytes(&bytes)
                .unwrap()
                .canonical_bytes()
                .unwrap(),
            bytes
        );
        let mut value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        value.as_object_mut().unwrap().remove("conversion");
        assert!(Stage::from_bytes(&serde_json::to_vec(&value).unwrap()).is_err());
        let mut value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        value["version"] = 1.into();
        assert!(Stage::from_bytes(&serde_json::to_vec(&value).unwrap()).is_err());
        let duplicate = String::from_utf8(bytes)
            .unwrap()
            .replacen("{", "{\"version\":2,", 1);
        assert!(Stage::from_bytes(duplicate.as_bytes()).is_err());
        use crate::provenance::conversion::ReportReference;
        conversion.conversion.as_mut().unwrap().verification.report = Some(ReportReference {
            path: "missing.json".into(),
            sha256: "a".repeat(64),
        });
        assert!(conversion.validate().is_err());
        conversion.conversion.as_mut().unwrap().verification.report = Some(ReportReference {
            path: conversion.manifest.files[0].path.clone(),
            sha256: conversion.manifest.files[0].sha256.clone(),
        });
        assert!(conversion.validate().is_ok());
    }

    #[test]
    fn shared_stage_vectors() {
        let a = Stage::from_bytes(include_bytes!(
            "../../test/data/provenance/acquisition-v1.json"
        ))
        .unwrap();
        let b = Stage::from_bytes(include_bytes!(
            "../../test/data/provenance/processing-v1.json"
        ))
        .unwrap();
        assert_eq!(
            a.canonical_bytes().unwrap(),
            include_bytes!("../../test/data/provenance/acquisition-v1.json")
        );
        assert_eq!(
            b.canonical_bytes().unwrap(),
            include_bytes!("../../test/data/provenance/processing-v1.json")
        );
        let checkpoint = vec![b.digest().unwrap()];
        let report = verify_lineage(&[b, a], Some(&checkpoint)).unwrap();
        assert!(report.checkpoint_checked);
        assert_eq!(report.stage_count, 2);
    }

    #[test]
    fn append_and_rollback() {
        let (a, b) = chain();
        let checkpoint = vec![b.digest().unwrap()];
        assert_eq!(
            verify_lineage(&[a.clone(), b.clone()], Some(&checkpoint))
                .unwrap()
                .heads,
            checkpoint
        );
        assert!(verify_lineage(&[a.clone()], Some(&checkpoint)).is_err());
        assert!(!verify_lineage(&[a], None).unwrap().checkpoint_checked);
        assert!(verify_lineage(&[b], None).is_err());
    }

    #[test]
    fn bindings_and_immutable_outputs() {
        let (a, mut b) = chain();
        b.inputs[0].sha256 = "c".repeat(64);
        assert!(verify_lineage(&[a.clone(), b], None).is_err());
        let (_, mut b) = chain();
        b.manifest.files[0].path = "raw.parquet".into();
        assert!(verify_lineage(&[a.clone(), b], None).is_err());
        let (_, mut b) = chain();
        let old = b.digest().unwrap();
        b.software.as_mut().unwrap().parameters_sha256 = "c".repeat(64);
        assert_ne!(old, b.digest().unwrap());
        b.kind = StageKind::Acquisition;
        assert!(b.validate().is_err());
        assert!(verify_lineage(&[a.clone(), a], None).is_err());
        assert!(verify_lineage(&[], None).is_err());
    }
}
