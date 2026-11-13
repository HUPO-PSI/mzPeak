"""Exercise both user-facing CLIs, including cross-language dual signing."""

import json
import os
from pathlib import Path
import subprocess
import sys
import time
import zipfile
from datetime import datetime, timezone

import pytest

from mzpeak.manifest import Manifest
from mzpeak.stages import Stage


ROOT = Path(__file__).resolve().parents[2]
HISTORY = ROOT / "test/data/provenance/history"
PYTHON = [sys.executable, "-m", "mzpeak.provenance_cli"]


def command(language):
    if language == "python":
        return PYTHON
    binary = os.environ.get("MZPEAK_TEST_RUST_PROVENANCE")
    if not binary:
        pytest.skip("Rust CLI integration requires MZPEAK_TEST_RUST_PROVENANCE")
    assert Path(binary).is_file(), "configured Rust CLI must exist"
    return [binary]


def invoke(cli, *args):
    return subprocess.run(
        [*cli, *map(str, args)], cwd=ROOT, capture_output=True, text=True, timeout=30
    )


def success(cli, *args):
    result = invoke(cli, *args)
    assert result.returncode == 0, result.stderr
    return json.loads(result.stdout)


def write_json(path, value):
    path.write_text(json.dumps(value), encoding="utf-8")
    return path


@pytest.fixture(params=["expired-certificate", "stale-crl"])
def stale_policy_history(tmp_path, request):
    """A real signature whose copied 2000-era policy used to bypass current time."""
    from cryptography import x509
    from cryptography.hazmat.primitives import hashes, serialization
    from cryptography.hazmat.primitives.asymmetric import ec
    from cryptography.x509.oid import NameOID
    from mzpeak.provenance_archive import capture_archive
    from mzpeak.signing import (
        Algorithm,
        OpenSslBackend,
        SignaturePolicy,
        SigningIdentity,
        SoftwareKey,
    )
    from mzpeak.trust import certificate_fingerprint

    beginning = datetime(2000, 1, 1, tzinfo=timezone.utc)
    old_expiry = datetime(2001, 1, 1, tzinfo=timezone.utc)
    future_expiry = datetime(2100, 1, 1, tzinfo=timezone.utc)
    fixed_time = int(datetime(2000, 6, 1, tzinfo=timezone.utc).timestamp())
    root_key, leaf_key = (ec.generate_private_key(ec.SECP256R1()) for _ in range(2))
    root_name = x509.Name([x509.NameAttribute(NameOID.COMMON_NAME, "CLI time test CA")])
    leaf_name = x509.Name(
        [x509.NameAttribute(NameOID.COMMON_NAME, "CLI time test signer")]
    )

    def certificate(subject, public_key, serial, ca, expires):
        return (
            x509.CertificateBuilder()
            .subject_name(subject)
            .issuer_name(root_name)
            .public_key(public_key)
            .serial_number(serial)
            .not_valid_before(beginning)
            .not_valid_after(expires)
            .add_extension(
                x509.BasicConstraints(ca=ca, path_length=0 if ca else None), True
            )
            .add_extension(
                x509.KeyUsage(
                    digital_signature=not ca,
                    content_commitment=False,
                    key_encipherment=False,
                    data_encipherment=False,
                    key_agreement=False,
                    key_cert_sign=ca,
                    crl_sign=ca,
                    encipher_only=None,
                    decipher_only=None,
                ),
                True,
            )
            .add_extension(x509.SubjectKeyIdentifier.from_public_key(public_key), False)
            .add_extension(
                x509.AuthorityKeyIdentifier.from_issuer_public_key(
                    root_key.public_key()
                ),
                False,
            )
            .sign(root_key, hashes.SHA256())
        )

    root = certificate(root_name, root_key.public_key(), 1, True, future_expiry)
    leaf = certificate(
        leaf_name,
        leaf_key.public_key(),
        2,
        False,
        old_expiry if request.param == "expired-certificate" else future_expiry,
    )
    crl = (
        x509.CertificateRevocationListBuilder()
        .issuer_name(root_name)
        .last_update(beginning)
        .next_update(old_expiry if request.param == "stale-crl" else future_expiry)
        .add_extension(
            x509.AuthorityKeyIdentifier.from_issuer_public_key(root_key.public_key()),
            False,
        )
        .sign(root_key, hashes.SHA256())
    )
    pem = lambda value: value.public_bytes(serialization.Encoding.PEM).decode("ascii")
    key = tmp_path / "expired-key.pem"
    key.write_bytes(
        leaf_key.private_bytes(
            serialization.Encoding.PEM,
            serialization.PrivateFormat.PKCS8,
            serialization.NoEncryption(),
        )
    )
    path = tmp_path / "stale-policy.mzpeak"
    with zipfile.ZipFile(path, "w") as archive:
        archive.writestr("measurement.bin", b"original measurement")
    stage = Stage(capture_archive(path, "time-test", "acquisition"), "acquisition")
    envelope = OpenSslBackend().sign(
        stage.canonical_bytes(),
        [SigningIdentity(Algorithm.ECDSA_P256_SHA256, SoftwareKey(key), pem(leaf))],
        SignaturePolicy.ECDSA,
    )
    record = write_json(
        tmp_path / "record.json",
        {
            "stage": stage.to_dict(),
            "envelope": json.loads(envelope.to_json()),
        },
    )
    trust = write_json(
        tmp_path / "trust.json",
        {
            "roots_pem": [pem(root)],
            "crls_pem": [pem(crl)],
            "validation_time": fixed_time,
            "authorizations": [
                {
                    "certificate_sha256": certificate_fingerprint(pem(leaf)),
                    "role": "acquisition",
                    "identity": "instrument-1",
                }
            ],
        },
    )
    identities = write_json(
        tmp_path / "identities.json", {"acquisition": "instrument-1"}
    )
    args = [
        "verify",
        path,
        record,
        "--trust",
        trust,
        "--identities",
        identities,
        "--policy",
        "ecdsa",
    ]
    return args, fixed_time, request.param


@pytest.mark.parametrize("language", ["python", "rust"])
def test_current_ignores_stale_policy_time(language, stale_policy_history):
    args, _, failure = stale_policy_history
    started = int(time.time())
    result = invoke(command(language), *args)
    ended = int(time.time())
    assert result.returncode == 1, result.stderr
    report = json.loads(result.stdout)
    assert not report["accepted"]
    assert report["verification"] == "current" and report["time_source"] == "current"
    assert started <= report["validation_time"] <= ended
    certificate = report["report"]["stages"][0]["certificates"][0]
    assert certificate["validation_time"] == report["validation_time"]
    if failure == "expired-certificate":
        assert certificate["path"] == "invalid"
    else:
        assert certificate["path"] == "valid" and certificate["revocation"] == "invalid"


@pytest.mark.parametrize("language", ["python", "rust"])
def test_fixed_time_requires_explicit_opt_in(language, stale_policy_history):
    args, fixed_time, _ = stale_policy_history
    report = success(command(language), *args, "--validation-time", fixed_time)
    assert report["accepted"] and report["verification"] == "fixed-time"
    assert (
        report["time_source"] == "explicit" and report["validation_time"] == fixed_time
    )
    # Current-mode policy files need not carry any frozen reference time.
    trust_path = args[args.index("--trust") + 1]
    policy = json.loads(trust_path.read_text())
    policy.pop("validation_time")
    write_json(trust_path, policy)
    assert success(command(language), *args, "--validation-time", fixed_time)[
        "accepted"
    ]


@pytest.mark.parametrize("language", ["python", "rust"])
@pytest.mark.parametrize("value", ["-1", "1.5", "9223372036854775808"])
def test_cli_rejects_invalid_explicit_time(language, archive, value):
    _, _, args = archive
    args = args[:-2]  # Remove the fixture's valid explicit test time.
    result = invoke(command(language), *args, "--validation-time", value)
    assert result.returncode == 2


@pytest.fixture
def archive(tmp_path):
    files = {
        name: bytes.fromhex(data)
        for name, data in json.loads((HISTORY / "files.json").read_text()).items()
    }
    archive = tmp_path / "history.mzpeak"
    with zipfile.ZipFile(archive, "w") as writer:
        for name, content in files.items():
            writer.writestr(name, content)
    identities = write_json(
        tmp_path / "identities.json",
        {"acquisition": "instrument-1", "processing": "installation-1"},
    )
    releases = json.loads((HISTORY / "releases.json").read_text())
    publishers = write_json(
        tmp_path / "publishers.json", {key: "publisher-1" for key in releases}
    )
    args = [
        "verify",
        archive,
        HISTORY / "records.json",
        "--trust",
        HISTORY / "trust.json",
        "--identities",
        identities,
        "--releases",
        HISTORY / "releases.json",
        "--publishers",
        publishers,
        "--heads",
        HISTORY / "heads.json",
        "--policy",
        "ecdsa",
        "--validation-time",
        json.loads((HISTORY / "trust.json").read_text())["validation_time"],
    ]
    return archive, files, args


@pytest.mark.parametrize("language", ["python", "rust"])
def test_cli_accepts_complete_history_and_rejects_changed_bytes(language, archive):
    cli = command(language)
    path, files, args = archive
    report = success(cli, *args)
    assert report["accepted"] is True
    assert (
        report["verification"] == "fixed-time" and report["time_source"] == "explicit"
    )
    assert report["report"]["lineage"]["checkpoint_checked"] is True
    with zipfile.ZipFile(path, "w") as writer:
        for name, content in files.items():
            writer.writestr(name, content + (b"tampered" if name == "raw.bin" else b""))
    result = invoke(cli, *args)
    assert result.returncode == 1, result.stderr
    report = json.loads(result.stdout)
    assert report["accepted"] is False
    assert "raw.bin" in report["report"]["coverage"]["changed"]


@pytest.mark.parametrize("language", ["python", "rust"])
def test_cli_capture_acquisition_and_no_overwrite(language, archive, tmp_path):
    cli = command(language)
    path, _, _ = archive
    manifest = tmp_path / "manifest.json"
    stage = tmp_path / "stage.json"
    captured = success(
        cli,
        "capture",
        path,
        manifest,
        "--run-id",
        "run-demo",
        "--stage-id",
        "acquisition",
    )
    loaded = Manifest.from_bytes(manifest.read_bytes())
    assert captured["manifest_sha256"] == loaded.digest()
    initialized = success(cli, "init-acquisition", manifest, stage)
    assert initialized["stage_sha256"] == Stage.from_bytes(stage.read_bytes()).digest()
    previous = manifest.read_bytes()
    refused = invoke(
        cli,
        "capture",
        path,
        manifest,
        "--run-id",
        "different",
        "--stage-id",
        "different",
    )
    assert refused.returncode != 0
    assert manifest.read_bytes() == previous
    previous = stage.read_bytes()
    assert invoke(cli, "init-acquisition", manifest, stage).returncode != 0
    assert stage.read_bytes() == previous


@pytest.mark.parametrize("language", ["python", "rust"])
def test_cli_duplicate_local_configuration_is_rejected(language, archive, tmp_path):
    cli = command(language)
    _, _, args = archive
    trust = tmp_path / "ambiguous-trust.json"
    original = (HISTORY / "trust.json").read_text()
    trust.write_text('{"validation_time":1893456000,' + original.lstrip()[1:])
    args[args.index("--trust") + 1] = trust
    result = invoke(cli, *args)
    assert result.returncode != 0
    assert "duplicate" in result.stderr.lower()


def test_capture_and_acquisition_canonical_bytes_match_between_clis(archive, tmp_path):
    rust = command("rust")
    path, _, _ = archive
    outputs = []
    for name, cli in [("python", PYTHON), ("rust", rust)]:
        manifest, stage = (
            tmp_path / f"{name}-manifest.json",
            tmp_path / f"{name}-stage.json",
        )
        success(
            cli,
            "capture",
            path,
            manifest,
            "--run-id",
            "interop",
            "--stage-id",
            "acquisition",
        )
        success(cli, "init-acquisition", manifest, stage)
        outputs.append(
            (
                Manifest.from_bytes(manifest.read_bytes()).canonical_bytes(),
                Stage.from_bytes(stage.read_bytes()).canonical_bytes(),
            )
        )
    assert outputs[0] == outputs[1]


@pytest.mark.parametrize(
    "policy,selected",
    [
        ("ecdsa", [0]),
        ("pqc", [1]),
        ("both", [0, 1]),
        ("ecdsa", [0, 2]),
        ("pqc", [1, 3]),
        ("both", [0, 1, 2, 3]),
    ],
)
def test_cms_signatures_interoperate_in_both_directions(
    archive, tmp_path, policy, selected
):
    rust = command("rust")
    path, _, _ = archive
    probe = subprocess.run(
        ["openssl", "list", "-signature-algorithms"], capture_output=True, text=True
    )
    if probe.returncode or "ML-DSA-65" not in probe.stdout:
        if os.environ.get("MZPEAK_REQUIRE_OPENSSL_PQC") == "1":
            pytest.fail("PQC CLI qualification requires OpenSSL 3.5+")
        pytest.skip("ML-DSA backend requires OpenSSL 3.5+")
    manifest, stage = tmp_path / "manifest.json", tmp_path / "stage.json"
    success(
        PYTHON,
        "capture",
        path,
        manifest,
        "--run-id",
        "interop",
        "--stage-id",
        "acquisition",
    )
    success(PYTHON, "init-acquisition", manifest, stage)
    signers = []
    for index, (algorithm, parameters) in enumerate(
        [
            ("ecdsa-p256-sha256", ["EC", "-pkeyopt", "ec_paramgen_curve:P-256"]),
            ("ml-dsa-65", ["ML-DSA-65"]),
        ]
        * 2
    ):
        key, certificate = (
            tmp_path / f"{index}-{algorithm}.key",
            tmp_path / f"{index}-{algorithm}.crt",
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
                str(certificate),
                "-days",
                "1",
                "-subj",
                "/CN=untrusted CLI interoperability leaf",
                "-addext",
                "basicConstraints=critical,CA:FALSE",
                "-addext",
                "keyUsage=critical,digitalSignature",
            ],
            check=True,
            capture_output=True,
        )
        signers.append(
            {
                "algorithm": algorithm,
                "provider": {
                    "kind": "software",
                    "key_path": key.name,
                    "passphrase_file": None,
                },
                "certificate_path": certificate.name,
                "chain_paths": [],
            }
        )
    config = write_json(tmp_path / "signers.json", [signers[i] for i in selected])
    identities = write_json(
        tmp_path / "identities.json", {"acquisition": "instrument-1"}
    )
    for name, signer_cli, verifier_cli in [
        ("python", PYTHON, rust),
        ("rust", rust, PYTHON),
    ]:
        record = tmp_path / f"{name}-signed.json"
        success(
            signer_cli, "sign", stage, record, "--signers", config, "--policy", policy
        )
        result = invoke(
            verifier_cli,
            "verify",
            path,
            record,
            "--trust",
            HISTORY / "trust.json",
            "--identities",
            identities,
            "--policy",
            policy,
        )
        assert result.returncode == 1, result.stderr
        report = json.loads(result.stdout)
        assert (
            report["accepted"] is False
        )  # Self-signed leaf certificates are not trusted.
        assert report["report"]["stages"][0]["signature"] == "valid"
        assert len(report["report"]["stages"][0]["certificates"]) == len(selected)
        previous = record.read_bytes()
        assert (
            invoke(
                signer_cli,
                "sign",
                stage,
                record,
                "--signers",
                config,
                "--policy",
                policy,
            ).returncode
            != 0
        )
        assert record.read_bytes() == previous


@pytest.mark.parametrize("language", ["python", "rust"])
def test_cli_metadata_input_larger_than_64_mib(language, archive, tmp_path):
    cli = command(language)
    path, _, _ = archive
    manifest, stage = tmp_path / "large-manifest.json", tmp_path / "stage.json"
    success(
        cli,
        "capture",
        path,
        manifest,
        "--run-id",
        "large-input",
        "--stage-id",
        "acquisition",
    )
    # JSON whitespace is legitimate input; canonicalization removes it afterward.
    with manifest.open("ab") as stream:
        stream.write(b" " * (64 * 1024 * 1024 + 1))
    success(cli, "init-acquisition", manifest, stage)
    assert Stage.from_bytes(stage.read_bytes()).manifest.run_id == "large-input"
