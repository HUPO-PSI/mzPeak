import click
import humanize

from mzpeak import MzPeakFile

@click.command
@click.option("-c", "--chromatograms", is_flag=True)
@click.option("-w", "--wavelength", is_flag=True)
@click.option("-p", "--peaks", is_flag=True)
@click.argument('path')
@click.argument("column_path")
def main(path: str, column_path: str, chromatograms: bool, peaks: bool, wavelength: bool):
    archive = MzPeakFile(path)
    if chromatograms:
        if column_path.startswith(("point", "chunk")):
            meta = archive.chromatogram_data.meta
        else:
            meta = archive.chromatogram_metadata.meta
    elif wavelength:
        if column_path.startswith(("point", "chunk")):
            meta = archive._wavelength_spectrum_data.meta
        else:
            meta = archive._wavelength_spectrum_metadata.meta
    elif peaks:
        meta = archive.spectrum_peak_data.meta
    else:
        if column_path.startswith(("point", 'chunk')):
            meta = archive.spectrum_data.meta
        else:
            meta = archive.spectrum_metadata.meta

    z = 0
    zu = 0
    min_val = float('inf')
    max_val = -float('inf')
    for i in range(0, meta.num_row_groups):
        rg = meta.row_group(i)
        for j in range(meta.num_columns):
            col_idx = rg.column(j)
            if (
                col_idx.path_in_schema == column_path
                or (col_idx.path_in_schema == column_path + ".list.item")
                or (col_idx.path_in_schema == column_path + ".list.element")
            ):
                if col_idx.statistics:
                    try:
                        min_val = min(min_val, col_idx.statistics.min)
                        max_val = max(max_val, col_idx.statistics.max)
                    except Exception:
                        pass
                z += col_idx.total_compressed_size
                zu += col_idx.total_uncompressed_size
                break
        else:
            raise click.ClickException(
                f"Column {column_path} was not found in {meta.schema}"
            )
    print(
        f"Compressed Size: {humanize.naturalsize(z, format='%.3f')} over {meta.num_row_groups} row groups"
    )
    print(f"Decompressed Size: {humanize.naturalsize(zu, format='%.3f')}")
    if (min_val != float('inf')):
        print(f"Min = {min_val}\nMax = {max_val}")

if __name__ == '__main__':
    main.main()