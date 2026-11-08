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
    /// Strict PQC-only profile: validate using only ML-DSA keys/signatures.
    /// Unused classical candidates do not constrain the selected path;
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

fn pq_certificate(cert: &Certificate) -> bool {
    pq(&cert
        .tbs_certificate
        .subject_public_key_info
        .algorithm
        .oid
        .to_string())
        && pq(&cert.signature_algorithm.oid.to_string())
}
fn permits_eku(cert: &Certificate, required: &str) -> bool {
    match cert.tbs_certificate.get::<ExtendedKeyUsage>() {
        Ok(None) => true,
        Ok(Some((_, eku))) => eku
            .0
            .iter()
            .any(|oid| oid.to_string() == required || oid.to_string() == "2.5.29.37.0"),
        Err(_) => false,
    }
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
    let inspect = (|| -> Result<_, String> {
        let leaf = certificate(leaf_pem)?;
        result.certificate_sha256 = certificate_fingerprint(leaf_pem)?;
        if policy.roots_pem.is_empty() {
            return Err("no externally configured trust anchors".into());
        }
        let mut roots = policy
            .roots_pem
            .iter()
            .map(|p| Ok((certificate(p)?, p.as_str())))
            .collect::<Result<Vec<_>, String>>()?;
        let mut chain = intermediates_pem
            .iter()
            .map(|p| Ok((certificate(p)?, p.as_str())))
            .collect::<Result<Vec<_>, String>>()?;
        for (root, _) in &roots {
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
        if policy.require_pqc_chain && !pq_certificate(&leaf) {
            return Err("PQC identity policy rejects a classical certificate path".into());
        }
        roots.retain(|(cert, _)| !policy.require_pqc_chain || pq_certificate(cert));
        chain.retain(|(cert, _)| !policy.require_pqc_chain || pq_certificate(cert));
        if roots.is_empty() {
            return Err("PQC identity policy rejects all configured anchors".into());
        }
        // Application authorization is independent of path and revocation.
        // A denied grant or EKU must not prevent those checks from running.
        let authorize = (|| -> Result<Option<String>, String> {
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
                return Err("no unique locally configured role and identity grant".into());
            }
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
            }
            Ok(required.map(str::to_owned))
        })();
        let required = match authorize {
            Ok(required) => {
                result.authorization = CheckStatus::Valid;
                required
            }
            Err(error) => {
                result.details.push(error);
                None
            }
        };
        Ok((roots, chain, required))
    })();
    let (roots, chain, required) = match inspect {
        Ok(parsed) => parsed,
        Err(error) => {
            result.details.push(error);
            return Ok(result);
        }
    };
    let directory = tempfile::tempdir()?;
    let leaf_path = directory.path().join("leaf.pem");
    let roots_path = directory.path().join("roots.pem");
    let chain_path = directory.path().join("chain.pem");
    let crls_path = directory.path().join("crls.pem");
    fs::write(&leaf_path, leaf_pem)?;
    if !policy.crls_pem.is_empty() {
        fs::write(&crls_path, policy.crls_pem.join("\n"))?;
    }
    let check_path = |selected_roots: &[(Certificate, &str)],
                      selected_chain: &[(Certificate, &str)],
                      revocation: bool|
     -> Result<Option<std::process::Output>, TrustError> {
        if selected_roots.is_empty() {
            return Ok(None);
        }
        fs::write(
            &roots_path,
            selected_roots
                .iter()
                .map(|(_, pem)| *pem)
                .collect::<Vec<_>>()
                .join("\n"),
        )?;
        let mut command = Command::new(openssl.as_ref());
        command
            .args(["verify", "-no-CApath", "-no-CAstore", "-CAfile"])
            .arg(&roots_path)
            .args([
                "-x509_strict",
                "-check_ss_sig",
                "-purpose",
                "any",
                "-attime",
            ])
            .arg(policy.validation_time.to_string());
        if !selected_chain.is_empty() {
            fs::write(
                &chain_path,
                selected_chain
                    .iter()
                    .map(|(_, pem)| *pem)
                    .collect::<Vec<_>>()
                    .join("\n"),
            )?;
            command.arg("-untrusted").arg(&chain_path);
        }
        if revocation {
            command
                .arg("-CRLfile")
                .arg(&crls_path)
                .arg("-crl_check_all");
        }
        Ok(Some(super::openssl_process::run(command.arg(&leaf_path))?))
    };
    let eligible_roots = roots
        .iter()
        .filter(|(cert, _)| required.as_ref().is_none_or(|oid| permits_eku(cert, oid)))
        .cloned()
        .collect::<Vec<_>>();
    let eligible_chain = chain
        .iter()
        .filter(|(cert, _)| required.as_ref().is_none_or(|oid| permits_eku(cert, oid)))
        .cloned()
        .collect::<Vec<_>>();
    let (mut selected_roots, mut selected_chain) = (&eligible_roots, &eligible_chain);
    let mut checked = check_path(selected_roots, selected_chain, false)?;
    if checked
        .as_ref()
        .is_none_or(|output| !output.status.success())
    {
        if eligible_roots.len() != roots.len() || eligible_chain.len() != chain.len() {
            // Diagnostic fallback only: an unauthorized path cannot lend its
            // valid revocation result to a different, authorized path.
            result.authorization = CheckStatus::Invalid;
            result
                .details
                .push("issuer extended key usage excludes locally required purpose".into());
            selected_roots = &roots;
            selected_chain = &chain;
            checked = check_path(selected_roots, selected_chain, false)?;
        }
        if checked
            .as_ref()
            .is_none_or(|output| !output.status.success())
        {
            if policy.require_pqc_chain {
                result.details.push("PQC identity policy rejects paths using classical certificates; no eligible path validates".into());
            }
            if let Some(checked) = checked {
                result.details.push(
                    String::from_utf8_lossy(&checked.stderr)
                        .chars()
                        .take(4096)
                        .collect(),
                );
            }
            return Ok(result);
        }
    }
    result.path = CheckStatus::Valid;
    if policy.crls_pem.is_empty() {
        result
            .details
            .push("revocation evidence not supplied".into());
        return Ok(result);
    }
    // Check CRLs with exactly the same eligible pool as the accepted path.
    let checked = check_path(selected_roots, selected_chain, true)?
        .expect("a validated path has a nonempty trust-anchor pool");
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
        let denied = check(
            "leaf.pem",
            &policy("leaf.pem", SignerRole::SoftwareRelease),
            SignerRole::SoftwareRelease,
        );
        assert_eq!(denied.path, CheckStatus::Valid);
        assert_eq!(denied.authorization, CheckStatus::Invalid);
        assert_eq!(denied.revocation, CheckStatus::Valid);
        assert!(!denied.accepted());
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
    fn wrong_or_malformed_required_eku_only_invalidates_authorization() {
        for required in ["1.3.6.1.5.5.7.3.1", "not-an-oid"] {
            let mut p = policy("release.pem", SignerRole::SoftwareRelease);
            p.authorizations[0].required_eku = Some(required.into());
            let denied = check("release.pem", &p, SignerRole::SoftwareRelease);
            assert_eq!(denied.path, CheckStatus::Valid, "{:?}", denied.details);
            assert_eq!(denied.authorization, CheckStatus::Invalid);
            assert_eq!(denied.revocation, CheckStatus::Valid);
            assert!(!denied.accepted());
        }
    }
    #[test]
    fn denied_authorization_preserves_independent_revocation_results() {
        let mut missing = policy("leaf.pem", SignerRole::SoftwareRelease);
        missing.crls_pem.clear();
        let mut stale = policy("leaf.pem", SignerRole::SoftwareRelease);
        stale.validation_time += 172800;
        for (leaf, p, revocation) in [
            ("leaf.pem", missing, CheckStatus::Unknown),
            ("leaf.pem", stale, CheckStatus::Invalid),
            (
                "revoked.pem",
                policy("revoked.pem", SignerRole::SoftwareRelease),
                CheckStatus::Invalid,
            ),
        ] {
            let denied = check(leaf, &p, SignerRole::SoftwareRelease);
            assert_eq!(denied.path, CheckStatus::Valid, "{:?}", denied.details);
            assert_eq!(denied.authorization, CheckStatus::Invalid);
            assert_eq!(denied.revocation, revocation);
            assert!(!denied.accepted());
        }
    }
    #[test]
    fn denied_authorization_does_not_hide_an_invalid_path() {
        let mut p = policy("leaf.pem", SignerRole::SoftwareRelease);
        p.roots_pem = vec![pem("other_root.pem")];
        let denied = check("leaf.pem", &p, SignerRole::SoftwareRelease);
        assert_eq!(denied.path, CheckStatus::Invalid);
        assert_eq!(denied.authorization, CheckStatus::Invalid);
        assert_eq!(denied.revocation, CheckStatus::Unknown);
        assert!(!denied.accepted());
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
        assert_eq!(denied.path, CheckStatus::Valid);
        assert_eq!(denied.authorization, CheckStatus::Invalid);
        assert_eq!(denied.revocation, CheckStatus::Valid);
        assert!(!denied.accepted());
        assert!(
            denied
                .details
                .iter()
                .any(|detail| detail.contains("issuer extended key usage excludes"))
        );
        p.roots_pem = vec![pem("eku-root-server.pem")];
        let denied = verify("eku-issuer-code.pem", &p);
        assert_eq!(denied.path, CheckStatus::Valid);
        assert_eq!(denied.authorization, CheckStatus::Invalid);
        assert_eq!(denied.revocation, CheckStatus::Valid);
        assert!(!denied.accepted());
    }

    #[test]
    fn unrelated_restrictive_candidates_do_not_deny_eligible_path() {
        for reverse in [false, true] {
            let mut roots = vec![pem("eku-root-server.pem"), pem("eku-root.pem")];
            let mut issuers = vec![pem("eku-issuer-server.pem"), pem("eku-issuer-code.pem")];
            if reverse {
                roots.reverse();
                issuers.reverse();
            }
            let configured = TrustPolicy {
                roots_pem: roots,
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
            let report = verify_certificate(
                &pem("eku-leaf.pem"),
                &issuers,
                &configured,
                SignerRole::SoftwareRelease,
                "publisher-1",
            )
            .unwrap();
            assert!(report.accepted(), "{report:?}");
        }
    }

    #[test]
    fn revoked_eligible_path_cannot_borrow_unauthorized_path_revocation() {
        let public = |name: &str| pem(&format!("path-selection/{name}.pem"));
        let configured = TrustPolicy {
            roots_pem: vec![public("root-a"), public("root-b")],
            authorizations: vec![Authorization {
                certificate_sha256: certificate_fingerprint(&public("leaf")).unwrap(),
                role: SignerRole::SoftwareRelease,
                identity: "publisher-1".into(),
                required_eku: None,
            }],
            validation_time: 1893456000,
            crls_pem: vec![
                public("root-a.crl"),
                public("root-b.crl"),
                public("issuer.crl"),
            ],
            require_pqc_chain: false,
        };
        let report = verify_certificate(
            &public("leaf"),
            &[public("issuer-a"), public("issuer-b")],
            &configured,
            SignerRole::SoftwareRelease,
            "publisher-1",
        )
        .unwrap();
        assert_eq!(
            (report.path, report.authorization, report.revocation),
            (CheckStatus::Valid, CheckStatus::Valid, CheckStatus::Invalid)
        );
        assert!(!report.accepted());
    }

    #[test]
    fn complete_pqc_certificate_and_crl_path() {
        let available = Command::new("openssl")
            .args(["list", "-signature-algorithms"])
            .output()
            .is_ok_and(|output| {
                output.status.success()
                    && String::from_utf8_lossy(&output.stdout).contains("ML-DSA-65")
            });
        if !available {
            assert_ne!(
                std::env::var("MZPEAK_REQUIRE_OPENSSL_PQC").as_deref(),
                Ok("1"),
                "PQC qualification requires OpenSSL 3.5+ ML-DSA"
            );
            eprintln!("ML-DSA backend requires OpenSSL 3.5+");
            return;
        }
        let base = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("test/data/provenance/signing/pqc-trust");
        let pem = |name: &str| fs::read_to_string(base.join(name)).unwrap();
        let metadata: serde_json::Value =
            serde_json::from_slice(&fs::read(base.join("validation.json")).unwrap()).unwrap();
        let policy = |root: &str, issuer: &str, leaf: &str, strict: bool| TrustPolicy {
            roots_pem: vec![pem(&format!("{root}.pem"))],
            authorizations: vec![Authorization {
                certificate_sha256: certificate_fingerprint(&pem(&format!("{leaf}.pem"))).unwrap(),
                role: SignerRole::Acquisition,
                identity: "pqc-instrument-1".into(),
                required_eku: None,
            }],
            validation_time: metadata["validation_time"].as_i64().unwrap(),
            crls_pem: vec![
                pem(&format!("{root}.crl.pem")),
                pem(&format!("{issuer}.crl.pem")),
            ],
            require_pqc_chain: strict,
        };
        let check = |issuer: &str, leaf: &str, policy: &TrustPolicy| {
            verify_certificate(
                &pem(&format!("{leaf}.pem")),
                &[pem(&format!("{issuer}.pem"))],
                policy,
                SignerRole::Acquisition,
                "pqc-instrument-1",
            )
            .unwrap()
        };
        let mut strict = policy("root", "issuer", "leaf", true);
        let accepted = check("issuer", "leaf", &strict);
        assert!(accepted.accepted(), "{accepted:?}");
        assert_eq!(
            (accepted.path, accepted.authorization, accepted.revocation),
            (CheckStatus::Valid, CheckStatus::Valid, CheckStatus::Valid)
        );
        for reverse in [false, true] {
            let mut roots = vec![pem("classical-root.pem"), pem("root.pem")];
            let mut issuers = vec![pem("classical-issuer.pem"), pem("issuer.pem")];
            if reverse {
                roots.reverse();
                issuers.reverse();
            }
            let mut configured = strict.clone();
            configured.roots_pem = roots;
            let selected = verify_certificate(
                &pem("leaf.pem"),
                &issuers,
                &configured,
                SignerRole::Acquisition,
                "pqc-instrument-1",
            )
            .unwrap();
            assert!(selected.accepted(), "{selected:?}");
        }
        strict.crls_pem.clear();
        let absent = check("issuer", "leaf", &strict);
        assert!(!absent.accepted());
        assert_eq!(absent.revocation, CheckStatus::Unknown);
        for (root, issuer, leaf) in [
            ("classical-root", "mixed-issuer", "mixed-leaf"),
            ("root", "classical-issuer", "classical-issuer-leaf"),
        ] {
            let mut configured = policy(root, issuer, leaf, false);
            let ordinary = check(issuer, leaf, &configured);
            assert!(ordinary.accepted(), "{ordinary:?}");
            configured.require_pqc_chain = true;
            let rejected = check(issuer, leaf, &configured);
            assert!(!rejected.accepted());
            assert!(
                rejected
                    .details
                    .iter()
                    .any(|detail| detail.contains("PQC identity policy rejects"))
            );
        }
    }
}
