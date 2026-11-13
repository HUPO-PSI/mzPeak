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
fields and unsupported versions are errors. There is no fixed whole-bundle byte limit; individual evidence bundles retain
their 16 MiB limit. Python read/write APIs accept optional `max_bytes`; Rust offers
`read_embedded_provenance_with_limit` and `embed_provenance_with_limit`. These
caller-selected uncompressed byte budgets reject oversized ZIP members before
reading them, and writers check the resulting bundle before replacing the archive.
They are deployment resource controls, not format validity rules. Default APIs
and CLIs have no fixed aggregate ceiling and materialize the bundle in memory;
untrusted-input services should select a budget appropriate to their workload.

Stage, release and evidence formats are unchanged. Signatures use the version-2
CMS envelope specified in `provenance-signing-v1.md`; the bundle remains version 1.
Complete detached CMS objects and certificate chains live in the signature envelopes; archived certificates, CRLs
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
Failures before replacement preserve the original archive. Updates require
exclusive writer access and enough free space for a second archive. Both writers
read and snapshot the same open source file, then compare file identity, size and
modification time before replacement. Rust uses device/inode IDs on Unix and
128-bit file IDs plus volume IDs on Windows; inability to obtain an identity is
an error. These checks detect replacement even when size and timestamp match,
but do not provide a writer lock. Writing through a symlink is refused.

Writers synchronize the completed temporary file (including its permissions)
before replacement. On POSIX/Unix they also synchronize the parent directory
after replacement. A directory-sync failure explicitly reports that replacement
already happened, with uncertain crash durability; callers must not interpret it
as a rollback. Windows has no portable directory-fsync equivalent in these APIs,
so the durability of the final name binding there remains OS/filesystem-dependent.

The CLI's `verify archive.mzpeak` discovers the embedded member automatically.
Missing, ambiguous or malformed embedded proof is an error. There is
no implicit fallback to adjacent files or to an integrity-only success. An
explicit records-file argument selects the older detached workflow instead.
Only the exact proof names `provenance/bundle-v1.json` and `provenance.parquet`
are excluded from measurement coverage. Unknown files elsewhere in the reserved
provenance namespace and case/directory aliases are rejected, rather than silently
left unverified. Readers cannot assign proof storage a measurement role.

Ordinary verification validates signatures, complete measurement coverage,
lineage and current certificate eligibility. Historical verification additionally
requires an independently selected TSA policy and the policy identifier/bytes
bound by archived evidence. The CLI samples the wall clock once per verification;
saved `validation_time` fields in policy files do not choose the time. Supplying
`--validation-time UNIX_SECONDS` explicitly selects a fixed-time check, reported
as `fixed-time` instead of `current`. Historical mode remains `historical` and
reports its selected reference time. That reference time rejects future tokens;
certificate/CRL checks for historical evidence still use authenticated token
times. Merely carrying a timestamp does not establish historical validity.
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
