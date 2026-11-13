# PQC certificate-path qualification fixtures

These public-only test certificates and CRLs were generated with OpenSSL 3.5.9.
No private keys are retained. `validation.json` records the fixed validation time:
2030-01-01 00:00:00 UTC (Unix second 1893456000). All certificates are valid from
2029-01-01 through 2031-01-01; CRLs are current from 2029-12-31 through 2030-01-02.

The positive chain is `root.pem` → `issuer.pem` → `leaf.pem`. Every key and
certificate signature uses pure ML-DSA-65, as do both `root.crl.pem` and
`issuer.crl.pem` signatures. The leaf has digitalSignature key usage and is not
a CA. The tests supply an explicit local acquisition-role and identity grant.
Rust and Python must accept this complete path with `require_pqc_chain=true`,
and must report missing CRLs as unknown rather than accepted.

Two additional chains are cryptographically valid and accepted under the ordinary
policy, but must be rejected by the strict PQC-only policy:

- `classical-root.pem` → `mixed-issuer.pem` → `mixed-leaf.pem`: an ECDSA root
  certifies an ML-DSA issuer, which certifies an ML-DSA leaf.
- `root.pem` → `classical-issuer.pem` → `classical-issuer-leaf.pem`: an ML-DSA
  root certifies an ECDSA issuer, which certifies an ML-DSA leaf.

Each issuer has a matching signed CRL named `<issuer-name>.crl.pem`. These negative
cases test a valid classical link, rather than merely swapping in an unrelated
certificate that would already fail ordinary path validation.
