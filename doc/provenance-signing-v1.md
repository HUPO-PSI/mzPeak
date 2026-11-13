# Experimental CMS signature profile v2

Python `mzpeak.signing` and Rust `provenance::signing` use standard CMS
SignedData (RFC 5652), ECDSA (RFC 5753) and ML-DSA (RFC 9882). The mzPeak
content and acceptance rules remain experimental and require specification review.
Existing Ed25519/CMS archive records remain supported by the separate legacy
verifier. The superseded experimental raw-signature prototype is replaced, not silently
accepted as CMS: its version-1 envelopes must be re-signed and re-timestamped.

## Algorithms and backend

The two explicit algorithms are:

| Identifier | Primitive | CMS content digest |
| --- | --- | --- |
| `ecdsa-p256-sha256` | ECDSA with named curve P-256 and SHA-256 | SHA-256 |
| `ml-dsa-65` | FIPS 204 pure ML-DSA-65, empty context | SHA-512 |

CMS hashes the content into a signed `messageDigest` attribute; ML-DSA signs
the DER signed attributes using pure mode, as specified in RFC 9882. This is
not HashML-DSA or the older Dilithium algorithm. X.509 public keys must
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

Each OpenSSL child has a 30-second timeout by default. Configure Python
`OpenSslBackend(timeout_seconds=...)` or Rust `OpenSslBackend { timeout:
Duration::from_secs(...), ..Default::default() }` for the local provider's needs.
The timeout applies to each OpenSSL invocation, including the immediate verification
after signing, rather than to the entire multi-signer call. On expiry the backend
terminates and reaps the child and returns a redacted error. Verification discards
OpenSSL's recovered content instead of writing a second payload copy; content,
signed-attribute and signature checks remain enabled.

## Signed bytes and envelope

Sign the canonical bytes of the complete manifest or stage, not a reserialized
selection of its fields. Every CMS object authenticates the same detached content:

1. ASCII `mzPeak-cms-signature-v2` followed by one zero byte.
2. One policy byte: `1` ECDSA, `2` PQC, or `3` both.
3. A big-endian unsigned 32-bit signer count (nonzero).
4. For each signer, in ascending `(algorithm name, leaf-certificate SHA-256)`
   order: its ASCII algorithm name's big-endian unsigned 32-bit byte length,
   the name, and the 32-byte SHA-256 digest of its leaf certificate in DER.
5. The manifest's big-endian unsigned 64-bit byte length, then its exact bytes.

Multiple signers may use the same algorithm. Duplicate signer descriptors
(the same algorithm and leaf-certificate digest) are rejected. There is no fixed
manifest byte limit; the transcript encodes its length as an unsigned 64-bit integer.
Each signer has exactly one PEM leaf certificate (at most 64 KiB), up to eight intermediate
certificates, and a lowercase hexadecimal DER CMS `ContentInfo` in `cms_der_hex`. The JSON envelope has
exactly `version`, `policy`, and `signatures`; a signature has exactly
`algorithm`, `certificate_pem`, `intermediates_pem`, and `cms_der_hex`.
Unknown and duplicate JSON fields are rejected. The envelope version is integer `2` (the manifest and bundle versions remain `1`).

Each descriptor carries one complete, detached CMS `SignedData` object with
exactly one `SignerInfo`. A `both` record contains at least one ECDSA and one
ML-DSA CMS proof, and may contain more signers. All CMS objects authenticate
the same content. This supports different digests and key providers without inventing CMS attributes or a composite algorithm.
Generic CMS tools can verify each object; mzPeak additionally enforces the
complete signer set, policy, content coverage, lineage and certificate eligibility.

The CMS profile requires canonical DER, `id-data`, SignedData/SignerInfo version 1,
issuer-and-serial signer identification, and signed `contentType` and
`messageDigest` attributes. Optional `signingTime` is only a signer assertion,
never a trusted timestamp. Duplicate/multivalued/unknown attributes, unsigned
attributes, attached content, extra signers and embedded CRLs are rejected.
Revocation and timestamp evidence uses the separately specified evidence bundle.
The certificate set must exactly match the descriptor's leaf and intermediates,
without duplicates. Signature algorithm parameters must be absent. SHA-256/SHA-512
digest parameters may be absent or NULL. Other algorithms are rejected. Each
CMS object is bounded to 1 MiB before decoding.

The verifier checks the CMS structure and certificate binding, then invokes
`openssl cms -verify -binary` with `-nointern` and the exact bound leaf.
`-noverify` disables only CMS certificate-path verification: the separate mzPeak
trust validator handles roots, roles, time and revocation. Content, signed
attributes and signature verification remain enabled. No embedded certificate
is automatically trusted.

The transcript binds the complete signer set, leaf identities, algorithms and
declared policy. An attacker cannot strip the PQC signature and relabel an
existing `both` envelope as `ecdsa`: the surviving signature no longer verifies.
The relying party independently supplies a minimum `ecdsa`, `pqc`, or `both`
policy. All included signatures must verify; a bad optional signature is not
silently ignored. Signing immediately verifies each produced signature against
its supplied leaf, rejecting a key/certificate mismatch.

`SignatureEnvelope.canonical_bytes()` returns compact ASCII JSON with object
keys sorted recursively, including complete CMS DER objects and all certificates.
Use these bytes for RFC 3161 timestamp requests. They are distinct from the
detached CMS content. Timestamp evidence must be regenerated when an envelope changes. The shared public-only fixture in
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


Signer count is not an approval quorum: `ecdsa`, `pqc` and `both` specify required
algorithm coverage, and every included signature must verify. The existing trust
layer still checks each certificate against the record's externally configured
identity and role. Multiple certificates do not by themselves establish independent
approvers; multi-party role/threshold policy is a separate feature.

Manifest/stage inputs and CLI metadata files have no fixed 64 MiB ceiling.
Current implementations materialize JSON and signing content in memory, so available
memory remains a practical limit. Services handling untrusted inputs must enforce
workload-appropriate resource budgets before loading them. Optional embedded-bundle
byte budgets are described in `provenance-embedded-v1.md`.
