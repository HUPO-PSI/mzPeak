"""Proof storage must never become a source of measurement data."""

import json
import zipfile

import numpy as np
import pytest

from mzpeak import MzPeakFile
from mzpeak.file_index import FileIndex


def _modified_archive(tmp_path, edit_index, added_members):
    path = tmp_path / "modified.mzpeak"
    with zipfile.ZipFile("small.mzpeak") as source, zipfile.ZipFile(path, "w") as dest:
        index = json.loads(source.read(FileIndex.FILE_NAME))
        edit_index(index)
        for member in source.infolist():
            if member.filename != FileIndex.FILE_NAME:
                dest.writestr(member.filename, source.read(member))
        for name, original in added_members:
            dest.writestr(name, source.read(original))
        dest.writestr(FileIndex.FILE_NAME, json.dumps(index))
    return path


@pytest.mark.parametrize("unpacked", [False, True])
@pytest.mark.parametrize("name", [
    "provenance/bundle-v1.json", "provenance.parquet",
    "Provenance/bundle-v1.json", "Provenance.parquet",
    "provenance/spectra_data.parquet",
])
def test_index_cannot_assign_measurements_to_proof_storage(tmp_path, name, unpacked):
    def edit_index(index):
        index["files"][0]["name"] = name

    path = _modified_archive(tmp_path, edit_index, [(name, "spectra_data.parquet")])
    if unpacked:
        directory = tmp_path / "unpacked"
        with zipfile.ZipFile(path) as archive:
            archive.extractall(directory)
        path = directory
    with pytest.raises(ValueError, match="provenance members cannot supply measurement data"):
        MzPeakFile(path)


@pytest.mark.parametrize("same_name", [False, True])
def test_duplicate_indexed_measurement_roles_are_rejected(tmp_path, same_name):
    name = "spectra_data.parquet" if same_name else "other/spectra_data.parquet"

    def edit_index(index):
        index["files"].append({**index["files"][0], "name": name})

    path = _modified_archive(
        tmp_path, edit_index, [] if same_name else [(name, "spectra_data.parquet")]
    )
    with pytest.raises(ValueError, match="duplicate indexed member|ambiguous measurement role"):
        MzPeakFile(path)


@pytest.mark.parametrize("name", [
    "provenance/spectra_data.parquet", "Provenance/spectra_data.parquet",
    "processing/spectra_data.parquet",
])
def test_unindexed_suffix_cannot_replace_indexed_measurements(tmp_path, name):
    path = _modified_archive(tmp_path, lambda index: None, [(name, "spectra_peaks.parquet")])
    with MzPeakFile("small.mzpeak") as original, MzPeakFile(path) as reader:
        np.testing.assert_array_equal(reader[0]["m/z array"], original[0]["m/z array"])
