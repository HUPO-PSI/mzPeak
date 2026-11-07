import hashlib
import hmac
from collections.abc import Sequence
from dataclasses import asdict, dataclass
from datetime import UTC, datetime
from typing import IO, TYPE_CHECKING, Self

try:
    from asn1crypto import algos, cms, core, pem
    from asn1crypto import x509 as x509_asn1
    from cryptography import x509
    from cryptography.exceptions import InvalidSignature, UnsupportedAlgorithm
    from cryptography.hazmat.primitives import serialization
    from cryptography.hazmat.primitives.asymmetric import ed25519
    has_crypto = True
except ImportError:
    has_crypto = False

if TYPE_CHECKING:
    import pyarrow


def _single_attr(attrs: "cms.CMSAttributes", name: str):
    """Return the one value of a signed attribute; reject absent or multi-valued."""
    matches = [a for a in attrs if a["type"].native == name]
    if len(matches) != 1 or len(matches[0]["values"]) != 1:
        raise ValueError(f"expected exactly one {name} attribute value")
    return matches[0]["values"][0].native


class VerificationError(Exception):
    pass


def _parse_cms_signature(signature: bytes) -> "cms.ContentInfo":
    """Parse a complete CMS SignedData value without asserting validity or trust."""
    if not has_crypto:
        raise ImportError("CMS parsing requires the provenance dependencies")

    def check_fields(value):
        if isinstance(value, core.Sequence):
            # asn1crypto permits extra elements even in a named ASN.1 SEQUENCE.
            # Its field definitions are needed to reject that malformed input.
            if value._fields and len(value) > len(value._fields):
                raise ValueError("unexpected ASN.1 sequence field")
            for index in range(len(value)):
                check_fields(value[index])
        elif isinstance(value, core.SequenceOf):
            for child in value:
                check_fields(child)
        elif isinstance(value, core.Choice):
            check_fields(value.chosen)
        elif isinstance(value, core.Any):
            check_fields(value.parsed)

    try:
        content_info = cms.ContentInfo.load(signature, strict=True)
        if content_info["content_type"].native != "signed_data":
            raise ValueError("expected CMS SignedData")
        # asn1crypto decodes lazily. Traverse the body so malformed nested values
        # cannot pass merely because the outer ContentInfo was readable.
        decoded = content_info.native
        check_fields(content_info)
        if content_info.dump(force=True) != signature:
            raise ValueError("expected ASN.1 DER encoding")
        if not decoded["content"]["signer_infos"]:
            raise ValueError("expected at least one CMS signer")
    except (ValueError, TypeError, KeyError) as error:
        raise ValueError("signature must contain a complete CMS SignedData value") from error
    return content_info


@dataclass
class SegmentSignature:
    """
    The SHA-512 checksum of the raw bytes of a single Parquet column chunk
    (dictionary page, if any, plus all data pages), as laid out in the file,
    or another file in the archive.
    """

    # The name of the file the column chunk was read from
    filename: str
    # The index of the row group this column chunk belongs to
    row_group: int | None
    # The index of the column within the row group
    column: int | None
    # The dotted path of the column in the Parquet schema
    path: str | None
    # The byte offset of the first page of the chunk in the file
    offset: int | None
    # The number of bytes in the chunk (total compressed size)
    length: int | None
    # The SHA-512 digest of the chunk's bytes, potentially signed in Cryptographic Messaging Syntax (CMS) ASN.1 DER
    # using the provider's private key.
    signature: bytes
    # In-memory completion state; never stored or used as a trust decision.
    signed: bool = False

    def copy(self) -> Self:
        return self.__class__(**asdict(self))

    def sign_with(self, private_key: "ed25519.Ed25519PrivateKey", certificate: "x509.Certificate") -> "cms.ContentInfo":
        if self.signed:
            raise ValueError(f"Cannot sign {self}, it is already signed")
        if not has_crypto:
            raise ImportError(f"Cannot sign {self}, do not have cryptography and/or asn1crypto")
        if not isinstance(private_key, ed25519.Ed25519PrivateKey):
            raise ValueError("legacy segments require an Ed25519 private key")
        cert_key = certificate.public_key()
        if not isinstance(cert_key, ed25519.Ed25519PublicKey) or (
            private_key.public_key().public_bytes(
                serialization.Encoding.Raw, serialization.PublicFormat.Raw
            ) != cert_key.public_bytes(serialization.Encoding.Raw, serialization.PublicFormat.Raw)
        ):
            raise ValueError("certificate does not match the signing key")
        if len(self.signature) != hashlib.sha512().digest_size:
            raise ValueError("unsigned segment must contain a SHA-512 digest")

        # We'll use the actual hashing machinery from :mod:`cryptography`, but as they do not
        # support the CMS standard, and their PKCS #7 implementation doesn't cover EdDSA, we
        # have to hand-write the CMS DER ourselves.

        # First, prepare the actual signed attributes. We'll store these and their signed form
        # together in the DER so we can verify they're linked with the files and the private key
        signed_attrs = cms.CMSAttributes(
            [
                cms.CMSAttribute({"type": "content_type", "values": ["data"]}),
                cms.CMSAttribute(
                    {
                        "type": "signing_time",
                        "values": [
                            cms.Time(
                                {
                                    "utc_time": datetime.now(UTC).replace(
                                        microsecond=0
                                    )
                                }
                            )
                        ],
                    }
                ),
                cms.CMSAttribute(
                    {"type": "message_digest", "values": [self.signature]}
                ),
            ]
        )

        # Sign the attributes, now `signature` is tied to the ASN.1 representation of the digest
        # and the metadata tags. This signs with the SET tag (0x31) here, but they'll be converted
        # to IMPLICIT tags inside SignerInfo
        signature = private_key.sign(signed_attrs.dump())

        # Note that we used SHA-512 to hash the data to produce this digest
        sha512 = algos.DigestAlgorithm({"algorithm": "sha512"})  # parameters absent

        # Now we translate from :mod:`cryptography` to :mod:`asn1crypto`'s representation of x509
        # certificate so
        _, _, cert_bytes = pem.unarmor(certificate.public_bytes(serialization.Encoding.PEM))
        cert_asn1 = x509_asn1.Certificate.load(cert_bytes)

        signer_info = cms.SignerInfo(
            {
                "version": "v1",
                "sid": cms.SignerIdentifier(
                    {
                        "issuer_and_serial_number": cms.IssuerAndSerialNumber(
                            {
                                "issuer": cert_asn1.issuer,
                                "serial_number": cert_asn1.serial_number,
                            }
                        )
                    }
                ),
                "digest_algorithm": sha512,
                "signed_attrs": signed_attrs,
                "signature_algorithm": algos.SignedDigestAlgorithm(
                    {"algorithm": "ed25519"}
                ),  # 1.3.101.112, no params
                "signature": signature,
            }
        )

        signed_data = cms.SignedData(
            {
                "version": "v1",
                "digest_algorithms": [sha512],
                "encap_content_info": {"content_type": "data"},
                "certificates": [
                    cms.CertificateChoices(name="certificate", value=cert_asn1)
                ],
                "signer_infos": [signer_info],
            }
        )

        content_info = cms.ContentInfo(
            {"content_type": "signed_data", "content": signed_data}
        )

        self.signature = content_info.dump()
        self.signed = True
        return content_info

    def verify_against(self, digest: bytes):
        """Check legacy CMS cryptography only; this does not establish CA trust.

        The returned signing time is asserted by the signer, not a trusted timestamp.
        Segment identity and coordinates are not authenticated by this legacy format.
        """
        if not self.signed:
            raise ValueError("Cannot verify an unsigned segment")
        if not has_crypto:
            raise ImportError(f"Cannot verify {self}, do not have cryptography and/or asn1crypto")
        try:
            return self._verify_against(digest)
        except VerificationError:
            raise
        except (
            ValueError, TypeError, KeyError, IndexError, InvalidSignature, UnsupportedAlgorithm
        ) as error:
            raise VerificationError("malformed or invalid legacy CMS signature") from error

    def _verify_against(self, digest: bytes):
        if len(digest) != hashlib.sha512().digest_size:
            raise VerificationError("digest must contain 64 bytes")
        content_info = _parse_cms_signature(self.signature)
        if content_info["content_type"].native != "signed_data":
            raise VerificationError("not a SignedData")
        signed_data = content_info["content"]

        if signed_data["encap_content_info"]["content_type"].native != "data":
            raise VerificationError("encapsulated content type must be id-data")

        if signed_data["encap_content_info"]["content"].native is not None:
            raise VerificationError("expected a detached signature")

        signer_infos = signed_data["signer_infos"]
        if len(signer_infos) != 1:
            raise ValueError("expected exactly one signer")
        signer_info = signer_infos[0]

        digests = signed_data["digest_algorithms"]
        if len(digests) != 1 or digests[0]["algorithm"].native != "sha512":
            raise VerificationError("SignedData digest algorithms must be SHA-512 only")

        # Check algorithms match expectations
        if signer_info["digest_algorithm"]["algorithm"].native != "sha512":
            raise VerificationError("digest algorithm must be SHA-512")
        sig_alg = signer_info["signature_algorithm"]
        if sig_alg["algorithm"].native != "ed25519" or sig_alg["parameters"].dump():
            raise ValueError("signature algorithm must be Ed25519 with absent parameters")

        # Locate the signer certificate by issuer + serial
        sid = signer_info["sid"]
        if sid.name != "issuer_and_serial_number":
            raise VerificationError("unsupported signer identifier")
        want_issuer = sid.chosen["issuer"]
        want_serial = sid.chosen["serial_number"].native

        certs = [c.chosen for c in signed_data["certificates"]
            if c.name == "certificate"
            and c.chosen.issuer == want_issuer
            and c.chosen.serial_number == want_serial]

        if len(certs) != 1:
            raise VerificationError("expected exactly one matching signer certificate")
        cert = certs[0]

        public_key = serialization.load_der_public_key(cert.public_key.dump())
        if not isinstance(public_key, ed25519.Ed25519PublicKey):
            raise VerificationError("signer key is not Ed25519")

        # Verify the signature over the signed attributes
        signed_attrs = signer_info["signed_attrs"]
        if not signed_attrs:
            raise VerificationError("signed attributes are required")

        # Stored bytes are [0] IMPLICIT (0xA0); the signature covers them as a
        # universal SET (0x31). Swap only the tag byte the length and contents
        # are identical, and we must use the original bytes, not a re-encoding.
        # This is per <https://www.rfc-editor.org/info/rfc5652/#section-5.4>
        raw = signed_attrs.dump()
        if raw[0] != 0xA0:
            raise VerificationError("unexpected signed attributes tag")
        to_verify = b"\x31" + raw[1:]

        public_key.verify(signer_info["signature"].native, to_verify)

        # Check the attributes now that they are authenticated
        if _single_attr(signed_attrs, "content_type") != "data":
            raise VerificationError("content-type attribute must be id-data")

        claimed_digest = _single_attr(signed_attrs, "message_digest")

        if not hmac.compare_digest(digest, claimed_digest):
            raise VerificationError("content does not match message-digest")

        # This is an authenticated claim by the signer, not evidence of wall-clock time.
        signing_time = (
            _single_attr(signed_attrs, "signing_time")
            if any(a["type"].native == "signing_time" for a in signed_attrs)
            else None
        )
        return cert, signing_time

    def read_and_digest(self, source: IO[bytes]):
        """Hash an exact bounded range, or the remaining bytes when length is None."""
        offset = source.tell() if self.offset is None else self.offset
        if offset < 0 or (self.length is not None and self.length < 0):
            raise ValueError("segment offset and length must be nonnegative")
        end = source.seek(0, 2)
        source.seek(offset)
        length = end - offset if self.length is None else self.length
        if offset > end or length > end - offset:
            raise EOFError("segment extends beyond the end of the stream")
        digest = hashlib.sha512()
        while length:
            buf = source.read(min(length, 65536))
            if not buf:
                raise EOFError("stream ended before the complete segment was read")
            digest.update(buf)
            length -= len(buf)
        return digest.digest()


def checksum_parquet_segments(
    reader: "IO[bytes]", filename: str
) -> list[SegmentSignature]:
    """
    Compute a SHA-512 checksum of every column chunk in every row group of a Parquet file.

    Parameters
    ----------
    reader : file-like
        A seekable, readable binary stream over the Parquet file.
    filename : str
        The name of the file, recorded on each :class:`SegmentSignature`.

    Returns
    -------
    list[:class:`SegmentSignature`]
    """
    from pyarrow import parquet as pq

    header = pq.ParquetFile(reader).metadata
    checksums = []
    for row_group in range(header.num_row_groups):
        rg = header.row_group(row_group)
        for column in range(rg.num_columns):
            col = rg.column(column)
            # The chunk starts at the dictionary page when there is one, else the first data page
            if col.dictionary_page_offset is not None:
                offset = col.dictionary_page_offset
            else:
                offset = col.data_page_offset
            length = col.total_compressed_size
            reader.seek(offset)
            blob = reader.read(length)
            if len(blob) != length:
                raise EOFError(
                    f"Expected {length} bytes for row group {row_group} column {col.path_in_schema}, "
                    f"read {len(blob)}"
                )
            checksums.append(
                SegmentSignature(
                    filename,
                    row_group,
                    column,
                    col.path_in_schema,
                    offset,
                    length,
                    hashlib.sha512(blob).digest(),
                )
            )
    return checksums


def checksum_stream(reader: IO[bytes], filename: str, bufsize: int = 2 ** 16) -> SegmentSignature:
    if bufsize <= 0:
        raise ValueError("bufsize must be positive")
    chk = hashlib.sha512()
    while buf := reader.read(bufsize):
        chk.update(buf)
    return SegmentSignature(
        filename,
        None,
        None,
        None,
        None,
        None,
        chk.digest(),
    )

def segment_signature_schema() -> "pyarrow.Schema":
    """Signature-table schema revision 1; see doc/provenance-signatures.md."""
    import pyarrow as pa

    return pa.schema(
        [
            pa.field("filename", pa.large_string(), False),
            pa.field("row_group", pa.uint64(), True),
            pa.field("column", pa.uint64(), True),
            pa.field("path", pa.large_string(), True),
            pa.field("offset", pa.uint64(), True),
            pa.field("length", pa.uint64(), True),
            pa.field("signature", pa.large_binary(), False),
        ], metadata={"mzpeak.provenance": "1"}
    )


def segment_signatures_to_table(
    checksums: Sequence[SegmentSignature],
) -> "pyarrow.Table":
    """
    Store completed signatures using :func:`segment_signature_schema`.

    Unsigned records and malformed CMS payloads raise :class:`ValueError`.
    Structural parsing does not establish cryptographic validity or trust.
    """
    import pyarrow as pa

    for record in checksums:
        if not record.signed:
            raise ValueError("cannot serialize an unsigned segment")
        _parse_cms_signature(record.signature)
    schema = segment_signature_schema()
    return pa.table(
        {f.name: [getattr(c, f.name) for c in checksums] for f in schema}, schema=schema
    )


def segment_signatures_from_table(
    table: "pyarrow.Table | pyarrow.RecordBatch",
) -> list[SegmentSignature]:
    """
    Convert a table or record batch produced by :func:`segment_signatures_to_table`
    (or read back from a Parquet file holding it) into :class:`SegmentSignature` records.

    Nullable coordinates represent whole-file records. Every payload must parse as
    complete CMS SignedData before its in-memory ``signed`` state is restored.
    This does not establish cryptographic validity or trust. Unmarked tables are
    accepted under the same rules; unsupported explicit schema revisions are rejected.
    """
    if len(set(table.schema.names)) != len(table.schema.names):
        raise ValueError("duplicate column names")
    revision = (table.schema.metadata or {}).get(b"mzpeak.provenance")
    if revision is not None and revision != b"1":
        raise ValueError(f"unsupported provenance schema revision {revision!r}")
    schema = segment_signature_schema()
    names = schema.names
    columns = {}
    for name in names:
        if name not in table.schema.names:
            raise KeyError(f"Missing column {name!r}")
        col = table.column(name)
        field = schema.field(name)
        if col.type != field.type:
            raise ValueError(f"Column {name!r} has unexpected type {col.type}")
        if not field.nullable and col.null_count > 0:
            raise ValueError(f"Column {name!r} contains nulls")
        columns[name] = col.to_pylist()
    records = []
    for row in zip(*(columns[name] for name in names)):
        _parse_cms_signature(row[-1])
        records.append(SegmentSignature(*row, signed=True))
    return records
