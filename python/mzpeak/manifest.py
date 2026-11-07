"""Versioned, deterministic content manifests for mzPeak provenance.

A digest match establishes content integrity only. It does not establish a signer,
certificate trust, instrument identity, or a certified processing operation.
"""
from __future__ import annotations

from dataclasses import dataclass, field
import hashlib
import io
import json
import re
from typing import BinaryIO, Mapping, Optional

MAX_INTEGER = (1 << 53) - 1
_BLOCK_SIZE = 1024 * 1024
_HEX = re.compile(r"[0-9a-f]{64}\Z")
_ID = re.compile(r"[A-Za-z0-9_-][A-Za-z0-9._-]{0,127}\Z")
_COMPONENT = re.compile(r"[A-Za-z0-9_.-][A-Za-z0-9_. -]*\Z")


def _integer(value, label):
    if type(value) is not int or not 0 <= value <= MAX_INTEGER:
        raise ValueError(f"{label} must be an integer between 0 and {MAX_INTEGER}")
    return value


def _identifier(value, label):
    if not isinstance(value, str) or not _ID.fullmatch(value):
        raise ValueError(f"invalid {label}")
    return value


def _digest(value, label="sha256"):
    if not isinstance(value, str) or not _HEX.fullmatch(value):
        raise ValueError(f"{label} must be a lowercase SHA-256 digest")
    return value


def _path(value, *, allow_provenance=False):
    if not isinstance(value, str) or not value or len(value) > 1024:
        raise ValueError("invalid content path")
    parts = value.split("/")
    if any(p in (".", "..") or not _COMPONENT.fullmatch(p) or p.endswith((" ", ".")) for p in parts):
        raise ValueError(f"unsafe or nonportable content path: {value!r}")
    if not allow_provenance and (parts[0].lower() == "provenance" or value.lower() == "provenance.parquet"):
        raise ValueError("provenance records cannot cover themselves")
    return value


def _keys(value, expected):
    if not isinstance(value, dict) or set(value) != set(expected):
        raise ValueError(f"expected exactly the fields {sorted(expected)}")


def _object_pairs(pairs):
    result = {}
    for key, value in pairs:
        if key in result:
            raise ValueError(f"duplicate JSON field: {key}")
        result[key] = value
    return result


def _load_json(data):
    return json.loads(data, object_pairs_hook=_object_pairs,
                      parse_constant=lambda value: (_ for _ in ()).throw(ValueError(f"invalid JSON constant {value}")))


def _canonical_json(value):
    # Our schemas contain strings, booleans, null, bounded integers, arrays and
    # objects only. Floats are forbidden by schema validation before this call.
    return json.dumps(value, sort_keys=True, separators=(",", ":"), ensure_ascii=False, allow_nan=False).encode("utf-8")


def sha256_stream(stream: BinaryIO, length: Optional[int] = None) -> str:
    """Hash from the current position; an explicit length requires exactly that many bytes."""
    if length is not None:
        _integer(length, "length")
    digest = hashlib.sha256()
    remaining = length
    while remaining is None or remaining:
        part = stream.read(_BLOCK_SIZE if remaining is None else min(_BLOCK_SIZE, remaining))
        if not part:
            if remaining is not None and remaining:
                raise EOFError("content is shorter than its signed length")
            break
        if remaining is not None and len(part) > remaining:
            raise ValueError("stream returned more bytes than requested")
        digest.update(part)
        if remaining is not None:
            remaining -= len(part)
    return digest.hexdigest()


@dataclass(frozen=True)
class ChunkDigest:
    offset: int
    length: int
    sha256: str
    row_group: Optional[int] = None
    column: Optional[int] = None
    column_path: Optional[str] = None

    def validate(self, file_size):
        _integer(self.offset, "offset")
        _integer(self.length, "length")
        _digest(self.sha256)
        if self.offset + self.length > file_size:
            raise ValueError("chunk extends beyond its file")
        coordinates = (self.row_group, self.column, self.column_path)
        if any(v is not None for v in coordinates):
            if any(v is None for v in coordinates):
                raise ValueError("Parquet chunk coordinates must be supplied together")
            _integer(self.row_group, "row_group")
            _integer(self.column, "column")
            if not isinstance(self.column_path, str) or not self.column_path or any(ord(c) < 32 or ord(c) > 126 for c in self.column_path):
                raise ValueError("invalid column_path")

    def to_dict(self):
        return {"offset": self.offset, "length": self.length, "sha256": self.sha256,
                "row_group": self.row_group, "column": self.column, "column_path": self.column_path}

    @classmethod
    def from_dict(cls, value):
        _keys(value, ("offset", "length", "sha256", "row_group", "column", "column_path"))
        return cls(**value)


@dataclass(frozen=True)
class FileDigest:
    path: str
    size: int
    sha256: str
    chunks: tuple[ChunkDigest, ...] = ()

    def validate(self):
        _path(self.path)
        _integer(self.size, "size")
        _digest(self.sha256)
        intervals = set()
        coordinates = set()
        end = 0
        for chunk in sorted(self.chunks, key=lambda c: (c.offset, c.length)):
            chunk.validate(self.size)
            pair = (chunk.offset, chunk.length)
            if pair in intervals or chunk.offset < end:
                raise ValueError("duplicate or overlapping chunks")
            intervals.add(pair)
            end = chunk.offset + chunk.length
            if chunk.row_group is not None:
                coordinate = (chunk.row_group, chunk.column)
                if coordinate in coordinates:
                    raise ValueError("duplicate Parquet chunk coordinates")
                coordinates.add(coordinate)

    def to_dict(self):
        return {"path": self.path, "size": self.size, "sha256": self.sha256,
                "chunks": [c.to_dict() for c in sorted(self.chunks, key=lambda c: (c.offset, c.length))]}

    @classmethod
    def from_dict(cls, value):
        _keys(value, ("path", "size", "sha256", "chunks"))
        if not isinstance(value["chunks"], list):
            raise ValueError("chunks must be an array")
        return cls(value["path"], value["size"], value["sha256"], tuple(ChunkDigest.from_dict(c) for c in value["chunks"]))

    @classmethod
    def from_stream(cls, path: str, stream: BinaryIO):
        """Read a seekable file from byte zero without retaining the file in memory."""
        stream.seek(0, io.SEEK_END)
        size = stream.tell()
        _integer(size, "size")
        stream.seek(0)
        item = cls(path, size, sha256_stream(stream, size))
        item.validate()
        return item

    def verify_stream(self, stream: BinaryIO) -> bool:
        self.validate()
        stream.seek(0, io.SEEK_END)
        if stream.tell() != self.size:
            return False
        stream.seek(0)
        try:
            if sha256_stream(stream, self.size) != self.sha256:
                return False
            for chunk in self.chunks:
                stream.seek(chunk.offset)
                if sha256_stream(stream, chunk.length) != chunk.sha256:
                    return False
        except EOFError:
            return False
        return True

    def verify_chunk(self, stream: BinaryIO, index: int) -> bool:
        """Check a selected range only; this makes no claim about the remaining file."""
        self.validate()
        if type(index) is not int or index < 0 or index >= len(self.chunks):
            raise IndexError("chunk index out of range")
        chunk = sorted(self.chunks, key=lambda c: (c.offset, c.length))[index]
        stream.seek(chunk.offset)
        try:
            return sha256_stream(stream, chunk.length) == chunk.sha256
        except EOFError:
            return False


@dataclass(frozen=True)
class CoverageReport:
    missing: tuple[str, ...]
    changed: tuple[str, ...]
    uncovered: tuple[str, ...]

    @property
    def complete(self):
        return not (self.missing or self.changed or self.uncovered)


@dataclass(frozen=True)
class Manifest:
    run_id: str
    stage_id: str
    files: tuple[FileDigest, ...]
    version: int = 1

    def validate(self):
        if type(self.version) is not int or self.version != 1:
            raise ValueError("unsupported manifest version")
        _identifier(self.run_id, "run_id")
        _identifier(self.stage_id, "stage_id")
        if not self.files:
            raise ValueError("a manifest must cover at least one file")
        paths = set()
        for item in self.files:
            item.validate()
            # Case collisions are unsafe on common extraction targets.
            if item.path.lower() in paths:
                raise ValueError("duplicate or case-colliding content path")
            paths.add(item.path.lower())

    def to_dict(self):
        self.validate()
        return {"version": self.version, "run_id": self.run_id, "stage_id": self.stage_id,
                "files": [f.to_dict() for f in sorted(self.files, key=lambda f: f.path)]}

    def canonical_bytes(self):
        return _canonical_json(self.to_dict())

    def digest(self):
        return hashlib.sha256(self.canonical_bytes()).hexdigest()

    @classmethod
    def from_dict(cls, value):
        _keys(value, ("version", "run_id", "stage_id", "files"))
        if not isinstance(value["files"], list):
            raise ValueError("files must be an array")
        item = cls(value["run_id"], value["stage_id"], tuple(FileDigest.from_dict(f) for f in value["files"]), value["version"])
        item.validate()
        return item

    @classmethod
    def from_bytes(cls, data):
        return cls.from_dict(_load_json(data))

    def verify_files(self, files: Mapping[str, bytes | BinaryIO]) -> CoverageReport:
        """Compare the complete caller-supplied *content* inventory to this stage.

        The archive adapter must provide every non-provenance entry, reject
        duplicate ZIP names, and provide immutable index snapshots. Unlisted
        content is reported, never silently certified. Streams remain open.
        """
        self.validate()
        expected = {f.path: f for f in self.files}
        missing = sorted(set(expected) - set(files))
        uncovered = sorted(set(files) - set(expected))
        changed = []
        for path in sorted(set(expected) & set(files)):
            source = files[path]
            stream = io.BytesIO(source) if isinstance(source, bytes) else source
            if not expected[path].verify_stream(stream):
                changed.append(path)
        return CoverageReport(tuple(missing), tuple(changed), tuple(uncovered))
