# Experimental embedded provenance v1

The default exchange form is a self-contained `.mzpeak` ZIP with the public
provenance bundle at the exact, case-sensitive member name
`provenance/bundle-v1.json`. This is part of the experimental reference profile,
not an assertion that the mzPeak specification has already adopted it.

```text
measurement.mzpeak
├── mzpeak_index.json
├── spectrum.parquet
├── ... other measurement content ...
└── provenance/
    └── bundle-v1.json
```

The bundle has exactly five JSON fields:

| Field | Content |
| --- | --- |
| `version` | Integer `1` |
| `records` | Nonempty array of existing `{stage, envelope}` signed-stage records |
| `releases` | Publisher `{statement, envelope}` approvals, keyed by release SHA-256 |
| `stage_evidence` | Existing evidence bundles, keyed by stage ID |
| `release_evidence` | Existing evidence bundles, keyed by release SHA-256 |

All fields are present; the three maps may be empty. Stage IDs are unique and all
stages belong to one run. Evidence must identify an existing signed record and
bind its canonical payload and complete envelope. Duplicate JSON keys, unknown
fields and unsupported versions are errors. The whole embedded member is limited
to 64 MiB uncompressed; individual evidence bundles retain their 16 MiB limit.

The existing stage, signature, release and evidence formats are unchanged.
Certificate chains live in the signature envelopes; archived certificates, CRLs
and RFC 3161 responses/requests live in evidence bundles. Private keys, HSM access
configuration, trusted-root selection, signer grants and local verification
policy are never inferred from this bundle. A bundle is a carrier for proof,
not a new trust anchor or a replacement for signature verification.

## Storage and verification

Python `mzpeak.embedded` and Rust `provenance::embedded` expose
`ProvenanceBundle`, `embed_provenance` and `read_embedded_provenance`.
Packaging merges additional records, approvals and evidence. Exact repeats are
idempotent; a different value under an existing stage ID, release digest or
evidence key is rejected. Existing proofs cannot be silently removed or replaced.
Evidence renewal/replacement needs a separately specified profile.

Writers validate ZIP names, types and uniqueness before reading proofs. Updates
use a temporary sibling ZIP, bounded content copying and atomic replacement.
Measurement member names and decompressed bytes, including the original root
index, remain unchanged. Rust preserves compressed member bytes; Python may
recompress them. ZIP transport bytes are not the signed content identity.
CRC/read/write failures abort and preserve the original archive. Updates require
exclusive writer access and enough free space for a second archive; detected
concurrent changes abort before replacement. Writing through a symlink is refused.

The CLI's `verify archive.mzpeak` discovers the embedded member automatically.
Missing, oversized, ambiguous or malformed embedded proof is an error. There is
no implicit fallback to adjacent files or to an integrity-only success. An
explicit records-file argument selects the older detached workflow instead.
Unknown files elsewhere in the reserved provenance namespace are not measurement
content and are not accepted as proof by this profile.

Ordinary verification validates signatures, complete measurement coverage,
lineage and current certificate eligibility. Historical verification additionally
requires an independently selected TSA policy and the policy identifier/bytes
bound by archived evidence. The CLI identifies its mode as `current` or
`historical`; merely carrying a timestamp does not establish historical validity.
Publisher approvals need their own timestamp evidence, as before.

Removing proofs makes default verification fail. Detecting replacement with an
entire older valid archive still requires the recipient's external trusted head
checkpoint. The signed root index remains immutable; this change does not add
automatic reader selection of later index snapshots.

## Default commands

Both reference CLIs accept the same commands. Replace the Python prefix below
with `cargo run --example provenance --` for Rust.

```sh
# Capture, sign and embed an acquisition in one operation.
python -m mzpeak.provenance_cli sign-archive measurement.mzpeak \
  --run-id demo-run --stage-id acquisition \
  --signers instrument-signers.json --policy ecdsa

# Discover the proofs inside the ZIP; configuration is independently supplied.
python -m mzpeak.provenance_cli verify measurement.mzpeak \
  --trust trust.json --identities identities.json --policy ecdsa

# A processing stage prepared with the stage API can be signed straight into it.
python -m mzpeak.provenance_cli sign processing-stage.json \
  --archive measurement.mzpeak --signers software-signers.json --policy ecdsa

# Import public approvals and timestamp evidence into the same portable file.
python -m mzpeak.provenance_cli embed measurement.mzpeak \
  --releases release-approvals.json --stage-evidence stage-evidence.json \
  --release-evidence release-evidence.json

python -m mzpeak.provenance_cli verify measurement.mzpeak \
  --trust trust.json --identities identities.json --publishers publishers.json \
  --heads trusted-heads.json --policy ecdsa --tsa-policy tsa-policy.json \
  --evidence-policy-id organization-policy-v1 --evidence-policy organization-policy.json
```

The evidence and approval files above are import inputs; recipients need only the
resulting archive and their independent verification configuration. Existing
detached `capture`, `init-acquisition`, `sign input.json output.json`,
`sign-release input.json output.json` and `verify archive records.json` remain
available as explicit low-level workflows. `sign` and `sign-release` accept either
`--archive` or a detached output path, never both.
