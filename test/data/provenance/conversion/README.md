# Public vendor-conversion interoperability fixture

TEST ONLY synthetic bytes and public credentials; no private keys are retained.
`source.json` identifies a two-file RAW directory represented by `raw-files.json`
(values are hex). `files.json` similarly contains the exact synthetic mzPeak
archive members, including the covered validation report and converter parameters.
These payloads test provenance and are not a real vendor RAW format or MS dataset.

`stage.json` is a version-2 conversion root. `records.json` includes that stage
and a version-1 processing child. Separate publisher approvals bind the converter
and processor releases. Vendor, installation and publisher use distinct test
certificates beneath one issuer; the TSA has an independent root. Every stage
and both release approvals have separate real RFC 3161 responses over their full
CMS envelopes. All keys were generated inside a temporary directory and discarded.

Current verification is fixed at 2027-01-01. Historical verification is fixed at
2030-01-01, after signing/TSA leaf expiry, and uses authenticated 2026 token times.
Certificate and CRL validity windows are stated in `metadata.json`; the claimed
RAW acquisition time of 2000-01-01 never determines certificate eligibility.

The fixture policy permits vendor-1 / vendor.raw / vendor-integrity-v1 /
Vendor Verifier 1.0. No source path is configured by default, so independent RAW
integrity is unknown while the explicitly allowed vendor-attestation route passes.
Tests can materialize `raw-files.json` and supply its directory independently.
Roots, grants, policies and expected heads are explicit external test inputs;
carrying these files in an archive must never automatically establish trust.
