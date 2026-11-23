# Baseline provenance profile for review

An instrument can sign one manifest for an acquisition and embed the result in
`provenance/bundle-v1.json`. The manifest authenticates member names, byte lengths
and digests, including the archive index. One certificate signs the entire
record using ECDSA P-256/SHA-256 or ML-DSA-65. Optional chunk digests belong in
that same manifest. See [the demo commands](provenance-demo.md#capture-and-sign-an-acquisition).

## What baseline verification checks

Select `verify --baseline` in either reference CLI. It accepts one
instrument-acquisition root followed by zero or more processing stages, with
one direct parent per processing stage and one head in the resulting history.
Every stage has exactly one signer. The recipient selects the algorithm,
expected signer identities, trust anchors and revocation data. Every signature
and role authorization must pass. The verifier rejects ambiguous ZIP members
and compares the archive inventory against the union of the signed manifests.
The caller may supply a trusted head to detect rollback.

The result identifies the `baseline` profile and reports current or explicitly
selected validation time. Publisher approval status is null. A processing
record still binds its inputs, outputs, software release and parameters, so a
recipient can verify the signer's claim without requiring publisher evidence.
The same embedded records can be evaluated with the full verifier when
publisher approvals or historical evidence are required. Existing full
verification still requires independent publisher approval for a recorded
software release. Vendor conversion uses its own role, source claim and policy.
Select full verification for conversion origins, merged histories or dual
signatures. These checks are documented in the [trust profile](provenance-trust.md)
and [evidence profile](provenance-evidence.md).

## Changes for subsequent wire revisions

The draft baseline reuses the current canonical stage and CMS envelope versions.
The following changes require separate wire definitions and shared fixtures
before either reference reader can accept them.

- Derive parent digests from signed input references when those references
  fully describe lineage. A signed predecessor may express archive history
  separately from exact input lineage. The two links answer different
  questions when processing uses an older input.
- Define signed replacement records that identify both the old descriptor
  and the replacement along with the authenticated current view. Preserve
  the old signed claim and report original bytes as unavailable when they
  have been removed. Continue to validate the old record's signature and
  authorization. Missing data without an authorized replacement remains a
  coverage failure.
- Allow the standard CMS signature timestamp attribute for a single signer in
  a new envelope revision. Its message imprint covers the signature value.
  Check the TSA independently, and keep the existing whole-envelope evidence
  format for existing records and histories requiring all signers to be
  timestamped together.
