"""Portable public provenance inside an mzPeak ZIP; trust stays external."""

from dataclasses import asdict, dataclass, field
from hashlib import sha256
import json
import os
from pathlib import Path
import shutil
import tempfile
import zipfile

from .evidence import EvidenceBundle
from .manifest import _keys, _load_json, _digest
from .provenance_archive import _inventory
from .signing import SignatureEnvelope
from .stages import Stage
from .verification import ReleaseStatement, SignedRelease, SignedStage

BUNDLE_PATH = "provenance/bundle-v1.json"


def _json(value):
    return json.dumps(
        value, sort_keys=True, separators=(",", ":"), ensure_ascii=True, allow_nan=False
    ).encode("ascii")


@dataclass
class ProvenanceBundle:
    records: list[SignedStage] = field(default_factory=list)
    releases: dict[str, SignedRelease] = field(default_factory=dict)
    stage_evidence: dict[str, EvidenceBundle] = field(default_factory=dict)
    release_evidence: dict[str, EvidenceBundle] = field(default_factory=dict)
    version: int = 1

    def to_dict(self):
        return {
            "version": self.version,
            "records": [
                {
                    "stage": r.stage.to_dict(),
                    "envelope": _load_json(r.envelope.to_json()),
                }
                for r in self.records
            ],
            "releases": {
                k: {
                    "statement": asdict(r.statement),
                    "envelope": _load_json(r.envelope.to_json()),
                }
                for k, r in self.releases.items()
            },
            "stage_evidence": {k: asdict(v) for k, v in self.stage_evidence.items()},
            "release_evidence": {
                k: asdict(v) for k, v in self.release_evidence.items()
            },
        }

    def validate(self):
        if type(self.version) is not int or self.version != 1 or not self.records:
            raise ValueError("unsupported or empty embedded provenance bundle")
        stages = {}
        runs = set()
        for record in self.records:
            record.stage.canonical_bytes()
            record.envelope.canonical_bytes()
            identifier = record.stage.manifest.stage_id
            if identifier in stages:
                raise ValueError("duplicate embedded stage ID")
            stages[identifier] = record
            runs.add(record.stage.manifest.run_id)
        if len(runs) != 1:
            raise ValueError("embedded stages must belong to one run")
        for digest, release in self.releases.items():
            release.statement.canonical_bytes()
            release.envelope.canonical_bytes()
            if digest != release.statement.release_sha256:
                raise ValueError("release key differs from signed statement")
        for evidence, records, payload in (
            (self.stage_evidence, stages, lambda r: r.stage.canonical_bytes()),
            (
                self.release_evidence,
                self.releases,
                lambda r: r.statement.canonical_bytes(),
            ),
        ):
            for key, bundle in evidence.items():
                record = records.get(key)
                if (
                    record is None
                    or type(bundle.version) is not int
                    or bundle.version != 1
                    or not isinstance(bundle.policy_id, str)
                    or not bundle.policy_id
                    or bundle.manifest_sha256 != sha256(payload(record)).hexdigest()
                    or bundle.envelope_sha256
                    != sha256(record.envelope.canonical_bytes()).hexdigest()
                ):
                    raise ValueError(
                        "embedded evidence is not bound to its signed record"
                    )
                _digest(bundle.policy_sha256)
                if len(bundle.to_json()) > 16 * 1024 * 1024:
                    raise ValueError("evidence bundle too large")

    def to_json(self):
        self.validate()
        data = _json(self.to_dict())
        return data

    @classmethod
    def from_json(cls, data):
        value = _load_json(data)
        _keys(
            value,
            ("version", "records", "releases", "stage_evidence", "release_evidence"),
        )
        if not isinstance(value["records"], list) or any(
            not isinstance(value[k], dict)
            for k in ("releases", "stage_evidence", "release_evidence")
        ):
            raise ValueError("invalid embedded provenance collections")
        records, releases = [], {}
        for record in value["records"]:
            _keys(record, ("stage", "envelope"))
            records.append(
                SignedStage(
                    Stage.from_dict(record["stage"]),
                    SignatureEnvelope.from_json(_json(record["envelope"])),
                )
            )
        for key, release in value["releases"].items():
            _keys(release, ("statement", "envelope"))
            _keys(release["statement"], ("name", "version", "release_sha256"))
            releases[key] = SignedRelease(
                ReleaseStatement(**release["statement"]),
                SignatureEnvelope.from_json(_json(release["envelope"])),
            )
        result = cls(
            records,
            releases,
            {
                k: EvidenceBundle.from_json(_json(v))
                for k, v in value["stage_evidence"].items()
            },
            {
                k: EvidenceBundle.from_json(_json(v))
                for k, v in value["release_evidence"].items()
            },
            value["version"],
        )
        result.validate()
        return result


def _budget(max_bytes):
    if max_bytes is not None and (type(max_bytes) is not int or max_bytes < 0):
        raise ValueError("bundle byte budget must be a nonnegative integer")


def _read_bundle(archive, max_bytes=None):
    _budget(max_bytes)
    _inventory(archive)
    for item in archive.infolist():
        if (
            item.filename.rstrip("/").lower() == BUNDLE_PATH.lower()
            and item.filename != BUNDLE_PATH
        ):
            raise ValueError("noncanonical embedded provenance member name")
    try:
        info = archive.getinfo(BUNDLE_PATH)
    except KeyError:
        return None
    if max_bytes is not None and info.file_size > max_bytes:
        raise ValueError("embedded provenance exceeds caller byte budget")
    with archive.open(info) as stream:
        data = stream.read() if max_bytes is None else stream.read(max_bytes + 1)
    if max_bytes is not None and len(data) > max_bytes:
        raise ValueError("embedded provenance exceeds caller byte budget")
    return ProvenanceBundle.from_json(data)


def read_embedded_provenance(path, *, max_bytes=None):
    """Read validated proof with an optional caller-selected decompression budget."""
    with zipfile.ZipFile(path) as archive:
        result = _read_bundle(archive, max_bytes)
    if result is None:
        raise ValueError("archive has no embedded provenance bundle")
    return result


def _merge(existing, additions):
    if type(additions.version) is not int or additions.version != 1:
        raise ValueError("unsupported embedded provenance version")
    old, new = (
        existing.to_dict() if existing else ProvenanceBundle().to_dict(),
        additions.to_dict(),
    )
    records = {r["stage"]["manifest"]["stage_id"]: r for r in old["records"]}
    # Validate duplicate additions as well as collisions with existing proofs.
    seen = set()
    for record in new["records"]:
        key = record["stage"]["manifest"]["stage_id"]
        if key in seen or (key in records and records[key] != record):
            raise ValueError("cannot replace or duplicate an embedded stage")
        seen.add(key)
        records[key] = record
    old["records"] = list(records.values())
    for name in ("releases", "stage_evidence", "release_evidence"):
        for key, value in new[name].items():
            if key in old[name] and old[name][key] != value:
                raise ValueError(f"cannot replace embedded {name}")
            old[name][key] = value
    return ProvenanceBundle.from_json(_json(old))


def embed_provenance(path, additions, *, max_bytes=None):
    """Atomically add public proofs, preserving all existing records and content.

    Uses a temporary sibling ZIP and bounded copying. Requires exclusive writer
    access; detected source changes abort before replacement. No keys or trust
    configuration belong in this bundle. Packaging does not certify its contents.
    """
    _budget(max_bytes)
    path = Path(path).absolute()
    if path.is_symlink():
        raise ValueError("refusing to rewrite an archive through a symbolic link")
    initial = path.stat()
    temporary = None
    try:
        with zipfile.ZipFile(path) as source:
            bundle = _merge(_read_bundle(source, max_bytes), additions)
            encoded = bundle.to_json()
            if max_bytes is not None and len(encoded) > max_bytes:
                raise ValueError("embedded provenance exceeds caller byte budget")
            with tempfile.NamedTemporaryFile(
                dir=path.parent,
                prefix=".mzpeak-provenance-",
                suffix=".zip",
                delete=False,
            ) as output:
                temporary = Path(output.name)
                with zipfile.ZipFile(output, "w", allowZip64=True) as target:
                    target.comment = source.comment
                    for item in source.infolist():
                        if item.filename == BUNDLE_PATH:
                            continue
                        with (
                            source.open(item) as reader,
                            target.open(item, "w", force_zip64=True) as writer,
                        ):
                            shutil.copyfileobj(reader, writer, 65536)
                    target.writestr(
                        BUNDLE_PATH, encoded, compress_type=zipfile.ZIP_STORED
                    )
                output.flush()
                os.fsync(output.fileno())
        current = path.stat()
        if path.is_symlink() or (
            current.st_dev,
            current.st_ino,
            current.st_size,
            current.st_mtime_ns,
        ) != (initial.st_dev, initial.st_ino, initial.st_size, initial.st_mtime_ns):
            raise ValueError("archive changed while embedding provenance")
        os.chmod(temporary, initial.st_mode & 0o777)
        os.replace(temporary, path)
        temporary = None
        return bundle
    finally:
        if temporary is not None:
            temporary.unlink(missing_ok=True)
