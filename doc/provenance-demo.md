# Provenance demo CLI

These commands exercise an experimental format proposal, not an approved mzPeak
standard or a regulatory certification. The Python and Rust implementations use
the same manifests, signed-stage records, signer configuration and trust policy.
They do not change the input archive or generate signing keys. Outputs must name
new files; an existing destination is never overwritten.

Install Python with the provenance extra (`pip install 'python/[provenance]'`) or
build Rust with `cargo build --example provenance`. Use OpenSSL 3.5 or newer for
ML-DSA-65. ECDSA uses P-256/SHA-256. The PKCS#11 provider additionally needs its
OpenSSL configuration and OpenSC, configured as shown below.

The examples below use `python -m mzpeak.provenance_cli`. The equivalent Rust
invocation is `cargo run --example provenance --` followed by the same arguments.
An optional global `--openssl /trusted/path/to/openssl` precedes the subcommand.

## Capture and sign an acquisition

```sh
python -m mzpeak.provenance_cli capture small.mzpeak acquisition-manifest.json \
  --run-id demo-run --stage-id acquisition
python -m mzpeak.provenance_cli init-acquisition acquisition-manifest.json acquisition-stage.json
python -m mzpeak.provenance_cli sign acquisition-stage.json acquisition-record.json \
  --signers instrument-signers.json --policy ecdsa
```

`capture` covers every non-provenance member, including JSON metadata, the index,
and complete Parquet files. Repeat `--select path` to capture just one processing
stage's output inventory. The complete-history verifier checks the union of stage
inventories and reports any other content as uncovered.

`init-acquisition` only constructs the stage object. Trust policy must explicitly
authorize the certificate and instrument identity before verification accepts its
acquisition claim. It cannot turn an ordinary converter into a trusted instrument.

The signer configuration is a nonempty JSON array. All filesystem paths resolve
relative to that configuration file. The following selects a file-backed key:

```json
[
  {
    "algorithm": "ecdsa-p256-sha256",
    "provider": {
      "kind": "software",
      "key_path": "instrument-key.pem",
      "passphrase_file": null
    },
    "certificate_path": "instrument-cert.pem",
    "chain_paths": ["manufacturer-ca.pem", "issuing-ca.pem"]
  }
]
```

For the Nitrokey, replace the provider object with:

```json
{
  "kind": "pkcs11",
  "key_uri": "pkcs11:token=mzpeak-demo;object=instrument-key;type=private",
  "config_path": "openssl-pkcs11.cnf"
}
```

The provider configuration is administrator-controlled and points to OpenSC and a
protected PIN file. Inline `pin-value` URI parameters are rejected. The key stays
in the token. Hardware qualification still requires testing against the actual
Nitrokey/VM/OpenSC/provider combination.

For ML-DSA, use algorithm `ml-dsa-65` with an appropriate certificate and software
key. For dual signatures, put both signer entries in the array and select
`--policy both`. The signed transcript binds the full signer set and policy.
Removing one signature does not produce a valid weaker envelope.

## Add processing and release approval

Use the `Stage`, `InputReference` and `SoftwareIdentity` APIs to construct a
processing-stage JSON object as described in `provenance-stages-v1.md`. Its parents
are canonical stage digests; inputs bind exact parent output paths and digests.
Write new outputs and immutable index snapshots, preserving acquisition content.
Sign this stage with a separately authorized processing identity using `sign`.
Keep the original root `mzpeak_index.json` unchanged: `capture` covers it, and
overwriting it invalidates the earlier stage. The provenance API describes the
new outputs, but ordinary `MzPeakReader` instances do not automatically switch
to a later index snapshot. A validated latest-index projection is outside this
experimental profile; arbitrary existing writer append operations are not yet
compatible with preserving acquisition evidence.

A publisher signs a release statement containing exactly these fields:

```json
{"name":"Example processor","version":"1.2.3","release_sha256":"<64 lowercase hex characters>"}
```

```sh
python -m mzpeak.provenance_cli sign-release release-statement.json release-approval.json \
  --signers publisher-signers.json --policy ecdsa
```

A history file is an array of signed-stage objects produced by `sign`. A release
approval file supplied to verification is an object mapping each release digest
to the corresponding signed approval produced by `sign-release`. The processing
stage's software name, version and release digest must match that approval.
Approval of a binary is distinct from evidence that it actually executed.

## Verify with explicit trust

```sh
python -m mzpeak.provenance_cli verify measurement.mzpeak records.json \
  --trust trust.json --identities identities.json \
  --releases releases.json --publishers publishers.json --heads heads.json \
  --policy ecdsa
```

`trust.json` is external relying-party configuration in the `TrustPolicy` schema:

- `roots_pem`: array of trusted root certificate PEM strings.
- `authorizations`: certificate SHA-256 fingerprints with explicit `role`,
  `identity` and optional `required_eku` grants. Roles are `acquisition`,
  `processing`, or `software_release`; software release defaults to codeSigning.
- `validation_time`: nonnegative Unix seconds chosen by the relying party.
- `crls_pem`: PEM CRLs covering the full path. Missing revocation evidence remains
  unknown and is not accepted.
- `require_pqc_chain`: optional boolean requiring PQC certificate paths as well
  as the signature algorithm policy. It defaults to false.

`identities.json` maps stage IDs to expected instrument or processing identities,
for example `{"acquisition":"instrument-1","processing":"installation-1"}`.
`publishers.json` maps each software release digest to its expected publisher
identity. `heads.json` is an array of expected final stage digests obtained from a
trusted independent record. Do not derive trust configuration or expected heads
from the file being checked and then treat them as independent evidence.

The result is JSON with `accepted` and a structured `report` containing coverage,
lineage, signature, certificate authorization/revocation and release results.
Exit status is 0 for acceptance, 1 for a completed but unaccepted verification,
and 2 for malformed input or an operational error. Errors are JSON on stderr.
Missing `--heads` leaves `checkpoint_checked` false: internal consistency alone
cannot detect removal of an entire final stage. The command performs current or
explicit-time validation; RFC 3161 historical validation uses the separate
`evidence` APIs, not a caller-chosen time asserted to be a trusted timestamp.

## Reproduce the public interoperability fixture

The repository includes public certificates, signatures, CRLs and synthetic
content, with no private signing keys. This builds a temporary archive from that
fixture and writes its external expected identities:

```python
import json, zipfile
from pathlib import Path
p = Path("test/data/provenance/history")
with zipfile.ZipFile("fixture.mzpeak", "w") as archive:
    for name, data in json.loads((p / "files.json").read_text()).items():
        archive.writestr(name, bytes.fromhex(data))
Path("fixture-identities.json").write_text(json.dumps({
    "acquisition": "instrument-1", "processing": "installation-1"}))
Path("fixture-publishers.json").write_text(json.dumps({
    digest: "publisher-1" for digest in json.loads((p / "releases.json").read_text())}))
```

```sh
python -m mzpeak.provenance_cli verify fixture.mzpeak \
  test/data/provenance/history/records.json \
  --trust test/data/provenance/history/trust.json \
  --identities fixture-identities.json \
  --releases test/data/provenance/history/releases.json \
  --publishers fixture-publishers.json \
  --heads test/data/provenance/history/heads.json --policy ecdsa
```

This fixture policy uses an artificial validation time within its test certificate
and CRL validity periods. It is a reproducible test, not deployment trust policy.
Running the same arguments with Rust produces the same acceptance and report.
