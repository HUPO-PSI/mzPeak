# Experimental provenance stage and lineage v1

This proposal extends the manifest profile in `provenance-manifest-v1.md` with
immutable processing history. Python `mzpeak.stages` and Rust
`provenance::stages` share acquisition/processing vectors in
`test/data/provenance/`. It remains subject to specification review.

Stage v1 and its canonical bytes are unchanged. The separate
[vendor-attested conversion extension](provenance-vendor-conversion-v2.md) adds
stage v2 conversion roots with external RAW inventories and explicit vendor
verification claims. Later v1 processing can reference those roots normally.

A signed stage object contains exactly `version`, `kind`, `manifest`, `parents`,
`inputs`, and `software`. Version is integer `1`. Kind is `acquisition` or
`processing`. The complete nested manifest and every lineage/software field are
included in the canonical bytes supplied to the signature implementation. The
stage identifier used by parents is SHA-256 of those canonical bytes, not merely
of the nested manifest.

The canonical encoding follows the manifest's constrained JSON encoding. Parent
digests are sorted lexically. Inputs are sorted by `(stage_sha256, path)`.
A parent digest names another complete stage. Each input contains exactly
`stage_sha256`, `path`, and `sha256`, identifying the precise covered file consumed
from that parent. Duplicate parents/inputs are invalid. A processing stage must
reference every and only its declared parents in its inputs, and every referenced
path and digest must match that parent's file inventory.

An acquisition has no parents or inputs. A processing stage requires at least one
parent and input, plus a software object containing exactly `name`, `version`,
`release_sha256`, and `parameters_sha256`. Software names and versions are
nonempty printable ASCII strings of at most 128 characters. The release digest
identifies the exact software artifact; the parameters digest identifies a stable
parameter representation chosen by that application. Applications should store
that representation as covered content, so an investigator can inspect it.
An acquisition may also record its acquisition software, but does not have to.
The `software` field is explicitly `null` when absent.

The lineage validator rejects missing parents, incorrect input references,
different run identifiers, duplicate stage identifiers, cycles, and replacement
of previously covered content at the same path. Each additional processing step
writes new outputs and interpretation snapshots, leaving acquisition and prior
stage content unchanged. The order in which stage objects are supplied does not
matter. An iterative traversal supports deep histories without recursion.

## Separate claims and checks

Lineage integrity is independent of whether any particular stage is signed,
trusted, authorized, or timestamped. A trusted signature on a processing stage
cannot retroactively certify the acquisition or its parents. The caller must
verify the signature and required role separately for each stage for which it
presents an assurance claim. A converter must not claim acquisition authority
merely because it converted an existing file.

Likewise, recording a software release digest is a claim about the software
used; it does not prove that this binary actually executed. Publisher approval
of a release, authorization of an execution signer and hardware/software
attestation are separate policies. Private signing credentials must not be
embedded in a distributed software binary to imply trusted execution.

The lineage result lists head stage digests and acquisition digests. A history
with an entirely removed final stage can still be internally valid. Callers can
provide trusted `expected_heads` from an external checkpoint (for example a lab
record, submission receipt or independent append-only ledger). Verification then
requires the exact head set, detecting removal, substitution or addition of a
head relative to that checkpoint. Without it, `checkpoint_checked` is false and
no completeness-of-history claim is warranted.

The validator describes structure only. Coverage reports and signature/trust/time
results remain separate, enabling applications to distinguish verified acquired
content, verified processing, unsigned additions, unknown trust and missing
history without one misleading archive-wide “certified” flag.
