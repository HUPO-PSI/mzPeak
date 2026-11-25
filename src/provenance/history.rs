use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::OsStr,
    fs,
    path::{Path, PathBuf},
};

use ::cms::signed_data::SignedData;
use const_oid::ObjectIdentifier;
use der::{
    Decode, Encode,
    asn1::{Any, OctetString},
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha512};

use super::archive::{Archive, Row};
use super::cms::{self, AlgorithmPolicy, Proof, SigningIdentity};
use super::conversion::{self, Conversion, SourceResult};
use super::statement::{ContentReference, Input, Member, Software, Stage, StageKind};
use super::timestamp::{self, TimestampPolicy};
use super::trust::{self, CertificateReport, CheckStatus, SignerRole, TrustPolicy};
use super::{AttributeOids, ROOT_INDEX, Result, digest_bytes, invalid};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AppendStage {
    pub kind: StageKind,
    #[serde(default)]
    pub inputs: Vec<Input>,
    pub software: Option<Software>,
    pub conversion: Option<Conversion>,
    #[serde(default)]
    pub parquet_chunks: bool,
    #[serde(default)]
    pub replacements: BTreeSet<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Checkpoint {
    pub index: u64,
    pub sha512: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConversionGrant {
    pub vendor: String,
    pub format: String,
    pub method: String,
    pub verifier: String,
    pub verifier_version: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Policy {
    pub oids: AttributeOids,
    pub trust: TrustPolicy,
    pub identities: BTreeMap<u64, String>,
    #[serde(default)]
    pub minimum_algorithms: AlgorithmPolicy,
    pub expected_checkpoint: Option<Checkpoint>,
    #[serde(default)]
    pub allow_replacements: bool,
    #[serde(default)]
    pub conversion_grants: Vec<ConversionGrant>,
    #[serde(default)]
    pub raw_sources: BTreeMap<u64, PathBuf>,
    #[serde(default)]
    pub require_source_bytes: bool,
    #[serde(default)]
    pub require_publisher_approval: bool,
    #[serde(default)]
    pub publishers: BTreeMap<String, String>,
    #[serde(default)]
    pub release_proofs: Vec<PathBuf>,
    pub archive_approver: Option<String>,
    pub historical: Option<TimestampPolicy>,
    /// Optional original RFC 3161 requests keyed by signature SHA-512.
    #[serde(default)]
    pub timestamp_queries: BTreeMap<String, PathBuf>,
}

#[derive(Debug, Default, Serialize)]
pub struct Coverage {
    pub missing: Vec<String>,
    pub changed: Vec<String>,
    pub unsigned: Vec<String>,
    pub historical_unavailable: Vec<u64>,
    pub derived: Vec<String>,
}
impl Coverage {
    pub fn complete(&self) -> bool {
        self.missing.is_empty() && self.changed.is_empty() && self.unsigned.is_empty()
    }
}

#[derive(Debug, Serialize)]
pub struct StageReport {
    pub index: u64,
    pub kind: StageKind,
    pub signature_valid: bool,
    pub algorithms_satisfied: bool,
    pub certificates: Vec<CertificateReport>,
    pub trusted_times: Vec<Option<i64>>,
    pub vendor_attestation: Option<CheckStatus>,
    pub source_integrity: Option<CheckStatus>,
    pub publisher_approval: Option<CheckStatus>,
    pub accepted: bool,
}

#[derive(Debug, Serialize)]
pub struct HistoryReport {
    pub accepted: bool,
    pub checkpoint: Checkpoint,
    pub checkpoint_verified: Option<bool>,
    pub coverage: Coverage,
    pub stages: Vec<StageReport>,
    pub archive_approval: Option<CheckStatus>,
}

fn checkpoint(rows: &[Row]) -> Result<Checkpoint> {
    let last = rows
        .last()
        .ok_or_else(|| invalid("no authenticated provenance table"))?;
    Ok(Checkpoint {
        index: last.index,
        sha512: hex::encode(Sha512::digest(&last.signature)),
    })
}

type Current = BTreeMap<String, u64>;

struct History {
    cms_rows: Vec<SignedData>,
    stages: Vec<Stage>,
    current: Current,
    owners: BTreeMap<u64, u64>,
    replaced: BTreeSet<u64>,
    derived: BTreeSet<String>,
}

fn optional_attribute(data: &SignedData, oid: ObjectIdentifier) -> Result<Option<&Any>> {
    let mut seen = None;
    for (index, signer) in data.signer_infos.0.iter().enumerate() {
        let value = cms::one(cms::attrs(signer)?, oid)?;
        if index > 0 && seen != value {
            return Err(invalid(
                "primary signers disagree on signed stage or content reference",
            ));
        }
        seen = value;
    }
    Ok(seen)
}

fn row_at<'a, T>(items: &'a [T], index: u64, label: &str) -> Result<&'a T> {
    usize::try_from(index)
        .ok()
        .and_then(|n| items.get(n))
        .ok_or_else(|| invalid(format!("unknown {label} row")))
}

fn confirm_replacement(old: &SignedData, new: &SignedData) -> Result<()> {
    let reference = optional_attribute(new, cms::CONTENT_REFERENCE)?
        .ok_or_else(|| invalid("duplicate current member without signed content-reference"))?
        .decode_as::<ContentReference>()?;
    if reference.content_type != cms::DATA {
        return Err(invalid("invalid replacement content type"));
    }
    for signer in old.signer_infos.0.iter() {
        if signer.signature == reference.signature
            && let Some(identifier) = cms::one(cms::attrs(signer)?, cms::CONTENT_IDENTIFIER)?
            && identifier.decode_as::<OctetString>()? == reference.identifier
        {
            return Ok(());
        }
    }
    Err(invalid(
        "replacement does not identify the original member signature",
    ))
}

fn read_history(openssl: &OsStr, rows: &[Row], stage_oid: ObjectIdentifier) -> Result<History> {
    let mut history = History {
        cms_rows: Vec::with_capacity(rows.len()),
        stages: Vec::new(),
        current: BTreeMap::new(),
        owners: BTreeMap::new(),
        replaced: BTreeSet::new(),
        derived: BTreeSet::new(),
    };
    let mut prefix = Sha512::new();
    let mut start = 0;
    for row in rows {
        let data = cms::parse(&row.signature)?;
        cms::detached(&data)?;
        if row.glob() {
            let digest = prefix.clone().finalize().into();
            let proofs = cms::verify(openssl, &data, &digest)?;
            let statement = optional_attribute(&data, stage_oid)?.ok_or_else(|| {
                invalid("glob lacks the configured authenticated stage attribute")
            })?;
            let stage = Stage::parse(&statement.to_der()?)?;
            if cms::proof_set(&proofs)? != stage.signers
                || stage.index != row.index
                || stage.members.len() as u64 != row.index - start
            {
                return Err(invalid(
                    "glob does not bind its signer set and complete stage inventory",
                ));
            }
            if start == 0
                && (stage.kind == StageKind::Processing
                    || !stage
                        .members
                        .iter()
                        .any(|member| member.path == ROOT_INDEX && member.whole()))
            {
                return Err(invalid(
                    "initial acquisition or conversion must sign the root index",
                ));
            }
            for input in &stage.inputs {
                if history.owners.get(&input.row) != Some(&input.stage)
                    || !row_at(rows, input.row, "input")?.descriptor(0).whole()
                {
                    return Err(invalid(
                        "input does not refer to an earlier stage's whole member",
                    ));
                }
            }
            let mut stage_whole = BTreeMap::new();
            let mut paths = BTreeSet::new();
            for (index, member) in (start..row.index).zip(&stage.members) {
                let member_row = row_at(rows, index, "member")?;
                if member_row.descriptor(member.size) != *member || member_row.glob() {
                    return Err(invalid("signed descriptor disagrees with its table row"));
                }
                let encoded = row_at(&history.cms_rows, index, "member CMS")?;
                if member.whole() {
                    if !paths.insert(member.path.to_ascii_lowercase()) {
                        return Err(invalid("duplicate or case-aliased stage output"));
                    }
                    if let Some(&previous) = history.current.get(&member.path) {
                        if stage.kind != StageKind::Processing
                            || !stage.inputs.iter().any(|input| input.row == previous)
                        {
                            return Err(invalid(
                                "a replacement must declare the covered input it supersedes",
                            ));
                        }
                        confirm_replacement(
                            row_at(&history.cms_rows, previous, "original CMS")?,
                            encoded,
                        )?;
                        history.replaced.insert(previous);
                        history.derived.insert(member.path.clone());
                    } else if stage.kind == StageKind::Processing {
                        let previous = history
                            .stages
                            .last()
                            .ok_or_else(|| invalid("missing prior checkpoint"))?;
                        confirm_replacement(
                            row_at(&history.cms_rows, previous.index, "prior glob")?,
                            encoded,
                        )?;
                        history.derived.insert(member.path.clone());
                    }
                    history.current.insert(member.path.clone(), index);
                    stage_whole.insert(member.path.as_str(), member.size);
                } else if stage_whole.get(member.path.as_str()) != Some(&member.size) {
                    return Err(invalid(
                        "Parquet range must follow the whole-member row of its stage",
                    ));
                }
                history.owners.insert(index, stage.index);
            }
            history.stages.push(stage);
            start = row.index + 1;
        } else {
            if optional_attribute(&data, stage_oid)?.is_some() {
                return Err(invalid("stage statement must be on a glob row"));
            }
            let signer = data.signer_infos.0.get(0).expect("nonempty CMS signer set");
            cms::verify(openssl, &data, &cms::message_digest(signer)?)?;
        }
        prefix.update(&row.signature);
        history.cms_rows.push(data);
    }
    Ok(history)
}

fn member_digest(data: &SignedData) -> Result<[u8; 64]> {
    cms::message_digest(data.signer_infos.0.get(0).expect("nonempty CMS signer set"))
}

fn check_coverage(
    archive: &mut Archive<'_>,
    history: &History,
    skip_replaced: &BTreeSet<String>,
) -> Result<Coverage> {
    let mut report = Coverage::default();
    let mut footer = BTreeMap::new();
    let mut changed = BTreeSet::new();
    let mut missing = BTreeSet::new();
    for stage in &history.stages {
        for member in &stage.members {
            if skip_replaced.contains(&member.path) {
                continue;
            }
            let current = history.current[&member.path];
            let historical = history.owners[&current] != stage.index;
            let data = row_at(&history.cms_rows, member.index, "signed content")?;
            let actual = archive.digest(member)?;
            let agrees = actual == Some(member_digest(data)?);
            if historical {
                if !agrees {
                    report.historical_unavailable.push(member.index);
                }
                continue;
            }
            if !archive.contents.contains_key(&member.path) {
                missing.insert(member.path.clone());
            } else if !agrees {
                changed.insert(member.path.clone());
            } else if member.row_group.is_some() {
                if !footer.contains_key(&member.path) {
                    let ranges = archive.parquet_ranges(&member.path)?;
                    footer.insert(member.path.clone(), ranges);
                }
                let mut descriptor = member.clone();
                descriptor.index = 0;
                if !footer[&member.path].contains(&descriptor) {
                    changed.insert(member.path.clone());
                }
            }
        }
    }
    report.missing = missing.into_iter().collect();
    report.changed = changed.into_iter().collect();
    report.unsigned = archive
        .contents
        .keys()
        .filter(|p| !history.current.contains_key(*p))
        .cloned()
        .collect();
    report.derived = history.derived.iter().cloned().collect();
    Ok(report)
}

fn terminal_digest(rows: &[Row]) -> [u8; 64] {
    let mut digest = Sha512::new();
    for row in rows {
        digest.update(&row.signature);
    }
    digest.finalize().into()
}

/// The caller must have exclusive write access during archive replacement.
pub fn append_stage(
    openssl: &OsStr,
    path: &Path,
    oids: &AttributeOids,
    signers: &[SigningIdentity],
    options: &AppendStage,
) -> Result<Checkpoint> {
    let (stage_oid, _) = oids.parse()?;
    let mut archive = Archive::open(path)?;
    let mut rows = archive.rows()?;
    let old = read_history(openssl, &rows, stage_oid)?;
    if options.kind != StageKind::Processing && !options.replacements.is_empty() {
        return Err(invalid("only processing can replace earlier members"));
    }
    if rows.is_empty() && options.kind == StageKind::Processing {
        return Err(invalid(
            "the first stage must be acquisition or vendor conversion",
        ));
    }
    for name in &options.replacements {
        if !old.current.contains_key(name) || !archive.contents.contains_key(name) {
            return Err(invalid(
                "replacement must name an existing signed and present member",
            ));
        }
    }
    let coverage = check_coverage(&mut archive, &old, &options.replacements)?;
    if !coverage.missing.is_empty() || !coverage.changed.is_empty() {
        return Err(invalid(
            "existing coverage changed outside the declared replacements",
        ));
    }
    let identities = cms::identities(signers)?;
    let mut members = Vec::new();
    let outputs = coverage
        .unsigned
        .into_iter()
        .chain(options.replacements.iter().cloned())
        .collect::<BTreeSet<_>>();
    for name in outputs {
        let size = archive.contents[&name].1;
        let mut descriptors = vec![Member {
            index: 0,
            path: name.clone(),
            size,
            row_group: None,
            column: None,
            column_path: None,
            offset: None,
            length: None,
        }];
        if options.parquet_chunks && name.ends_with(".parquet") {
            descriptors.extend(archive.parquet_ranges(&name)?);
        }
        for mut member in descriptors {
            member.index = rows.len() as u64;
            member.validate()?;
            let digest = archive
                .digest(&member)?
                .ok_or_else(|| invalid("archive changed before signing"))?;
            let mut id = Sha512::new();
            id.update(b"mzPeak content identifier v1\0");
            id.update(member.to_der()?);
            id.update(digest);
            let attr = cms::attribute(
                cms::CONTENT_IDENTIFIER,
                Any::encode_from(&OctetString::new(id.finalize().to_vec())?)?,
            )?;
            let mut extra = vec![attr];
            if member.whole() && options.kind == StageKind::Processing {
                let previous = old
                    .current
                    .get(&name)
                    .copied()
                    .or_else(|| old.stages.last().map(|stage| stage.index))
                    .ok_or_else(|| invalid("processing has no prior checkpoint"))?;
                let source = row_at(&old.cms_rows, previous, "derivation source")?;
                let signer = source.signer_infos.0.get(0).expect("nonempty CMS signers");
                let original_id = cms::one(cms::attrs(signer)?, cms::CONTENT_IDENTIFIER)?
                    .ok_or_else(|| {
                        invalid("derivation source requires a signed content identifier")
                    })?;
                extra.push(cms::attribute(
                    cms::CONTENT_REFERENCE,
                    Any::encode_from(&ContentReference {
                        content_type: cms::DATA,
                        identifier: original_id.decode_as::<OctetString>()?,
                        signature: signer.signature.clone(),
                    })?,
                )?);
            }
            rows.push(Row {
                index: member.index,
                filename: member.path.clone(),
                row_group: member.row_group,
                column: member.column,
                path: member.column_path.clone(),
                offset: member.offset,
                length: member.length,
                signature: cms::sign(openssl, &identities, &digest, None, &extra)?,
            });
            members.push(member);
        }
    }
    let conversion = options
        .conversion
        .as_ref()
        .map(|conversion| {
            let report = conversion
                .report_path
                .as_ref()
                .map(|path| {
                    members
                        .iter()
                        .find(|m| m.path == *path && m.whole())
                        .map(|m| m.index)
                        .ok_or_else(|| invalid("vendor report must be a new covered member"))
                })
                .transpose()?;
            conversion.encode(report)
        })
        .transpose()?;
    let software = options
        .software
        .as_ref()
        .map(TryInto::try_into)
        .transpose()?;
    let mut inputs = options.inputs.clone();
    inputs.sort();
    let stage = Stage {
        version: 1,
        kind: options.kind,
        index: rows.len() as u64,
        members,
        signers: cms::signer_set(&identities)?,
        inputs,
        software,
        conversion,
    };
    stage.validate()?;
    let attribute = cms::attribute(stage_oid, Any::encode_from(&stage)?)?;
    let identifier = cms::attribute(
        cms::CONTENT_IDENTIFIER,
        Any::encode_from(&OctetString::new(Sha512::digest(stage.to_der()?).to_vec())?)?,
    )?;
    rows.push(Row {
        index: stage.index,
        filename: "*".to_owned(),
        row_group: None,
        column: None,
        path: None,
        offset: None,
        length: None,
        signature: cms::sign(
            openssl,
            &identities,
            &terminal_digest(&rows),
            None,
            &[attribute, identifier],
        )?,
    });
    // Validate relationships and signed inventory before the atomic update.
    read_history(openssl, &rows, stage_oid)?;
    let result = checkpoint(&rows)?;
    archive.rewrite(&rows)?;
    Ok(result)
}

fn cert_reports<'a>(
    openssl: &OsStr,
    data: &'a SignedData,
    proofs: &[Proof<'a>],
    trust_policy: &TrustPolicy,
    role: SignerRole,
    identity: &str,
    time: Option<i64>,
) -> Result<Vec<CertificateReport>> {
    let mut policy = trust_policy.clone();
    if let Some(time) = time {
        policy.validation_time = time;
    }
    if policy.crls_pem.is_empty() {
        policy.crls_pem = cms::crls(data)?;
    }
    let chain = cms::certificates(data)?;
    proofs
        .iter()
        .map(|proof| {
            let intermediates = chain
                .iter()
                .filter(|cert| **cert != proof.certificate)
                .map(|cert| cms::pem(cert))
                .collect::<Result<Vec<_>>>()?;
            Ok(trust::verify_certificate_with(
                openssl,
                &cms::pem(proof.certificate)?,
                &intermediates,
                &policy,
                role,
                identity,
            )?)
        })
        .collect()
}

fn trusted_times(
    openssl: &OsStr,
    proofs: &[Proof<'_>],
    policy: &Policy,
) -> Result<Vec<Option<i64>>> {
    let Some(tsa) = &policy.historical else {
        return Ok(Vec::new());
    };
    proofs
        .iter()
        .map(|proof| {
            let token = proof
                .info
                .unsigned_attrs
                .as_ref()
                .map(|attrs| cms::one(attrs, cms::TIMESTAMP))
                .transpose()?
                .flatten()
                .map(Encode::to_der)
                .transpose()?;
            let signature = proof.info.signature.as_bytes();
            let query = policy
                .timestamp_queries
                .get(&hex::encode(Sha512::digest(signature)))
                .map(fs::read)
                .transpose()?;
            let result = timestamp::verify_timestamp_with(
                openssl,
                signature,
                token.as_deref(),
                query.as_deref(),
                tsa,
            )?;
            Ok(result.accepted().then_some(result.trusted_time).flatten())
        })
        .collect()
}

fn role(kind: StageKind) -> SignerRole {
    match kind {
        StageKind::Acquisition => SignerRole::Acquisition,
        StageKind::Processing => SignerRole::Processing,
        StageKind::Conversion => SignerRole::VendorConversion,
    }
}

fn conversion_checks(
    stage: &Stage,
    policy: &Policy,
    identity: &str,
) -> (Option<CheckStatus>, Option<CheckStatus>) {
    let Some(claim) = &stage.conversion else {
        return (None, None);
    };
    let attestation = claim.vendor == identity
        && claim.result == SourceResult::Passed
        && policy.conversion_grants.iter().any(|grant| {
            grant.vendor == claim.vendor
                && grant.format == claim.format
                && grant.method == claim.method
                && grant.verifier == claim.verifier
                && grant.verifier_version == claim.verifier_version
        });
    let independent = match policy.raw_sources.get(&stage.index) {
        None => CheckStatus::Unknown,
        Some(path) => match conversion::verify_source(path, claim) {
            Ok(true) => CheckStatus::Valid,
            Ok(false) | Err(_) => CheckStatus::Invalid,
        },
    };
    (
        Some(if attestation {
            CheckStatus::Valid
        } else {
            CheckStatus::Invalid
        }),
        Some(independent),
    )
}

fn release_approved(
    openssl: &OsStr,
    stage: &Stage,
    policy: &Policy,
    releases: &[(super::statement::Release, [u8; 64], SignedData)],
) -> Result<Option<CheckStatus>> {
    let Some(software) = &stage.software else {
        return Ok(None);
    };
    let publisher = policy
        .publishers
        .get(&hex::encode(software.release.as_bytes()));
    if publisher.is_none() && !policy.require_publisher_approval {
        return Ok(None);
    }
    let Some(publisher) = publisher else {
        return Ok(Some(CheckStatus::Unknown));
    };
    for (release, digest, data) in releases {
        if release.digest != software.release
            || release.name != software.name
            || release.software_version != software.version
        {
            continue;
        }
        let proofs = cms::verify(openssl, data, digest)?;
        if cms::proof_set(&proofs)? != release.signers
            || !policy.minimum_algorithms.accepts(&proofs)
        {
            continue;
        }
        let times = trusted_times(openssl, &proofs, policy)?;
        if times.iter().any(Option::is_none) {
            continue;
        }
        let mut all_valid = true;
        for (i, proof) in proofs.iter().enumerate() {
            let reports = cert_reports(
                openssl,
                data,
                std::slice::from_ref(proof),
                &policy.trust,
                SignerRole::SoftwareRelease,
                publisher,
                times.get(i).copied().flatten(),
            )?;
            all_valid &= reports.iter().all(CertificateReport::accepted);
        }
        if all_valid {
            return Ok(Some(CheckStatus::Valid));
        }
    }
    Ok(Some(CheckStatus::Invalid))
}

fn archive_approval(
    openssl: &OsStr,
    data: &SignedData,
    policy: &Policy,
) -> Result<Option<CheckStatus>> {
    let Some(identity) = &policy.archive_approver else {
        return Ok(None);
    };
    let certs = cms::certificates(data)?;
    for primary in data.signer_infos.0.iter() {
        let Some(attrs) = &primary.unsigned_attrs else {
            continue;
        };
        for attr in attrs
            .iter()
            .filter(|attr| attr.oid == cms::COUNTERSIGNATURE)
        {
            for value in attr.values.iter() {
                let signer = value.decode_as()?;
                let digest = Sha512::digest(primary.signature.as_bytes()).into();
                let proof = cms::check_signature(openssl, &signer, &certs, &digest, true)?;
                if cert_reports(
                    openssl,
                    data,
                    &[proof],
                    &policy.trust,
                    SignerRole::ArchiveApproval,
                    identity,
                    None,
                )?
                .iter()
                .all(CertificateReport::accepted)
                {
                    return Ok(Some(CheckStatus::Valid));
                }
            }
        }
    }
    Ok(Some(CheckStatus::Invalid))
}

pub fn verify_archive(openssl: &OsStr, path: &Path, policy: &Policy) -> Result<HistoryReport> {
    let (stage_oid, publisher_oid) = policy.oids.parse()?;
    let mut archive = Archive::open(path)?;
    let rows = archive.rows()?;
    let checkpoint = checkpoint(&rows)?;
    let history = read_history(openssl, &rows, stage_oid)?;
    for stage in policy.raw_sources.keys() {
        if !history
            .stages
            .iter()
            .any(|s| s.index == *stage && s.kind == StageKind::Conversion)
        {
            return Err(invalid(
                "source mapping does not identify a conversion stage",
            ));
        }
    }
    let coverage = check_coverage(&mut archive, &history, &BTreeSet::new())?;
    let checkpoint_verified = policy.expected_checkpoint.as_ref().map(|expected| {
        expected.index == checkpoint.index
            && digest_bytes(&expected.sha512).ok() == digest_bytes(&checkpoint.sha512).ok()
    });
    let mut release_bytes = BTreeSet::new();
    if policy.require_publisher_approval || !policy.publishers.is_empty() {
        for stage in &history.stages {
            let data = row_at(&history.cms_rows, stage.index, "stage")?;
            for value in cms::unsigned_values(data, publisher_oid)? {
                release_bytes.insert(value.to_der()?);
            }
        }
        for proof in &policy.release_proofs {
            release_bytes.insert(fs::read(proof)?);
        }
    }
    let releases = release_bytes
        .iter()
        .map(|encoded| {
            let data = cms::parse(encoded)?;
            let (release, digest) = cms::read_release(&data)?;
            Ok((release, digest, data))
        })
        .collect::<Result<Vec<_>>>()?;
    let mut stages = Vec::new();
    for stage in &history.stages {
        let data = row_at(&history.cms_rows, stage.index, "stage")?;
        let identity = policy
            .identities
            .get(&stage.index)
            .map(String::as_str)
            .unwrap_or("");
        let proofs = cms::verify(openssl, data, &member_digest(data)?)?;
        let times = trusted_times(openssl, &proofs, policy)?;
        let algorithms_satisfied = policy.minimum_algorithms.accepts(&proofs);
        let accepted_times = times.iter().all(Option::is_some);
        let mut certificates = Vec::new();
        if accepted_times {
            for (i, proof) in proofs.iter().enumerate() {
                certificates.extend(cert_reports(
                    openssl,
                    data,
                    std::slice::from_ref(proof),
                    &policy.trust,
                    role(stage.kind),
                    identity,
                    times.get(i).copied().flatten(),
                )?);
            }
            // The stage commitment authenticates every member-row proof by its
            // earliest verified stage timestamp. Every row's signer is authorized separately.
            let stage_time = times.iter().copied().flatten().min();
            for member in &stage.members {
                let data = row_at(&history.cms_rows, member.index, "member")?;
                let signers = cms::verify(openssl, data, &member_digest(data)?)?;
                certificates.extend(cert_reports(
                    openssl,
                    data,
                    &signers,
                    &policy.trust,
                    role(stage.kind),
                    identity,
                    stage_time,
                )?);
            }
        }
        let (vendor_attestation, source_integrity) = conversion_checks(stage, policy, identity);
        let release = release_approved(openssl, stage, policy, &releases)?;
        let accepted = !identity.is_empty()
            && algorithms_satisfied
            && accepted_times
            && certificates.iter().all(CertificateReport::accepted)
            && vendor_attestation
                .as_ref()
                .is_none_or(|s| *s == CheckStatus::Valid)
            && source_integrity.as_ref().is_none_or(|s| {
                *s == CheckStatus::Valid
                    || *s == CheckStatus::Unknown && !policy.require_source_bytes
            })
            && release.as_ref().is_none_or(|s| *s == CheckStatus::Valid);
        stages.push(StageReport {
            index: stage.index,
            kind: stage.kind,
            signature_valid: true,
            algorithms_satisfied,
            certificates,
            trusted_times: times,
            vendor_attestation,
            source_integrity,
            publisher_approval: release,
            accepted,
        });
    }
    let approval = archive_approval(
        openssl,
        history.cms_rows.last().expect("nonempty history"),
        policy,
    )?;
    archive.stable()?;
    Ok(HistoryReport {
        accepted: coverage.complete()
            && checkpoint_verified != Some(false)
            && (history.replaced.is_empty() || policy.allow_replacements)
            && stages.iter().all(|s| s.accepted)
            && approval.as_ref().is_none_or(|s| *s == CheckStatus::Valid),
        checkpoint,
        checkpoint_verified,
        coverage,
        stages,
        archive_approval: approval,
    })
}

/// Return a packed archive's most recent glob CMS for inspection or timestamp queries.
pub fn terminal_glob(path: &Path) -> Result<Vec<u8>> {
    let mut archive = Archive::open(path)?;
    let rows = archive.rows()?;
    archive.stable()?;
    Ok(rows
        .into_iter()
        .last()
        .ok_or_else(|| invalid("archive has no provenance"))?
        .signature)
}

fn edit_terminal(
    openssl: &OsStr,
    path: &Path,
    oids: &AttributeOids,
    edit: impl FnOnce(&mut SignedData) -> Result<()>,
) -> Result<Checkpoint> {
    let (stage_oid, _) = oids.parse()?;
    let mut archive = Archive::open(path)?;
    let mut rows = archive.rows()?;
    read_history(openssl, &rows, stage_oid)?;
    let last = rows
        .last_mut()
        .ok_or_else(|| invalid("archive has no provenance"))?;
    let mut data = cms::parse(&last.signature)?;
    edit(&mut data)?;
    last.signature = cms::encode(&data)?;
    read_history(openssl, &rows, stage_oid)?;
    let result = checkpoint(&rows)?;
    archive.rewrite(&rows)?;
    Ok(result)
}

pub fn attach_release(
    openssl: &OsStr,
    path: &Path,
    oids: &AttributeOids,
    proof: &[u8],
) -> Result<Checkpoint> {
    let (_, publisher_oid) = oids.parse()?;
    let data = cms::parse(proof)?;
    let (release, digest) = cms::read_release(&data)?;
    if cms::proof_set(&cms::verify(openssl, &data, &digest)?)? != release.signers {
        return Err(invalid(
            "publisher proof does not bind its complete signer set",
        ));
    }
    edit_terminal(openssl, path, oids, |terminal| {
        let mut signers: Vec<_> = terminal.signer_infos.0.iter().cloned().collect();
        cms::add_unsigned(&mut signers[0], publisher_oid, Any::from_der(proof)?)?;
        terminal.signer_infos = ::cms::signed_data::SignerInfos::try_from(signers)?;
        Ok(())
    })
}

pub fn attach_timestamp(
    openssl: &OsStr,
    path: &Path,
    oids: &AttributeOids,
    response: &[u8],
    query: Option<&[u8]>,
) -> Result<Checkpoint> {
    edit_terminal(openssl, path, oids, |data| {
        timestamp::attach(data, response, query)
    })
}

pub fn approve_archive(
    openssl: &OsStr,
    path: &Path,
    oids: &AttributeOids,
    signers: &[SigningIdentity],
) -> Result<Checkpoint> {
    edit_terminal(openssl, path, oids, |data| {
        cms::countersign(openssl, data, signers)
    })
}
