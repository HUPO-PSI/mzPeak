"""Self-signed archive signing demo and its finalized signature-table format."""

import hashlib
import io
from datetime import UTC, datetime, timedelta

import pyarrow as pa
import pytest
from asn1crypto import cms, parser
from cryptography import x509
from cryptography.exceptions import InvalidSignature
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
    values = {
        "filename": "spectrum.parquet",
        "row_group": 0,
        "column": 0,
        "path": "point.mz",
        "offset": 0,
        "length": 6,
        "signature": hashlib.sha512(b"abcdef").digest(),
    }
    return SegmentSignature(**(values | kwargs))


@pytest.fixture
def signing_key():
    key = ed25519.Ed25519PrivateKey.from_private_bytes(bytes(range(32)))
    name = x509.Name([x509.NameAttribute(NameOID.COMMON_NAME, "Self-signed test signer")])
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


def test_completed_chunk_and_whole_file_signatures_roundtrip(signing_key, tmp_path):
    from pyarrow import parquet

    unsigned = checksum_stream(io.BytesIO(b"metadata"), "metadata.json")
    signed_record = unsigned.copy()
    signed_record.sign_with(*signing_key)
    records = [signed(signing_key), signed_record]
    path = tmp_path / "provenance.parquet"
    parquet.write_table(segment_signatures_to_table(records), path)
    table = parquet.read_table(path)
    assert "signed" not in table.schema.names
    assert table.schema.metadata[b"mzpeak.provenance"] == b"1"
    result = segment_signatures_from_table(table)
    assert result == records
    assert result[1].verify_against(unsigned.signature)


@pytest.mark.parametrize("record_batch", [False, True])
def test_unmarked_completed_signature_tables_are_readable(signing_key, record_batch):
    records = [signed(signing_key)]
    table = segment_signatures_to_table(records).replace_schema_metadata(None)
    source = table.to_batches()[0] if record_batch else table
    assert segment_signatures_from_table(source) == records


def test_required_fields_reject_nulls(signing_key):
    table = segment_signatures_to_table([signed(signing_key)])
    for name in ["filename", "signature"]:
        i = table.schema.get_field_index(name)
        malformed = table.set_column(
            i, name, pa.array([None], type=table.column(i).type)
        )
        with pytest.raises(ValueError):
            segment_signatures_from_table(malformed)


def test_writing_rejects_unfinished_records(signing_key):
    complete = signed(signing_key)
    unfinished = complete.copy()
    unfinished.signed = False
    for records in [[segment()], [complete, segment()], [unfinished]]:
        with pytest.raises(ValueError, match="unsigned"):
            segment_signatures_to_table(records)


def malformed_payloads(record):
    def append_field(encoded):
        cls, method, tag, _, content, _ = parser.parse(encoded, strict=True)
        return parser.emit(cls, method, tag, content + b"\x05\x00")

    no_signers = cms.ContentInfo.load(record.signature)
    no_signers["content"]["signer_infos"] = []
    body = cms.ContentInfo.load(record.signature)["content"].untag().dump()
    _, _, _, _, outer_content, _ = parser.parse(record.signature, strict=True)
    length = len(outer_content).to_bytes((len(outer_content).bit_length() + 7) // 8, "big")
    extra_body_field = parser.emit(
        0, 1, 16,
        cms.ContentType("signed_data").dump() + parser.emit(2, 1, 0, append_field(body)),
    )
    return [
        b"",
        b"x" * 64,
        b"x" * 65,
        b"\x30\x00",
        # Readable ContentInfo with id-signedData, but a malformed inner body.
        bytes.fromhex("300f06092a864886f70d010702a0023000"),
        cms.ContentInfo({"content_type": "data", "content": b"abcdef"}).dump(),
        record.signature[:-1],
        record.signature + b"\x00",
        append_field(record.signature),
        extra_body_field,
        b"\x30\x80" + outer_content + b"\x00\x00",  # Indefinite BER length.
        b"\x30" + bytes([0x81 + len(length)]) + b"\x00" + length + outer_content,
        no_signers.dump(),
    ]


@pytest.mark.parametrize("unmarked", [False, True])
def test_malformed_cms_is_rejected_on_read_and_write(signing_key, unmarked):
    record = signed(signing_key)
    table = segment_signatures_to_table([record])
    if unmarked:
        table = table.replace_schema_metadata(None)
    for payload in malformed_payloads(record):
        bad_table = table.set_column(
            table.schema.get_field_index("signature"),
            table.schema.field("signature"),
            pa.array([payload], type=pa.large_binary()),
        )
        for source in [bad_table, bad_table.to_batches()[0]]:
            with pytest.raises(ValueError, match="CMS SignedData"):
                segment_signatures_from_table(source)
        malformed = record.copy()
        malformed.signature = payload
        with pytest.raises(ValueError, match="CMS SignedData"):
            segment_signatures_to_table([malformed])


def test_stored_state_cannot_override_cms_parsing(signing_key):
    record = signed(signing_key)
    table = segment_signatures_to_table([record]).append_column("signed", pa.array([False]))
    assert segment_signatures_from_table(table) == [record]
    malformed = table.set_column(
        table.schema.get_field_index("signature"),
        table.schema.field("signature"),
        pa.array([b"not CMS"], type=pa.large_binary()),
    ).set_column(table.schema.get_field_index("signed"), "signed", pa.array([True]))
    with pytest.raises(ValueError, match="CMS SignedData"):
        segment_signatures_from_table(malformed)


@pytest.mark.parametrize("revision", [b"", b"0", b"2", b"future"])
def test_unsupported_schema_revisions_are_rejected(signing_key, revision):
    table = segment_signatures_to_table([signed(signing_key)]).replace_schema_metadata(
        {b"mzpeak.provenance": revision}
    )
    with pytest.raises(ValueError, match="unsupported provenance schema revision"):
        segment_signatures_from_table(table)


def test_parsing_does_not_establish_signature_validity(signing_key):
    record = signed(signing_key)
    content = cms.ContentInfo.load(record.signature)
    content["content"]["signer_infos"][0]["signature"] = bytes(64)
    record.signature = content.dump()
    loaded = segment_signatures_from_table(segment_signatures_to_table([record]))[0]
    assert loaded.signed
    with pytest.raises(InvalidSignature):
        loaded.verify_against(hashlib.sha512(b"abcdef").digest())


@pytest.fixture
def sign_archive():
    import importlib.util
    from pathlib import Path

    path = Path(__file__).resolve().parents[1] / "scripts/sign_archive.py"
    spec = importlib.util.spec_from_file_location("self_signed_archive_demo", path)
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
    output = tmp_path / "self-signed-signatures.parquet"
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
