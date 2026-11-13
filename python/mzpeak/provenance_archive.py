"""Streaming ZIP adapters for the experimental content manifest profile."""
from __future__ import annotations

import hashlib
import stat
from pathlib import Path
from typing import Iterable, Optional
import zipfile

from .manifest import (CoverageReport, FileDigest, Manifest, _BLOCK_SIZE,
                       _integer, _path, sha256_stream)


def _inventory(archive: zipfile.ZipFile):
    """Validate all transport names/types before excluding provenance records."""
    content = {}
    names = set()
    for item in archive.infolist():
        # zipfile can truncate NULs (or normalize separators) before exposing
        # filename. Never validate or exclude an alias of the on-disk name.
        if item.filename != item.orig_filename:
            raise ValueError("normalized ZIP member name is not permitted")
        name = item.filename
        normalized = name[:-1] if item.is_dir() else name
        # Reserved names are valid transport records, but retain all path checks.
        _path(normalized, allow_provenance=True)
        if normalized.lower() in names:
            raise ValueError("duplicate or case-colliding ZIP member name")
        names.add(normalized.lower())
        # ZipFile.open checks that the local header repeats this exact central
        # name, including records excluded from content coverage below.
        with archive.open(item):
            pass
        mode = item.external_attr >> 16
        kind = stat.S_IFMT(mode)
        if kind not in (0, stat.S_IFREG, stat.S_IFDIR):
            raise ValueError("ZIP links and special files are not content files")
        if (kind == stat.S_IFDIR and not item.is_dir()) or (kind == stat.S_IFREG and item.is_dir()):
            raise ValueError("inconsistent ZIP directory type")
        if item.is_dir():
            if item.file_size:
                raise ValueError("ZIP directory member must be empty")
            if normalized.lower() in ("provenance/bundle-v1.json", "provenance.parquet"):
                raise ValueError("provenance proof member must be a file")
            continue
        # Only these exact proof members are outside signed content coverage.
        # Other reserved paths and case aliases fail the content-path check.
        if name in ("provenance/bundle-v1.json", "provenance.parquet"):
            continue
        _path(name)
        _integer(item.file_size, "ZIP member size")
        content[name] = item
    return content


def capture_archive(path: str | Path, run_id: str, stage_id: str,
                    selected_paths: Optional[Iterable[str]] = None) -> Manifest:
    """Capture file bytes, metadata and index entries without modifying the ZIP.

    An optional selection captures a stage's exact output inventory. Verification
    against the entire ZIP still reports nonselected content as uncovered.
    """
    with zipfile.ZipFile(path) as archive:
        content = _inventory(archive)
        if selected_paths is None:
            selected = set(content)
        else:
            paths = tuple(selected_paths)
            selected = set(paths)
            if len(selected) != len(paths):
                raise ValueError("duplicate selected path")
            if not selected <= set(content):
                raise ValueError("selected path is missing from archive content")
        files = []
        for name in sorted(selected):
            item = content[name]
            with archive.open(item) as stream:
                digest = sha256_stream(stream, item.file_size)
                if stream.read(1):
                    raise ValueError("ZIP member exceeds its declared size")
            files.append(FileDigest(name, item.file_size, digest))
    result = Manifest(run_id, stage_id, tuple(files))
    result.validate()
    return result


def _verify_member(stream, descriptor):
    """Hash the whole member and optional ranges together in one forward pass."""
    digest = hashlib.sha256()
    chunks = sorted(descriptor.chunks, key=lambda c: (c.offset, c.length))
    chunk_digests = [hashlib.sha256() for _ in chunks]
    offset = 0
    remaining = descriptor.size
    chunk_cursor = 0
    while remaining:
        block = stream.read(min(_BLOCK_SIZE, remaining))
        if not block:
            return False
        digest.update(block)
        end = offset + len(block)
        for index in range(chunk_cursor, len(chunks)):
            chunk = chunks[index]
            if chunk.offset >= end:
                break
            start = max(offset, chunk.offset)
            stop = min(end, chunk.offset + chunk.length)
            if start < stop:
                chunk_digests[index].update(block[start - offset:stop - offset])
            if chunk.offset + chunk.length <= end:
                chunk_cursor = index + 1
        offset = end
        remaining -= len(block)
    if stream.read(1):
        return False
    return digest.hexdigest() == descriptor.sha256 and all(
        digest.hexdigest() == chunk.sha256 for chunk, digest in zip(chunks, chunk_digests))


def verify_archive(path: str | Path, manifest: Manifest) -> CoverageReport:
    """Verify complete decompressed member coverage; malformed ZIPs raise errors."""
    manifest.validate()
    expected = {f.path: f for f in manifest.files}
    with zipfile.ZipFile(path) as archive:
        content = _inventory(archive)
        missing = sorted(set(expected) - set(content))
        uncovered = sorted(set(content) - set(expected))
        changed = []
        for name in sorted(set(expected) & set(content)):
            descriptor = expected[name]
            item = content[name]
            if item.file_size != descriptor.size:
                changed.append(name)
                continue
            with archive.open(item) as stream:
                if not _verify_member(stream, descriptor):
                    changed.append(name)
    return CoverageReport(tuple(missing), tuple(changed), tuple(uncovered))
