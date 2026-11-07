import sys
from datetime import datetime, timedelta, timezone
from pathlib import Path

import mzpeak
from cryptography import x509
from cryptography.hazmat.primitives.asymmetric import ed25519
from cryptography.x509.oid import NameOID
from mzpeak import provenance
from pyarrow import parquet


def sign_archive(archive: mzpeak.MzPeakFile, output_path: Path):
    """Legacy self-signed demonstration; does not certify archive identity or coverage."""
    blocks: list[provenance.SegmentSignature] = []
    for entry in archive.file_index:
        with archive.open_stream(entry) as stream:
            if entry.name.endswith(".parquet"):
                blocks.extend(provenance.checksum_parquet_segments(stream, entry.name))
            else:
                blocks.append(provenance.checksum_stream(stream, entry.name))

    if not blocks:
        raise ValueError("archive contains no segments to sign")

    issuer = x509.Name(
        [
            x509.NameAttribute(NameOID.COUNTRY_NAME, "US"),
            x509.NameAttribute(NameOID.STATE_OR_PROVINCE_NAME, "Massachussets"),
            x509.NameAttribute(NameOID.LOCALITY_NAME, "Boston"),
            x509.NameAttribute(NameOID.ORGANIZATION_NAME, "mzPeak"),
            x509.NameAttribute(NameOID.COMMON_NAME, "mzpeak.com"),
        ]
    )
    private_key = ed25519.Ed25519PrivateKey.generate()
    public_key = private_key.public_key()
    cert = x509.CertificateBuilder(
        issuer,
        issuer,
        public_key,
        x509.random_serial_number(),
        datetime.now(timezone.utc),
        datetime.now(timezone.utc) + timedelta(days=10),
    ).sign(private_key, None)

    for block in blocks:
        block.sign_with(private_key, cert)
        with archive.open_stream(block.filename) as stream:
            if block.length is None:
                digest = provenance.checksum_stream(stream, block.filename).signature
            else:
                digest = block.read_and_digest(stream)
            block.verify_against(digest)

    table = provenance.segment_signatures_to_table(blocks)

    with parquet.ParquetWriter(output_path, table.schema) as writer:
        writer.write_table(table)


def main():
    inpath = sys.argv[1]
    outpath = sys.argv[2]
    sign_archive(mzpeak.MzPeakFile(inpath), Path(outpath))

if __name__ == '__main__':
    main()
