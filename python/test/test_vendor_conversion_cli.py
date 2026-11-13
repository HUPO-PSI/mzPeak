"""Real vendor conversion proofs, policy boundaries and both reference CLIs."""

from datetime import datetime, timezone
import hashlib
import json
import subprocess
import time
import zipfile
from pathlib import Path

import pytest

from mzpeak.conversion import SourceManifest
from mzpeak.stages import Stage
from test_provenance_cli import command, invoke, success, write_json
from test_signing import identities  # noqa: F401

ROOT = Path(__file__).resolve().parents[2]
DATA = ROOT / "test/data/provenance/conversion"


def read(name):
    return json.loads((DATA / name).read_bytes())


@pytest.fixture
def conversion_case(tmp_path):
    archive = tmp_path / "converted.mzpeak"
    with zipfile.ZipFile(archive, "w") as z:
        for path, data in read("files.json").items():
            z.writestr(path, bytes.fromhex(data))
    raw = tmp_path / "raw"
    raw.mkdir()
    for name, data in read("raw-files.json").items():
        path = raw / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(bytes.fromhex(data))
    policy = read("conversion-policy.json")
    policy["sources"] = {"conversion": "raw"}
    policy["require_source_bytes"] = True
    policy_path = write_json(tmp_path / "conversion-policy.json", policy)
    args = [
        "verify",
        archive,
        DATA / "records.json",
        "--trust",
        DATA / "trust.json",
        "--identities",
        DATA / "identities.json",
        "--releases",
        DATA / "releases.json",
        "--publishers",
        DATA / "publishers.json",
        "--heads",
        DATA / "heads.json",
        "--conversion-policy",
        policy_path,
        "--policy",
        "ecdsa",
        "--validation-time",
        read("trust.json")["validation_time"],
    ]
    return archive, raw, policy_path, args


def rejected(cli, args):
    result = invoke(cli, *args)
    assert result.returncode == 1, result.stderr
    report = json.loads(result.stdout)
    assert not report["accepted"]
    return report["report"]


@pytest.mark.parametrize("language", ["python", "rust"])
def test_real_conversion_root_processing_and_raw_policy(language, conversion_case):
    _, raw, policy_path, args = conversion_case
    cli = command(language)
    report = success(cli, *args)["report"]
    assert not report["lineage"]["acquisitions"]
    assert len(report["lineage"]["conversions"]) == 1
    conversion = report["stages"][0]
    assert conversion["kind"] == "conversion"
    assert conversion["conversion"]["vendor_attestation"] == "valid"
    assert conversion["conversion"]["source_integrity"] == "valid"
    policy = json.loads(policy_path.read_text())
    policy["sources"] = {}
    policy["require_source_bytes"] = False
    write_json(policy_path, policy)
    report = success(cli, *args)["report"]
    assert report["stages"][0]["conversion"]["source_integrity"] == "unknown"
    policy["require_source_bytes"] = True
    write_json(policy_path, policy)
    rejected(cli, args)
    policy["require_source_bytes"] = False
    policy["sources"] = {"conversion": "raw"}
    write_json(policy_path, policy)
    (raw / "unlisted.bin").write_bytes(b"not the attested file set")
    report = rejected(cli, args)
    assert report["stages"][0]["conversion"]["source_integrity"] == "invalid"
    args = list(args)
    index = args.index("--conversion-policy")
    del args[index : index + 2]
    rejected(cli, args)


@pytest.mark.parametrize("language", ["python", "rust"])
@pytest.mark.parametrize(
    "denial", ["role", "vendor", "format", "method", "verifier_version"]
)
def test_vendor_authority_is_separate_and_locally_constrained(
    language, denial, conversion_case, tmp_path
):
    _, _, policy_path, args = conversion_case
    args = list(args)
    if denial == "role":
        trust = read("trust.json")
        for grant in trust["authorizations"]:
            if grant["role"] == "vendor_conversion":
                grant["role"] = "processing"
        path = write_json(tmp_path / "wrong-role.json", trust)
        args[args.index("--trust") + 1] = path
    else:
        policy = json.loads(policy_path.read_text())
        policy["allowed_verifications"][0][denial] = "not-authorized"
        write_json(policy_path, policy)
    report = rejected(command(language), args)
    assert report["stages"][0]["signature"] == "valid"


@pytest.mark.parametrize("language", ["python", "rust"])
@pytest.mark.parametrize("source_stage", ["misspelled-conversion", "processing"])
def test_cli_rejects_unused_raw_source_targets(language, source_stage, conversion_case):
    _, _, policy_path, args = conversion_case
    policy = json.loads(policy_path.read_text())
    policy["require_source_bytes"] = False
    policy["sources"] = {source_stage: "does-not-exist"}
    write_json(policy_path, policy)
    result = invoke(command(language), *args)
    assert result.returncode == 2, result.stdout
    assert json.loads(result.stderr)["accepted"] is False


@pytest.mark.parametrize("language", ["python", "rust"])
@pytest.mark.parametrize("member", ["converted.bin", "validation/vendor-report.json"])
def test_attested_source_does_not_override_changed_outputs(
    language, member, conversion_case
):
    archive, _, _, args = conversion_case
    with zipfile.ZipFile(archive) as z:
        files = {name: z.read(name) for name in z.namelist()}
    files[member] += b" changed after attestation"
    with zipfile.ZipFile(archive, "w") as z:
        for name, content in files.items():
            z.writestr(name, content)
    report = rejected(command(language), args)
    assert member in report["coverage"]["changed"]
    assert report["stages"][0]["conversion"]["source_integrity"] == "valid"


@pytest.mark.parametrize("producer,consumer", [("python", "rust"), ("rust", "python")])
def test_embedded_conversion_historical_interchange(
    producer, consumer, conversion_case
):
    archive, _, _, args = conversion_case
    success(
        command(producer),
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
    args = list(args)
    args.remove(DATA / "records.json")
    index = args.index("--releases")
    del args[index : index + 2]
    args[args.index("--validation-time") + 1] = read("tsa-policy.json")[
        "validation_time"
    ]
    # At the later reference time the ordinary certificate check fails, regardless
    # of the much older acquisition date asserted inside the RAW-derived claim.
    current = rejected(command(consumer), args)
    assert current["stages"][0]["signature"] == "valid"
    args += [
        "--tsa-policy",
        DATA / "tsa-policy.json",
        "--evidence-policy-id",
        read("stage-evidence.json")["conversion"]["policy_id"],
        "--evidence-policy",
        DATA / "policy.json",
    ]
    historical = success(command(consumer), *args)
    assert historical["verification"] == "historical"
    report = historical["report"]
    certificate = report["history"]["stages"][0]["certificates"][0]
    claimed = read("stage.json")["conversion"]["claimed_acquisition_time"]
    assert certificate["validation_time"] != claimed
    assert certificate["validation_time"] < historical["validation_time"]
    assert report["history"]["stages"][0]["conversion"]["source_integrity"] == "valid"


def test_source_capture_and_reordered_conversion_canonical_bytes_interoperate(
    conversion_case, tmp_path
):
    archive, raw, _, _ = conversion_case
    values = []
    for language in ["python", "rust"]:
        cli = command(language)
        source = tmp_path / f"{language}-source.json"
        captured = success(
            cli,
            "capture-source",
            raw,
            source,
            "--format",
            read("source.json")["format"],
        )
        assert (
            captured["source_sha256"]
            == SourceManifest.from_dict(read("source.json")).digest()
        )
        loaded = json.loads(source.read_text())
        loaded["files"].reverse()
        write_json(source, loaded)
        manifest = write_json(
            tmp_path / f"{language}-manifest.json", read("stage.json")["manifest"]
        )
        stage = tmp_path / f"{language}-stage.json"
        result = success(
            cli,
            "init-conversion",
            manifest,
            source,
            stage,
            "--vendor-verification",
            DATA / "verification.json",
            "--software",
            DATA / "software.json",
            "--claimed-acquisition-time",
            read("stage.json")["conversion"]["claimed_acquisition_time"],
        )
        value = Stage.from_bytes(stage.read_bytes())
        assert result["stage_sha256"] == value.digest()
        values.append(value.canonical_bytes())
        before = source.read_bytes()
        assert (
            invoke(
                cli, "capture-source", raw, source, "--format", loaded["format"]
            ).returncode
            == 2
        )
        assert source.read_bytes() == before
    assert (
        values[0] == values[1] == Stage.from_dict(read("stage.json")).canonical_bytes()
    )


@pytest.fixture(scope="module")
def trusted_interop_signers(identities, tmp_path_factory):
    """Issue actual ECDSA/ML-DSA leaves under one explicit test trust domain."""
    from cryptography import x509
    from cryptography.hazmat.primitives import hashes, serialization
    from cryptography.hazmat.primitives.asymmetric import ec
    from cryptography.x509.oid import NameOID
    from mzpeak.trust import certificate_fingerprint

    directory = tmp_path_factory.mktemp("vendor-interop-pki")
    key = ec.generate_private_key(ec.SECP256R1())
    name = x509.Name(
        [x509.NameAttribute(NameOID.COMMON_NAME, "TEST ONLY vendor interop CA")]
    )
    start, end = (
        datetime(2020, 1, 1, tzinfo=timezone.utc),
        datetime(2100, 1, 1, tzinfo=timezone.utc),
    )
    root = (
        x509.CertificateBuilder()
        .subject_name(name)
        .issuer_name(name)
        .public_key(key.public_key())
        .serial_number(100)
        .not_valid_before(start)
        .not_valid_after(end)
        .add_extension(x509.BasicConstraints(ca=True, path_length=0), True)
        .add_extension(
            x509.KeyUsage(False, False, False, False, False, True, True, False, False),
            True,
        )
        .add_extension(
            x509.SubjectKeyIdentifier.from_public_key(key.public_key()), False
        )
        .add_extension(
            x509.AuthorityKeyIdentifier.from_issuer_public_key(key.public_key()), False
        )
        .sign(key, hashes.SHA256())
    )
    crl = (
        x509.CertificateRevocationListBuilder()
        .issuer_name(name)
        .last_update(start)
        .next_update(end)
        .add_extension(
            x509.AuthorityKeyIdentifier.from_issuer_public_key(key.public_key()), False
        )
        .sign(key, hashes.SHA256())
    )
    pem = lambda v: v.public_bytes(serialization.Encoding.PEM).decode("ascii")
    root_path, key_path = directory / "root.pem", directory / "root.key"
    root_path.write_text(pem(root))
    key_path.write_bytes(
        key.private_bytes(
            serialization.Encoding.PEM,
            serialization.PrivateFormat.PKCS8,
            serialization.NoEncryption(),
        )
    )
    extensions = directory / "leaf.ext"
    extensions.write_text(
        "basicConstraints=critical,CA:FALSE\nkeyUsage=critical,digitalSignature\nextendedKeyUsage=codeSigning\nsubjectKeyIdentifier=hash\nauthorityKeyIdentifier=keyid,issuer\n"
    )
    configs, grants = [], []
    for i, identity in enumerate(identities, 1):
        request, cert = directory / f"leaf-{i}.csr", directory / f"leaf-{i}.pem"
        for args in [
            [
                "req",
                "-new",
                "-key",
                identity.key.key_path,
                "-subj",
                f"/CN=TEST ONLY vendor signer{i}",
                "-out",
                request,
            ],
            [
                "x509",
                "-req",
                "-in",
                request,
                "-CA",
                root_path,
                "-CAkey",
                key_path,
                "-set_serial",
                str(i),
                "-days",
                "7300",
                "-extfile",
                extensions,
                "-out",
                cert,
            ],
        ]:
            subprocess.run(
                ["openssl", *map(str, args)],
                check=True,
                capture_output=True,
                timeout=30,
            )
        configs.append(
            {
                "algorithm": identity.algorithm.value,
                "provider": {
                    "kind": "software",
                    "key_path": str(identity.key.key_path),
                    "passphrase_file": None,
                },
                "certificate_path": str(cert),
                "chain_paths": [],
            }
        )
        fingerprint = certificate_fingerprint(cert.read_text())
        grants += [
            {
                "certificate_sha256": fingerprint,
                "role": role,
                "identity": who,
                "required_eku": None,
            }
            for role, who in [
                ("vendor_conversion", "vendor-1"),
                ("software_release", "publisher-1"),
            ]
        ]
    return configs, {
        "roots_pem": [pem(root)],
        "crls_pem": [pem(crl)],
        "authorizations": grants,
    }


@pytest.mark.parametrize("producer,consumer", [("python", "rust"), ("rust", "python")])
@pytest.mark.parametrize(
    "algorithm,selected", [("ecdsa", [0]), ("pqc", [1]), ("both", [0, 1])]
)
def test_trusted_conversion_and_publisher_signatures_all_algorithms(
    producer,
    consumer,
    algorithm,
    selected,
    trusted_interop_signers,
    conversion_case,
    tmp_path,
):
    archive, _, policy_path, _ = conversion_case
    configs, trust = trusted_interop_signers
    stage = read("stage.json")
    # Only this conversion root is signed in this test. Its original content and
    # source/report bindings remain the same as the public fixture.
    content = read("files.json")
    with zipfile.ZipFile(archive, "w") as z:
        for file in stage["manifest"]["files"]:
            z.writestr(file["path"], bytes.fromhex(content[file["path"]]))
    stage_path = write_json(tmp_path / "stage.json", stage)
    config = write_json(tmp_path / "signers.json", [configs[i] for i in selected])
    release = {
        key: stage["software"][key] for key in ["name", "version", "release_sha256"]
    }
    release_path = write_json(tmp_path / "release.json", release)
    release_record = tmp_path / "approval.json"
    writer, reader = command(producer), command(consumer)
    success(
        writer,
        "sign-release",
        release_path,
        release_record,
        "--signers",
        config,
        "--policy",
        algorithm,
    )
    success(
        writer,
        "sign",
        stage_path,
        "--archive",
        archive,
        "--signers",
        config,
        "--policy",
        algorithm,
    )
    approval = write_json(
        tmp_path / "approvals.json",
        {release["release_sha256"]: json.loads(release_record.read_text())},
    )
    success(writer, "embed", archive, "--releases", approval)
    trust_path = write_json(tmp_path / "trust.json", trust)
    identities_path = write_json(
        tmp_path / "identities.json", {"conversion": "vendor-1"}
    )
    publishers = write_json(
        tmp_path / "publishers.json", {release["release_sha256"]: "publisher-1"}
    )
    heads = write_json(tmp_path / "heads.json", [Stage.from_dict(stage).digest()])
    report = success(
        reader,
        "verify",
        archive,
        "--trust",
        trust_path,
        "--identities",
        identities_path,
        "--publishers",
        publishers,
        "--heads",
        heads,
        "--conversion-policy",
        policy_path,
        "--policy",
        algorithm,
        "--validation-time",
        int(time.time()) + 60,
    )
    stage_report = report["report"]["stages"][0]
    assert stage_report["signature"] == stage_report["software_release"] == "valid"
    assert len(stage_report["certificates"]) == len(selected)
    assert stage_report["conversion"]["source_integrity"] == "valid"
