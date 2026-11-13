# Self-signed Python-to-Rust signature fixture

`signatures.parquet` was produced by `python/scripts/sign_archive.py` for the
two-column `spectrum.parquet` and the whole-file `metadata.json`. It contains
three real Ed25519 CMS signatures and their public self-signed certificate. The
whole-file row has null coordinates. The table uses schema revision 1 and has no
stored `signed` column; each reader parses the CMS payload before restoring that
in-memory state. See [the schema documentation](../../../../doc/provenance-signatures.md).
No private key is stored. The fixture demonstrates cryptographic compatibility,
not CA trust or certified archive coverage.

The local Git attributes preserve the signed JSON bytes on every platform;
checkout line-ending conversion would invalidate its signature.

The Rust integration tests load the Python-written Parquet table, check both
chunk and whole-file coordinates, verify each signature against its original
content, and reject changed content. They also check unmarked tables, malformed
CMS, unfinished records, null required fields, and unsupported schema revisions.

Regenerate from the repository root with the Python test/provenance extras
installed:

```sh
PYTHONPATH=python python test/data/provenance/self-signed-demo/generate.py
```

Each regeneration creates a fresh ephemeral signing key and current signing
time, so the signature bytes change. Private material exists only in memory.
