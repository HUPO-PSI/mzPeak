"""Command-line demo for the experimental mzPeak provenance proposal.

Run ``python -m mzpeak.provenance_cli --help``. Signer configuration and trust
policy are local inputs, never discovered or trusted from archive contents.
"""

from __future__ import annotations

import argparse
from dataclasses import asdict
import json
from pathlib import Path
import sys
import time

from .manifest import Manifest, _keys, _load_json
from .provenance_archive import capture_archive
from .embedded import ProvenanceBundle, embed_provenance, read_embedded_provenance
from .evidence import EvidenceBundle, TsaPolicy
from .historical import verify_historical_archive_history
from .signing import (
    Algorithm,
    OpenSslBackend,
    Pkcs11Key,
    SignatureEnvelope,
    SignaturePolicy,
    SigningIdentity,
    SoftwareKey,
)
from .stages import Stage
from .trust import Authorization, SignerRole, TrustPolicy
from .verification import (
    ReleaseStatement,
    SignedRelease,
    SignedStage,
    verify_archive_history,
)


def _read(path):
    # No format-level metadata size ceiling. Service deployments must apply
    # resource budgets appropriate to their workloads before loading inputs.
    return Path(path).read_bytes()


def _json(path):
    return _load_json(_read(path))


def _write(path, value):
    encoded = (
        json.dumps(
            value, indent=2, sort_keys=True, ensure_ascii=True, allow_nan=False
        ).encode("ascii")
        + b"\n"
    )
    # Never replace an existing measurement, key or previously signed record.
    with Path(path).open("xb") as stream:
        stream.write(encoded)


def _text(value, label):
    if not isinstance(value, str) or not value:
        raise ValueError(f"{label} must be a nonempty string")
    return value


def _string_map(path):
    if path is None:
        return {}
    value = _json(path)
    if not isinstance(value, dict) or any(
        not isinstance(k, str) or not isinstance(v, str) or not v
        for k, v in value.items()
    ):
        raise ValueError("identity configuration must be an object of nonempty strings")
    return value


def _signers(path):
    config = _json(path)
    if not isinstance(config, list) or not config:
        raise ValueError("signers must be a nonempty array")
    base = Path(path).resolve().parent

    def resolve(value):
        return base / _text(value, "file path")

    result = []
    for item in config:
        _keys(item, ("algorithm", "provider", "certificate_path", "chain_paths"))
        provider = item["provider"]
        if not isinstance(provider, dict):
            raise ValueError("provider must be an object")
        if provider.get("kind") == "software":
            _keys(provider, ("kind", "key_path", "passphrase_file"))
            key = SoftwareKey(
                resolve(provider["key_path"]),
                None
                if provider["passphrase_file"] is None
                else resolve(provider["passphrase_file"]),
            )
        elif provider.get("kind") == "pkcs11":
            _keys(provider, ("kind", "key_uri", "config_path"))
            key = Pkcs11Key(
                _text(provider["key_uri"], "key URI"), resolve(provider["config_path"])
            )
        else:
            raise ValueError("unsupported provider kind")
        if not isinstance(item["chain_paths"], list):
            raise ValueError("chain_paths must be an array")
        result.append(
            SigningIdentity(
                Algorithm(item["algorithm"]),
                key,
                _read(resolve(item["certificate_path"])).decode("ascii"),
                tuple(_read(resolve(p)).decode("ascii") for p in item["chain_paths"]),
            )
        )
    return result


def _unix_second(value):
    try:
        parsed = int(value)
    except (TypeError, ValueError):
        raise argparse.ArgumentTypeError("expected a nonnegative Unix second") from None
    if not 0 <= parsed <= 2**63 - 1:
        raise argparse.ArgumentTypeError(
            "expected a nonnegative signed-64-bit Unix second"
        )
    return parsed


def _policy_at(path, validation_time):
    value = _json(path)
    if not isinstance(value, dict):
        raise ValueError("policy must be an object")
    # Keep old policy files readable, but a stored date cannot select CLI time.
    if "validation_time" in value and (
        type(value["validation_time"]) is not int
        or not 0 <= value["validation_time"] <= 2**63 - 1
    ):
        raise ValueError(
            "validation_time must be a nonnegative signed-64-bit Unix second"
        )
    return {**value, "validation_time": validation_time}


def _trust(path, validation_time):
    value = _policy_at(path, validation_time)
    if (
        not isinstance(value, dict)
        or set(value)
        - {
            "roots_pem",
            "authorizations",
            "validation_time",
            "crls_pem",
            "require_pqc_chain",
        }
        or not {"roots_pem", "authorizations", "validation_time"} <= set(value)
    ):
        raise ValueError("invalid trust policy fields")
    if type(value["validation_time"]) is not int or value["validation_time"] < 0:
        raise ValueError("validation_time must be a nonnegative Unix second")
    if type(value.get("require_pqc_chain", False)) is not bool:
        raise ValueError("require_pqc_chain must be boolean")
    for name in ("roots_pem", "crls_pem"):
        if not isinstance(value.get(name, []), list) or not all(
            isinstance(p, str) for p in value.get(name, [])
        ):
            raise ValueError(f"{name} must be an array of PEM strings")
    if not isinstance(value["authorizations"], list):
        raise ValueError("authorizations must be an array")
    authorizations = []
    for grant in value["authorizations"]:
        if (
            not isinstance(grant, dict)
            or set(grant) - {"certificate_sha256", "role", "identity", "required_eku"}
            or not {"certificate_sha256", "role", "identity"} <= set(grant)
        ):
            raise ValueError("invalid authorization fields")
        for name in ("certificate_sha256", "identity"):
            _text(grant[name], name)
        if grant.get("required_eku") is not None:
            _text(grant["required_eku"], "required_eku")
        authorizations.append(
            Authorization(**{**grant, "role": SignerRole(grant["role"])})
        )
    return TrustPolicy(**{**value, "authorizations": authorizations})


def _envelope(value):
    return SignatureEnvelope.from_json(json.dumps(value).encode("utf-8"))


def _records(path):
    value = _json(path)
    if isinstance(value, dict):
        value = [value]
    if not isinstance(value, list) or not value:
        raise ValueError(
            "records must be a signed stage or nonempty signed-stage array"
        )
    records = []
    for record in value:
        _keys(record, ("stage", "envelope"))
        records.append(
            SignedStage(Stage.from_dict(record["stage"]), _envelope(record["envelope"]))
        )
    return records


def _release_statement(value):
    _keys(value, ("name", "version", "release_sha256"))
    statement = ReleaseStatement(**value)
    statement.canonical_bytes()
    return statement


def _releases(path):
    if path is None:
        return {}
    values = _json(path)
    if not isinstance(values, dict):
        raise ValueError("release approvals must be an object keyed by release digest")
    result = {}
    for digest, value in values.items():
        _keys(value, ("statement", "envelope"))
        statement = _release_statement(value["statement"])
        if statement.release_sha256 != digest:
            raise ValueError("release map key does not match its statement")
        result[digest] = SignedRelease(statement, _envelope(value["envelope"]))
    return result


def _evidence(path):
    if path is None:
        return {}
    value = _json(path)
    if not isinstance(value, dict):
        raise ValueError(
            "evidence must be an object keyed by stage ID or release digest"
        )
    return {
        k: EvidenceBundle.from_json(json.dumps(v).encode()) for k, v in value.items()
    }


def parser():
    result = argparse.ArgumentParser(description=__doc__)
    result.add_argument(
        "--openssl",
        default="openssl",
        help="trusted OpenSSL executable (3.5+ for ML-DSA)",
    )
    sub = result.add_subparsers(dest="command", required=True)
    signed_archive = sub.add_parser(
        "sign-archive", help="sign an acquisition and embed its provenance in the ZIP"
    )
    signed_archive.add_argument("archive")
    signed_archive.add_argument("--run-id", required=True)
    signed_archive.add_argument("--stage-id", required=True)
    signed_archive.add_argument("--signers", required=True)
    signed_archive.add_argument(
        "--policy", choices=[p.value for p in SignaturePolicy], required=True
    )
    embed = sub.add_parser(
        "embed", help="add signed stages, approvals and evidence to the ZIP"
    )
    embed.add_argument("archive")
    for name in ("records", "releases", "stage-evidence", "release-evidence"):
        embed.add_argument("--" + name)
    capture = sub.add_parser(
        "capture", help="capture decompressed ZIP content inventory"
    )
    capture.add_argument("archive")
    capture.add_argument("output")
    capture.add_argument("--run-id", required=True)
    capture.add_argument("--stage-id", required=True)
    capture.add_argument(
        "--select", action="append", help="repeat to capture a stage's selected outputs"
    )
    acquisition = sub.add_parser(
        "init-acquisition", help="wrap a manifest as an acquisition stage"
    )
    acquisition.add_argument("manifest")
    acquisition.add_argument("output")
    for name in ("sign", "sign-release"):
        command = sub.add_parser(
            name,
            help="sign a stage"
            if name == "sign"
            else "sign a software release approval",
        )
        command.add_argument("input")
        command.add_argument(
            "output", nargs="?", help="explicit detached output (instead of --archive)"
        )
        command.add_argument(
            "--archive", help="embed the signed record in this mzPeak ZIP"
        )
        command.add_argument(
            "--signers", required=True, help="local signer provider configuration JSON"
        )
        command.add_argument(
            "--policy", choices=[p.value for p in SignaturePolicy], required=True
        )
    verify = sub.add_parser(
        "verify",
        help="verify content, lineage, signatures and externally configured trust",
    )
    verify.add_argument("archive")
    verify.add_argument(
        "records",
        nargs="?",
        help="explicit detached records; omitted means embedded provenance",
    )
    verify.add_argument("--trust", required=True)
    verify.add_argument("--identities", required=True)
    verify.add_argument("--releases")
    verify.add_argument("--publishers")
    verify.add_argument(
        "--validation-time",
        type=_unix_second,
        help="explicit fixed Unix second for reproducible verification; default is the current clock",
    )
    verify.add_argument(
        "--tsa-policy", help="external TSA policy for historical verification"
    )
    verify.add_argument("--evidence-policy-id")
    verify.add_argument(
        "--evidence-policy", help="external policy bytes bound by historical evidence"
    )
    verify.add_argument(
        "--heads",
        help="external trusted head digest array; absent means no rollback assurance",
    )
    verify.add_argument(
        "--policy", choices=[p.value for p in SignaturePolicy], required=True
    )
    return result


def main(argv=None):
    args = parser().parse_args(argv)
    try:
        backend = OpenSslBackend(args.openssl)
        if args.command == "sign-archive":
            stage = Stage(
                capture_archive(args.archive, args.run_id, args.stage_id), "acquisition"
            )
            envelope = backend.sign(
                stage.canonical_bytes(),
                _signers(args.signers),
                SignaturePolicy(args.policy),
            )
            bundle = embed_provenance(
                args.archive, ProvenanceBundle([SignedStage(stage, envelope)])
            )
            print(
                json.dumps(
                    {"archive": args.archive, "embedded_stages": len(bundle.records)}
                )
            )
        elif args.command == "embed":
            if not any(
                (
                    args.records,
                    args.releases,
                    args.stage_evidence,
                    args.release_evidence,
                )
            ):
                raise ValueError("supply records, approvals or evidence to embed")
            bundle = embed_provenance(
                args.archive,
                ProvenanceBundle(
                    _records(args.records) if args.records else [],
                    _releases(args.releases),
                    _evidence(args.stage_evidence),
                    _evidence(args.release_evidence),
                ),
            )
            print(
                json.dumps(
                    {"archive": args.archive, "embedded_stages": len(bundle.records)}
                )
            )
        elif args.command == "capture":
            manifest = capture_archive(
                args.archive, args.run_id, args.stage_id, args.select
            )
            _write(args.output, manifest.to_dict())
            print(
                json.dumps(
                    {"output": str(args.output), "manifest_sha256": manifest.digest()}
                )
            )
        elif args.command == "init-acquisition":
            stage = Stage(Manifest.from_bytes(_read(args.manifest)), "acquisition")
            _write(args.output, stage.to_dict())
            print(
                json.dumps({"output": str(args.output), "stage_sha256": stage.digest()})
            )
        elif args.command in ("sign", "sign-release"):
            if bool(args.output) == bool(args.archive):
                raise ValueError(
                    "select --archive for embedded output or an explicit detached output path"
                )
            identities = _signers(args.signers)
            if args.command == "sign":
                stage = Stage.from_bytes(_read(args.input))
                envelope = backend.sign(
                    stage.canonical_bytes(), identities, SignaturePolicy(args.policy)
                )
                value = {
                    "stage": stage.to_dict(),
                    "envelope": _load_json(envelope.to_json()),
                }
            else:
                statement = _release_statement(_json(args.input))
                envelope = backend.sign(
                    statement.canonical_bytes(),
                    identities,
                    SignaturePolicy(args.policy),
                )
                value = {
                    "statement": asdict(statement),
                    "envelope": _load_json(envelope.to_json()),
                }
            if args.archive:
                if args.command == "sign":
                    additions = ProvenanceBundle([SignedStage(stage, envelope)])
                else:
                    additions = ProvenanceBundle(
                        releases={
                            statement.release_sha256: SignedRelease(statement, envelope)
                        }
                    )
                embed_provenance(args.archive, additions)
                print(json.dumps({"archive": args.archive}))
            else:
                _write(args.output, value)
                print(json.dumps({"output": str(args.output)}))
        else:
            # Sample the wall clock once for all current checks in this invocation.
            validation_time = (
                int(time.time())
                if args.validation_time is None
                else args.validation_time
            )
            if not 0 <= validation_time <= 2**63 - 1:
                raise ValueError("current time is outside supported Unix seconds")
            time_source = "current" if args.validation_time is None else "explicit"
            bundle = None if args.records else read_embedded_provenance(args.archive)
            if bundle is not None and args.releases:
                raise ValueError(
                    "embed approvals for embedded verification; --releases is for detached records"
                )
            records = _records(args.records) if args.records else bundle.records
            releases = _releases(args.releases) if args.records else bundle.releases
            heads = None if args.heads is None else _json(args.heads)
            if heads is not None and (
                not isinstance(heads, list)
                or not all(isinstance(v, str) for v in heads)
            ):
                raise ValueError("heads must be an array of digests")
            historical = (
                args.tsa_policy,
                args.evidence_policy_id,
                args.evidence_policy,
            )
            extra = {}
            verifier = verify_archive_history
            if any(historical):
                if not all(historical) or bundle is None:
                    raise ValueError(
                        "historical verification requires embedded proof and all three external policy options"
                    )
                verifier = verify_historical_archive_history
                extra = dict(
                    tsa_policy=TsaPolicy(
                        **_policy_at(args.tsa_policy, validation_time)
                    ),
                    policy_id=args.evidence_policy_id,
                    policy_bytes=_read(args.evidence_policy),
                    stage_evidence=bundle.stage_evidence,
                    release_evidence=bundle.release_evidence,
                )
            report = verifier(
                args.archive,
                records,
                _trust(args.trust, validation_time),
                SignaturePolicy(args.policy),
                _string_map(args.identities),
                releases=releases,
                publishers=_string_map(args.publishers),
                expected_heads=heads,
                backend=backend,
                **extra,
            )
            print(
                json.dumps(
                    {
                        "accepted": report.accepted,
                        "verification": (
                            "historical"
                            if extra
                            else "current"
                            if args.validation_time is None
                            else "fixed-time"
                        ),
                        "validation_time": validation_time,
                        "time_source": time_source,
                        "report": asdict(report),
                    },
                    sort_keys=True,
                    indent=2,
                )
            )
            return 0 if report.accepted else 1
        return 0
    except Exception as error:
        # Never print configuration contents or underlying provider stderr.
        print(json.dumps({"accepted": False, "error": str(error)}), file=sys.stderr)
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
