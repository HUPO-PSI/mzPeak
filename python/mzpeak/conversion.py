"""External RAW source inventories and signed vendor-verification claims.

These records describe vendor assertions. They do not execute proprietary
verification or conversion software and do not infer acquisition-time trust.
Callers must keep a source immutable while verifying and converting it.
"""

from __future__ import annotations

from dataclasses import dataclass
import hashlib
import os
from pathlib import Path
import stat

from .manifest import (
    CoverageReport,
    _canonical_json,
    _digest,
    _integer,
    _keys,
    _load_json,
    _path,
    sha256_stream,
)


def _text(value, label):
    if (
        not isinstance(value, str)
        or not value
        or len(value) > 128
        or any(ord(c) < 32 or ord(c) > 126 for c in value)
    ):
        raise ValueError(f"invalid {label}")


@dataclass(frozen=True)
class SourceFile:
    path: str
    size: int
    sha256: str

    def validate(self):
        _path(self.path, allow_provenance=True)
        _integer(self.size, "source size")
        _digest(self.sha256)

    def to_dict(self):
        self.validate()
        return {"path": self.path, "size": self.size, "sha256": self.sha256}

    @classmethod
    def from_dict(cls, value):
        _keys(value, ("path", "size", "sha256"))
        result = cls(**value)
        result.validate()
        return result


@dataclass(frozen=True)
class SourceManifest:
    format: str
    files: tuple[SourceFile, ...]
    version: int = 1

    def validate(self):
        if type(self.version) is not int or self.version != 1:
            raise ValueError("unsupported source manifest version")
        _text(self.format, "source format")
        if not isinstance(self.files, (tuple, list)) or not self.files:
            raise ValueError("source manifest must contain files")
        names = set()
        for item in self.files:
            item.validate()
            if item.path.lower() in names:
                raise ValueError("duplicate or case-colliding source path")
            names.add(item.path.lower())

    def to_dict(self):
        self.validate()
        return {
            "version": self.version,
            "format": self.format,
            "files": [f.to_dict() for f in sorted(self.files, key=lambda f: f.path)],
        }

    def canonical_bytes(self):
        return _canonical_json(self.to_dict())

    def digest(self):
        return hashlib.sha256(self.canonical_bytes()).hexdigest()

    @classmethod
    def from_dict(cls, value):
        _keys(value, ("version", "format", "files"))
        if not isinstance(value["files"], list):
            raise ValueError("source files must be an array")
        result = cls(
            value["format"],
            tuple(SourceFile.from_dict(f) for f in value["files"]),
            value["version"],
        )
        result.validate()
        return result

    @classmethod
    def from_bytes(cls, data):
        return cls.from_dict(_load_json(data))


@dataclass(frozen=True)
class ReportReference:
    path: str
    sha256: str

    def validate(self):
        _path(self.path)
        _digest(self.sha256)

    def to_dict(self):
        self.validate()
        return {"path": self.path, "sha256": self.sha256}

    @classmethod
    def from_dict(cls, value):
        _keys(value, ("path", "sha256"))
        result = cls(**value)
        result.validate()
        return result


@dataclass(frozen=True)
class VendorVerification:
    vendor: str
    method: str
    verifier_name: str
    verifier_version: str
    source_sha256: str
    result: str
    report: ReportReference | None

    def validate(self):
        for name in ("vendor", "method", "verifier_name", "verifier_version"):
            _text(getattr(self, name), name)
        _digest(self.source_sha256, "source_sha256")
        if self.result not in ("passed", "failed", "unknown"):
            raise ValueError("unsupported vendor verification result")
        if self.report is not None:
            self.report.validate()

    def to_dict(self):
        self.validate()
        return {
            "vendor": self.vendor,
            "method": self.method,
            "verifier_name": self.verifier_name,
            "verifier_version": self.verifier_version,
            "source_sha256": self.source_sha256,
            "result": self.result,
            "report": None if self.report is None else self.report.to_dict(),
        }

    @classmethod
    def from_dict(cls, value):
        _keys(
            value,
            (
                "vendor",
                "method",
                "verifier_name",
                "verifier_version",
                "source_sha256",
                "result",
                "report",
            ),
        )
        result = cls(
            **(
                value
                | {
                    "report": None
                    if value["report"] is None
                    else ReportReference.from_dict(value["report"])
                }
            )
        )
        result.validate()
        return result


@dataclass(frozen=True)
class ConversionClaim:
    source: SourceManifest
    verification: VendorVerification
    claimed_acquisition_time: int | None

    def validate(self):
        self.source.validate()
        self.verification.validate()
        if self.verification.source_sha256 != self.source.digest():
            raise ValueError("vendor verification differs from source manifest")
        if self.claimed_acquisition_time is not None:
            _integer(self.claimed_acquisition_time, "claimed acquisition time")

    def to_dict(self):
        self.validate()
        return {
            "source": self.source.to_dict(),
            "verification": self.verification.to_dict(),
            "claimed_acquisition_time": self.claimed_acquisition_time,
        }

    def canonical_bytes(self):
        return _canonical_json(self.to_dict())

    def digest(self):
        return hashlib.sha256(self.canonical_bytes()).hexdigest()

    @classmethod
    def from_dict(cls, value):
        _keys(value, ("source", "verification", "claimed_acquisition_time"))
        result = cls(
            SourceManifest.from_dict(value["source"]),
            VendorVerification.from_dict(value["verification"]),
            value["claimed_acquisition_time"],
        )
        result.validate()
        return result

    @classmethod
    def from_bytes(cls, data):
        return cls.from_dict(_load_json(data))


def _identity(info):
    return (info.st_dev, info.st_ino, info.st_size, info.st_mtime_ns, info.st_ctime_ns)


def _is_link(info):
    return stat.S_ISLNK(info.st_mode) or bool(
        getattr(info, "st_file_attributes", 0)
        & getattr(stat, "FILE_ATTRIBUTE_REPARSE_POINT", 0x400)
    )


def _source_inventory(path):
    """Snapshot only local regular files; never follow source symlinks."""
    path = Path(path)
    info = path.lstat()
    files, directories = {}, {}
    if _is_link(info):
        raise ValueError("source symlinks are not permitted")
    if stat.S_ISREG(info.st_mode):
        _path(path.name, allow_provenance=True)
        files[path.name] = (path, _identity(info))
    elif stat.S_ISDIR(info.st_mode):

        def visit(directory):
            before = directory.lstat()
            if not stat.S_ISDIR(before.st_mode) or _is_link(before):
                raise ValueError("source directory changed or is a symlink")
            directories[str(directory)] = _identity(before)
            with os.scandir(directory) as entries:
                for entry in sorted(entries, key=lambda entry: entry.name):
                    member = Path(entry.path)
                    name = member.relative_to(path).as_posix()
                    _path(name, allow_provenance=True)
                    entry_info = member.lstat()
                    if _is_link(entry_info):
                        raise ValueError("source symlinks are not permitted")
                    if stat.S_ISDIR(entry_info.st_mode):
                        visit(member)
                    elif stat.S_ISREG(entry_info.st_mode):
                        files[name] = (member, _identity(entry_info))
                    else:
                        raise ValueError("source special files are not permitted")

        visit(path)
    else:
        raise ValueError("source must be a regular file or directory")
    names = [name.lower() for name in files]
    if len(names) != len(set(names)):
        raise ValueError("duplicate or case-colliding source path")
    return files, directories


def _capture_files(path):
    files, directories = _source_inventory(path)
    digests = []
    for name, (member, initial) in sorted(files.items()):
        descriptor = os.open(
            member,
            os.O_RDONLY | getattr(os, "O_BINARY", 0) | getattr(os, "O_NOFOLLOW", 0),
        )
        with os.fdopen(descriptor, "rb") as stream:
            before = os.fstat(stream.fileno())
            if not stat.S_ISREG(before.st_mode) or _identity(before) != initial:
                raise ValueError("source changed during capture")
            _integer(before.st_size, "source size")
            digest = sha256_stream(stream, before.st_size)
            if stream.read(1) or _identity(os.fstat(stream.fileno())) != initial:
                raise ValueError("source changed during capture")
        digests.append(SourceFile(name, before.st_size, digest))
    if _source_inventory(path) != (files, directories):
        raise ValueError("source changed during capture")
    return tuple(digests)


def capture_source(path, format):
    """Capture a stable local RAW file/dataset without retaining its bytes."""
    _text(format, "source format")
    result = SourceManifest(format, _capture_files(path))
    result.validate()
    return result


def verify_source(path, manifest):
    """Compare explicitly supplied source bytes to their signed RAW inventory."""
    manifest.validate()
    expected_files = {f.path: f for f in manifest.files}
    actual_files = {f.path: f for f in _capture_files(path)}
    return CoverageReport(
        tuple(sorted(set(expected_files) - set(actual_files))),
        tuple(
            sorted(
                name
                for name in set(expected_files) & set(actual_files)
                if expected_files[name] != actual_files[name]
            )
        ),
        tuple(sorted(set(actual_files) - set(expected_files))),
    )
