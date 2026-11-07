"""Portable public proofs, atomic updates and Python/Rust interchange."""

from dataclasses import replace
import json
import os
import subprocess
from pathlib import Path
import shutil
import zipfile
from datetime import datetime, timedelta, timezone

import pytest

from mzpeak.embedded import (
    BUNDLE_PATH,
    ProvenanceBundle,
    embed_provenance,
    read_embedded_provenance,
)
from mzpeak.provenance_archive import capture_archive
from mzpeak.signing import OpenSslBackend, SignaturePolicy
from test_provenance_cli import command, invoke, success, write_json

ROOT = Path(__file__).resolve().parents[2]
DATA = ROOT / "test/data/provenance/timestamps/history"


def read(name):
    return json.loads((DATA / name).read_bytes())


def bundle():
    return ProvenanceBundle.from_json(
        json.dumps(
            {
                "version": 1,
                "records": read("records.json"),
                "releases": read("releases.json"),
                "stage_evidence": read("stage-evidence.json"),
                "release_evidence": read("release-evidence.json"),
            }
        ).encode()
    )


@pytest.fixture
def archive(tmp_path):
    path = tmp_path / "measurement.mzpeak"
    with zipfile.ZipFile(path, "w", compression=zipfile.ZIP_DEFLATED) as output:
        output.comment = b"preserved archive comment"
        for name, value in read("files.json").items():
            output.writestr(name, bytes.fromhex(value))
    return path


def test_embedding_preserves_content_and_adds_evidence_without_replacing_proofs(
    archive,
):
    before = capture_archive(archive, "run", "stage")
    full = bundle()
    first = ProvenanceBundle(full.records, full.releases)
    embed_provenance(archive, first)
    embed_provenance(
        archive,
        ProvenanceBundle(
            stage_evidence=full.stage_evidence, release_evidence=full.release_evidence
        ),
    )
    embed_provenance(
        archive, full
    )  # Exact repeats are idempotent, never duplicate ZIP names.
    assert read_embedded_provenance(archive).to_dict() == full.to_dict()
    assert capture_archive(archive, "run", "stage") == before
    with zipfile.ZipFile(archive) as source:
        assert source.namelist().count(BUNDLE_PATH) == 1
        assert source.comment == b"preserved archive comment"
    original = archive.read_bytes()
    conflicting = bundle()
    original_record = conflicting.records[0]
    conflicting.records[0] = replace(
        original_record,
        stage=replace(
            original_record.stage,
            manifest=replace(original_record.stage.manifest, run_id="different"),
        ),
    )
    with pytest.raises(ValueError, match="replace"):
        embed_provenance(archive, conflicting)
    assert archive.read_bytes() == original


def test_failed_commit_preserves_original_archive_and_cleans_temporary_file(
    archive, monkeypatch
):
    original = archive.read_bytes()

    def fail(*args):
        raise OSError("simulated replacement failure")

    monkeypatch.setattr("mzpeak.embedded.os.replace", fail)
    with pytest.raises(OSError, match="simulated"):
        embed_provenance(archive, bundle())
    assert archive.read_bytes() == original
    assert not list(archive.parent.glob(".mzpeak-provenance-*"))


@pytest.mark.parametrize("alias", ["Provenance/bundle-v1.json", BUNDLE_PATH + "/"])
def test_missing_duplicate_and_aliased_bundle_never_verifies(archive, alias):
    with pytest.raises(ValueError, match="no embedded"):
        read_embedded_provenance(archive)
    with zipfile.ZipFile(archive, "a") as output:
        output.writestr(alias, b"" if alias.endswith("/") else bundle().to_json())
    original = archive.read_bytes()
    with pytest.raises(ValueError, match="noncanonical"):
        embed_provenance(archive, bundle())
    assert archive.read_bytes() == original
    with zipfile.ZipFile(archive, "a") as output:
        output.writestr(BUNDLE_PATH, bundle().to_json())
    with pytest.raises(ValueError, match="duplicate or case-colliding"):
        read_embedded_provenance(archive)


def test_bundle_rejects_unknown_fields_duplicate_keys_and_unbound_evidence():
    raw = bundle().to_dict()
    with pytest.raises(ValueError):
        ProvenanceBundle.from_json(json.dumps(raw | {"trust": {}}).encode())
    encoded = bundle().to_json()
    with pytest.raises(ValueError, match="duplicate"):
        ProvenanceBundle.from_json(b'{"version":1,' + encoded[1:])
    raw["stage_evidence"]["acquisition"]["envelope_sha256"] = "0" * 64
    with pytest.raises(ValueError, match="bound"):
        ProvenanceBundle.from_json(json.dumps(raw).encode())


@pytest.mark.parametrize("version", [1, 2.0])
@pytest.mark.parametrize("record_kind", ["stage", "release"])
@pytest.mark.parametrize("operation", ["validate", "to_json", "embed"])
def test_direct_bundle_rejects_non_v2_envelopes(
    archive, version, record_kind, operation
):
    value = bundle()
    # Isolate envelope validation from the independent evidence binding checks.
    value.stage_evidence.clear()
    value.release_evidence.clear()
    if record_kind == "stage":
        record = value.records[0]
        value.records[0] = replace(
            record, envelope=replace(record.envelope, version=version)
        )
    else:
        digest = next(iter(value.releases))
        release = value.releases[digest]
        value.releases[digest] = replace(
            release, envelope=replace(release.envelope, version=version)
        )
    original = archive.read_bytes()
    with pytest.raises(ValueError, match="envelope"):
        if operation == "embed":
            embed_provenance(archive, value)
        else:
            getattr(value, operation)()
    assert archive.read_bytes() == original
    assert not list(archive.parent.glob(".mzpeak-provenance-*"))


def test_direct_v2_envelopes_round_trip_and_embed(archive):
    source = bundle()
    value = ProvenanceBundle(
        records=[
            replace(record, envelope=replace(record.envelope, version=2))
            for record in source.records
        ],
        releases={
            digest: replace(release, envelope=replace(release.envelope, version=2))
            for digest, release in source.releases.items()
        },
    )
    value.validate()
    assert ProvenanceBundle.from_json(value.to_json()).to_dict() == value.to_dict()
    embed_provenance(archive, value)
    assert read_embedded_provenance(archive).to_dict() == value.to_dict()


def test_rewrite_streams_content_and_enforces_bundle_limit(archive, monkeypatch):
    data = b"measurement" * 50000
    with zipfile.ZipFile(archive, "a") as output:
        output.writestr("large.bin", data)
    original_read = zipfile.ZipExtFile.read

    def bounded(self, size=-1):
        if self.name == "large.bin":
            assert 0 <= size <= 65536
        return original_read(self, size)

    with monkeypatch.context() as patch:
        patch.setattr(zipfile.ZipExtFile, "read", bounded)
        embed_provenance(archive, bundle())
    with zipfile.ZipFile(archive) as source:
        assert source.read("large.bin") == data
    original = archive.read_bytes()
    with pytest.raises(ValueError, match="caller byte budget"):
        embed_provenance(archive, bundle(), max_bytes=32)
    assert archive.read_bytes() == original


@pytest.mark.parametrize("language", ["python", "rust"])
@pytest.mark.parametrize(
    "policy,copies",
    [("ecdsa", 1), ("pqc", 1), ("both", 1), ("ecdsa", 2), ("pqc", 2), ("both", 2)],
)
def test_sign_archive_and_sign_into_archive_do_not_need_detached_outputs(
    language, archive, tmp_path, policy, copies
):
    from cryptography import x509
    from cryptography.hazmat.primitives import hashes, serialization
    from cryptography.hazmat.primitives.asymmetric import ec
    from cryptography.x509.oid import NameOID

    cli = command(language)
    key = ec.generate_private_key(ec.SECP256R1())
    name = x509.Name([x509.NameAttribute(NameOID.COMMON_NAME, "Embedded signing test")])
    now = datetime.now(timezone.utc)
    certificate = (
        x509.CertificateBuilder()
        .subject_name(name)
        .issuer_name(name)
        .public_key(key.public_key())
        .serial_number(123)
        .not_valid_before(now - timedelta(days=1))
        .not_valid_after(now + timedelta(days=1))
        .sign(key, hashes.SHA256())
    )
    key_path, cert_path = tmp_path / "key.pem", tmp_path / "certificate.pem"
    key_path.write_bytes(
        key.private_bytes(
            serialization.Encoding.PEM,
            serialization.PrivateFormat.PKCS8,
            serialization.NoEncryption(),
        )
    )
    cert_path.write_bytes(certificate.public_bytes(serialization.Encoding.PEM))
    config = write_json(
        tmp_path / "signers.json",
        [
            {
                "algorithm": "ecdsa-p256-sha256",
                "provider": {
                    "kind": "software",
                    "key_path": str(key_path),
                    "passphrase_file": None,
                },
                "certificate_path": str(cert_path),
                "chain_paths": [],
            }
        ],
    )
    if policy != "ecdsa":
        probe = subprocess.run(
            ["openssl", "list", "-signature-algorithms"], capture_output=True, text=True
        )
        if probe.returncode or "ML-DSA-65" not in probe.stdout:
            if os.environ.get("MZPEAK_REQUIRE_OPENSSL_PQC") == "1":
                pytest.fail("PQC qualification requires OpenSSL 3.5+")
            pytest.skip("ML-DSA backend requires OpenSSL 3.5+")
        pq_key, pq_cert = tmp_path / "pq.key", tmp_path / "pq.pem"
        subprocess.run(
            ["openssl", "genpkey", "-algorithm", "ML-DSA-65", "-out", str(pq_key)],
            check=True,
            capture_output=True,
        )
        subprocess.run(
            [
                "openssl",
                "req",
                "-new",
                "-x509",
                "-key",
                str(pq_key),
                "-out",
                str(pq_cert),
                "-subj",
                "/CN=Embedded PQC test",
                "-days",
                "1",
            ],
            check=True,
            capture_output=True,
        )
        signers = json.loads(config.read_text()) if policy == "both" else []
        signers.append(
            {
                "algorithm": "ml-dsa-65",
                "provider": {
                    "kind": "software",
                    "key_path": str(pq_key),
                    "passphrase_file": None,
                },
                "certificate_path": str(pq_cert),
                "chain_paths": [],
            }
        )
        write_json(config, signers)
    if copies == 2:
        signers = json.loads(config.read_text())
        for index, original in enumerate(list(signers)):
            key, cert = tmp_path / f"extra-{index}.key", tmp_path / f"extra-{index}.crt"
            parameters = (
                ["EC", "-pkeyopt", "ec_paramgen_curve:P-256"]
                if original["algorithm"] == "ecdsa-p256-sha256"
                else ["ML-DSA-65"]
            )
            subprocess.run(
                ["openssl", "genpkey", "-algorithm", *parameters, "-out", str(key)],
                check=True,
                capture_output=True,
            )
            subprocess.run(
                [
                    "openssl",
                    "req",
                    "-new",
                    "-x509",
                    "-key",
                    str(key),
                    "-out",
                    str(cert),
                    "-subj",
                    "/CN=Additional embedded signer",
                    "-days",
                    "1",
                ],
                check=True,
                capture_output=True,
            )
            signers.append(
                {
                    "algorithm": original["algorithm"],
                    "provider": {
                        "kind": "software",
                        "key_path": str(key),
                        "passphrase_file": None,
                    },
                    "certificate_path": str(cert),
                    "chain_paths": [],
                }
            )
        write_json(config, signers)
    second = tmp_path / "second.mzpeak"
    shutil.copyfile(archive, second)
    paths_before = set(tmp_path.iterdir())
    success(
        cli,
        "sign-archive",
        archive,
        "--run-id",
        "portable",
        "--stage-id",
        "acquisition",
        "--signers",
        config,
        "--policy",
        policy,
    )
    assert set(tmp_path.iterdir()) == paths_before
    record = read_embedded_provenance(archive).records[0]
    assert len(
        OpenSslBackend().verify(
            record.stage.canonical_bytes(), record.envelope, SignaturePolicy(policy)
        )
    ) == copies * (2 if policy == "both" else 1)
    assert b"PRIVATE KEY" not in read_embedded_provenance(archive).to_json()
    stage = write_json(tmp_path / "stage.json", record.stage.to_dict())
    success(
        cli,
        "sign",
        stage,
        "--archive",
        second,
        "--signers",
        config,
        "--policy",
        policy,
    )
    assert len(read_embedded_provenance(second).records) == 1
    release = write_json(
        tmp_path / "release.json",
        {"name": "processor", "version": "1", "release_sha256": "a" * 64},
    )
    success(
        cli,
        "sign-release",
        release,
        "--archive",
        second,
        "--signers",
        config,
        "--policy",
        policy,
    )
    approval = read_embedded_provenance(second).releases["a" * 64]
    assert OpenSslBackend().verify(
        approval.statement.canonical_bytes(), approval.envelope, SignaturePolicy(policy)
    )


@pytest.mark.parametrize("language", ["python", "rust"])
def test_crc_failure_during_embedding_keeps_original_bytes(language, archive):
    cli = command(language)
    files = {name: bytes.fromhex(value) for name, value in read("files.json").items()}
    with zipfile.ZipFile(archive, "w", compression=zipfile.ZIP_STORED) as output:
        for name, content in files.items():
            output.writestr(name, content)
    damaged = bytearray(archive.read_bytes())
    offset = damaged.index(files["raw.bin"])
    damaged[offset] ^= 1
    archive.write_bytes(damaged)
    result = invoke(cli, "embed", archive, "--records", DATA / "records.json")
    assert result.returncode != 0
    assert archive.read_bytes() == damaged
    assert not list(archive.parent.glob(".mzpeak-provenance-*"))


@pytest.mark.parametrize("language", ["python", "rust"])
def test_default_current_verification_discovers_embedded_history(language, tmp_path):
    cli = command(language)
    data = ROOT / "test/data/provenance/history"
    archive = tmp_path / "current.mzpeak"
    files = {
        k: bytes.fromhex(v)
        for k, v in json.loads((data / "files.json").read_bytes()).items()
    }
    with zipfile.ZipFile(archive, "w") as output:
        for name, content in files.items():
            output.writestr(name, content)
    success(
        cli,
        "embed",
        archive,
        "--records",
        data / "records.json",
        "--releases",
        data / "releases.json",
    )
    identities = write_json(
        tmp_path / "identities.json",
        {"acquisition": "instrument-1", "processing": "installation-1"},
    )
    releases = json.loads((data / "releases.json").read_bytes())
    publishers = write_json(
        tmp_path / "publishers.json", {key: "publisher-1" for key in releases}
    )
    args = [
        "verify",
        archive,
        "--trust",
        data / "trust.json",
        "--identities",
        identities,
        "--publishers",
        publishers,
        "--heads",
        data / "heads.json",
        "--policy",
        "ecdsa",
    ]
    report = success(cli, *args)
    assert report["accepted"] and report["verification"] == "current"
    with zipfile.ZipFile(archive) as source:
        saved = source.read(BUNDLE_PATH)
    with zipfile.ZipFile(archive, "w") as output:
        for name, content in files.items():
            output.writestr(name, content + (b"tampered" if name == "raw.bin" else b""))
        output.writestr(BUNDLE_PATH, saved)
    changed = invoke(cli, *args)
    assert changed.returncode == 1
    assert not json.loads(changed.stdout)["accepted"]


@pytest.mark.parametrize(
    "producer,consumer",
    [("python", "python"), ("python", "rust"), ("rust", "python"), ("rust", "rust")],
)
def test_move_only_archive_and_verify_historical_proof_across_languages(
    producer, consumer, archive, tmp_path
):
    writer, reader = command(producer), command(consumer)
    success(
        writer,
        "embed",
        archive,
        "--records",
        DATA / "records.json",
        "--releases",
        DATA / "releases.json",
        "--stage-evidence",
        DATA / "stage-evidence.json",
        "--release-evidence",
        DATA / "release-evidence.json",
    )
    moved = tmp_path / "recipient" / "renamed.mzpeak"
    moved.parent.mkdir()
    shutil.move(archive, moved)
    assert list(moved.parent.iterdir()) == [moved]
    identities = write_json(
        tmp_path / "identities.json",
        {"acquisition": "instrument-1", "processing": "installation-1"},
    )
    publishers = write_json(
        tmp_path / "publishers.json",
        {key: "publisher-1" for key in read("releases.json")},
    )
    args = [
        "verify",
        moved,
        "--trust",
        DATA / "trust.json",
        "--identities",
        identities,
        "--publishers",
        publishers,
        "--heads",
        DATA / "heads.json",
        "--policy",
        "ecdsa",
        "--tsa-policy",
        DATA / "tsa-policy.json",
        "--evidence-policy-id",
        "historical-demo-v1",
        "--evidence-policy",
        DATA / "policy.json",
    ]
    report = success(reader, *args)
    assert report["accepted"] and report["verification"] == "historical"
    assert len(report["report"]["stage_timestamps"]) == 2
    assert len(report["report"]["release_timestamps"]) == 1
    # Configuration still comes from the recipient, even with complete embedded certificates.
    wrong = write_json(
        tmp_path / "wrong-trust.json", read("trust.json") | {"roots_pem": []}
    )
    wrong_args = list(args)
    wrong_args[wrong_args.index("--trust") + 1] = wrong
    assert invoke(reader, *wrong_args).returncode != 0
    # Removing the embedded proofs must not turn verification into an integrity-only success.
    with zipfile.ZipFile(moved) as source:
        files = {
            name: source.read(name) for name in source.namelist() if name != BUNDLE_PATH
        }
    with zipfile.ZipFile(moved, "w") as output:
        for name, content in files.items():
            output.writestr(name, content)
    assert invoke(reader, *args).returncode != 0


def test_large_embedded_input_and_explicit_resource_budget(archive):
    with zipfile.ZipFile(archive, "a", compression=zipfile.ZIP_DEFLATED) as output:
        output.writestr(BUNDLE_PATH, b" " * (64 * 1024 * 1024 + 1) + bundle().to_json())
    with pytest.raises(ValueError, match="caller byte budget"):
        read_embedded_provenance(archive, max_bytes=64 * 1024 * 1024)
    assert len(read_embedded_provenance(archive).records) == len(bundle().records)
    previous = archive.read_bytes()
    with pytest.raises(ValueError, match="caller byte budget"):
        embed_provenance(archive, bundle(), max_bytes=32)
    assert archive.read_bytes() == previous
    embed_provenance(archive, bundle())
    assert read_embedded_provenance(archive, max_bytes=len(bundle().to_json())).records
