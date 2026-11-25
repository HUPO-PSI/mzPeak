"""Qualification of the shared §6.4 CLI with real local CA and TSA proofs."""
import hashlib
import io
import json
import os
import shutil
import ssl
import subprocess
import time
from pathlib import Path
from zipfile import ZipFile

import pyarrow as pa
import pyarrow.parquet as pq
import pytest

from mzpeak import provenance_cli

BINARY = os.environ.get("MZPEAK_PROVENANCE_BIN")
OPENSSL = shutil.which("openssl")
pytestmark = pytest.mark.skipif(
    not BINARY or not OPENSSL, reason="set MZPEAK_PROVENANCE_BIN and install OpenSSL"
)
OIDS = {
    # RFC 5612 documentation PEN. These are not mzPeak assignments.
    "stage": "1.3.6.1.4.1.32473.6.4.1",
    "publisher_approval": "1.3.6.1.4.1.32473.6.4.2",
}


def run(*arguments, expect=0):
    result = subprocess.run(
        [BINARY, *map(str, arguments)], capture_output=True, text=True, timeout=40
    )
    assert result.returncode == expect, result.stderr or result.stdout
    if result.stdout:
        return json.loads(result.stdout)
    if result.stderr:
        return json.loads(result.stderr)


def openssl(*arguments):
    result = subprocess.run(
        [OPENSSL, *map(str, arguments)], capture_output=True, text=True, timeout=30
    )
    assert result.returncode == 0, result.stderr
    return result.stdout


def write(path, value):
    path.write_text(json.dumps(value))
    return path


class Authority:
    def __init__(self, path):
        self.path = path
        self.root = path / "ca.pem"
        self.key = path / "ca.key"
        self.crl = path / "ca.crl"
        self.config = path / "ca.cnf"
        (path / "issued").mkdir()
        (path / "index.txt").touch()
        (path / "serial").write_text("1000\n")
        (path / "crlnumber").write_text("1000\n")
        openssl("genpkey", "-algorithm", "EC", "-pkeyopt", "ec_paramgen_curve:P-256", "-out", self.key)
        openssl("req", "-new", "-x509", "-key", self.key, "-out", self.root, "-days", 30,
                "-subj", "/CN=mzPeak qualification CA", "-addext", "basicConstraints=critical,CA:TRUE",
                "-addext", "keyUsage=critical,keyCertSign,cRLSign",
                "-addext", "subjectKeyIdentifier=hash")
        self.config.write_text(
            f"""[ca]
default_ca = issuer
[issuer]
database = {(path / "index.txt").as_posix()}
new_certs_dir = {(path / "issued").as_posix()}
serial = {(path / "serial").as_posix()}
crlnumber = {(path / "crlnumber").as_posix()}
certificate = {self.root.as_posix()}
private_key = {self.key.as_posix()}
default_days = 2
default_crl_days = 14
default_md = sha512
policy = leaf
[leaf]
commonName = supplied
"""
        )

    def issue(self, name, algorithm="ecdsa-p256-sha512", eku=None):
        key = self.path / f"{name}.key"
        req = self.path / f"{name}.csr"
        cert = self.path / f"{name}.pem"
        extensions = self.path / f"{name}.ext"
        if algorithm == "ecdsa-p256-sha512":
            openssl("genpkey", "-algorithm", "EC", "-pkeyopt", "ec_paramgen_curve:P-256", "-out", key)
        elif algorithm == "ecdsa-p521-sha512":
            openssl("genpkey", "-algorithm", "EC", "-pkeyopt", "ec_paramgen_curve:P-521", "-out", key)
        else:
            openssl("genpkey", "-algorithm", algorithm.upper(), "-out", key)
        openssl("req", "-new", "-key", key, "-out", req, "-subj", f"/CN={name}")
        extensions.write_text(
            "basicConstraints=critical,CA:FALSE\nkeyUsage=critical,digitalSignature\n"
            "subjectKeyIdentifier=hash\nauthorityKeyIdentifier=keyid:always\n"
            + (f"extendedKeyUsage={eku}\n" if eku else "")
        )
        openssl("ca", "-batch", "-notext", "-config", self.config,
                "-extfile", extensions, "-in", req, "-out", cert)
        openssl("ca", "-gencrl", "-config", self.config, "-out", self.crl)
        return {"algorithm": algorithm, "certificate_pem": cert.read_text(),
                "key": {"type": "software", "path": str(key)}}

    def policy(self, identities, grants, **options):
        return dict(oids=OIDS, identities=identities, trust=dict(
            roots_pem=[self.root.read_text()],
            crls_pem=[self.crl.read_text()],
            authorizations=grants,
        ), **options)


def authorize(profile, role, identity):
    der = ssl.PEM_cert_to_DER_cert(profile["certificate_pem"])
    return dict(certificate_sha256=hashlib.sha256(der).hexdigest(), role=role, identity=identity)


def signing(path, *profiles):
    return write(path, dict(oids=OIDS, signers=profiles))


def archive(path, **extras):
    with ZipFile(path, "w") as output:
        output.writestr("mzpeak_index.json", '{"files":[],"metadata":{}}')
        output.writestr("spectra.bin", b"instrument data")
        for name, data in extras.items():
            output.writestr(name, data)
    return path


def proof_table(path):
    with ZipFile(path) as package:
        return pq.read_table(io.BytesIO(package.read("provenance.parquet")))


def replace_members(path, changes):
    with ZipFile(path) as package:
        data = {name: package.read(name) for name in package.namelist()}
    data.update(changes)
    with ZipFile(path, "w") as output:
        for name, value in data.items():
            output.writestr(name, value)


def row_for(path, name):
    return proof_table(path)["filename"].to_pylist().index(name)


def test_acquisition_cms_and_signed_inventory(tmp_path):
    authority = Authority(tmp_path)
    leaf = authority.issue("instrument")
    path = archive(tmp_path / "one.mzpeak")
    sign = signing(tmp_path / "sign.json", leaf)
    stage = write(tmp_path / "acquisition.json", {"kind": "acquisition"})
    head = provenance_cli.append_stage(path, sign, stage)
    policy = write(tmp_path / "policy.json", authority.policy(
        {head["index"]: "instrument"}, [authorize(leaf, "acquisition", "instrument")],
        expected_checkpoint=head,
    ))
    assert provenance_cli.verify_archive(path, policy)["accepted"]
    table = proof_table(path)
    assert table["index"].to_pylist() == [0, 1, 2]
    assert table["filename"].to_pylist() == ["mzpeak_index.json", "spectra.bin", "*"]
    assert table["length"].to_pylist() == [None, None, None]
    row = tmp_path / "row.der"
    row.write_bytes(table["signature"][1].as_py())
    raw = tmp_path / "spectra.bin"
    raw.write_bytes(b"instrument data")
    assert openssl("cms", "-verify", "-binary", "-inform", "DER", "-content",
                   raw, "-in", row, "-noverify").encode() == raw.read_bytes()

    with ZipFile(path, "a") as output:
        output.writestr("uncovered.bin", b"extra")
    rejected = provenance_cli.verify_archive(path, policy)
    assert not rejected["accepted"] and rejected["coverage"]["unsigned"] == ["uncovered.bin"]
    renamed = table.set_column(table.schema.get_field_index("filename"), "filename",
                               pa.array(["mzpeak_index.json", "uncovered.bin", "*"]))
    buf = io.BytesIO()
    pq.write_table(renamed, buf)
    replace_members(path, {"provenance.parquet": buf.getvalue()})
    assert "descriptor" in run("verify", path, "--policy", policy, expect=2)["error"]


def test_processing_publisher_countersignature_and_rollback(tmp_path):
    authority = Authority(tmp_path)
    device = authority.issue("device")
    operator = authority.issue("processor")
    publisher = authority.issue("publisher", eku="codeSigning")
    approver = authority.issue("approver")
    grants = [authorize(device, "acquisition", "device"), authorize(operator, "processing", "processor"),
              authorize(publisher, "software_release", "publisher"),
              authorize(approver, "archive_approval", "approver")]
    path = archive(tmp_path / "history.mzpeak")
    capture = write(tmp_path / "capture.json", {"kind": "acquisition"})
    head = run("append", path, "--profile", signing(tmp_path / "device.json", device), "--stage", capture)
    earlier = tmp_path / "earlier.mzpeak"
    shutil.copyfile(path, earlier)
    before = proof_table(path)["signature"].to_pylist()
    input_row = row_for(path, "spectra.bin")
    software = tmp_path / "processor.exe"
    software.write_bytes(b"release executable")
    artifact_digest = hashlib.sha512(software.read_bytes()).hexdigest()
    run("sign-release", software, "--name", "processor", "--version", "1.0",
        "--profile", signing(tmp_path / "publisher.json", publisher), "--output", tmp_path / "release.der")
    with ZipFile(path, "a") as output:
        output.writestr("derived.bin", b"processed output")
    stage = write(tmp_path / "processing.json", {
        "kind": "processing", "inputs": [{"stage": head["index"], "row": input_row}],
        "software": dict(name="processor", version="1.0", release_sha512=artifact_digest,
                         parameters_sha512=hashlib.sha512(b"params").hexdigest()),
    })
    current = run("append", path, "--profile", signing(tmp_path / "operator.json", operator), "--stage", stage)
    assert proof_table(path)["signature"].to_pylist()[:len(before)] == before
    identity = {head["index"]: "device", current["index"]: "processor"}
    inputs = authority.policy(identity, grants, require_publisher_approval=True,
                              publishers={artifact_digest: "publisher"})
    policy = write(tmp_path / "policy.json", inputs)
    assert run("verify", path, "--policy", policy, expect=1)["stages"][1]["publisher_approval"] == "invalid"
    run("attach-release", path, "--oids", write(tmp_path / "oids.json", OIDS), tmp_path / "release.der")
    inputs["archive_approver"] = "approver"
    policy = write(policy, inputs)
    assert run("verify", path, "--policy", policy, expect=1)["archive_approval"] == "invalid"
    last = run("approve", path, "--profile", signing(tmp_path / "approver.json", approver))
    report = run("verify", path, "--policy", write(policy, inputs | {"expected_checkpoint": last}))
    assert report["accepted"] and report["checkpoint_verified"]
    assert report["archive_approval"] == "valid" and report["coverage"]["derived"] == ["derived.bin"]
    assert report["stages"][1]["publisher_approval"] == "valid"
    stale = run("verify", earlier, "--policy",
                write(policy, authority.policy(identity, grants, expected_checkpoint=last)), expect=1)
    assert stale["checkpoint_verified"] is False


def test_authorized_replacement_preserves_original_proof(tmp_path):
    authority = Authority(tmp_path)
    leaf = authority.issue("leaf")
    path = archive(tmp_path / "replacement.mzpeak")
    signing_profile = signing(tmp_path / "sign.json", leaf)
    acquisition = run("append", path, "--profile", signing_profile, "--stage",
                      write(tmp_path / "capture.json", {"kind": "acquisition"}))
    old_row = row_for(path, "spectra.bin")
    replace_members(path, {"spectra.bin": b"valid transformed spectrum"})
    digest = "00" * 64
    current = run("append", path, "--profile", signing_profile, "--stage",
                  write(tmp_path / "process.json", {
                      "kind": "processing", "inputs": [{"stage": acquisition["index"], "row": old_row}],
                      "replacements": ["spectra.bin"], "software": {
                          "name": "converter", "version": "1", "release_sha512": digest, "parameters_sha512": digest,
                      },
                  }))
    policy_data = authority.policy({acquisition["index"]: "source", current["index"]: "processor"},
                                   [authorize(leaf, "acquisition", "source"), authorize(leaf, "processing", "processor")])
    policy = write(tmp_path / "policy.json", policy_data)
    report = run("verify", path, "--policy", policy, expect=1)
    assert report["coverage"]["historical_unavailable"] == [old_row]
    accepted = run("verify", path, "--policy", write(policy, policy_data | {"allow_replacements": True}))
    assert accepted["coverage"]["derived"] == ["spectra.bin"]


def test_vendor_conversion_requires_authorized_method_and_checks_raw(tmp_path):
    authority = Authority(tmp_path)
    vendor = authority.issue("vendor", algorithm="ed25519")
    source = tmp_path / "original.raw"
    source.write_bytes(b"vendor content")
    inventory = run("capture-source", source)
    path = archive(tmp_path / "vendor.mzpeak")
    grant = dict(vendor="vendor", format="vendor.raw", method="validate", verifier="vendor tool", verifier_version="1")
    head = run("append", path, "--profile", signing(tmp_path / "sign.json", vendor), "--stage",
               write(tmp_path / "conversion.json", {"kind": "conversion", "conversion": grant | {
                    "sources": inventory, "result": "passed",
               }}))
    options = authority.policy({head["index"]: "vendor"}, [authorize(vendor, "vendor_conversion", "vendor")],
                               conversion_grants=[grant], raw_sources={head["index"]: str(source)})
    policy = write(tmp_path / "policy.json", options)
    assert run("verify", path, "--policy", policy)["stages"][0]["source_integrity"] == "valid"
    source.write_bytes(b"different file")
    rejected = run("verify", path, "--policy", policy, expect=1)
    assert rejected["stages"][0]["source_integrity"] == "invalid"
    assert rejected["stages"][0]["vendor_attestation"] == "valid"


def test_inline_timestamp_survives_signer_expiry(tmp_path):
    authority = Authority(tmp_path)
    device = authority.issue("device")
    tsa = authority.issue("tsa", eku="critical,timeStamping")
    path = archive(tmp_path / "timestamped.mzpeak")
    head = run("append", path, "--profile", signing(tmp_path / "sign.json", device),
               "--stage", write(tmp_path / "capture.json", {"kind": "acquisition"}))
    request_id, query = next(iter(run("timestamp-requests", path).items()))
    query_path = tmp_path / "query.der"
    query_path.write_bytes(bytes.fromhex(query))
    config = tmp_path / "tsa.cnf"
    policy_oid = "1.3.6.1.4.1.32473.6.4.3"
    (tmp_path / "tsserial").write_text("01\n")
    config.write_text(f"""[tsa]
default_tsa = signer
[signer]
serial = {(tmp_path / "tsserial").as_posix()}
crypto_device = builtin
signer_cert = {(tmp_path / "tsa.pem").as_posix()}
signer_key = {Path(tsa['key']['path']).as_posix()}
certs = {authority.root.as_posix()}
signer_digest = sha512
default_policy = {policy_oid}
digests = sha512
ess_cert_id_alg = sha256
""")
    response = tmp_path / "response.der"
    openssl("ts", "-reply", "-config", config, "-queryfile", query_path, "-out", response)
    run("attach-timestamp", path, "--oids", write(tmp_path / "oids.json", OIDS), response, "--query", query_path)
    policy = authority.policy({head["index"]: "device"}, [authorize(device, "acquisition", "device")])
    later = int(time.time()) + 3 * 24 * 3600
    policy_path = write(tmp_path / "policy.json", policy)
    run("verify", path, "--policy", policy_path, "--at", later, expect=1)
    tsa_policy = dict(roots_pem=[authority.root.read_text()], allowed_policy_oids=[policy_oid],
                      crls_pem=[authority.crl.read_text()])
    write(policy_path, policy | dict(historical=tsa_policy, timestamp_queries={request_id: str(query_path)}))
    assert run("verify", path, "--policy", policy_path, "--at", later)["accepted"]


def test_named_parquet_ranges_use_footer_coordinates(tmp_path):
    authority = Authority(tmp_path)
    device = authority.issue("device", algorithm="ecdsa-p521-sha512")
    parquet = io.BytesIO()
    pq.write_table(pa.table({"mz": [100.1, 100.2], "intensity": [20.0, 40.0]}), parquet)
    path = archive(tmp_path / "tags.mzpeak", **{"table.parquet": parquet.getvalue()})
    head = run("append", path, "--profile", signing(tmp_path / "sign.json", device),
               "--stage", write(tmp_path / "capture.json", {"kind": "acquisition", "parquet_chunks": True}))
    table = proof_table(path)
    named = [r for r in table.to_pylist() if r["row_group"] is not None]
    assert [(r["row_group"], r["column"], r["path"]) for r in named] == [(0, 0, "mz"), (0, 1, "intensity")]
    policy = authority.policy({head["index"]: "device"}, [authorize(device, "acquisition", "device")])
    assert run("verify", path, "--policy", write(tmp_path / "policy.json", policy))["accepted"]

    with ZipFile(path, "a") as output:
        output.writestr("PROVENANCE.PARQUET", b"alias")
    assert "ambiguous" in run("verify", path, "--policy", tmp_path / "policy.json", expect=2)["error"]


@pytest.mark.parametrize("algorithm", ["ml-dsa-44", "ml-dsa-65", "ml-dsa-87"])
def test_dual_cms_verification_checks_complete_signer_set(tmp_path, algorithm):
    supported = openssl("list", "-signature-algorithms")
    if algorithm.upper() not in supported:
        pytest.skip(f"OpenSSL lacks {algorithm} support")
    authority = Authority(tmp_path)
    classical = authority.issue("ecdsa")
    pqc = authority.issue("postquantum", algorithm=algorithm)
    path = archive(tmp_path / "both.mzpeak")
    head = run("append", path, "--profile", signing(tmp_path / "sign.json", classical, pqc),
               "--stage", write(tmp_path / "capture.json", {"kind": "acquisition"}))
    options = authority.policy({head["index"]: "instrument"},
                               [authorize(signer, "acquisition", "instrument") for signer in (classical, pqc)],
                               minimum_algorithms="both")
    policy = write(tmp_path / "policy.json", options)
    assert run("verify", path, "--policy", policy)["accepted"]

    # In this test CA the ML-DSA leaf is issued by an EC root.
    options["trust"]["require_pqc_chain"] = True
    assert not run("verify", path, "--policy", write(policy, options), expect=1)["accepted"]

    # Keep the original signed attributes intact while dropping a primary
    # SignerInfo. Even a recipient with no dual-signature minimum rejects it.
    from asn1crypto import cms

    table = proof_table(path)
    signatures = table["signature"].to_pylist()
    envelope = cms.ContentInfo.load(signatures[-1])
    del envelope["content"]["signer_infos"][0]
    signatures[-1] = envelope.dump()
    changed = table.set_column(table.schema.get_field_index("signature"), "signature", pa.array(signatures))
    buffer = io.BytesIO()
    pq.write_table(changed, buffer)
    replace_members(path, {"provenance.parquet": buffer.getvalue()})
    options["trust"]["require_pqc_chain"] = False
    options["minimum_algorithms"] = "any"
    assert "signer set" in run("verify", path, "--policy", write(policy, options), expect=2)["error"]
