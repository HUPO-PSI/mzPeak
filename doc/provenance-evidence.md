# Experimental timestamp and archived-evidence profile

The reference implementations support RFC 3161 timestamp requests and verification through OpenSSL. This is a demo profile, not a regulatory certification or a final mzPeak standard. The cryptographic backend and ASN.1 libraries are open source.

Timestamp the bytes returned by `SignatureEnvelope.canonical_bytes()` **after** all signatures are present. This includes the actual signature values, signer certificates, intermediate certificates, algorithm identifiers and the declared signature policy. Timestamping only the unsigned manifest would not prove that its signatures already existed. The timestamp establishes that this envelope existed by the TSA's time; it does not establish the instrument's acquisition time or the actual time a processing operation ran.

## Verification order

1. Verify the versioned manifest, signature envelope, required algorithms and content coverage with the ordinary provenance verifiers.
2. Supply a `TsaPolicy` from local configuration, with dedicated TSA root certificates, an allowed list of TSA policy OIDs and a trusted current verification time. Instrument-signing trust roots do not implicitly authorize a TSA.
3. Call `verify_timestamp` with the complete canonical envelope, the DER response and, when retained, the original DER request. The verifier checks the SHA-256 message imprint, request nonce and policy, timestamp signature, TSA certificate path and full-chain CRLs. Embedded certificates cannot become trust anchors. Missing revocation evidence yields `unknown` and no trusted time.
4. If the timestamp passes, use `verify_historical_signer` to validate the signer certificate at that verified time using archived CRLs. Local signer roots, role grants and PQC-chain requirements remain authoritative. This checks identity eligibility; signature correctness and archive coverage must still pass separately.

Both implementations parse `genTime` from ASN.1, never from command-line output. They do not return it as trusted until all checks pass. Future timestamps are rejected. TSA path validity and CRL validity are checked at `genTime`, so an expired certificate is not automatically rejected when verified historical evidence is available.

The demo profile accepts whole UTC seconds and no nonzero `accuracy` assertion. Fractional times and nonzero accuracy fail explicitly; a future interoperable profile needs an explicit uncertainty interval before using them for certificate validity boundaries. Query creation requests SHA-256 and generates a fresh nonce. When the original query is supplied, its nonce is mandatory and must match. When it is unavailable, the envelope imprint is still verified, but the verifier cannot establish correspondence to the original request session.

## Evidence bundle

`EvidenceBundle` version 1 stores SHA-256 bindings for the manifest, complete canonical envelope and independently supplied policy bytes, together with the policy identifier, optional timestamp query/response DER as lowercase hexadecimal, intermediate certificates and CRLs. The caller must compare bindings against independent inputs using `check_binding`, or call `verify_historical_signer`, which does so first.

The bundle contains evidence, not authority. Root certificates and role grants must come from local configuration. Every archived certificate and CRL is revalidated. The policy identifier/hash is compared with the locally selected policy; it does **not** prove which policy an organization historically applied. The RFC 3161 token authenticates the envelope's imprint, not every field of the bundle. An external audit record, archive seal or later timestamp over an evidence package is needed to authenticate preservation of the whole bundle. Replacing an untrusted CRL with another valid issuer-signed CRL is adjudicated by validation policy, not by the bundle's metadata.

Historical validation is deliberately conservative. It requires CRLs covering the verification time and does not infer that missing revocation evidence means “good.” It does not implement retrospective revocation/compromise adjudication, OCSP archival, evidence-record renewal, trusted clock operation or indefinite cryptographic preservation. A classical timestamp chain also does not acquire post-quantum security merely by timestamping an ML-DSA envelope.

## Python example

```python
from mzpeak.evidence import create_timestamp_query, verify_timestamp

payload = envelope.canonical_bytes()
request_der = create_timestamp_query(payload, policy_oid=authorized_tsa_policy_oid)
# Send request_der to the configured TSA using the deployment's authenticated transport.
# Retain the actual request and response; the library performs no implicit network requests.
report = verify_timestamp(payload, response_der, request_der, local_tsa_policy)
if report.accepted:
    verified_upper_bound = report.trusted_time
```

Rust exposes the same operations in `provenance::evidence` when the `verification` feature is enabled. The `*_with` variants select an explicitly configured OpenSSL executable; the Python operations accept `openssl=`. Backend execution errors propagate rather than becoming a successful or missing-evidence result.

The shared fixtures in `test/data/provenance/timestamps` contain public certificates, CRLs and a real OpenSSL-generated timestamp of the shared signature envelope. Their fixed verification time is later than the TSA and historical signer leaf expiry. Tests demonstrate successful historical validation, rejected current-time validation, imprint/signature/nonce tampering, future time, absent/stale CRLs, unauthorized TSA policies, policy/content substitution and the preservation of external role grants. No private key is retained.

Protocol references: [RFC 3161](https://www.rfc-editor.org/rfc/rfc3161.html), [OpenSSL timestamp command](https://docs.openssl.org/3.5/man1/openssl-ts/).

## Complete history verification

Use `historical.verify_historical_history` (or `verify_historical_archive_history` for streaming ZIP archives) when the acceptance decision requires historical validation. These wrappers first run the ordinary history verifier, retain its coverage, lineage and signature results, and reverify each actual signed envelope before using its timestamp to validate certificate eligibility. A timestamp can never repair a changed file, invalid stage signature, mismatched software release or missing external role grant.

The caller supplies evidence indexed by stage ID and separate evidence indexed by software release digest, plus the externally chosen policy identifier/bytes and dedicated TSA policy. Every stage requires a valid timestamp. Each publisher approval requires a timestamp of **its own complete approval envelope**; a processing-stage timestamp cannot replace it. Missing or invalid evidence leaves the relevant certificate/release decision unaccepted. The combined report includes both the history result and timestamp reports, and its `accepted` result requires all necessary checks.

```python
from mzpeak.historical import verify_historical_history

report = verify_historical_history(
    records, content_files, local_signer_trust, required_signature_policy,
    local_stage_identities,
    tsa_policy=local_tsa_policy,
    policy_id=local_policy_identifier,
    policy_bytes=local_policy_bytes,
    stage_evidence=archived_stage_evidence,
    release_evidence=archived_release_evidence,
    releases=publisher_approvals,
    publishers=local_publisher_identities,
    expected_heads=external_head_checkpoint,
)
if not report.accepted:
    raise ValueError("history did not satisfy the configured verification policy")
```

Rust exposes matching functions in `provenance::historical`, with the ordinary `HistoryPolicy` and a separate `HistoricalPolicy`. The public fixtures in `test/data/provenance/timestamps/history` exercise both implementations end to end with real signed acquisition/processing stages, an independently signed publisher approval, and three separate OpenSSL RFC 3161 responses. Both signing and TSA leaves are expired at the fixed later verification time. The archived evidence permits historical validation while current-time verification fails.
