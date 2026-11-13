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

Rust and Python invoke the external `openssl` executable so they share the same
path and CRL validation backend. OpenSSL must be installed and available on
`PATH`, or supplied explicitly through the verifier's executable argument.
The strict ML-DSA profile requires an ML-DSA-capable backend (OpenSSL 3.5+).

A `TrustPolicy` contains public root certificates, a Unix validation time, CRLs,
and explicit `Authorization` grants. Each grant pins a DER certificate SHA-256,
role (`acquisition`, `processing`, `software_release`, or `vendor_conversion`), expected instrument /
installation / publisher identity, and optionally a required EKU OID. Software
release approval requires codeSigning by default. These local grants are a
conservative demo mechanism, not a proposed assignment of mzPeak certificate
OIDs or an automated manufacturer enrollment service. Certificate renewal needs
an explicit grant update. Establish interoperable role/identity profiles with
the standard before removing these pins.

Vendor conversion additionally requires explicit vendor/format/method grants;
see the [conversion profile](provenance-vendor-conversion-v2.md). An ordinary
processing grant or publisher software-release grant does not authorize a signer
to attest vendor RAW verification. Claimed acquisition time is never used to
backdate certificate eligibility.

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

Required issuer/anchor EKUs constrain the path OpenSSL actually validates.
The verifier builds an eligible candidate pool rather than rejecting a signer
because an unrelated configured root or unused intermediate has a different EKU.
It passes only that pool to both path and full-chain CRL validation. If no
authorized path validates, an unrestricted path may still supply diagnostic
path/revocation results, but authorization remains invalid. A revoked authorized
path can never borrow the unrevoked result of an unauthorized alternative.

The strict post-quantum cryptography (PQC) profile, `require_pqc_chain`, requires
every certificate public key and certificate signature in the validated path,
including its trust anchor, to use standardized ML-DSA. Classical candidates are
excluded before path building; unrelated
classical roots do not invalidate an otherwise eligible path. This is the strict
PQC-only identity profile. The default ECDSA or dual-signature demo does not imply
PQC certificate-chain assurance merely because an ML-DSA data signature passes.

Certificate and timestamp OpenSSL operations have a 30-second per-child deadline.
The Rust backend terminates and reaps a stalled child, bounds diagnostic output,
and reports execution failures without including command arguments. No child
process uses a shell or performs certificate-directed network discovery.

This code supports a regulatory evidence architecture. It does not by itself
establish Part 11 / GMP / GLP / anti-doping compliance, human electronic signature
meaning, validated instrument acquisition, trusted software execution, or a
qualified operational CA. Policy, access control, procedures and system
validation remain necessary outside the file format.

References: [OpenSSL path validation](https://docs.openssl.org/3.5/man1/openssl-verify/),
[RFC 5280](https://www.rfc-editor.org/rfc/rfc5280),
[ML-DSA certificates, RFC 9881](https://www.rfc-editor.org/rfc/rfc9881.html).
