# Experimental provenance manifest v1

This is an implementation proposal for review alongside the mzPeak specification,
not an assertion that the standard already requires this format. Existing archive
readers and the legacy `provenance.parquet` format remain supported separately.
The Python `mzpeak.manifest` and Rust `provenance::manifest` APIs implement the same
schema and share the vectors in `test/data/provenance/manifest-v1.*`.

A manifest binds a run identifier, a stage identifier, and the exact content
inventory for that stage. Every content file has its archive-relative path,
uncompressed byte length, and SHA-256 digest. Whole-file digests cover all bytes,
including JSON metadata and Parquet schemas, footers, indexes and compressed page
bytes. Optional chunk records bind each selected byte range to a digest and,
where applicable, the Parquet row group, column number and column path. Those
coordinates are part of the signed payload, not unsigned lookup hints.

The manifest is detached from its content. Only the exact proof-storage names
`provenance/bundle-v1.json` and legacy `provenance.parquet` are exempt from
content hashing; detached proofs may also live outside the archive. Other names
under `provenance/` are reserved and rejected. Data consumers must not interpret
proof-storage members as measurement content.

## Schema and canonical bytes

The JSON object has exactly `version`, `run_id`, `stage_id`, and `files` fields.
`version` is integer `1`. Each file has exactly `path`, `size`, `sha256`, and
`chunks`. Each chunk has exactly `offset`, `length`, `sha256`, `row_group`,
`column`, and `column_path`. All three Parquet coordinate fields are either
present values or explicit JSON `null`. Missing fields, duplicate object keys,
unknown fields, unsupported versions, floating point values and non-finite values
are rejected. There is no extension map whose contents could be silently ignored.

Canonical bytes are UTF-8 JSON with no whitespace, object keys in lexical order,
files sorted by path, and chunks sorted by `(offset, length)`. Standard JSON string
escaping is used. All allowed strings in this schema are printable ASCII, so
Unicode normalization and alternate Unicode escaping are not involved. Integers
are written in decimal without leading zeroes and are restricted to the range
`0..=9007199254740991` (`2^53-1`). Hashes are exactly 64 lowercase hexadecimal
characters. Hashing canonical bytes with SHA-256 gives the manifest identifier.
A verifier parses and validates before reconstructing canonical bytes; source
JSON indentation and member order carry no meaning.

Run/stage identifiers are 1–128 ASCII characters. The first is an alphanumeric,
`_`, or `-`; subsequent characters may additionally contain `.`. Paths are at most
1024 ASCII characters, use `/` separators, and contain nonempty components made
of alphanumerics, `_`, `-`, `.`, and internal spaces. Components cannot start with
a space, end with a space or dot, or equal `.` or `..`. Absolute paths, drive
prefixes, backslashes, control characters, duplicate paths and case-colliding
paths are rejected. Parquet column paths are nonempty printable ASCII strings.

Chunk ranges must lie inside the declared file. Duplicate or overlapping ranges
and repeated `(row_group, column)` coordinates within a file are rejected. An
explicit length of zero means zero bytes. Hashing a declared length requires
exactly that many bytes; premature end of file is an error. File helpers stream
bounded buffers rather than reading the entire measurement into memory.

## Verification scope

`verify_files` compares the caller's complete content inventory with the manifest
and separately reports `missing`, `changed` and `uncovered` paths. Its `complete`
property is true only when all three lists are empty. This is an integrity result;
it does not establish signature validity, certificate trust, role authorization,
trusted time, or regulatory compliance.

An archive adapter must enumerate every content entry, reject duplicate ZIP
member names before constructing a map, and exclude only the exact proof-storage
filenames `provenance/bundle-v1.json` and `provenance.parquet`. Other files under
the reserved `provenance/` namespace, case aliases, and directories masquerading
as either proof filename are rejected; a namespace prefix never exempts arbitrary
content from verification. File lengths and hashes refer to decompressed ZIP
member bytes, not ZIP container headers or compression choices. Changing ZIP compression alone
does not change the represented measurement content. Optional Parquet chunk
ranges refer to bytes of that decompressed member (the Parquet page bytes may
still be compressed internally).

ZIP directory markers must be empty and cannot be assigned measurement roles in
the root index. A directory entry carrying bytes is rejected, so directory syntax
cannot hide unsigned measurement data.

Only empty directory records are transport metadata. Directory entries carrying
payload bytes are rejected, including entries under the reserved namespace.
Indexed measurement references must name files, never directory entries.

`verify_chunk` intentionally checks only a selected range. A successful result
must be presented as a range check, never as whole-file or whole-archive coverage.
The whole-file check also verifies the chunk descriptors when they are present.

Stages can add immutable snapshots of interpretation metadata such as
`snapshots/<stage-id>/mzpeak_index.json`. Applications must interpret each stage
using its covered metadata. The current capture API also covers the original
root `mzpeak_index.json`: changing that file invalidates its acquisition evidence.
Processing may append new outputs and snapshots while preserving this root
index. Existing readers are not automatically redirected to a later snapshot.
When a root index exists, its exact member references are authoritative: an
unindexed member with a measurement-like suffix cannot override them. Readers
reject multiple files assigned to the same measurement role and reject indexes
that use proof storage as measurement data. Rust retains suffix discovery for
legacy archives with no index, while still rejecting ambiguous roles and ignoring
the reserved proof namespace during that discovery.
A mutable latest-index projection and certified-reader integration require a
separate specification and implementation; ordinary index-rewriting append
operations are deliberately rejected by this profile.

## ZIP adapter API

Python `mzpeak.provenance_archive.capture_archive(path, run_id, stage_id)` and
Rust `provenance::archive::capture_archive(path, run_id, stage_id, None)` capture
all content in an existing ZIP without modifying it. An optional selected-path
list records one stage's outputs. `verify_archive(path, manifest)` returns the
same separate coverage lists as the in-memory API. When checking a history,
construct an inventory from the union of the stages' covered immutable files;
checking a single incremental stage alone correctly reports other content as
uncovered.

The adapters hash each decompressed member in bounded buffers and update optional
chunk hashes in the same forward pass. They reject links, special files,
case-aliased and duplicate ZIP names, and inconsistent local/central member
names. Rust's ZIP library indexes entries by name and would otherwise collapse
exact duplicates, so the adapter first checks the central-directory entry count
and raw names. No archive member is extracted to the filesystem. Malformed ZIPs
and CRC/read failures are errors, not successful verification reports.
