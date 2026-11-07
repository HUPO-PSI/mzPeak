# Legacy Python-to-Rust signature fixture

`signatures.parquet` was produced by `python/scripts/sign_archive.py` for the
two-column `spectrum.parquet` and the whole-file `metadata.json`. It contains
three real Ed25519 CMS signatures and their public self-signed certificate. The
whole-file row has null coordinates; all three rows persist `signed=true`.
No private key is stored. The fixture demonstrates cryptographic compatibility,
not CA trust or certified archive coverage.

The Rust integration test loads the Python-written Parquet table, checks both
chunk and whole-file coordinates, verifies each signature against its original
content, and rejects changed content. It also projects away `signed` to exercise
legacy table inference. Separate round-trip tests cover signed and unsigned
records and rejection of null required fields.

Regenerate from the repository root with the Python test/provenance extras
installed:

```sh
PYTHONPATH=python python test/data/provenance/legacy-demo/generate.py
```

Each regeneration creates a fresh ephemeral signing key and current signing
time, so the signature bytes change. Private material exists only in memory.
