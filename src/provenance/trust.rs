//! Explicit local trust anchors and role grants. Embedded certificates are not trust anchors.
//! OpenSSL validates RFC 5280 paths and full-chain CRLs. No network fetches occur.
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{fs, process::Command};
use x509_cert::{
    Certificate,
    der::{DecodePem, Encode},
    ext::pkix::{BasicConstraints, ExtendedKeyUsage, KeyUsage},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckStatus {
    Valid,
    Invalid,
    Unknown,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SignerRole {
    Acquisition,
    Processing,
    SoftwareRelease,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Authorization {
    pub certificate_sha256: String,
    pub role: SignerRole,
    pub identity: String,
    pub required_eku: Option<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TrustPolicy {
    pub roots_pem: Vec<String>,
    pub authorizations: Vec<Authorization>,
    pub validation_time: i64,
    #[serde(default)]
    pub crls_pem: Vec<String>,
    /// Strict PQC-only profile: the leaf and every supplied issuer/anchor must
    /// use ML-DSA keys/signatures. This requires a separate PQC trust domain;
    /// dual data signatures alone do not upgrade a classical certificate path.
    #[serde(default)]
    pub require_pqc_chain: bool,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CertificateReport {
    pub certificate_sha256: String,
    pub path: CheckStatus,
    pub authorization: CheckStatus,
    pub revocation: CheckStatus,
    pub validation_time: i64,
    pub details: Vec<String>,
}
impl CertificateReport {
    /// Missing revocation evidence is unknown and never accepted.
    pub fn accepted(&self) -> bool {
        [self.path, self.authorization, self.revocation]
            .iter()
            .all(|s| *s == CheckStatus::Valid)
    }
}
#[derive(Debug, thiserror::Error)]
pub enum TrustError {
    #[error("I/O or OpenSSL execution error: {0}")]
    Io(#[from] std::io::Error),
    #[error("invalid policy: {0}")]
    Policy(String),
}
fn certificate(pem: &str) -> Result<Certificate, String> {
    if pem.matches("-----BEGIN CERTIFICATE-----").count() != 1
        || pem.matches("-----END CERTIFICATE-----").count() != 1
        || !pem.trim().starts_with("-----BEGIN CERTIFICATE-----")
        || !pem.trim().ends_with("-----END CERTIFICATE-----")
    {
        return Err("expected exactly one PEM certificate".into());
    }
    Certificate::from_pem(pem).map_err(|e| e.to_string())
}
pub fn certificate_fingerprint(pem: &str) -> Result<String, String> {
    let cert = certificate(pem)?;
    Ok(hex::encode(Sha256::digest(
        cert.to_der().map_err(|e| e.to_string())?,
    )))
}
fn pq(oid: &str) -> bool {
    matches!(
        oid,
        "2.16.840.1.101.3.4.3.17" | "2.16.840.1.101.3.4.3.18" | "2.16.840.1.101.3.4.3.19"
    )
}

/// Validate eligibility at an explicit time, which is NOT proof of signing time.
/// Signature correctness and content coverage must be checked separately.
pub fn verify_certificate(
    leaf_pem: &str,
    intermediates_pem: &[String],
    policy: &TrustPolicy,
    role: SignerRole,
    identity: &str,
) -> Result<CertificateReport, TrustError> {
    verify_certificate_with(
        "openssl",
        leaf_pem,
        intermediates_pem,
        policy,
        role,
        identity,
    )
}
pub fn verify_certificate_with(
    openssl: impl AsRef<std::ffi::OsStr>,
    leaf_pem: &str,
    intermediates_pem: &[String],
    policy: &TrustPolicy,
    role: SignerRole,
    identity: &str,
) -> Result<CertificateReport, TrustError> {
    if policy.validation_time < 0 {
        return Err(TrustError::Policy("negative validation_time".into()));
    }
    let mut result = CertificateReport {
        certificate_sha256: String::new(),
        path: CheckStatus::Invalid,
        authorization: CheckStatus::Invalid,
        revocation: CheckStatus::Unknown,
        validation_time: policy.validation_time,
        details: vec![],
    };
    let inspect = (|| -> Result<(), String> {
        let leaf = certificate(leaf_pem)?;
        result.certificate_sha256 = certificate_fingerprint(leaf_pem)?;
        if policy.roots_pem.is_empty() {
            return Err("no externally configured trust anchors".into());
        }
        let roots = policy
            .roots_pem
            .iter()
            .map(|p| certificate(p))
            .collect::<Result<Vec<_>, _>>()?;
        let chain = intermediates_pem
            .iter()
            .map(|p| certificate(p))
            .collect::<Result<Vec<_>, _>>()?;
        for root in &roots {
            if !root
                .tbs_certificate
                .get::<BasicConstraints>()
                .map_err(|e| e.to_string())?
                .is_some_and(|(_, bc)| bc.ca)
            {
                return Err("trust anchors must be CA certificates".into());
            }
        }
        let tbs = &leaf.tbs_certificate;
        let bc = tbs
            .get::<BasicConstraints>()
            .map_err(|e| e.to_string())?
            .ok_or("missing basicConstraints")?
            .1;
        let ku = tbs
            .get::<KeyUsage>()
            .map_err(|e| e.to_string())?
            .ok_or("missing keyUsage")?
            .1;
        if bc.ca || !ku.digital_signature() || ku.key_cert_sign() {
            return Err("signer must be an end entity with digitalSignature key usage".into());
        }
        if policy.require_pqc_chain {
            for cert in std::iter::once(&leaf)
                .chain(chain.iter())
                .chain(roots.iter())
            {
                if !pq(&cert
                    .tbs_certificate
                    .subject_public_key_info
                    .algorithm
                    .oid
                    .to_string())
                    || !pq(&cert.signature_algorithm.oid.to_string())
                {
                    return Err("PQC identity policy rejects a classical certificate path".into());
                }
            }
        }
        let grants: Vec<_> = policy
            .authorizations
            .iter()
            .filter(|a| {
                a.certificate_sha256 == result.certificate_sha256
                    && a.role == role
                    && a.identity == identity
                    && !identity.is_empty()
            })
            .collect();
        if grants.len() != 1 {
            result
                .details
                .push("no unique locally configured role and identity grant".into());
        } else {
            let required =
                grants[0]
                    .required_eku
                    .as_deref()
                    .or(if role == SignerRole::SoftwareRelease {
                        Some("1.3.6.1.5.5.7.3.3")
                    } else {
                        None
                    });
            if let Some(oid) = required {
                let eku = tbs
                    .get::<ExtendedKeyUsage>()
                    .map_err(|e| e.to_string())?
                    .ok_or("missing extendedKeyUsage")?
                    .1;
                if !eku.0.iter().any(|v| v.to_string() == oid) {
                    return Err("certificate lacks locally required extended key usage".into());
                }
                // purpose=any does not enforce application-specific EKUs. An
                // explicit issuer EKU must allow the locally required purpose.
                // Checking all supplied candidates is deliberately conservative.
                for issuer in chain.iter().chain(roots.iter()) {
                    if let Some((_, eku)) = issuer
                        .tbs_certificate
                        .get::<ExtendedKeyUsage>()
                        .map_err(|e| e.to_string())?
                    {
                        if !eku
                            .0
                            .iter()
                            .any(|v| v.to_string() == oid || v.to_string() == "2.5.29.37.0")
                        {
                            return Err(
                                "issuer extended key usage excludes locally required purpose"
                                    .into(),
                            );
                        }
                    }
                }
            }
            result.authorization = CheckStatus::Valid;
        }
        Ok(())
    })();
    if let Err(e) = inspect {
        result.details.push(e);
        return Ok(result);
    }
    let directory = tempfile::tempdir()?;
    let leaf = directory.path().join("leaf.pem");
    let roots = directory.path().join("roots.pem");
    let chain = directory.path().join("chain.pem");
    let crls = directory.path().join("crls.pem");
    fs::write(&leaf, leaf_pem)?;
    fs::write(&roots, policy.roots_pem.join("\n"))?;
    let args = vec![
        "verify".to_string(),
        "-no-CApath".into(),
        "-no-CAstore".into(),
        "-CAfile".into(),
        roots.to_string_lossy().into_owned(),
        "-x509_strict".into(),
        "-check_ss_sig".into(),
        "-purpose".into(),
        "any".into(),
        "-attime".into(),
        policy.validation_time.to_string(),
    ];
    let mut command = Command::new(openssl.as_ref());
    command.args(&args);
    if !intermediates_pem.is_empty() {
        fs::write(&chain, intermediates_pem.join("\n"))?;
        command.arg("-untrusted").arg(&chain);
    }
    let checked = command.arg(&leaf).output()?;
    if !checked.status.success() {
        result.details.push(
            String::from_utf8_lossy(&checked.stderr)
                .chars()
                .take(4096)
                .collect(),
        );
        return Ok(result);
    }
    result.path = CheckStatus::Valid;
    if policy.crls_pem.is_empty() {
        result
            .details
            .push("revocation evidence not supplied".into());
        return Ok(result);
    }
    fs::write(&crls, policy.crls_pem.join("\n"))?;
    let mut command = Command::new(openssl.as_ref());
    command.args(&args);
    if !intermediates_pem.is_empty() {
        command.arg("-untrusted").arg(&chain);
    }
    let checked = command
        .arg("-CRLfile")
        .arg(&crls)
        .arg("-crl_check_all")
        .arg(&leaf)
        .output()?;
    result.revocation = if checked.status.success() {
        CheckStatus::Valid
    } else {
        CheckStatus::Invalid
    };
    if !checked.status.success() {
        result.details.push(
            String::from_utf8_lossy(&checked.stderr)
                .chars()
                .take(4096)
                .collect(),
        );
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn pem(name: &str) -> String {
        fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("test/data/provenance/trust")
                .join(name),
        )
        .unwrap()
    }
    fn policy(name: &str, role: SignerRole) -> TrustPolicy {
        TrustPolicy {
            roots_pem: vec![pem("root.pem")],
            authorizations: vec![Authorization {
                certificate_sha256: certificate_fingerprint(&pem(name)).unwrap(),
                role,
                identity: "instrument-1".into(),
                required_eku: None,
            }],
            validation_time: 1893456000,
            crls_pem: vec![pem("root.crl.pem"), pem("issuer.crl.pem")],
            require_pqc_chain: false,
        }
    }
    fn check(name: &str, p: &TrustPolicy, role: SignerRole) -> CertificateReport {
        verify_certificate(&pem(name), &[pem("issuer.pem")], p, role, "instrument-1").unwrap()
    }
    #[test]
    fn valid_path_requires_full_crls_and_role_grant() {
        let mut p = policy("leaf.pem", SignerRole::Acquisition);
        assert!(check("leaf.pem", &p, SignerRole::Acquisition).accepted());
        p.crls_pem.clear();
        let r = check("leaf.pem", &p, SignerRole::Acquisition);
        assert_eq!(r.path, CheckStatus::Valid);
        assert_eq!(r.revocation, CheckStatus::Unknown);
        assert!(!r.accepted());
        p = policy("leaf.pem", SignerRole::Processing);
        assert!(!check("leaf.pem", &p, SignerRole::Acquisition).accepted());
    }
    #[test]
    fn rejects_revoked_expired_ca_and_wrong_usage() {
        for name in ["revoked.pem", "expired.pem", "issuer.pem", "bad_usage.pem"] {
            assert!(
                !check(
                    name,
                    &policy(name, SignerRole::Acquisition),
                    SignerRole::Acquisition
                )
                .accepted(),
                "{name}"
            );
        }
    }
    #[test]
    fn rejects_wrong_root_missing_issuer_stale_crls_and_pqc_downgrade() {
        let mut p = policy("leaf.pem", SignerRole::Acquisition);
        p.roots_pem = vec![pem("other_root.pem")];
        assert!(!check("leaf.pem", &p, SignerRole::Acquisition).accepted());
        p = policy("leaf.pem", SignerRole::Acquisition);
        assert!(
            !verify_certificate(
                &pem("leaf.pem"),
                &[],
                &p,
                SignerRole::Acquisition,
                "instrument-1"
            )
            .unwrap()
            .accepted()
        );
        p.validation_time += 172800;
        assert!(!check("leaf.pem", &p, SignerRole::Acquisition).accepted());
        p = policy("leaf.pem", SignerRole::Acquisition);
        p.require_pqc_chain = true;
        assert!(!check("leaf.pem", &p, SignerRole::Acquisition).accepted());
    }
    #[test]
    fn software_release_requires_eku() {
        assert!(
            !check(
                "leaf.pem",
                &policy("leaf.pem", SignerRole::SoftwareRelease),
                SignerRole::SoftwareRelease
            )
            .accepted()
        );
        assert!(
            check(
                "release.pem",
                &policy("release.pem", SignerRole::SoftwareRelease),
                SignerRole::SoftwareRelease
            )
            .accepted()
        );
    }
    #[test]
    fn malformed_and_bundled_leaf_are_not_trusted() {
        for leaf in ["garbage".into(), pem("leaf.pem") + &pem("root.pem")] {
            assert!(
                !verify_certificate(
                    &leaf,
                    &[],
                    &policy("leaf.pem", SignerRole::Acquisition),
                    SignerRole::Acquisition,
                    "instrument-1"
                )
                .unwrap()
                .accepted()
            );
        }
    }
    #[test]
    fn issuer_and_anchor_eku_constrain_software_release() {
        let mut p = TrustPolicy {
            roots_pem: vec![pem("eku-root.pem")],
            authorizations: vec![Authorization {
                certificate_sha256: certificate_fingerprint(&pem("eku-leaf.pem")).unwrap(),
                role: SignerRole::SoftwareRelease,
                identity: "publisher-1".into(),
                required_eku: None,
            }],
            validation_time: 1893456000,
            crls_pem: vec![pem("eku-root.crl.pem"), pem("eku-issuer.crl.pem")],
            require_pqc_chain: false,
        };
        let verify = |issuer: &str, policy: &TrustPolicy| {
            verify_certificate(
                &pem("eku-leaf.pem"),
                &[pem(issuer)],
                policy,
                SignerRole::SoftwareRelease,
                "publisher-1",
            )
            .unwrap()
        };
        assert!(verify("eku-issuer-code.pem", &p).accepted());
        assert!(verify("eku-issuer-any.pem", &p).accepted());
        let denied = verify("eku-issuer-server.pem", &p);
        assert!(!denied.accepted());
        assert!(
            denied
                .details
                .iter()
                .any(|detail| detail.contains("issuer extended key usage excludes"))
        );
        p.roots_pem = vec![pem("eku-root-server.pem")];
        assert!(!verify("eku-issuer-code.pem", &p).accepted());
    }
}
