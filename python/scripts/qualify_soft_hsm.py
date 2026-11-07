"""Exercise the OpenSSL PKCS#11 backend against an isolated SoftHSM token.

Example (use installed package paths for your platform)::

    python python/scripts/qualify_soft_hsm.py \
        --module /usr/lib/softhsm/libsofthsm2.so \
        --provider /usr/lib/x86_64-linux-gnu/ossl-modules/pkcs11.so \
        --run-rust

Only the SoftHSM module is accepted. Tokens, a disposable test PIN, and keys live
in a temporary directory that is deleted on exit. No physical HSM is initialized.
This does not qualify Nitrokey firmware or the OpenSC driver on the target host.
"""

import argparse
import os
from pathlib import Path
import subprocess
import sys
import tempfile


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--module", required=True, type=Path, help="SoftHSM2 PKCS#11 module"
    )
    parser.add_argument(
        "--provider", required=True, type=Path, help="OpenSSL pkcs11-provider module"
    )
    parser.add_argument("--softhsm-util", default="softhsm2-util")
    parser.add_argument("--run-rust", action="store_true")
    args = parser.parse_args()
    module = args.module.resolve(strict=True)
    provider = args.provider.resolve(strict=True)
    repo = Path(__file__).resolve().parents[2]

    with tempfile.TemporaryDirectory(prefix="mzpeak-softhsm-") as temporary:
        root = Path(temporary)
        token_dir = root / "tokens"
        token_dir.mkdir(mode=0o700)
        soft_config = root / "softhsm2.conf"
        soft_config.write_text(
            f"directories.tokendir = {token_dir}\nobjectstore.backend = file\n"
            "log.level = ERROR\nslots.removable = false\n"
        )
        env = dict(os.environ, SOFTHSM2_CONF=str(soft_config))

        def run(command, *, environment=env):
            return subprocess.run(
                command,
                env=environment,
                cwd=repo,
                check=True,
                capture_output=True,
                text=True,
                timeout=120,
            )

        info = run(["pkcs11-tool", "--module", str(module), "--show-info"])
        if "SoftHSM" not in info.stdout + info.stderr:
            raise RuntimeError("Refusing token initialization: module is not SoftHSM")
        # These are public disposable test PINs, never actual operator secrets.
        run(
            [
                args.softhsm_util,
                "--module",
                str(module),
                "--init-token",
                "--free",
                "--label",
                "mzpeak-qualification",
                "--so-pin",
                "12345678",
                "--pin",
                "123456",
            ]
        )
        run(
            [
                "pkcs11-tool",
                "--module",
                str(module),
                "--token-label",
                "mzpeak-qualification",
                "--login",
                "--pin",
                "123456",
                "--keypairgen",
                "--key-type",
                "EC:prime256v1",
                "--id",
                "01",
                "--label",
                "mzpeak-sign",
            ]
        )
        pin = root / "test-pin"
        pin.write_text("123456\n")
        pin.chmod(0o600)
        config = root / "openssl.cnf"
        config.write_text(f"""openssl_conf = openssl_init
[openssl_init]
providers = provider_sect
[provider_sect]
default = default_sect
pkcs11 = pkcs11_sect
[default_sect]
activate = 1
[pkcs11_sect]
module = {provider}
pkcs11-module-path = {module}
pkcs11-module-token-pin = file:{pin}
pkcs11-module-cache-keys = false
pkcs11-module-quirks = no-deinit
activate = 1
""")
        # This combination needs disabled session key caching and no-deinit to
        # avoid a provider/SoftHSM shutdown fault. Qualify other driver versions
        # separately rather than copying quirks blindly to production.
        key_uri = "pkcs11:token=mzpeak-qualification;object=mzpeak-sign;type=private"
        openssl_env = dict(env, OPENSSL_CONF=str(config))
        public = root / "public.pem"
        named_public = root / "named-public.pem"
        certificate = root / "certificate.pem"
        run(
            ["openssl", "pkey", "-in", key_uri, "-pubout", "-out", str(public)],
            environment=openssl_env,
        )
        # Normalize only the public EC parameters to the required named curve.
        run(
            [
                "openssl",
                "ec",
                "-pubin",
                "-in",
                str(public),
                "-param_enc",
                "named_curve",
                "-pubout",
                "-out",
                str(named_public),
            ],
            environment=openssl_env,
        )
        run(
            [
                "openssl",
                "x509",
                "-new",
                "-force_pubkey",
                str(named_public),
                "-key",
                key_uri,
                "-out",
                str(certificate),
                "-days",
                "1",
                "-subj",
                "/CN=mzPeak disposable SoftHSM test",
            ],
            environment=openssl_env,
        )
        test_env = dict(
            env,
            MZPEAK_TEST_PKCS11_CONFIG=str(config),
            MZPEAK_TEST_PKCS11_URI=key_uri,
            MZPEAK_TEST_PKCS11_CERTIFICATE=str(certificate),
        )
        print(
            "Qualifying Python with a temporary non-extractable SoftHSM key", flush=True
        )
        subprocess.run(
            [sys.executable, "-m", "pytest", "-q", "python/test/test_signing.py"],
            cwd=repo,
            env=test_env,
            check=True,
        )
        if args.run_rust:
            print("Qualifying Rust with the same token and provider", flush=True)
            subprocess.run(
                [
                    "cargo",
                    "test",
                    "--locked",
                    "provenance::signing::tests",
                    "--",
                    "--nocapture",
                ],
                cwd=repo,
                env=test_env,
                check=True,
            )


if __name__ == "__main__":
    main()
