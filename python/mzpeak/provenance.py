import hashlib
import hmac
from collections.abc import Sequence
from dataclasses import asdict, dataclass
from dataclasses import fields as dataclasses_fields
from datetime import UTC, datetime
from typing import IO, TYPE_CHECKING, Self

try:
    from asn1crypto import algos, cms, pem
    from asn1crypto import x509 as x509_asn1
    from cryptography import x509
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
    row_group: int
    # The index of the column within the row group
    column: int
    # The dotted path of the column in the Parquet schema
    path: str
    # The byte offset of the first page of the chunk in the file
    offset: int
    # The number of bytes in the chunk (total compressed size)
    length: int
    # The SHA-512 digest of the chunk's bytes, potentially signed in Cryptographic Messaging Syntax (CMS) ASN.1 DER
    # using the provider's private key.
    signature: bytes
    # Whether the chunk is signed or not
    signed: bool = False

    def copy(self) -> Self:
        return self.__class__(**asdict(self))

    def sign_with(self, private_key: "ed25519.Ed25519PrivateKey", certificate: "x509.Certificate") -> "cms.ContentInfo":
        if self.signed:
            raise ValueError(f"Cannot sign {self}, it is already signed")
        if not has_crypto:
            raise ImportError(f"Cannot sign {self}, do not have cryptography and/or asn1crypto")

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
        if not self.signed:
            raise ValueError("Cannot verify an unsigned segment")
        if not has_crypto:
            raise ImportError(f"Cannot verify {self}, do not have cryptography and/or asn1crypto")
        content_info = cms.ContentInfo.load(self.signature)
        if content_info["content_type"].native != "signed_data":
            raise VerificationError("not a SignedData")
        signed_data = content_info["content"]

        if signed_data["encap_content_info"]["content"].native is not None:
            raise VerificationError("expected a detached signature")

        signer_infos = signed_data["signer_infos"]
        if len(signer_infos) != 1:
            raise ValueError("expected exactly one signer")
        signer_info = signer_infos[0]

        # Check algorithms match expectations
        if signer_info["digest_algorithm"]["algorithm"].native != "sha512":
            raise VerificationError("digest algorithm must be SHA-512")
        sig_alg = signer_info["signature_algorithm"]
        if sig_alg["algorithm"].native != "ed25519" or sig_alg["parameters"].native is not None:
            raise ValueError("signature algorithm must be Ed25519 with absent parameters")

        # Locate the signer certificate by issuer + serial
        sid = signer_info["sid"]
        if sid.name != "issuer_and_serial_number":
            raise VerificationError("unsupported signer identifier")
        want_issuer = sid.chosen["issuer"]
        want_serial = sid.chosen["serial_number"].native

        cert = next(
            (c.chosen for c in signed_data["certificates"]
            if c.name == "certificate"
            and c.chosen.issuer == want_issuer
            and c.chosen.serial_number == want_serial),
            None,
        )

        if cert is None:
            raise VerificationError("message does not contain a certificate to retrieve a public key from")

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

        # Get the signing time, if available. This can be used to check
        # if the ceritificate was valid when it was claimed to be used, and/or
        # if it had been revoked.
        signing_time = next(
            (
                a["values"][0].native
                for a in signed_attrs
                if a["type"].native == "signing_time"
            ),
            None,
        )
        return cert, signing_time



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
            if col.dictionary_page_offset:
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
    """The Arrow schema used to store :class:`SegmentSignature` records"""
    import pyarrow as pa

    return pa.schema(
        [
            pa.field("filename", pa.large_string(), False),
            pa.field("row_group", pa.uint64(), False),
            pa.field("column", pa.uint64(), False),
            pa.field("path", pa.large_string(), False),
            pa.field("offset", pa.uint64(), False),
            pa.field("length", pa.uint64(), False),
            pa.field("signature", pa.large_binary(), False),
        ]
    )


def segment_signatures_to_table(
    checksums: Sequence[SegmentSignature],
) -> "pyarrow.Table":
    """
    Convert a sequence of :class:`SegmentSignature` into a :class:`pyarrow.Table`
    with the schema from :func:`segment_signature_schema`, suitable for writing to Parquet.
    """
    import pyarrow as pa

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

    Columns are looked up by name. A :class:`KeyError` is raised if one is missing and a
    :class:`ValueError` if one contains nulls.
    """
    names = [f.name for f in dataclasses_fields(SegmentSignature) if f.name != 'signed']
    columns = {}
    for name in names:
        if name not in table.schema.names:
            raise KeyError(f"Missing column {name!r}")
        col = table.column(name)
        if col.null_count > 0:
            raise ValueError(f"Column {name!r} contains nulls")
        columns[name] = col.to_pylist()
    return [SegmentSignature(*row) for row in zip(*(columns[n] for n in names))]
