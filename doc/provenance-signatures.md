# Segment signature tables, revision 1

The self-signed archive signing demo (`python/scripts/sign_archive.py`) writes a Parquet
table of completed CMS signatures. Each signature covers the SHA-512 digest of a
Parquet column chunk or an entire non-Parquet archive member.

## Schema revision marker

Writers attach the Arrow schema metadata entry `mzpeak.provenance = "1"`, which
is preserved when the table is written to Parquet. In PyArrow it is exposed as
`{b"mzpeak.provenance": b"1"}`. Revision **1** is the first explicitly numbered
revision of this signature-table schema. It is independent of the mzPeak file
format version, the package version, and the version fields inside CMS.

Both the Python and Rust readers accept revision `1`. They also accept tables
without the marker under the same field and CMS parsing rules, to support
unmarked signature tables from the original prototype. Any other explicit
revision is rejected rather than interpreted as revision 1. A future incompatible
schema change must define a new revision and its reader behavior.

The marker describes the table layout; it is not signed proof of a format version
and does not establish authenticity, certificate trust, or archive coverage.

## Stored columns

| Column | Arrow type | Nullable | Meaning |
| --- | --- | --- | --- |
| `filename` | `large_string` | No | Archive member name |
| `row_group` | `uint64` | Yes | Parquet row group index |
| `column` | `uint64` | Yes | Column index within the row group |
| `path` | `large_string` | Yes | Dotted Parquet column path |
| `offset` | `uint64` | Yes | Start of the column chunk in bytes |
| `length` | `uint64` | Yes | Compressed column chunk length in bytes |
| `signature` | `large_binary` | No | Complete ASN.1 DER CMS `ContentInfo` containing `SignedData` |

Whole-file records use null values for all five coordinate fields. The `signed`
flag exists only on in-memory objects and is not a stored column. Readers look up
required columns by name and ignore extra columns, including any `signed` column
from a draft table. Such a column cannot bypass payload parsing.

## Completion and verification

Checksumming produces an unsigned intermediate object. Signing replaces its
digest with CMS bytes and sets its in-memory `signed` flag. Signature-table
writers reject unsigned records and parse every CMS payload before serialization;
setting `signed=True` on arbitrary bytes does not make a record writable. The
Rust writer returns `io::Result<RecordBatch>` so callers can handle these errors.

Readers parse the complete `ContentInfo` and nested `SignedData`, require at
least one signer, and reject malformed or truncated ASN.1, trailing bytes,
different CMS content types, and raw digest values. They restore `signed=True`
only after parsing succeeds. This also applies to unmarked tables; there is no
payload-length heuristic and no fallback to treating malformed input as a digest.

Successful parsing establishes structure and completion state only. Callers must
still verify the signature against the content digest. Certificate trust, signer
identity, and complete archive coverage require additional checks; the self-signed
signing demo does not establish those properties.

Python signature-table I/O requires the `provenance` optional dependencies. Rust
signature-table I/O always includes the CMS parser; cryptographic verification
remains controlled by the `verification` feature.
