"""Regenerate public interoperability fixtures using the self-signed Python demo.

Run with the repository's Python package and provenance/test extras installed.
The demo generates a fresh private key in memory; no private material is saved.
"""

import importlib.util
from pathlib import Path
from types import SimpleNamespace

import pyarrow as pa
from pyarrow import parquet

directory = Path(__file__).resolve().parent
root = directory.parents[3]
spec = importlib.util.spec_from_file_location(
    "self_signed_archive_demo", root / "python/scripts/sign_archive.py"
)
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)

parquet.write_table(
    pa.table({"mz": [101.5, 202.5], "intensity": [10.0, 20.0]}),
    directory / "spectrum.parquet",
    compression="NONE",
)
(directory / "metadata.json").write_bytes(b'{"run":"self-signed-interop"}\n')


class Archive:
    file_index = (
        SimpleNamespace(name="spectrum.parquet"),
        SimpleNamespace(name="metadata.json"),
    )

    def open_stream(self, entry):
        name = entry if isinstance(entry, str) else entry.name
        return (directory / name).open("rb")


module.sign_archive(Archive(), directory / "signatures.parquet")
