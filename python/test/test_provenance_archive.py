import hashlib
import stat
import struct
import warnings
import zipfile
from dataclasses import replace

import pytest

from mzpeak.manifest import ChunkDigest
from mzpeak.provenance_archive import capture_archive, verify_archive


def write_archive(path, members, compression=zipfile.ZIP_STORED):
    with zipfile.ZipFile(path, "w", compression=compression) as archive:
        for name, data in members:
            archive.writestr(name, data)


def test_capture_covers_metadata_and_transport_independent(tmp_path):
    path = tmp_path / "data.mzpeak"
    members = [("mzpeak_index.json", b"{}"), ("spectra.parquet", b"abcdef"), ("provenance/envelope.json", b"untrusted")]
    write_archive(path, members)
    manifest = capture_archive(path, "run-1", "stage-1")
    assert len(manifest.files) == 2
    assert verify_archive(path, manifest).complete
    write_archive(path, members, zipfile.ZIP_DEFLATED)
    assert verify_archive(path, manifest).complete
    write_archive(path, [("mzpeak_index.json", b"[]"), ("extra.json", b"{}")])
    report = verify_archive(path, manifest)
    assert report.changed == ("mzpeak_index.json",)
    assert report.missing == ("spectra.parquet",)
    assert report.uncovered == ("extra.json",)


def test_selected_output_never_claims_complete_archive(tmp_path):
    path = tmp_path / "data.mzpeak"
    write_archive(path, [("a", b"a"), ("b", b"b")])
    manifest = capture_archive(path, "run-1", "stage-1", ["a"])
    assert verify_archive(path, manifest).uncovered == ("b",)
    with pytest.raises(ValueError):
        capture_archive(path, "run-1", "stage-1", ["missing"])
    with pytest.raises(ValueError):
        capture_archive(path, "run-1", "stage-1", ["a", "a"])


@pytest.mark.parametrize("members", [[("a", b"a"), ("a", b"b")], [("a", b"a"), ("A", b"b")], [("../escape", b"a")]])
def test_ambiguous_and_unsafe_inventory_rejected(tmp_path, members):
    path = tmp_path / "data.mzpeak"
    with warnings.catch_warnings():
        warnings.simplefilter("ignore", UserWarning)
        write_archive(path, members)
    with pytest.raises(ValueError):
        capture_archive(path, "run-1", "stage-1")


def test_symlink_rejected_even_in_provenance_namespace(tmp_path):
    path = tmp_path / "data.mzpeak"
    with zipfile.ZipFile(path, "w") as archive:
        info = zipfile.ZipInfo("provenance/link")
        info.create_system = 3
        info.external_attr = (stat.S_IFLNK | 0o777) << 16
        archive.writestr(info, "target")
    with pytest.raises(ValueError, match="links"):
        capture_archive(path, "run-1", "stage-1")


def test_streaming_optional_ranges_and_zero_chunk(tmp_path):
    path = tmp_path / "data.mzpeak"
    data = b"a" * (1024 * 1024 + 5)
    write_archive(path, [("spectra.parquet", data)], zipfile.ZIP_DEFLATED)
    manifest = capture_archive(path, "run-1", "stage-1")
    chunks = (ChunkDigest(0, 0, hashlib.sha256(b"").hexdigest()),
              ChunkDigest(1024 * 1024 - 2, 7, hashlib.sha256(b"a" * 7).hexdigest()))
    manifest = replace(manifest, files=(replace(manifest.files[0], chunks=chunks),))
    assert verify_archive(path, manifest).complete
    manifest = replace(manifest, files=(replace(manifest.files[0], chunks=(replace(chunks[1], sha256="a" * 64),)),))
    assert verify_archive(path, manifest).changed == ("spectra.parquet",)


def test_local_central_filename_disagreement_rejected(tmp_path):
    path = tmp_path / "data.mzpeak"
    write_archive(path, [("a", b"data")])
    data = bytearray(path.read_bytes())
    data[30] = ord("b")
    path.write_bytes(data)
    with pytest.raises(zipfile.BadZipFile):
        capture_archive(path, "run-1", "stage-1")


@pytest.mark.parametrize("raw_name", [
    "a\x00bad", "provenance/envelope.json\x00bad",
    "provenance.parquet\x00bad", "provenance/\x00bad",
])
@pytest.mark.parametrize("operation", ["capture", "verify"])
def test_normalized_zip_filename_rejected(tmp_path, raw_name, operation):
    path = tmp_path / "data.mzpeak"
    normalized = raw_name.split("\x00", 1)[0]
    write_archive(path, [("spectra.parquet", b"data"), (normalized, b"proof")])
    manifest = capture_archive(path, "run-1", "stage-1")

    # ZipInfo truncates NULs even when writing, so patch both on-disk headers.
    placeholder = raw_name.replace("\x00", "!")
    write_archive(path, [("spectra.parquet", b"data"), (placeholder, b"proof")])
    data = path.read_bytes()
    assert data.count(placeholder.encode()) == 2
    path.write_bytes(data.replace(placeholder.encode(), raw_name.encode()))
    with zipfile.ZipFile(path) as archive:
        item = archive.infolist()[1]
        assert item.filename == normalized
        assert item.orig_filename == raw_name
        # Matching central/local original names pass zipfile's own check.
        assert archive.read(item) == b"proof"

    with pytest.raises(ValueError, match="normalized ZIP member name"):
        if operation == "capture":
            capture_archive(path, "run-1", "stage-1")
        else:
            verify_archive(path, manifest)
