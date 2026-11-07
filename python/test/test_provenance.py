"""Regression tests for the legacy segment envelope and Arrow representation."""
import hashlib
import io
from datetime import UTC, datetime, timedelta

import pytest
from asn1crypto import cms
from cryptography import x509
from cryptography.hazmat.primitives.asymmetric import ed25519
from cryptography.x509.oid import NameOID

from mzpeak.provenance import (
    SegmentSignature, VerificationError, checksum_stream,
)


def segment(**kwargs):
    values = dict(filename="spectrum.parquet", row_group=0, column=0,
                  path="point.mz", offset=0, length=6,
                  signature=hashlib.sha512(b"abcdef").digest())
    return SegmentSignature(**(values | kwargs))


@pytest.fixture
def signing_key():
    key = ed25519.Ed25519PrivateKey.from_private_bytes(bytes(range(32)))
    name = x509.Name([x509.NameAttribute(NameOID.COMMON_NAME, "Legacy test signer")])
    now = datetime.now(UTC)
    cert = (x509.CertificateBuilder().subject_name(name).issuer_name(name)
            .public_key(key.public_key()).serial_number(123)
            .not_valid_before(now - timedelta(days=1))
            .not_valid_after(now + timedelta(days=1)).sign(key, None))
    return key, cert


def signed(signing_key):
    record = segment()
    record.sign_with(*signing_key)
    return record


def test_explicit_zero_offset_and_length():
    source = io.BytesIO(b"abcdef")
    source.seek(3)
    assert segment().read_and_digest(source) == hashlib.sha512(b"abcdef").digest()
    assert segment(length=0).read_and_digest(source) == hashlib.sha512(b"").digest()


def test_none_length_hashes_remaining_stream():
    source = io.BytesIO(b"abcdef")
    source.seek(3)
    assert segment(offset=None, length=None).read_and_digest(source) == hashlib.sha512(b"def").digest()


@pytest.mark.parametrize("offset,length", [(7, 0), (4, 3), (0, 7), (7, None)])
def test_ranges_past_end_rejected(offset, length):
    with pytest.raises(EOFError):
        segment(offset=offset, length=length).read_and_digest(io.BytesIO(b"abcdef"))


@pytest.mark.parametrize("offset,length", [(-1, 0), (0, -1)])
def test_negative_ranges_rejected(offset, length):
    with pytest.raises(ValueError):
        segment(offset=offset, length=length).read_and_digest(io.BytesIO(b"abcdef"))


def test_partial_reads_are_accumulated_and_premature_eof_rejected():
    class Partial(io.BytesIO):
        def read(self, size=-1):
            return super().read(min(size, 2))

    class Truncated(Partial):
        def read(self, size=-1):
            if self.tell() >= 2:
                return b""
            return super().read(size)

    assert segment().read_and_digest(Partial(b"abcdef")) == hashlib.sha512(b"abcdef").digest()
    with pytest.raises(EOFError):
        segment().read_and_digest(Truncated(b"abcdef"))










def test_signature_matches_content(signing_key):
    record = signed(signing_key)
    assert record.verify_against(hashlib.sha512(b"abcdef").digest())
    with pytest.raises(VerificationError):
        record.verify_against(hashlib.sha512(b"abcdeg").digest())


@pytest.mark.parametrize("payload", [b"", b"garbage", b"\x30\x00", b"\x30\x80"])
def test_malformed_cms_rejected(payload):
    with pytest.raises(VerificationError):
        segment(signature=payload, signed=True).verify_against(bytes(64))


def test_trailing_data_and_invalid_signature_rejected(signing_key):
    record = signed(signing_key)
    record.signature += b"\x00"
    with pytest.raises(VerificationError):
        record.verify_against(segment().signature)
    record = signed(signing_key)
    envelope = cms.ContentInfo.load(record.signature)
    envelope["content"]["signer_infos"][0]["signature"] = bytes(64)
    record.signature = envelope.dump()
    with pytest.raises(VerificationError):
        record.verify_against(segment().signature)


def mutate(record, callback):
    envelope = cms.ContentInfo.load(record.signature)
    callback(envelope["content"])
    record.signature = envelope.dump()
    return record


def test_multiple_signers_and_attached_content_rejected(signing_key):
    def duplicate(data):
        data["signer_infos"] = [data["signer_infos"][0], data["signer_infos"][0]]
    record = mutate(signed(signing_key), duplicate)
    with pytest.raises(VerificationError):
        record.verify_against(segment().signature)
    record = mutate(signed(signing_key), lambda data: data["encap_content_info"].__setitem__("content", b"abcdef"))
    with pytest.raises(VerificationError):
        record.verify_against(segment().signature)


@pytest.mark.parametrize("attribute", ["content_type", "message_digest", "signing_time"])
def test_duplicate_signed_attributes_rejected_even_with_valid_signature(signing_key, attribute):
    key, _ = signing_key
    record = signed(signing_key)
    envelope = cms.ContentInfo.load(record.signature)
    signer = envelope["content"]["signer_infos"][0]
    attrs = list(signer["signed_attrs"])
    attrs.append(next(attr for attr in attrs if attr["type"].native == attribute))
    attrs = cms.CMSAttributes(attrs)
    signer["signed_attrs"] = attrs
    signer["signature"] = key.sign(attrs.dump())
    record.signature = envelope.dump()
    with pytest.raises(VerificationError):
        record.verify_against(segment().signature)


def test_key_certificate_mismatch_rejected(signing_key):
    _, cert = signing_key
    with pytest.raises(ValueError, match="does not match"):
        segment().sign_with(ed25519.Ed25519PrivateKey.generate(), cert)


def test_zero_buffer_size_rejected():
    with pytest.raises(ValueError):
        checksum_stream(io.BytesIO(b"abcdef"), "metadata", bufsize=0)
