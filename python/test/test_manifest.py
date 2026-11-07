import hashlib
import io
import json
from dataclasses import replace
from pathlib import Path

import pytest

from mzpeak.manifest import ChunkDigest, FileDigest, Manifest, MAX_INTEGER, sha256_stream

FIXTURES = Path(__file__).resolve().parents[2] / "test" / "data" / "provenance"


def manifest():
    return Manifest("run-1", "acquisition-1", (
        FileDigest.from_stream("spectra.parquet", io.BytesIO(b"abcdef")),
        FileDigest.from_stream("index.json", io.BytesIO(b"{}")),
    ))


def test_shared_vector_and_ordering():
    data = (FIXTURES / "manifest-v1.json").read_bytes()
    value = Manifest.from_bytes(data)
    assert value.canonical_bytes() == data
    assert value.digest() == (FIXTURES / "manifest-v1.sha256").read_text().strip()
    assert replace(value, files=tuple(reversed(value.files))).canonical_bytes() == data
    assert Manifest.from_bytes(json.dumps(json.loads(data), indent=2)).digest() == value.digest()


def test_complete_coverage_and_tampering():
    value = manifest()
    files = {"spectra.parquet": b"abcdef", "index.json": b"{}"}
    assert value.verify_files(files).complete
    files["spectra.parquet"] = b"abcdeg"
    del files["index.json"]
    files["extra.json"] = b"{}"
    report = value.verify_files(files)
    assert report.changed == ("spectra.parquet",)
    assert report.missing == ("index.json",)
    assert report.uncovered == ("extra.json",)
    assert not report.complete


def test_metadata_path_and_coordinate_binding():
    value = manifest()
    original = value.digest()
    descriptor = value.files[0]
    assert replace(value, files=(replace(descriptor, path="other.parquet"), value.files[1])).digest() != original
    chunk = ChunkDigest(0, 3, hashlib.sha256(b"abc").hexdigest(), 0, 0, "mz")
    value = replace(value, files=(replace(descriptor, chunks=(chunk,)), value.files[1]))
    digest = value.digest()
    assert replace(value, files=(replace(value.files[0], chunks=(replace(chunk, column=1),)), value.files[1])).digest() != digest
    # A footer/schema/index change is covered by the whole-file hash even when
    # none of the optional selected chunks changed.
    assert not value.verify_files({"spectra.parquet": b"abcxyz", "index.json": b"{}"}).complete


def test_exact_zero_and_partial_checks():
    assert sha256_stream(io.BytesIO(b"abc"), 0) == hashlib.sha256(b"").hexdigest()
    with pytest.raises(EOFError):
        sha256_stream(io.BytesIO(b"abc"), 4)
    chunk = ChunkDigest(0, 3, hashlib.sha256(b"abc").hexdigest(), 0, 1, "intensity")
    descriptor = replace(manifest().files[0], chunks=(chunk,))
    stream = io.BytesIO(b"abcXYZ")
    stream.seek(4)
    assert descriptor.verify_chunk(stream, 0)
    assert not descriptor.verify_stream(stream)
    assert not descriptor.verify_chunk(io.BytesIO(b"ab"), 0)
    with pytest.raises(IndexError):
        descriptor.verify_chunk(stream, -1)


@pytest.mark.parametrize("path", ["../escape", "/absolute", "C:/drive", r"a\b", "a//b", "provenance/stage.json", "PROVENANCE/x", "provenance.parquet", "a/./b", "a.", "a/", "a\x00b", "ä.parquet"])
def test_safe_paths(path):
    value = manifest()
    with pytest.raises(ValueError):
        replace(value, files=(replace(value.files[0], path=path),)).canonical_bytes()


@pytest.mark.parametrize("change", [
    {"version": 2}, {"version": True}, {"files": ()}, {"run_id": "../run"},
])
def test_invalid_manifest(change):
    with pytest.raises(ValueError):
        replace(manifest(), **change).canonical_bytes()


def test_reject_duplicate_and_case_colliding_paths():
    value = manifest()
    for other in (value.files[0], replace(value.files[0], path="Spectra.parquet")):
        with pytest.raises(ValueError):
            replace(value, files=value.files + (other,)).canonical_bytes()


@pytest.mark.parametrize("size", [-1, MAX_INTEGER + 1, 1.0, True])
def test_bounded_integer_size(size):
    value = manifest()
    with pytest.raises(ValueError):
        replace(value, files=(replace(value.files[0], size=size),)).canonical_bytes()


def test_chunk_validation():
    value = manifest().files[0]
    chunk = ChunkDigest(0, 3, hashlib.sha256(b"abc").hexdigest(), 0, 1, "intensity")
    for chunks in ((chunk, chunk), (chunk, replace(chunk, offset=1, length=1, column=2)),
                   (replace(chunk, offset=5),), (replace(chunk, column=None),),
                   (chunk, replace(chunk, offset=3))):
        with pytest.raises(ValueError):
            replace(value, chunks=chunks).validate()


def test_strict_json():
    payload = manifest().to_dict()
    for key, value in (("extension", "ignored"), ("version", 1.5)):
        with pytest.raises(ValueError):
            Manifest.from_bytes(json.dumps({**payload, key: value}))
    with pytest.raises(ValueError):
        Manifest.from_bytes('{"version":1,"version":1,"run_id":"x","stage_id":"y","files":[]}')
    payload["files"][0]["chunks"] = [{"offset": 0, "length": 0, "sha256": "a" * 64}]
    with pytest.raises(ValueError):
        Manifest.from_bytes(json.dumps(payload))


def test_streaming_reads_are_bounded():
    class Guarded(io.BytesIO):
        def read(self, size=-1):
            assert 0 <= size <= 1024 * 1024
            return super().read(size)
    data = b"x" * (2 * 1024 * 1024 + 1)
    value = FileDigest.from_stream("data.parquet", Guarded(data))
    assert value.sha256 == hashlib.sha256(data).hexdigest()
    assert value.verify_stream(Guarded(data))
