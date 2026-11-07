"""Legacy signing demo and its nullable, state-preserving table format."""

import hashlib
import io
from datetime import UTC, datetime, timedelta

import pyarrow as pa
import pytest
from cryptography import x509
from cryptography.hazmat.primitives.asymmetric import ed25519
from cryptography.x509.oid import NameOID

from mzpeak.provenance import (
    SegmentSignature,
    VerificationError,
    checksum_stream,
    segment_signatures_from_table,
    segment_signatures_to_table,
)


def segment(**kwargs):
    values = dict(
        filename="spectrum.parquet",
        row_group=0,
        column=0,
        path="point.mz",
        offset=0,
        length=6,
        signature=hashlib.sha512(b"abcdef").digest(),
    )
    return SegmentSignature(**(values | kwargs))


@pytest.fixture
def signing_key():
    key = ed25519.Ed25519PrivateKey.from_private_bytes(bytes(range(32)))
    name = x509.Name([x509.NameAttribute(NameOID.COMMON_NAME, "Legacy test signer")])
    now = datetime.now(UTC)
    cert = (
        x509.CertificateBuilder()
        .subject_name(name)
        .issuer_name(name)
        .public_key(key.public_key())
        .serial_number(123)
        .not_valid_before(now - timedelta(days=1))
        .not_valid_after(now + timedelta(days=1))
        .sign(key, None)
    )
    return key, cert


def signed(signing_key):
    record = segment()
    record.sign_with(*signing_key)
    return record


def test_signed_and_unsigned_whole_file_roundtrip(signing_key, tmp_path):
    from pyarrow import parquet

    unsigned = checksum_stream(io.BytesIO(b"metadata"), "metadata.json")
    signed_record = unsigned.copy()
    signed_record.sign_with(*signing_key)
    records = [unsigned, signed_record]
    path = tmp_path / "provenance.parquet"
    parquet.write_table(segment_signatures_to_table(records), path)
    result = segment_signatures_from_table(parquet.read_table(path))
    assert result == records
    assert result[1].verify_against(unsigned.signature)


def test_legacy_schema_without_state_is_readable(signing_key):
    records = [segment(), signed(signing_key)]
    table = segment_signatures_to_table(records).drop(["signed"])
    assert segment_signatures_from_table(table) == records


def test_required_fields_and_state_reject_nulls():
    table = segment_signatures_to_table([segment()])
    for name in ["filename", "signature", "signed"]:
        i = table.schema.get_field_index(name)
        malformed = table.set_column(
            i, name, pa.array([None], type=table.column(i).type)
        )
        with pytest.raises(ValueError):
            segment_signatures_from_table(malformed)


def test_malformed_state_type_rejected():
    table = segment_signatures_to_table([segment()])
    table = table.set_column(
        table.schema.get_field_index("signed"), "signed", pa.array(["false"])
    )
    with pytest.raises(ValueError):
        segment_signatures_from_table(table)


@pytest.fixture
def sign_archive():
    import importlib.util
    from pathlib import Path

    path = Path(__file__).resolve().parents[1] / "scripts/sign_archive.py"
    spec = importlib.util.spec_from_file_location("legacy_sign_archive_demo", path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module.sign_archive


class DemoArchive:
    def __init__(self, files, changed_on_reopen=None, stream_type=io.BytesIO):
        from types import SimpleNamespace

        self.files = files
        self.file_index = [SimpleNamespace(name=name) for name in files]
        self.changed_on_reopen = changed_on_reopen or {}
        self.open_counts = {}
        self.streams = []
        self.stream_type = stream_type

    def open_stream(self, entry):
        name = entry if isinstance(entry, str) else entry.name
        count = self.open_counts.get(name, 0)
        self.open_counts[name] = count + 1
        data = (
            self.changed_on_reopen.get(name, self.files[name])
            if count
            else self.files[name]
        )
        stream = self.stream_type(data)
        self.streams.append(stream)
        return stream


@pytest.mark.parametrize("metadata", [b'{"run":1}', b""])
def test_demo_outputs_readable_verified_parquet_and_whole_file_records(
    sign_archive, metadata, tmp_path
):
    from pyarrow import parquet

    content = io.BytesIO()
    parquet.write_table(
        pa.table({"mz": [101.5, 202.5], "intensity": [10.0, 20.0]}), content
    )
    archive = DemoArchive(
        {"spectrum.parquet": content.getvalue(), "metadata.json": metadata}
    )
    output = tmp_path / "legacy-signatures.parquet"
    sign_archive(archive, output)
    assert all(stream.closed for stream in archive.streams)
    records = segment_signatures_from_table(parquet.read_table(output))
    assert {r.filename for r in records} == set(archive.files)
    assert all(r.signed for r in records)
    whole = next(r for r in records if r.filename == "metadata.json")
    assert (whole.row_group, whole.column, whole.path, whole.offset, whole.length) == (
        None,
    ) * 5
    for record in records:
        assert record.verify_against(
            record.read_and_digest(io.BytesIO(archive.files[record.filename]))
        )


def test_demo_rejects_empty_input(sign_archive, tmp_path):
    output = tmp_path / "empty.parquet"
    with pytest.raises(ValueError, match="no segments"):
        sign_archive(DemoArchive({}), output)
    assert not output.exists()


def test_demo_rejects_content_changed_after_checksum(sign_archive, tmp_path):
    archive = DemoArchive(
        {"metadata.json": b'{"run":1}'}, {"metadata.json": b'{"run":2}'}
    )
    output = tmp_path / "changed.parquet"
    with pytest.raises(VerificationError):
        sign_archive(archive, output)
    assert all(stream.closed for stream in archive.streams)
    assert not output.exists()


def test_demo_verifies_whole_file_with_bounded_reads(sign_archive, tmp_path):
    from pyarrow import parquet

    class BoundedStream(io.BytesIO):
        def __init__(self, data):
            super().__init__(data)
            self.bytes_read = 0

        def read(self, size=-1):
            assert 0 <= size <= 65536, "whole-file verification must stream reads"
            data = super().read(size)
            self.bytes_read += len(data)
            return data

    data = b"auxiliary metadata\n" * 20000
    archive = DemoArchive({"metadata.json": data}, stream_type=BoundedStream)
    output = tmp_path / "streamed.parquet"
    sign_archive(archive, output)
    assert len(archive.streams) == 2
    assert all(stream.closed and stream.bytes_read == len(data) for stream in archive.streams)
    records = segment_signatures_from_table(parquet.read_table(output))
    assert len(records) == 1 and records[0].length is None
    assert records[0].verify_against(hashlib.sha512(data).digest())
