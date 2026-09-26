import sys

import hashlib
import pyarrow as pa
from pyarrow import parquet as pq
from mzpeak import MzPeakFile


def main():
    path = sys.argv[1]
    archive = MzPeakFile(path)

    try:
        index = int(sys.argv[2])
    except IndexError:
        index = 0

    entry = archive.file_index[index]
    reader = pa.PythonFile(archive.open_stream(entry))

    header = pq.ParquetFile(reader).metadata

    for rg_i in range(header.num_row_groups):
        rg = header.row_group(rg_i)
        for col_i in range(rg.num_columns):
            col = rg.column(col_i)
            if col.dictionary_page_offset:
                reader.seek(col.dictionary_page_offset)
            else:
                reader.seek(col.data_page_offset)
            blob = reader.read(col.total_compressed_size)
            assert len(blob) == col.total_compressed_size
            print(rg_i, col.path_in_schema, hashlib.sha512(blob).hexdigest())


if __name__ == '__main__':
    main()