# Experimental vendor-attested conversion stages

This extension permits a vendor to attest, after acquisition, that it checked an
identified vendor RAW dataset and converted it to the covered mzPeak outputs.
It is an explicit provenance origin, distinct from an instrument-signed acquisition.
It does not turn the conversion signer into an acquisition signer or establish
the truth of an acquisition date extracted from the RAW metadata.

The cryptographic container remains CMS envelope v2. Evidence remains v1 and the
embedded carrier remains `provenance/bundle-v1.json`: its stage records have their
own versions. Existing stage-v1 canonical bytes, signatures, timestamps and
acquisition/processing workflows remain unchanged. Older readers reject the new
stage version rather than silently assigning it acquisition or processing authority.

Rust source callers constructing `Stage` or `HistoryPolicy` with struct literals
must initialize their new optional conversion fields (`None` for an existing
workflow). This is a source API extension; the v1 wire and cryptographic byte
compatibility guarantee applies to existing records and fixtures.

## Signed source and conversion records

A source manifest has exactly `version`, `format`, and `files`. Version is `1`;
`format` is a nonempty printable ASCII format identifier of at most 128 characters.
Each source file has exactly `path`, `size`, and `sha256`. File paths are relative
to a dataset root and follow the existing portable ASCII transport-path rules;
sizes use the existing `0..=2^53-1` integer range and digests are lowercase SHA-256.
The inventory must be nonempty, with no duplicate or case-colliding paths.
Canonicalization sorts files by path and object keys as in the manifest profile.
Its SHA-256 digest identifies the file set and format together.

For a single RAW file the relative path is its basename. For a directory dataset,
enumerate all regular files recursively using `/` separators; empty directories
are not data. Capture rejects links, reparse points, special files, unsafe names
and detected input changes. Capture and conversion require stable source content;
these checks do not provide filesystem locking or an immutable snapshot.

A stage-v2 conversion has exactly the six stage-v1 keys plus `conversion`:

| Field | Requirement |
| --- | --- |
| `version` | `2` |
| `kind` | `"conversion"` |
| `manifest` | Existing v1 manifest of the mzPeak outputs, including the root index |
| `parents`, `inputs` | Empty arrays for this external-source root |
| `software` | Required existing converter name/version/release/parameters identity |
| `conversion` | Required non-null claim described below |

The conversion claim has exactly `source`, `verification`, and
`claimed_acquisition_time`. The latter is required but nullable, with a non-null
value expressed as a nonnegative whole Unix second within the canonical integer
range. It is signed descriptive metadata, never a certificate-validation time.

The vendor verification object has exactly these keys:

- `vendor`: the vendor identity expected by the recipient's local stage-identity map.
- `method`, `verifier_name`, `verifier_version`: explicit method and verifier identifiers.
- `source_sha256`: digest of the canonical source manifest; it must match `source`.
- `result`: exactly `passed`, `failed`, or `unknown`.
- `report`: required nullable object with exactly `path` and `sha256`.

The four identifier strings use the same 1-128 printable ASCII constraint. When a
report is referenced, its path and digest must match a file in this conversion
stage's own output manifest. Retain the report as ordinary covered content, for
example `validation/vendor-report.json`; arbitrary additional files in the
reserved `provenance/` namespace remain invalid. A later stage cannot retroactively
supply the report binding for an earlier conversion.

Every source, verification, report, software, output and claimed-time field is
included in the signed canonical stage. Source file order does not affect its
canonical identity. Stage v1 cannot contain `conversion`, including a null value;
stage v2 in this extension is only for conversion. Unknown keys, duplicate keys,
unknown versions and malformed digests fail explicitly.

## Trust and acceptance

The conversion certificate needs an external `vendor_conversion` role grant and
the expected vendor identity. Acquisition, processing and software-release grants
do not implicitly confer this role. The existing certificate path, key-usage,
EKU and full-chain revocation checks still apply. The signed vendor identity must
also match the recipient's configured expected stage identity.

The recipient additionally supplies an optional conversion policy:

```json
{
  "allowed_verifications": [
    {
      "vendor": "vendor-1",
      "format": "vendor.raw",
      "method": "vendor-integrity-v1",
      "verifier_name": "Vendor Verifier",
      "verifier_version": "1.0"
    }
  ],
  "require_source_bytes": false,
  "sources": {}
}
```

All five grant fields must match the signed record, and only `passed` qualifies.
Omitting conversion policy leaves conversion origins unaccepted, preserving the
existing acquisition-origin workflow. To independently check the source, supply
`sources` as a map from conversion stage ID to a local file or dataset directory.
The CLI resolves relative paths against the policy file. Source locations are
caller inputs; the verifier does not follow archive-provided URLs or run a vendor
tool while validating a received archive.

Every supplied source-map key must name a conversion stage in the supplied
history. Unknown keys and keys naming ordinary acquisition/processing stages are
configuration errors, preventing a typo from silently disabling a requested RAW
check. Unused vendor/method grants remain permitted for reusable recipient policy.

Reports distinguish three questions:

| Finding | Meaning |
| --- | --- |
| Stage signature and certificate results | Who signed, and whether that identity is authorized |
| `vendor_attestation` | Whether the vendor/method/result satisfy conversion policy |
| `source_integrity` | Whether caller-supplied original RAW bytes match the signed inventory |

An absent source is `unknown`, and can be accepted only when source bytes are not
required. A supplied source with missing, extra or changed files is invalid even
when source bytes are optional. Missing or unreadable explicitly supplied paths
must not become a successful or absent-source result. The RAW inventory is
external to mzPeak output coverage: absent original RAW files are not classified
as missing mzPeak members. Every present ordinary archive member must still be
covered by the history, including any retained verification report.

Acceptance also requires the existing independently signed publisher approval of
the converter's exact software release. Approval of that binary is distinct from
the vendor's verification/conversion attestation and does not prove execution.

The lineage result reports conversion roots separately from acquisition roots.
An ordinary v1 processing stage may consume the conversion's covered outputs by
the existing parent-stage digest and exact input-reference mechanism. Missing
internal parents, cycles, changed earlier outputs and external head-checkpoint
mismatches continue to fail. This extension does not add multi-run archives,
versioned reader views, omitted internal ancestor data or evidence renewal.

## Time evidence

Current verification uses the selected current/fixed certificate time. Historical
verification uses the authenticated time of the complete conversion envelope's
RFC 3161 token and the conversion signer role. Converter publisher approval needs
its own token as before. A claimed RAW acquisition time never backdates the
conversion certificate, substitutes for a timestamp, or authenticates the RAW
bytes before the vendor attestation. Original vendor signatures, when available,
can be retained in the covered report for the vendor's method to describe; this
generic extension does not independently adjudicate every proprietary proof format.

## Construct and sign with either CLI

These steps consume the actual verifier/converter integration's results:

```sh
# The vendor adapter verifies and converts a stable source dataset, and writes
# its validation report into the mzPeak archive as ordinary covered content.
python -m mzpeak.provenance_cli capture-source raw-dataset source.json \
  --format vendor.raw

python -m mzpeak.provenance_cli capture converted.mzpeak manifest.json \
  --run-id run-1 --stage-id conversion

# verification.json includes source_sha256 printed by capture-source;
# software.json is the existing SoftwareIdentity object for the converter.
python -m mzpeak.provenance_cli init-conversion \
  manifest.json source.json conversion-stage.json \
  --vendor-verification verification.json --software software.json

python -m mzpeak.provenance_cli sign conversion-stage.json \
  --archive converted.mzpeak --signers vendor-signers.json --policy ecdsa

# Embed the independently signed converter release approval before verification.
python -m mzpeak.provenance_cli verify converted.mzpeak \
  --trust trust.json --identities identities.json --publishers publishers.json \
  --conversion-policy conversion-policy.json --policy ecdsa
```

Rust uses the same arguments after `cargo run --example provenance --`.
`capture-source` streams the file-set hashes. `init-conversion` validates and
constructs a statement; it does not itself run proprietary vendor verification or
conversion. Existing `sign-archive` remains the acquisition shortcut and must not
be used to relabel a converted vendor RAW file as instrument-signed acquisition.

The adapter is responsible for performing its actual checks, describing their
strength honestly, and ensuring verification and conversion use the same stable
source bytes. A checksum-only method and a cryptographically authenticated RAW
method should have different method identifiers and recipient grants. The vendor
signing credential stays in its authorized signing system; a private publisher
key must not be embedded in a distributed converter binary.
