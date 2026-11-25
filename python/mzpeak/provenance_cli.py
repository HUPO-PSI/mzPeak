"""Python entry point for the §6.4 Parquet provenance commands.

The CLI delegates CMS and archive validation to the installed mzpeak-provenance
executable. This keeps verification policy and wire encoding identical across
the Rust and Python entry points.
"""

import json
import os
from pathlib import Path
import shutil
import subprocess
import sys


def _executable(executable):
    value = executable or os.environ.get("MZPEAK_PROVENANCE_BIN", "mzpeak-provenance")
    path = shutil.which(value)
    if path is None:
        raise FileNotFoundError(
            "Install the mzpeak-provenance Rust binary or set MZPEAK_PROVENANCE_BIN"
        )
    return path


def _run_json(arguments, *, executable=None, allow_unaccepted=False):
    result = subprocess.run(
        [_executable(executable), *map(os.fspath, arguments)],
        capture_output=True,
        text=True,
        check=False,
    )
    if result.returncode != 0 and not (allow_unaccepted and result.returncode == 1):
        raise ValueError(result.stderr.strip() or "mzPeak provenance command failed")
    return json.loads(result.stdout)


def append_stage(archive, profile, stage, *, executable=None):
    """Append one authorized-for-signing stage using local profile and stage files."""
    return _run_json(
        ["append", Path(archive), "--profile", Path(profile), "--stage", Path(stage)],
        executable=executable,
    )


def verify_archive(archive, policy, *, at=None, executable=None):
    """Return the structured report, including a false acceptance decision."""
    arguments = ["verify", Path(archive), "--policy", Path(policy)]
    if at is not None:
        arguments.extend(["--at", str(at)])
    return _run_json(arguments, executable=executable, allow_unaccepted=True)


def main():
    try:
        return subprocess.run([_executable(None), *sys.argv[1:]], check=False).returncode
    except OSError as error:
        print(str(error), file=sys.stderr)
        return 2


if __name__ == "__main__":
    sys.exit(main())
