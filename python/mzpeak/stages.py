"""Immutable acquisition/processing lineage bound into signed stage bytes.

The graph checks references and content identity. Signatures, trust policies and
release approval must be checked separately for every stage requiring assurance.
"""

from __future__ import annotations

from dataclasses import dataclass
import hashlib
from typing import Iterable, Optional

from .conversion import ConversionClaim

from .manifest import (
    Manifest,
    _canonical_json,
    _digest,
    _identifier,
    _keys,
    _load_json,
    _path,
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
class SoftwareIdentity:
    name: str
    version: str
    release_sha256: str
    parameters_sha256: str

    def validate(self):
        _text(self.name, "software name")
        _text(self.version, "software version")
        _digest(self.release_sha256, "release_sha256")
        _digest(self.parameters_sha256, "parameters_sha256")

    def to_dict(self):
        self.validate()
        return {
            "name": self.name,
            "version": self.version,
            "release_sha256": self.release_sha256,
            "parameters_sha256": self.parameters_sha256,
        }

    @classmethod
    def from_dict(cls, value):
        _keys(value, ("name", "version", "release_sha256", "parameters_sha256"))
        item = cls(**value)
        item.validate()
        return item


@dataclass(frozen=True)
class InputReference:
    stage_sha256: str
    path: str
    sha256: str

    def validate(self):
        _digest(self.stage_sha256, "stage_sha256")
        _path(self.path)
        _digest(self.sha256)

    def to_dict(self):
        self.validate()
        return {
            "stage_sha256": self.stage_sha256,
            "path": self.path,
            "sha256": self.sha256,
        }

    @classmethod
    def from_dict(cls, value):
        _keys(value, ("stage_sha256", "path", "sha256"))
        item = cls(**value)
        item.validate()
        return item


@dataclass(frozen=True)
class Stage:
    manifest: Manifest
    kind: str
    parents: tuple[str, ...] = ()
    inputs: tuple[InputReference, ...] = ()
    software: Optional[SoftwareIdentity] = None
    version: int = 1
    conversion: Optional[ConversionClaim] = None

    def validate(self):
        if type(self.version) is not int or self.version not in (1, 2):
            raise ValueError("unsupported stage version")
        self.manifest.validate()
        if self.version == 1 and (
            self.kind not in ("acquisition", "processing")
            or self.conversion is not None
        ):
            raise ValueError(
                "version 1 supports acquisition/processing without conversion"
            )
        if self.version == 2 and (self.kind != "conversion" or self.conversion is None):
            raise ValueError("version 2 requires a conversion claim")
        if self.kind not in ("acquisition", "processing", "conversion"):
            raise ValueError("unsupported stage kind")
        for parent in self.parents:
            _digest(parent, "parent")
        if len(set(self.parents)) != len(self.parents):
            raise ValueError("duplicate parents")
        refs = set()
        used_parents = set()
        for item in self.inputs:
            item.validate()
            ref = (item.stage_sha256, item.path)
            if ref in refs:
                raise ValueError("duplicate input reference")
            refs.add(ref)
            used_parents.add(item.stage_sha256)
        if self.kind == "acquisition":
            if self.parents or self.inputs:
                raise ValueError("an acquisition cannot claim parent processing stages")
        elif self.kind == "processing":
            if not self.parents or not self.inputs or self.software is None:
                raise ValueError(
                    "processing requires parents, exact inputs and a software identity"
                )
            if used_parents != set(self.parents):
                raise ValueError("inputs must reference every and only declared parent")
        else:
            if self.parents or self.inputs or self.software is None:
                raise ValueError(
                    "conversion requires external source, software, and no internal parents or inputs"
                )
            self.conversion.validate()
            report = self.conversion.verification.report
            if report is not None and not any(
                f.path == report.path and f.sha256 == report.sha256
                for f in self.manifest.files
            ):
                raise ValueError(
                    "vendor report is not covered by this conversion stage"
                )
        if self.software is not None:
            self.software.validate()

    def to_dict(self):
        self.validate()
        value = {
            "version": self.version,
            "kind": self.kind,
            "manifest": self.manifest.to_dict(),
            "parents": sorted(self.parents),
            "inputs": [
                r.to_dict()
                for r in sorted(self.inputs, key=lambda r: (r.stage_sha256, r.path))
            ],
            "software": None if self.software is None else self.software.to_dict(),
        }
        if self.version == 2:
            value["conversion"] = self.conversion.to_dict()
        return value

    def canonical_bytes(self):
        return _canonical_json(self.to_dict())

    def digest(self):
        return hashlib.sha256(self.canonical_bytes()).hexdigest()

    @classmethod
    def from_dict(cls, value):
        if not isinstance(value, dict) or type(value.get("version")) is not int:
            raise ValueError("unsupported stage version")
        fields = ("version", "kind", "manifest", "parents", "inputs", "software")
        if value["version"] == 2:
            fields += ("conversion",)
        elif value["version"] != 1:
            raise ValueError("unsupported stage version")
        _keys(value, fields)
        if not isinstance(value["parents"], list) or not isinstance(
            value["inputs"], list
        ):
            raise ValueError("parents and inputs must be arrays")
        item = cls(
            Manifest.from_dict(value["manifest"]),
            value["kind"],
            tuple(value["parents"]),
            tuple(InputReference.from_dict(r) for r in value["inputs"]),
            None
            if value["software"] is None
            else SoftwareIdentity.from_dict(value["software"]),
            value["version"],
            ConversionClaim.from_dict(value["conversion"])
            if value["version"] == 2
            else None,
        )
        item.validate()
        return item

    @classmethod
    def from_bytes(cls, data):
        return cls.from_dict(_load_json(data))


@dataclass(frozen=True)
class LineageReport:
    heads: tuple[str, ...]
    acquisitions: tuple[str, ...]
    stage_count: int
    checkpoint_checked: bool
    conversions: tuple[str, ...] = ()


def verify_lineage(
    stages: Iterable[Stage], expected_heads: Optional[Iterable[str]] = None
) -> LineageReport:
    """Validate the full supplied history, optionally against an external checkpoint.

    A signed graph cannot prove that an entire later stage was not removed. The
    caller must supply trusted expected head digests to detect that rollback.
    This function does not check signatures or confer certification on any stage.
    """
    records = {}
    identifiers = set()
    paths = {}
    run_id = None
    for stage in stages:
        stage.validate()
        digest = stage.digest()
        if digest in records or stage.manifest.stage_id in identifiers:
            raise ValueError("duplicate stage or stage identifier")
        records[digest] = stage
        identifiers.add(stage.manifest.stage_id)
        if run_id is not None and stage.manifest.run_id != run_id:
            raise ValueError("stages belong to different runs")
        run_id = stage.manifest.run_id
        for item in stage.manifest.files:
            previous = paths.get(item.path.lower())
            identity = (item.path, item.size, item.sha256)
            if previous is not None and previous != identity:
                raise ValueError("a later stage overwrites previously covered content")
            paths[item.path.lower()] = identity
    if not records:
        raise ValueError("empty lineage")
    referenced = set()
    acquisitions = []
    conversions = []
    for digest, stage in records.items():
        if stage.kind == "acquisition":
            acquisitions.append(digest)
        elif stage.kind == "conversion":
            conversions.append(digest)
        for parent in stage.parents:
            if parent not in records:
                raise ValueError("missing parent stage")
            referenced.add(parent)
        for item in stage.inputs:
            parent = records.get(item.stage_sha256)
            if parent is None:
                raise ValueError("missing input stage")
            descriptor = next(
                (f for f in parent.manifest.files if f.path == item.path), None
            )
            if descriptor is None or descriptor.sha256 != item.sha256:
                raise ValueError(
                    "input does not match the parent stage's covered output"
                )
    # Iterative topological traversal also handles very deep histories safely.
    pending = {digest: len(stage.parents) for digest, stage in records.items()}
    children = {digest: [] for digest in records}
    for digest, stage in records.items():
        for parent in stage.parents:
            children[parent].append(digest)
    ready = [digest for digest, count in pending.items() if count == 0]
    visited = 0
    while ready:
        digest = ready.pop()
        visited += 1
        for child in children[digest]:
            pending[child] -= 1
            if pending[child] == 0:
                ready.append(child)
    if visited != len(records):
        raise ValueError("cyclic lineage")
    heads = tuple(sorted(set(records) - referenced))
    if not acquisitions and not conversions:
        raise ValueError("lineage contains no acquisition or conversion root")
    if expected_heads is not None:
        expected = tuple(expected_heads)
        for digest in expected:
            _digest(digest, "expected head")
        if (
            len(set(expected)) != len(expected)
            or not expected
            or set(heads) != set(expected)
        ):
            raise ValueError("lineage does not match the external head checkpoint")
    return LineageReport(
        heads,
        tuple(sorted(acquisitions)),
        len(records),
        expected_heads is not None,
        tuple(sorted(conversions)),
    )
