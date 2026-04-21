import click
import time
import logging

from collections import Counter

from mzpeak import MzPeakFile

logger = logging.getLogger("read_mzpeak")

@click.command()
@click.argument("path", type=click.Path(exists=True, readable=True))
def main(path):
    logging.basicConfig(
        level=logging.INFO,
        stream=click.get_text_stream("stderr"),
        format="%(asctime)s | %(levelname)-6s | %(name)-9s | %(message)s",
        datefmt="%Y-%m-%d %H:%M:%S",
    )
    reader = MzPeakFile(path)
    n_points = 0
    start = time.monotonic()
    it = iter(reader)
    n = len(reader)
    last_points = 0
    spec_repr = Counter()
    ms_levels = Counter()
    for i, spec in enumerate(it):
        n_points += len(spec['m/z array'])
        spec_repr[spec["spectrum representation"]] += 1
        ms_levels[spec["ms level"]] += 1
        if i % 1000 == 0 or n_points - last_points > 1e6:
            logger.info(
                f"Read spectrum {i:,}/{n:,} ({i/n * 100:0.2f}%), {n_points:,} points read so far\n"
                f"\tRepresentations: {dict(spec_repr)}\n"
                f"\tMS levels: {dict(ms_levels)}"
            )
            last_points = n_points

    end = time.monotonic()
    logger.info(f"Read {n_points:,} points from {path} over {i} spectra in {end - start:0.3f} seconds")


if __name__ == "__main__":
    main()