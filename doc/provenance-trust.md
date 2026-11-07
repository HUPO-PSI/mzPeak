# Experimental certificate trust policy

The legacy `SegmentSignature.verify_against` verifies mathematical signatures
only. The new `provenance::trust` / `mzpeak.trust` module performs a separate
certificate eligibility check. Neither primitive by itself certifies an archive.

Configure trust anchors outside the archive. An embedded root never adds trust.
The reference backend uses OpenSSL `verify` with strict X.509 checks, explicit
anchors, an explicit validation time, disabled default CA directories/stores,
and full-chain CRLs. It never fetches certificate-supplied URLs. Issuers require
CA constraints; a signing leaf requires `CA:FALSE` and `digitalSignature` without
`keyCertSign`. No private keys enter the trust verifier.

A `TrustPolicy` contains public root certificates, a Unix validation time, CRLs,
and explicit `Authorization` grants. Each grant pins a DER certificate SHA-256,
role (`acquisition`, `processing`, or `software_release`), expected instrument /
installation / publisher identity, and optionally a required EKU OID. Software
release approval requires codeSigning by default. These local grants are a
conservative demo mechanism, not a proposed assignment of mzPeak certificate
OIDs or an automated manufacturer enrollment service. Certificate renewal needs
an explicit grant update. Establish interoperable role/identity profiles with
the standard before removing these pins.

`CertificateReport` separates path validation, authorization and revocation.
Only three valid results produce `accepted`; absent CRLs remain unknown. Bad,
stale, incomplete or revoked-chain CRL evidence cannot qualify. There is no OCSP
client in this first profile; export issuer CRLs into the evidence bundle. An
explicit validation time alone is not evidence of when a signature existed;
use the timestamp layer for historical validation.

A missing role grant or a grant-specific EKU restriction invalidates
authorization without suppressing the independent path and CRL checks. Thus a
valid, unrevoked certificate can still be unauthorized for the requested role;
its report preserves all three results and remains unaccepted.

`require_pqc_chain` conservatively requires every supplied certificate public key
and certificate signature, including configured roots, to use standardized
ML-DSA. This is the strict PQC-only identity profile. Supply a separate PQC root
bundle for this check. The default ECDSA or dual-signature demo does not imply
PQC certificate-chain assurance merely because an ML-DSA data signature passes.

This code supports a regulatory evidence architecture. It does not by itself
establish Part 11 / GMP / GLP / anti-doping compliance, human electronic signature
meaning, validated instrument acquisition, trusted software execution, or a
qualified operational CA. Policy, access control, procedures and system
validation remain necessary outside the file format.

References: [OpenSSL path validation](https://docs.openssl.org/3.5/man1/openssl-verify/),
[RFC 5280](https://www.rfc-editor.org/rfc/rfc5280),
[ML-DSA certificates, RFC 9881](https://www.rfc-editor.org/rfc/rfc9881.html).
