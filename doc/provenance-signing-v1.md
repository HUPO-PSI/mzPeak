# Experimental detached-signature profile v1

Python `mzpeak.signing` and Rust `provenance::signing` implement this prototype
profile. It requires review in the mzPeak specification repository before it
becomes a standard. Existing Ed25519/CMS provenance records remain a separate
legacy format. The new envelope is **not CMS** and makes no RFC 9882 compatibility
claim: the OpenSSL 3.5 backend supports native ML-DSA signing but does not provide
the required CMS ML-DSA implementation.

## Algorithms and backend

The two explicit algorithms are:

| Identifier | Primitive | Signature encoding |
| --- | --- | --- |
| `ecdsa-p256-sha256` | ECDSA with named curve P-256 and SHA-256 | ASN.1 DER sequence of integers |
| `ml-dsa-65` | FIPS 204 pure ML-DSA-65, empty context | 3309 raw bytes |

ML-DSA is not HashML-DSA or the older Dilithium algorithm. X.509 public keys must
use the corresponding standardized OIDs and parameters: EC named-curve P-256,
or ML-DSA-65 with absent parameters. Unknown algorithms, other curves, mislabeled
certificates and noncanonical DER certificates are rejected.

`OpenSslBackend` invokes a configured OpenSSL executable using argument vectors,
without a shell. ECDSA works with OpenSSL 3; ML-DSA requires OpenSSL 3.5 or newer.
Install a maintained security patch release. This optional prototype backend
does not download software, keys, certificates, CRLs or algorithm providers.
Private file keys remain at the caller's chosen path; encrypted keys accept a
passphrase file. PKCS#11 keys are referenced by URI and are never exported.
Temporary signing/verification workspaces use private temporary directories.
The executable and provider configuration are trusted administrator inputs and
must never be taken from an archive. Backend failures return errors without
falling back to another algorithm or echoing provider diagnostics containing
key references or secrets.

## Signed bytes and envelope

Sign the canonical bytes of the complete manifest or stage, not a reserialized
selection of its fields. Every signature covers the same transcript:

1. ASCII `mzPeak-signature-v1` followed by one zero byte.
2. One policy byte: `1` ECDSA, `2` PQC, or `3` both.
3. A big-endian unsigned 32-bit signer count, one or two.
4. For each signer, in ascending algorithm-name order: its ASCII name's
   big-endian unsigned 32-bit byte length, the name, and the 32-byte SHA-256
   digest of its leaf certificate in DER.
5. The manifest's big-endian unsigned 64-bit byte length, then its exact bytes.

Duplicate algorithms are rejected. The manifest limit is 64 MiB. Each signer
has exactly one PEM leaf certificate (at most 64 KiB), up to eight intermediate
certificates, and a lowercase hexadecimal signature. The JSON envelope has
exactly `version`, `policy`, and `signatures`; a signature has exactly
`algorithm`, `certificate_pem`, `intermediates_pem`, and `signature_hex`.
Unknown and duplicate JSON fields are rejected. The version is integer `1`.

The transcript binds the complete signer set, leaf identities, algorithms and
declared policy. An attacker cannot strip the PQC signature and relabel an
existing `both` envelope as `ecdsa`: the surviving signature no longer verifies.
The relying party independently supplies a minimum `ecdsa`, `pqc`, or `both`
policy. All included signatures must verify; a bad optional signature is not
silently ignored. Signing immediately verifies each produced signature against
its supplied leaf, rejecting a key/certificate mismatch.

`SignatureEnvelope.canonical_bytes()` returns compact ASCII JSON with object
keys sorted recursively, including actual signature values and all certificates.
Use these bytes for RFC 3161 timestamp requests. They are distinct from the
binary signature transcript. The shared public-only fixture in
`test/data/provenance/signing/` tests transcript bytes, canonical envelope bytes,
and both signature algorithms across Rust and Python.

## Signature validity is one part of certification

`verify()` returns `CryptographicSigner` evidence, not a trusted identity. Pass
each returned leaf and intermediate list to the trust validator using externally
configured roots, identity/role grants, validation time and revocation evidence.
Also verify content coverage, lineage and software publisher approval. Embedded
certificates never become trust anchors automatically.

`both` requires valid classical and PQC data signatures. It does not by itself
establish quantum-resistant PKI: an ML-DSA leaf issued under a classical CA can
still be forged by a sufficiently capable quantum attacker. The initial
`TrustPolicy.require_pqc_chain` option is a strict **PQC-only** profile checking
the leaf and every supplied intermediate/anchor for ML-DSA keys and signatures.
It requires a separate PQC trust configuration and cannot be applied unchanged
to the classical signer in a `both` envelope. A combined production policy with
separately validated classical and PQC certification paths needs further
specification; this prototype does not claim to implement a hybrid X.509 path.

## Nitrokey HSM 2 demo configuration

Use the open-source [OpenSSL PKCS#11 provider](https://github.com/openssl-projects/pkcs11-provider)
and [OpenSC](https://github.com/OpenSC/OpenSC). Provider configuration follows
the upstream [configuration guide](https://github.com/openssl-projects/pkcs11-provider/blob/main/docs/provider-pkcs11.7.md).
Paths depend on the Linux distribution; example:

```ini
openssl_conf = openssl_init

[openssl_init]
providers = provider_sect

[provider_sect]
default = default_sect
pkcs11 = pkcs11_sect

[default_sect]
activate = 1

[pkcs11_sect]
module = /usr/lib/x86_64-linux-gnu/ossl-modules/pkcs11.so
pkcs11-module-path = /usr/lib/x86_64-linux-gnu/opensc-pkcs11.so
pkcs11-module-token-pin = file:/etc/mzpeak/signing-pin
activate = 1
```

Protect the configuration and PIN file using the CA/signing service account's
filesystem permissions; the PIN file contains the PIN on its first line.
Do not put a PIN in the key URI or command line. The APIs reject `pin-value`
URI parameters. Obtain the key's actual token/ID/label from the provisioned
device, then configure `Pkcs11Key` in Python or `KeyProvider::Pkcs11` in Rust:

```python
identity = SigningIdentity(
    Algorithm.ECDSA_P256_SHA256,
    Pkcs11Key("pkcs11:token=mzpeak-demo;object=signing-key;type=private",
              Path("/etc/mzpeak/openssl.cnf")),
    Path("instrument-or-software-leaf.pem").read_text(),
    (Path("issuing-ca.pem").read_text(),),
)
envelope = OpenSslBackend().sign(stage.canonical_bytes(), [identity], SignaturePolicy.ECDSA)
```

This configures an already provisioned key; it does not initialize or reset a
token. The Nitrokey HSM 2 profile uses ECDSA; ML-DSA currently uses a software
key. If a provider exports explicit EC parameters, normalize its **public** key
to named-curve P-256 before certificate issuance. Never export the private key
to perform that conversion.

## Repeatable qualification

`python/scripts/qualify_soft_hsm.py` initializes an isolated, temporary SoftHSM
token after verifying the selected module identifies itself as SoftHSM. It
generates a non-extractable P-256 key, creates a test certificate and runs the
Python tests and, with `--run-rust`, Rust tests through that provider. It does
not initialize physical tokens or change system configuration.

The tested Linux combination is OpenSSL 3.5.5, pkcs11-provider 1.2.0 and SoftHSM2
2.6.1. That combination needed `pkcs11-module-cache-keys = false` and
`pkcs11-module-quirks = no-deinit` to avoid a provider shutdown fault; the script
sets them only in its temporary test configuration. Qualify the actual
Nitrokey/OpenSC combination independently rather than copying workarounds
without testing. Physical Nitrokey HSM 2 operation has not been tested here.

The environment variables `MZPEAK_TEST_PKCS11_CONFIG`, `MZPEAK_TEST_PKCS11_URI`
and `MZPEAK_TEST_PKCS11_CERTIFICATE` enable the provider tests against an existing
test key. `MZPEAK_REQUIRE_OPENSSL_PQC=1` makes a missing OpenSSL/ML-DSA backend a
test failure instead of an optional-backend skip.
