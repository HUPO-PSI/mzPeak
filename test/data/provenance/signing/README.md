# Experimental CMS signature interoperability fixture

`manifest.json` is a deliberately small byte string, not a complete archive
manifest. `envelope.json` contains version-2 ECDSA and ML-DSA-65 CMS proofs
produced by the Python implementation with OpenSSL 3.5.9 and verified by Rust.
Only public, self-signed test certificates are included; private keys were
generated transiently and discarded. These leaves are not trusted identities.

`cms-content.bin` is the common detached content: domain `mzPeak-cms-signature-v2`
plus the complete signer plan and manifest, as specified in
`doc/provenance-signing-v1.md`. `transcript.sha256` hashes those bytes;
`canonical-envelope.sha256` hashes the complete canonical JSON proof package.

`ecdsa.cms.der` and `pqc.cms.der` are standard detached CMS SignedData objects.
They can be independently verified (cryptographic integrity only) using:

```sh
openssl cms -verify -binary -inform DER -in ecdsa.cms.der -content cms-content.bin -noverify
openssl cms -verify -binary -inform DER -in pqc.cms.der -content cms-content.bin -noverify
```

`cms-invalid.json` contains mutated ECDSA CMS cases. Both reference verifiers
must reject each one, including extra/missing signers, missing/duplicated signed
attributes, wrong algorithm identifiers, mismatched certificates, attached content,
corrupt signatures, trailing bytes and malformed DER. The companion valid dual
fixture must continue to verify; rejection must not result from blanket CMS denial.

CMS verification alone does not establish mzPeak content coverage, roles,
certificate trust, revocation or a trusted signing time. Every signer authenticates
the entire signer plan so removing PQC or relabeling `both` invalidates the proof.
