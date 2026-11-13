"""Bind rewrites to one source file and durably publish the completed ZIP."""

import os
import stat

import pytest

from mzpeak import embedded
from test_embedded_provenance import archive, bundle  # noqa: F401


@pytest.mark.skipif(os.name != "posix", reason="POSIX directory fsync contract")
def test_file_sync_precedes_replace_and_directory_sync_follows(archive, monkeypatch):
    events = []
    fsync, replace = os.fsync, os.replace

    def sync(descriptor):
        events.append("directory" if stat.S_ISDIR(os.fstat(descriptor).st_mode) else "file")
        fsync(descriptor)

    def commit(source, target):
        replace(source, target)
        events.append("replace")

    monkeypatch.setattr(embedded.os, "fsync", sync)
    monkeypatch.setattr(embedded.os, "replace", commit)
    embedded.embed_provenance(archive, bundle())
    assert events == ["file", "replace", "directory"]


def test_directory_sync_failure_reports_that_replacement_already_happened(
    archive, monkeypatch
):
    before = archive.read_bytes()

    def fail(_parent):
        raise OSError("simulated directory flush failure")

    monkeypatch.setattr(embedded, "_sync_directory", fail)
    with pytest.raises(OSError, match="archive was replaced but parent directory"):
        embedded.embed_provenance(archive, bundle())
    assert archive.read_bytes() != before
    assert embedded.read_embedded_provenance(archive).to_dict() == bundle().to_dict()
    assert not list(archive.parent.glob(".mzpeak-provenance-*"))


@pytest.mark.skipif(os.name != "posix", reason="replace a path while its source is open")
def test_same_size_and_mtime_replacement_is_not_overwritten(archive, monkeypatch):
    before = archive.read_bytes()
    initial = archive.stat()
    original_sync = os.fsync
    original_replace = os.replace
    swapped = False

    def swap_at_file_sync(descriptor):
        nonlocal swapped
        original_sync(descriptor)
        if not swapped and stat.S_ISREG(os.fstat(descriptor).st_mode):
            replacement = archive.with_name("replacement.mzpeak")
            replacement.write_bytes(before)
            os.utime(replacement, ns=(initial.st_atime_ns, initial.st_mtime_ns))
            original_replace(replacement, archive)
            swapped = True

    monkeypatch.setattr(embedded.os, "fsync", swap_at_file_sync)
    with pytest.raises(ValueError, match="archive changed"):
        embedded.embed_provenance(archive, bundle())
    assert swapped
    assert archive.stat().st_ino != initial.st_ino
    assert archive.read_bytes() == before
    assert not list(archive.parent.glob(".mzpeak-provenance-*"))


@pytest.mark.skipif(os.name != "posix", reason="replace paths while ZIP handles are open")
def test_zip_reader_uses_the_snapshotted_file_not_a_reopened_path(archive, monkeypatch):
    real_zip = embedded.zipfile.ZipFile
    with real_zip(archive) as source:
        original_members = {name: source.read(name) for name in source.namelist()}
    substitute = archive.with_name("substitute.mzpeak")
    with real_zip(substitute, "w") as output:
        for name, content in original_members.items():
            output.writestr(name, content + b"attacker substitute")
    saved = archive.with_name("saved-original.mzpeak")
    swapped = False

    def open_during_path_swap(file, *args, **kwargs):
        nonlocal swapped
        mode = args[0] if args else kwargs.get("mode", "r")
        if swapped or mode != "r":
            return real_zip(file, *args, **kwargs)
        swapped = True
        os.replace(archive, saved)
        os.replace(substitute, archive)
        try:
            return real_zip(file, *args, **kwargs)
        finally:
            os.replace(saved, archive)

    monkeypatch.setattr(embedded.zipfile, "ZipFile", open_during_path_swap)
    embedded.embed_provenance(archive, bundle())
    assert swapped
    with real_zip(archive) as result:
        for name, content in original_members.items():
            assert result.read(name) == content
