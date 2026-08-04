import logging  # noqa: I001
import json
import zipfile
import zlib

from dataclasses import dataclass, field
from pathlib import Path
from collections.abc import Iterable, Sequence
from typing import IO, Any, ClassVar, Optional, TYPE_CHECKING
from collections.abc import Iterator, Callable
from enum import Enum, auto

import numpy as np
import pandas as pd

import pynumpress
import pyarrow as pa

from pyarrow import parquet as pq

from .mz_reader import _DataBatchIter, MzPeakArrayDataReader, _SpectrumArrays
from .file_index import FileEntry, FileIndex, DataKind, EntityType
from .util import _SeekableIter, OntologyMapper, DTYPES, _NameCleaningNode

try:
    has_upath = True
    from upath import UPath
except ImportError:
    has_upath = False
    from pathlib import Path as UPath

if TYPE_CHECKING:
    from upath import UPath  # noqa: TC004



logger = logging.getLogger(__name__)
logger.addHandler(logging.NullHandler())

CV_MAPPER = OntologyMapper(
    overrides={"mz_signal_continuity": "spectrum representation"}
)


class ArchiveStorage(Enum):
    Zip = auto()
    Directory = auto()
    FileSpecZip = auto()
    FileSpecDirectory = auto()


def _value_normalize(val: dict):
    for v in val.values():
        if v is not None:
            return v
    return None


class RTLocator:
    def __init__(self, reader):
        self._reader = reader

    def resolve(self, time: float | slice):
        if isinstance(time, slice):
            start_time = time.start or 0.0
            end_time = time.stop or self._reader.spectra["time"].iloc[-1]
            start_hit = self._get_scan_by_time(start_time)
            end_hit = self._get_scan_by_time(end_time)
            if not start_hit:
                return []
            start_index, _ = start_hit
            end_index, _ = end_hit
            return slice(start_index, end_index + 1)
        else:
            hit = self._get_scan_by_time(time)
            if not hit:
                raise KeyError(time)
            (index, _) = hit
            return index

    def _get_scan_by_time(self, time: float) -> tuple[int, float] | None:
        """
        Retrieve the scan object for the specified scan time.

        Parameters
        ----------
        time : float
            The time to get the nearest scan from
        Returns
        -------
        tuple: (scan_index, scan_time)
        """
        spectra_df = self._reader.spectra
        times = spectra_df["time"]
        indices = spectra_df.index

        lo = 0
        hi = len(indices)

        if hi == 0:
            return None

        best_error = float("inf")
        best_time = None
        best_id = None

        if time == float("inf"):
            return indices[-1], times[-1]

        while hi != lo:
            mid = (hi + lo) // 2
            sid = indices[mid]
            scan_time = times[sid]
            err = abs(scan_time - time)
            if err < best_error:
                best_error = err
                best_time = scan_time
                best_id = sid
            if scan_time == time:
                return sid, scan_time
            elif (hi - lo) == 1:
                return best_id, best_time
            elif scan_time > time:
                hi = mid
            else:
                lo = mid

        if time == float("inf"):
            return indices[-1], times[-1]
        else:
            return None

    def __getitem__(self, time: float | slice):
        idx = self.resolve(time)
        return self._reader[idx]


def _format_curie(curie: dict | str):
    if curie is None:
        return None
    elif isinstance(curie, str):
        return curie
    else:
        raise NotImplementedError()


def _format_param(param: dict):
    param = param.copy()
    param["value"] = _value_normalize(param["value"])
    param["accession"] = _format_curie(param["accession"])
    if param.get("unit"):
        param["unit"] = _format_curie(param["unit"])
    return param


def _clean_frame(df: pd.DataFrame, clean_columns: bool = True):
    columns = df.columns[~df.isna().all(axis=0)]
    df = df[columns]
    if clean_columns:
        df = CV_MAPPER.clean_column_names(df)
    return df


class _AuxiliaryArrayDecoder:
    """
    A helper class for decoding extra arrays packed in with the metadata table.
    """

    compression: ClassVar[dict[str, Callable]] = {
        "MS:1000576": lambda x: x,
        "MS:1000574": zlib.decompress,
        "MS:1002314": pynumpress.decode_slof,
        "MS:1002313": pynumpress.decode_pic,
        "MS:1002312": pynumpress.decode_linear,
    }

    dtypes = DTYPES
    ascii_code = "MS:1001479"

    @classmethod
    def decode(cls, arr: dict):
        data: np.ndarray = arr["data"]
        compression_acc: str = _format_curie(arr["compression"])
        dtype_acc: str = _format_curie(arr["data_type"])
        name_param = _format_param(arr["name"])
        if name_param["name"] == "non-standard data array":
            name = name_param["value"]
        else:
            name = name_param["name"]
        unit = arr['unit']
        parameters = [_format_param(v) for v in arr.get("parameters", [])]
        data: np.ndarray = cls.compression[compression_acc](data)
        if cls.ascii_code != dtype_acc:
            data = np.asarray(bytearray(data)).view(cls.dtypes[dtype_acc])
        else:
            data = bytearray(data).strip().split(b"\0")
            data = np.array(data, dtype=np.object_)
        return AuxiliaryArray(name, data, parameters, unit)

    @classmethod
    def _unpack(cls, spec: dict):
        if "auxiliary_arrays" in spec:
            auxiliary_arrays = spec.pop("auxiliary_arrays")
            if auxiliary_arrays is not None:
                for v in auxiliary_arrays:
                    v = _AuxiliaryArrayDecoder.decode(v)
                    spec[v.name] = v.values
                    spec[f"{v.name} unit"] = v.unit
                    if v.parameters:
                        spec[f"{v.name} parameters"] = v.parameters


@dataclass
class AuxiliaryArray:
    """
    An extra array that was not registered as globally as part of the data schema
    that has been decoded.

    Attributes
    ----------
    name : str
        The name of the array
    values : np.ndarray
        The decoded data associated with the array
    parameters : list[dict]
        The parameters, controlled or otherwise, not already covered by the decoded array attributes
    """

    name: str
    values: np.ndarray
    parameters: list[dict]
    unit: str | None = None


class _DataPointCountMixin:
    def _table(self) -> pd.DataFrame:
        raise NotImplementedError()

    def data_point_count(self, indices: int | Sequence[int] | Sequence[bool]):
        '''Get the number of profile data points for the requested indices'''
        series = self._table().get("number of data points")
        if series is None:
            return np.ones_like(indices) * np.nan
        try:
            return series[indices]
        except (KeyError, IndexError):
            return np.nan

    def peak_count(self, indices: int | Sequence[int] | Sequence[bool]):
        """Get the number of peaks for the requested indices"""
        series = self._table().get("number of peaks")
        if series is None:
            return np.ones_like(indices) * np.nan
        try:
            return series[indices]
        except (KeyError, IndexError):
            return np.nan


class _MzPeakDataIter(Iterator[tuple[int, _SpectrumArrays, DataKind]]):
    """
    An iterator over two :class:`~._DataBatchIter` that dispatches based upon the
    data in :class:`_DataPointCountMixin`
    """
    metadata: _DataPointCountMixin
    data_iter: _DataBatchIter | None
    peak_iter: _DataBatchIter | None
    size: int
    index: int
    prefer_peaks: bool = False

    def __init__(
        self,
        metadata: _DataPointCountMixin,
        data_iter: _DataBatchIter | None,
        peak_iter: _DataBatchIter | None,
        size: int,
        index: int = 0,
        prefer_peaks: bool = False
    ):
        self.metadata = metadata
        self.data_iter = data_iter
        self.peak_iter = peak_iter
        self.size = size
        self.index = index
        self.prefer_peaks = prefer_peaks

    def read_data(self, i: int):
        if (
            not pd.isna(self.metadata.data_point_count(i))
            and self.data_iter is not None
        ):
            idx = self.data_iter.index()
            if idx is not None:
                if idx < i:
                    self.data_iter.seek(i)
                if self.data_iter.at_index(i):
                    data = next(self.data_iter)
                    return data

    def read_peaks(self, i: int):
        if not pd.isna(self.metadata.peak_count(i)) and self.peak_iter is not None:
            idx = self.peak_iter.index()
            if idx is not None:
                if idx < i:
                    self.peak_iter.seek(i)
                if self.peak_iter.at_index(i):
                    data = next(self.peak_iter)
                    return data

    def empty_arrays(self) -> _SpectrumArrays | None:
        if self.prefer_peaks and self.peak_iter:
            return self.peak_iter.empty_arrays()
        elif self.data_iter:
            return self.data_iter.empty_arrays()
        elif self.peak_iter:
            return self.peak_iter.empty_arrays()

    def __next__(self):
        i = self.index
        self.index += 1
        if self.prefer_peaks:
            peaks = self.read_peaks(i)
            if peaks:
                return (*peaks, DataKind.Peaks)
            data = self.read_data(i)
            if data:
                return (*data, DataKind.DataArrays)
            return i, None, None
        else:
            data = self.read_data(i)
            if data:
                return (*data, DataKind.DataArrays)
            peaks = self.read_peaks(i)
            if peaks:
                return (*peaks, DataKind.Peaks)
            return i, None, None

    def __iter__(self):
        return self

    def __len__(self):
        return self.size

    def __repr__(self):
        return (f"{self.__class__.__name__}({self.index}/{self.size}, {self.data_iter}, "
                f"{self.peak_iter}, prefer_peaks={self.prefer_peaks})")


class _PrecursorReadMixin:
    """Provides :meth:`_read_precursors` and :meth:`_read_selected_ions` that are shared amongst metadata entities"""

    handle: pq.ParquetFile
    meta: pq.FileMetaData

    precursors: pd.DataFrame
    selected_ions: pd.DataFrame

    def _unpack_precursors(self, spec: dict, i: int):
        precursors_of = self.precursors.loc[[i]]
        precursors_of["activation"] = precursors_of["activation"].apply(
            lambda x: [_format_param(v) for v in x["parameters"]]
        )
        try:
            ions = self.selected_ions.loc[[i]]
            ions["parameters"] = ions["parameters"].apply(
                lambda x: [_format_param(v) for v in x]
            )
            if 'precursor_index' in precursors_of.columns:
                ions_per_precursor = {k: v.to_dict("records") for k, v in ions.groupby('precursor_index')}
                precursors_of['selected_ions'] = precursors_of["precursor_index"].map(ions_per_precursor)
            else:
                precursors_of['selected_ions'] = [ions.to_dict("records")]

        except KeyError:
            pass
        spec["precursors"] = precursors_of.to_dict("records")


@dataclass
class MzPeakNamespaceAggregation:
    entity_type: EntityType
    metadata: pq.ParquetFile | None = None
    scans: pq.ParquetFile | None = None
    precursors: pq.ParquetFile | None = None
    selected_ions: pq.ParquetFile | None = None
    products: pq.ParquetFile | None = None
    file_index: FileIndex | None = field(default=None, repr=False)

    @property
    def parquet_metadata(self) -> pq.FileMetaData | None:
        if self.metadata is None:
            return None
        return self.metadata.metadata

    def is_concatenated_storage(self):
        """
        Test if all the tables are all together in a single Parquet file.
        """
        has_main = self.metadata is not None
        not_has_support = not (self.scans is not None or
                               self.precursors is not None or
                               self.selected_ions is not None or
                               self.products is not None)
        return has_main and not_has_support

    def storage_reader(self):
        if self.is_concatenated_storage():
            raise NotImplementedError("Old concatenated storage layout not supported")
        else:
            return MultiFileStorage(self, self.entity_type)


class StorageStrategyBase:
    def _read_spectra(self) -> tuple[pd.DataFrame, pd.Series]:
        raise NotImplementedError()

    def _read_chromatograms(self) -> tuple[pd.DataFrame, pd.Series]:
        raise NotImplementedError()

    def _read_scans(self) -> pd.DataFrame:
        raise NotImplementedError()

    def _read_precursors(self) -> pd.DataFrame:
        raise NotImplementedError()

    def _read_selected_ions(self) -> pd.DataFrame:
        raise NotImplementedError()

    def _read_products(self) -> pd.DataFrame:
        raise NotImplementedError()


class MultiFileStorage(StorageStrategyBase):
    namespaces: MzPeakNamespaceAggregation
    entity_type: EntityType

    def __init__(self, namespaces: MzPeakNamespaceAggregation, entity_type: EntityType):
        self.namespaces = namespaces
        self.entity_type = entity_type

    def find_file_index_entry(self, entity_type: EntityType, data_kind: DataKind) -> FileEntry | None:
        return self.namespaces.file_index.find(entity_type, data_kind)

    def _read_spectra(self):
        if self.namespaces.metadata is None:
            df = pd.DataFrame(
                [],
                columns=[
                    "index",
                    "id",
                ],
            )
            id_index = pd.Series()
            return (df, id_index)

        bat = self.namespaces.metadata.read()
        index_entry = self.find_file_index_entry(self.entity_type, DataKind.Metadata)

        name_map = index_entry.renaming_map()
        bat = _NameCleaningNode.clean_table(bat, mapper=lambda x: name_map.get(x, x))

        spectra = _clean_frame(
            bat.to_pandas(types_mapper=pd.ArrowDtype).set_index("index"),
            clean_columns=False,
        )

        if (np.diff(spectra.index) == 1).all():
            spectra.index = pd.RangeIndex(
                spectra.index[0],
                spectra.index[-1] + 1,
                name="index",
            )
        if "id" in spectra.columns:
            id_index = spectra[["id"]].reset_index().set_index("id")["index"]
        else:
            id_index = pd.Series()
        return spectra, id_index

    def _read_scans(self):
        if self.namespaces.scans is None:
            return pd.DataFrame(
                [],
                columns=[
                    "source_index",
                ],
            )
        bat = self.namespaces.scans.read()
        if "spectrum_index" in bat.column_names:
            index_col = "spectrum_index"
        else:
            index_col = "source_index"

        index_entry = self.find_file_index_entry(self.entity_type, DataKind.Scans)
        name_map = index_entry.renaming_map()
        bat = _NameCleaningNode.clean_table(bat, mapper=lambda x: name_map.get(x, x))

        scans = _clean_frame(
            bat.to_pandas(types_mapper=pd.ArrowDtype).set_index(index_col),
            clean_columns=False,
        )
        if (np.diff(scans.index) == 1).all():
            scans.index = pd.RangeIndex(
                scans.index[0],
                scans.index[-1] + 1,
            )
            scans.index.name = "source_index"
        return scans

    def _read_precursors(self):
        if self.namespaces.precursors:
            bat = self.namespaces.precursors.read()
            if "spectrum_index" in bat.column_names:
                index_col = "spectrum_index"
            else:
                index_col = "source_index"

            index_entry = self.find_file_index_entry(self.entity_type, DataKind.Precursors)
            name_map = index_entry.renaming_map()
            bat = _NameCleaningNode.clean_table(bat, mapper=lambda x: name_map.get(x, x))

            precursors = _clean_frame(
                bat.to_pandas(types_mapper=pd.ArrowDtype).set_index(index_col),
                clean_columns=False,
            )

            return precursors
        else:
            return pd.DataFrame(
                [],
                columns=[
                    "source_index",
                    "precursor_index",
                ],
            )

    def _read_selected_ions(self):
        if self.namespaces.selected_ions:
            bat = self.namespaces.selected_ions.read()
            if "spectrum_index" in bat.column_names:
                index_col = "spectrum_index"
            else:
                index_col = "source_index"

            index_entry = self.find_file_index_entry(self.entity_type, DataKind.SelectedIons)
            name_map = index_entry.renaming_map()
            bat = _NameCleaningNode.clean_table(
                bat, mapper=lambda x: name_map.get(x, x)
            )

            selected_ions = _clean_frame(
                bat.to_pandas(types_mapper=pd.ArrowDtype).set_index(index_col),
                clean_columns=False,
            )
            return selected_ions
        else:
            return pd.DataFrame(
                [],
                columns=[
                    "source_index",
                    "precursor_index",
                ],
            )

    def _read_chromatograms(self):
        return self._read_spectra()


class MzPeakSpectrumMetadataReader(_PrecursorReadMixin, _DataPointCountMixin):
    """
    A reader for spectrum metadata in an mzPeak file.

    Attributes
    ----------
    handle : :class:`pyarrow.parquet.ParquetFile`
        The underlying Parquet file reader
    meta : :class:`pyarrow.parquet.FileMetaData`
        The metadata segment of the underlying Parquet file
    num_spectra : int
        The number of distinct spectra in the metadata table
    spectra : :class:`pandas.DataFrame`
        A data frame holding spectrum-level metadata like MS level, scan time, centroid status,
        and polarity.
    id_index : :class:`pandas.Series`
        A series mapping spectrum ID to index
    precursors : :class:`pandas.DataFrame`
        A data frame holding precursor-level metadata like precursor scan ID, isolation window,
        and activation parameters. See :attr:`MzPeakSpectrumMetadataReader.selected_ions` for ion-level information.
    selected_ions : :class:`pandas.Dataframe`
        A data frame holding selected ions connected to precursors and spectra including selected
        ion m/z, charge, intensity, and possibly ion mobility.
    scans : :class:`pandas.Dataframe`
        A data frame holding scan-level metadata like scan start time, injection time, filter strings
        and scan ranges.
    """
    namespace: MzPeakNamespaceAggregation

    id_index: pd.Series
    spectra: pd.DataFrame
    scans: pd.DataFrame
    precursors: pd.DataFrame
    selected_ions: pd.DataFrame

    def __init__(self, namespace: MzPeakNamespaceAggregation):
        self.namespace = namespace
        storage = self.namespace.storage_reader()
        self.spectra, self.id_index = storage._read_spectra()
        self.scans = storage._read_scans()
        self.precursors = storage._read_precursors()
        self.selected_ions = storage._read_selected_ions()

    @property
    def meta(self) -> pq.FileMetaData:
        return self.namespace.parquet_metadata

    def extract_tic(self):
        """
        Extract the implicit total ion chromatogram (TIC) from the spectrum metadata table.

        The TIC is read from the spectrum metadata table's "total ion current" column.

        Returns
        -------
        np.ndarray : time_array
            The time axis of the total ion chromatogram
        np.ndarray : intensity_array
            The intensity of the total ion chromatogram
        """
        return np.array(self.spectra["time"]), np.array(
            self.spectra["total ion current"]
        )

    def extract_bpc(self):
        """
        Extract the implicit base peak chromatogram (BPC) from the spectrum metadata table.

        The BPC is read from the spectrum metadata table's "base peak intensity" column.

        Returns
        -------
        np.ndarray : time_array
            The time axis of the base peak chromatogram
        np.ndarray : intensity_array
            The intensity of the base peak chromatogram
        """
        return np.array(self.spectra["time"]), np.array(
            self.spectra["base peak intensity"]
        )

    def __getitem__(self, i: int | str):
        if isinstance(i, str):
            i = self.id_index[i]
        spec = self.spectra.loc[i].to_dict()
        spec["parameters"] = [_format_param(v) for v in spec["parameters"]]

        spec["scans"] = self.scans.loc[i]
        if isinstance(spec["scans"], pd.DataFrame):
            spec["scans"] = spec["scans"].reset_index(drop=True).to_dict(orient='records')
        else:
            spec["scans"] = spec['scans'].to_dict()

        if isinstance(spec["scans"], dict):
            spec["scans"]["parameters"] = [
                _format_param(v) for v in spec["scans"]["parameters"]
            ]
            spec["scans"] = [spec["scans"]]
        else:
            for scan in spec["scans"]:
                scan["parameters"] = [_format_param(v) for v in scan["parameters"]]
        try:
            self._unpack_precursors(spec, i)
        except KeyError:
            pass
        spec["index"] = i
        _AuxiliaryArrayDecoder._unpack(spec)
        return spec

    def __len__(self):
        return self.spectra.index.size

    def __repr__(self):
        return f"{self.__class__.__name__}({self.namespace})"

    def _table(self) -> pd.DataFrame:
        return self.spectra

    def _get_mz_delta_model(self):
        if "median_delta" in self.spectra:
            return self.spectra["median_delta"].to_numpy()
        elif "mz_delta_model" in self.spectra:
            return self.spectra["mz_delta_model"].to_numpy()
        return None


class MzPeakChromatogramMetadataReader(_PrecursorReadMixin, _DataPointCountMixin):
    """
    A reader for chromatogram metadata in an mzPeak file.

    Attributes
    ----------
    handle : :class:`pyarrow.parquet.ParquetFile`
        The underlying Parquet file reader
    meta : :class:`pyarrow.parquet.FileMetaData`
        The metadata segment of the underlying Parquet file
    num_chromatograms : int
        The number of distinct chromatograms in the metadata table
    chromatograms : :class:`pandas.DataFrame`
        A data frame holding chromatogram-level metadata like MS level, scan time, centroid status,
        and polarity.
    id_index : :class:`pandas.Series`
        A series mapping chromatogram ID to index
    precursors : :class:`pandas.DataFrame`
        A data frame holding precursor-level metadata like precursor scan ID, isolation window,
        and activation parameters. See :attr:`MzPeakChromatogramMetadataReader.selected_ions` for ion-level information.
    selected_ions : :class:`pandas.Dataframe`
        A data frame holding selected ions connected to precursors and chromatograms including selected
        ion m/z, charge, intensity, and possibly ion mobility.
    """
    namespace: MzPeakNamespaceAggregation

    id_index: pd.Series
    chromatograms: pd.DataFrame
    precursors: pd.DataFrame
    selected_ions: pd.DataFrame

    def __init__(self, namespace: MzPeakNamespaceAggregation):
        self.namespace = namespace
        storage = self.namespace.storage_reader()
        self.chromatograms, self.id_index = storage._read_chromatograms()
        self.precursors = storage._read_precursors()
        self.selected_ions = storage._read_selected_ions()

    def _table(self) -> pd.DataFrame:
        return self.chromatograms

    def __getitem__(self, i: int | str):
        if isinstance(i, str):
            i = self.id_index[i]
        spec = self.chromatograms.loc[i].to_dict()
        spec["parameters"] = [_format_param(v) for v in spec["parameters"]]
        try:
            self._unpack_precursors(spec, i)
        except KeyError:
            pass
        spec["index"] = i
        _AuxiliaryArrayDecoder._unpack(spec)
        return spec


_SpectrumType = dict[str, Any]


class MzPeakFileIter(Iterator["_SpectrumType"]):
    data_iter: _SeekableIter
    metadata: "MzPeakSpectrumMetadataReader"
    index: int
    size: int

    @classmethod
    def from_archive_spectra(cls, reader: "MzPeakFile") -> "MzPeakFileIter":
        profile_iter = None
        peak_iter = None
        if reader.spectrum_data is not None:
            profile_iter = reader.spectrum_data._data_iterator(0)
        if reader.spectrum_peak_data is not None:
            peak_iter = reader.spectrum_peak_data._data_iterator(0)
        data_iter = _MzPeakDataIter(
            reader.spectrum_metadata,
            profile_iter,
            peak_iter,
            len(reader.spectrum_metadata),
            prefer_peaks=reader.prefer_peaks
        )
        return cls(data_iter, reader.spectrum_metadata)

    def __init__(
        self,
        data_iter: _MzPeakDataIter,
        metadata: "MzPeakSpectrumMetadataReader",
        index: int=0,
    ):
        self.data_iter = _SeekableIter(data_iter)
        self.metadata = metadata
        self.index = index
        self.size = len(metadata)

    def __next__(self) -> "_SpectrumType":
        i = self.index
        if i >= self.size:
            raise StopIteration()
        self.index += 1
        self.data_iter.seek(i)
        _j, data, mode = next(self.data_iter)
        meta = self.metadata[i]
        if data is None:
            data = self.data_iter.inner.empty_arrays()
        meta["data_kind"] = mode
        meta.update(data)
        return meta

    def seek(self, index: int) -> bool:
        self.index = index
        return self.data_iter.seek(index)

    def __len__(self):
        return self.size

    def __iter__(self):
        return self


class _EntityCollectionMixin(Sequence[_SpectrumType]):
    spectrum_metadata: MzPeakSpectrumMetadataReader | None = None
    spectrum_data: MzPeakArrayDataReader | None = None
    spectrum_peak_data: MzPeakArrayDataReader | None = None
    prefer_peaks: bool = False

    def read_spectrum(
        self, index: int | str | Iterable[int | str] | slice
    ) -> _SpectrumType | list[_SpectrumType]:
        """
        Read a spectrum by its ``index`` or ``id`` attribute.

        If a list is provided, each of those spectra will be
        retrieved. If a :class:`slice` is provided, the consecutive
        spectra will be returned.

        Parameters
        ----------
        index : :class:`int`, :class:`str`, :class:`Iterable`, or :class:`slice`
            The identifier or index (or plurality thereof) to retrieve.

        Returns
        -------
        :class:`dict` or :class:`list` of :class:`dict`
            The spectrum or spectra requested
        """
        if isinstance(index, (int, str)):
            spec = self.spectrum_metadata[index]
            index = spec["index"]
            dp = self.spectrum_metadata.data_point_count(index)
            pk = self.spectrum_metadata.peak_count(index)
            data = None
            mode = None
            if self.prefer_peaks:
                if not pd.isna(pk) and pk > 0 and self.spectrum_peak_data is not None:
                    data = self.spectrum_peak_data[index]
                    mode = DataKind.Peaks
                elif not pd.isna(dp) and dp > 0 and self.spectrum_data is not None:
                    data = self.spectrum_data[index]
                    mode = DataKind.DataArrays
            else:
                if not pd.isna(dp) and dp > 0 and self.spectrum_data is not None:
                    data = self.spectrum_data[index]
                    mode = DataKind.DataArrays
                elif not pd.isna(pk) and pk > 0 and self.spectrum_peak_data is not None:
                    data = self.spectrum_peak_data[index]
                    mode = DataKind.Peaks
            if not data:
                if self.prefer_peaks and self.spectrum_peak_data:
                    data = self.spectrum_peak_data._empty_array_map()
                    mode = DataKind.Peaks
                else:
                    data = self.spectrum_data._empty_array_map()
                    mode = DataKind.DataArrays
            if data:
                spec.update(data)
            spec["data_kind"] = mode

        elif isinstance(index, Iterable):
            if not index:
                return []
            spec = [self.read_spectrum(i) for i in index]
        elif isinstance(index, slice):
            start = index.start or 0
            end = index.stop or len(self)
            step = index.step or 1
            if step == 1:
                it = iter(self)
                it.seek(start)
                spec = []
                for s in it:
                    spec.append(s)
                    if s["index"] == (end - 1):
                        break
            else:
                spec = self.read_spectrum(range(start, end, step))
        return spec

    def __iter__(self) -> MzPeakFileIter:
        return MzPeakFileIter.from_archive_spectra(self)

    def __len__(self):
        return len(self.spectrum_metadata)

    def __getitem__(
        self, index: int | str | Iterable[int | str] | slice
    ) -> _SpectrumType | list[_SpectrumType]:
        """An alias for :meth:`read_spectrum`."""
        return self.read_spectrum(index)

    def spectra_signal_for_indices(
        self, index_range: slice | list[int]
    ) -> dict[str, np.ndarray]:
        return self.spectrum_data.read_data_for_range(index_range)

    @property
    def time(self) -> RTLocator:
        return RTLocator(self)


class MzPeakFile(_EntityCollectionMixin):
    """
    An mzPeak reader for mass spectra, chromatograms, and other
    data types.

    This may be initialized from a path to a packed zip archive or an unpacked directory.
    Files may be stored locally. If :mod:`universal_pathlib` (``upath``) is installed,
    any supported protocol path is also supported.

    This type is an :class:`Sequence` over mass spectra with support for point and slicing
    access. Chromatograms are accessed via :meth:`read_chromatogram`. Wavelength spectra are
    are exposed by the :attr:`wavelength_data`.

    Mass spectra may be stored in profile mode, centroid mode AKA peaks, or both. By default,
    this type will prefer to load the profile mode data and let the user load peaks
    :meth:`read_peaks_for`. Setting :attr:`prefer_peaks` to :const:`True` will preferentially
    load peaks when both modalities are available.

    Attributes
    ----------
    spectrum_data : :class:`~.MzPeakArrayDataReader`
        The facet of the data file for reading spectrum signal data from. This
        may be profile or centroid data, depending upon what was stored in the
        file.
    spectrum_metadata : :class:`~.MzPeakSpectrumMetadataReader`
        The facet of the data file for reading spectrum descriptive metadata,
        like scan time, MS level, precursor information, et cetera. Should not
        be necessary to interact with this attribute directly. Instead, see
        :attr:`spectra`, :attr:`precursors`, :attr:`scans`
        and :attr:`selected_ions`.
    spectrum_peak_data : :class:`~.MzPeakArrayDataReader` or :const:`None`
        The facet of the data file for reading explicitly stored spectrum centroid
        data from. This will only be present if the file was written with a separate
        centroid stream to store both centroids and profile data side-by-side, as
        in some instrument vendor formats.
    prefer_peaks : :class:`bool`
        Whether to preferentially load peak or profile data when both are available for
        the same spectrum.
    chromatogram_data : :class:`~.MzPeakArrayDataReader` or :const:`None`
        The facet of the data file for reading chromatogram signal data from. This
        will only be present if the writer specifically writes chromatogram data.
    chromatogram_metadata : :class:`~.MzPeakChromatogramMetadataReader`
        The facet of the data file for reading chromatogram descriptive metadata.
        Should not be necessary to interact with this attribute directly. Instead, see
        :attr:`chromatograms`
    file_index : :class:`~.FileIndex`
        A listing of the recorded files within the archive, mapping names to specific
        data content types.
    file_metadata: dict[str, Any]
        A mapping of the run-level metadata for the archive, covering things like instrument
        configurations, file content description, sample metadata, and the like.
    spectra : :class:`pandas.DataFrame`
        A data frame holding spectrum-level metadata like MS level, scan time, centroid status,
        and polarity.
    precursors : :class:`pandas.DataFrame`
        A data frame holding precursor-level metadata like precursor scan ID, isolation window,
        and activation parameters. See :attr:`selected_ions` for ion-level information.
    selected_ions : :class:`pandas.DataFrame`
        A data frame holding selected ions connected to precursors and spectra including selected
        ion m/z, charge, intensity, and possibly ion mobility.
    scans : :class:`pandas.DataFrame`
        A data frame holding scan-level metadata like scan start time, injection time, filter strings
        and scan ranges.
    chromatograms : :class:`pandas.DataFrame` or :const:`None`
        A data frame holding chromatogram-level metadata. This will only be present if
        :attr:`chromatogram_metadata` is present.
    wavelength_data : :class:`WavelengthFacet` or :const:`None`
        A facet for accessing wavelength spectra if it is available.
    """

    _archive: zipfile.ZipFile | Path | UPath
    """The actual storage backend that routes file opening and I/O operations"""
    _archive_storage: ArchiveStorage
    """The kind of storage being used that differentiates between local and remote and zip archive vs. unpacked directory"""
    _source: Any
    """The object provided to open the file"""

    _spectrum_namespace_aggregator: MzPeakNamespaceAggregation
    """The collection of files that compose spectrum metadata. :attr:`spectrum_metadata` is an in-memory view of it."""

    spectrum_metadata: MzPeakSpectrumMetadataReader | None = None
    spectrum_data: MzPeakArrayDataReader | None = None
    spectrum_peak_data: MzPeakArrayDataReader | None = None

    _chromatogram_namespace_aggregator: MzPeakNamespaceAggregation
    """The collection of files that compose chromatogram metadata. :attr:`chromatogram_metadata` is an in-memory view of it."""

    chromatogram_metadata: MzPeakChromatogramMetadataReader | None = None
    chromatogram_data: MzPeakArrayDataReader | None = None

    _wavelength_spectrum_namespace_aggregator: MzPeakNamespaceAggregation
    """The collection of files that compose wavelength spectrum metadata. :attr:`_wavelength_spectrum_metadata` is an in-memory view of it."""

    _wavelength_spectrum_metadata: MzPeakSpectrumMetadataReader | None = None
    _wavelength_spectrum_data: MzPeakArrayDataReader | None = None

    file_metadata: dict[str, Any]

    file_index: FileIndex

    @property
    def filename(self) -> str | None:
        """The name of the data file"""
        if isinstance(self._source, (Path, UPath)):
            return self._source.name
        elif isinstance(self._source, zipfile.ZipFile):
            return self._source.filename

    def _upath_opener(self, f: UPath) -> pq.ParquetFile:
        """Open a UPath-based file using the :class:`Path`-like API"""
        return pq.ParquetFile(pa.PythonFile(f.open("rb")))

    def _path_opener(self, f: Path) -> pq.ParquetFile:
        """Open a native file system file using :class:`pa.OSFile` with less Python overhead"""
        return pq.ParquetFile(pa.OSFile(str(f)))

    def _zip_opener(self, f: zipfile.ZipExtFile) -> pq.ParquetFile:
        """Open a :class:`zipfile.ZipExtFile` file-like object"""
        return pq.ParquetFile(pa.PythonFile(f))

    def _from_directory(self, path: Path):
        self._archive_storage = ArchiveStorage.Directory
        self._archive = path
        index_path = path / FileIndex.FILE_NAME
        visited = set()
        if has_upath and isinstance(path, UPath):
            opener = self._upath_opener
        else:
            opener = self._path_opener
        if index_path.exists():
            self.file_index = FileIndex.from_json(json.load(index_path.open()))
            for e in self.file_index:
                f = path / e.name
                if f in visited:
                    continue
                visited.add(f)
                self._receive_entry(e, f, opener=opener)
        else:
            raise FileNotFoundError(f"Failed to find {FileIndex.FILE_NAME} in unpacked mzPeak archive {path}")

    def _receive_entry(self, e: FileEntry, f, opener: Callable[[Any], pq.ParquetFile]):
        match e.entry_type():
            # receive mass spectra
            case (EntityType.Spectrum, DataKind.DataArrays):
                self.spectrum_data = MzPeakArrayDataReader(opener(f), namespace="spectrum")
            case (EntityType.Spectrum, DataKind.Metadata):
                self._spectrum_namespace_aggregator.metadata = opener(f)
            case (EntityType.Spectrum, DataKind.Scans):
                self._spectrum_namespace_aggregator.scans = opener(f)
            case (EntityType.Spectrum, DataKind.Precursors):
                self._spectrum_namespace_aggregator.precursors = opener(f)
            case (EntityType.Spectrum, DataKind.SelectedIons):
                self._spectrum_namespace_aggregator.selected_ions = opener(f)
            case (EntityType.Spectrum, DataKind.Peaks):
                self.spectrum_peak_data = MzPeakArrayDataReader(opener(f), namespace="spectrum")

            # receive chromatograms
            case (EntityType.Chromatogram, DataKind.DataArrays):
                self.chromatogram_data = MzPeakArrayDataReader(opener(f), namespace="chromatogram")
            case (EntityType.Chromatogram, DataKind.Metadata):
                self._chromatogram_namespace_aggregator.metadata = opener(f)
            case (EntityType.Chromatogram, DataKind.Precursors):
                self._chromatogram_namespace_aggregator.precursors = opener(f)
            case (EntityType.Chromatogram, DataKind.SelectedIons):
                self._chromatogram_namespace_aggregator.selected_ions = opener(f)
            case (EntityType.Chromatogram, DataKind.Products):
                self._chromatogram_namespace_aggregator.products = opener(f)

            # receive wavelength spectra
            case (EntityType.WavelengthSpectrum, DataKind.DataArrays):
                self._wavelength_spectrum_data = MzPeakArrayDataReader(opener(f), namespace="wavelength_spectrum",
                )
            case (EntityType.WavelengthSpectrum, DataKind.Metadata):
                self._wavelength_spectrum_namespace_aggregator.metadata = opener(f)
            case (EntityType.WavelengthSpectrum, DataKind.Scans):
                self._wavelength_spectrum_namespace_aggregator.scans = opener(f)

            # Something else
            case _:
                pass

    def _from_zip_archive(self, archive: zipfile.ZipFile):
        self._archive_storage = ArchiveStorage.Zip
        self._archive = archive
        visited = set()

        try:
            f = archive.getinfo(FileIndex.FILE_NAME)
        except KeyError as err:
            raise FileNotFoundError(
                f"Failed to find {FileIndex.FILE_NAME} in mzPeak ZIP archive {archive}"
            ) from err

        self.file_index = FileIndex.from_json(json.load(archive.open(f)))
        for e in self.file_index:
            if e.name in visited:
                continue
            visited.add(e.name)
            f = archive.open(e.name)
            self._receive_entry(e, f, self._zip_opener)

    def _from_path(self, path: Path):
        if path.is_dir():
            if path.is_file():
                try:
                    archive = zipfile.ZipFile(path.open('rb'))
                    self._from_zip_archive(archive)
                    return
                except (OSError, ValueError):
                    pass
            self._from_directory(path)
        else:
            archive = zipfile.ZipFile(path.open('rb'))
            self._from_zip_archive(archive)

    def open_stream(self, name: str) -> IO[bytes]:
        match self._archive_storage:
            case ArchiveStorage.Zip:
                return self._archive.open(name)
            case ArchiveStorage.Directory:
                return (self._archive / name).open(mode="rb")
            case _:
                raise TypeError(
                    f"Do not understand how to open a stream from {self._archive} of type {self._archive_storage}"
                )

    def list_files(self) -> list[str]:
        match self._archive_storage:
            case ArchiveStorage.Zip:
                return [f.filename for f in self._archive.filelist]
            case ArchiveStorage.Directory:
                return [f.name for f in self._archive.glob("*")]
            case _:
                raise TypeError(
                    f"Do not understand how to list files from {self._archive} of type {self._archive_storage}"
                )

    def read_peaks_for(self, index: int) -> _SpectrumArrays | None:
        '''
        Read the centroid mass spectrum peak list for ``index`` if one is available.

        Parameters
        ----------
        index : int
            The index to read peaks for.

        Returns
        -------
        dict[str, np.ndarray]
            A map of named peak dimensions as :class:`np.ndarray`
        '''
        if self.spectrum_peak_data is not None:
            return self.spectrum_peak_data[index]

    def _init_metadata(self):
        metadata = {}
        if self.spectrum_metadata:
            for k, v in self.spectrum_metadata.meta.metadata.items():
                k = k.decode("utf8")
                if k == "ARROW:schema":
                    continue
                try:
                    v = json.loads(v)
                except json.JSONDecodeError:
                    pass
                metadata[k] = v
        metadata.update(self.file_index.metadata)
        self.file_metadata = metadata

        if self.spectrum_data and self.spectrum_metadata:
            self.spectrum_data._delta_model_series = (
                self.spectrum_metadata._get_mz_delta_model()
            )

    def _unpack_namespaces(self):
        # Provide the file index
        self._spectrum_namespace_aggregator.file_index = self.file_index
        self._chromatogram_namespace_aggregator.file_index = self.file_index
        self._wavelength_spectrum_namespace_aggregator.file_index = self.file_index

        self.spectrum_metadata = MzPeakSpectrumMetadataReader(self._spectrum_namespace_aggregator)
        self.chromatogram_metadata = MzPeakChromatogramMetadataReader(self._chromatogram_namespace_aggregator)
        if self._wavelength_spectrum_namespace_aggregator.metadata is not None:
            self._wavelength_spectrum_metadata = MzPeakSpectrumMetadataReader(self._wavelength_spectrum_namespace_aggregator)
        else:
            self._wavelength_spectrum_metadata = None

    def __init__(self, path: str | Path | UPath | zipfile.ZipFile | IO[bytes]):
        self.file_index = FileIndex()
        self._spectrum_namespace_aggregator = MzPeakNamespaceAggregation(EntityType.Spectrum)
        self._chromatogram_namespace_aggregator = MzPeakNamespaceAggregation(EntityType.Chromatogram)
        self._wavelength_spectrum_namespace_aggregator = MzPeakNamespaceAggregation(EntityType.WavelengthSpectrum)

        if isinstance(path, zipfile.ZipFile):
            self._source = path
            self._from_zip_archive(path)
        elif isinstance(path, (str, Path, UPath)):
            if isinstance(path, str):
                if has_upath and "://" in path:
                    path = UPath(path)
                else:
                    if "://" in path:
                        logger.warning("%r resembles a URI but `universal_pathlib` is not installed", path)
                    path = Path(path)
            self._source = path
            self._from_path(path)
        else:
            self._source = path
            self._from_zip_archive(zipfile.ZipFile(path))

        self._unpack_namespaces()
        self._init_metadata()

    def read_chromatogram(
        self, index: int | str | Iterable[int | str] | slice
    ) -> _SpectrumType | list[_SpectrumType]:
        """
        Read a chromatogram by its ``index`` or ``id`` attribute.

        If a list is provided, each of those chromatograms will be
        retrieved. If a :class:`slice` is provided, the consecutive
        chromatograms will be returned.

        Parameters
        ----------
        index : :class:`int`, :class:`str`, :class:`Iterable`, or :class:`slice`
            The identifier or index (or plurality thereof) to retrieve.

        Returns
        -------
        :class:`dict` or :class:`list` of :class:`dict`
            The chromatogram or chromatograms requested
        """
        if isinstance(index, (int, str)):
            chrom = self.chromatogram_metadata[index]
            index = chrom["index"]
            data = self.chromatogram_data[index]
            chrom.update(data)
        elif isinstance(index, Iterable):
            if not index:
                return []
            chrom = [self.read_chromatogram(i) for i in index]
        elif isinstance(index, slice):
            start = index.start or 0
            end = index.stop or len(self)
            step = index.step or 1
            chrom = self.read_chromatogram(range(start, end, step))
        return chrom

    def observed_mz_range(self) -> tuple[float | None, float | None]:
        """
        Query the spectrum metadata to obtain the lowest and highest observed m/z as reported
        by columns mapped to `MS:1000528` and `MS:1000527`.

        This queries Parquet row group statistics.

        Returns
        -------
        min_mz : float | None
            The lowest observed m/z
        max_mz : float | None
            The highest observed m/z
        """
        ns = self.spectrum_metadata.namespace
        fi = ns.file_index.find(EntityType.Spectrum, DataKind.Metadata)
        lowest = fi.mapping(accession="MS:1000528")
        highest = fi.mapping(accession="MS:1000527")
        pq_meta: pq.FileMetaData = ns.metadata.metadata
        lowest_q = lowest.find_column(pq_meta.schema)
        highest_q = highest.find_column(pq_meta.schema)
        if not lowest_q or not highest_q:
            return (None, None)
        min_mz = None
        max_mz = None
        for i in range(pq_meta.num_row_groups):
            rg: pq.RowGroupMetaData = pq_meta.row_group(i)

            col_meta: pq.ColumnChunkMetaData = rg.column(lowest_q[0])
            stats = col_meta.statistics
            if stats:
                min_mz = stats.min if min_mz is None else min(min_mz, stats.min)

            col_meta: pq.ColumnChunkMetaData = rg.column(highest_q[0])
            stats = col_meta.statistics
            if stats:
                max_mz = stats.max if max_mz is None else max(max_mz, stats.max)
        return (min_mz, max_mz)


    def __repr__(self):
        return f"{self.__class__.__name__}({self.filename!r}, prefer_peaks={self.prefer_peaks})"

    def extract_tic(self) -> tuple[np.ndarray, np.ndarray]:
        """
        Extract the implicit total ion chromatogram (TIC) from the spectrum metadata table.

        The TIC is read from the spectrum metadata table's "total ion current" column.

        Returns
        -------
        np.ndarray : time_array
            The time axis of the total ion chromatogram
        np.ndarray : intensity_array
            The intensity of the total ion chromatogram
        """
        return self.spectrum_metadata.extract_tic()

    def extract_bpc(self) -> tuple[np.ndarray, np.ndarray]:
        """
        Extract the implicit base peak chromatogram (BPC) from the spectrum metadata table.

        The BPC is read from the spectrum metadata table's "base peak intensity" column.

        Returns
        -------
        np.ndarray : time_array
            The time axis of the base peak chromatogram
        np.ndarray : intensity_array
            The intensity of the base peak chromatogram
        """
        return self.spectrum_metadata.extract_bpc()

    @property
    def has_secondary_peaks_data(self) -> bool:
        """Detect if a separate table of centroid peaks has been stored alongside profile spectra."""
        return self.spectrum_peak_data is not None

    @property
    def spectra(self) -> pd.DataFrame:
        return self.spectrum_metadata.spectra

    @property
    def precursors(self) -> pd.DataFrame:
        return self.spectrum_metadata.precursors

    @property
    def selected_ions(self) -> pd.DataFrame:
        return self.spectrum_metadata.selected_ions

    @property
    def scans(self) -> pd.DataFrame:
        return self.spectrum_metadata.scans

    @property
    def chromatograms(self) -> pd.DataFrame | None:
        if self.chromatogram_metadata is not None:
            return self.chromatogram_metadata.chromatograms

    def to_sql(self, **kwargs):
        import datafusion
        ctx = datafusion.SessionContext(**kwargs)
        ctx.from_arrow(pa.table(self.spectra.reset_index()), "spectra")
        ctx.from_arrow(pa.table(self.scans.reset_index()), "scans")
        ctx.from_arrow(pa.table(self.precursors.reset_index()), "precursors")
        ctx.from_arrow(pa.table(self.selected_ions.reset_index()), "selected_ions")
        ctx.from_arrow(pa.table(self.chromatograms.reset_index()), "chromatograms")
        return ctx

    @property
    def wavelength_data(self) -> Optional["WavelengthFacet"]:
        if self._wavelength_spectrum_metadata is None:
            return None
        return WavelengthFacet(
            self._wavelength_spectrum_metadata,
            self._wavelength_spectrum_data
        )


class WavelengthFacet(_EntityCollectionMixin):
    spectrum_metadata: MzPeakSpectrumMetadataReader | None = None
    spectrum_data: MzPeakArrayDataReader | None = None
    spectrum_peak_data: MzPeakArrayDataReader | None = None

    def __init__(self, spectrum_metadata: MzPeakSpectrumMetadataReader, spectrum_data: MzPeakArrayDataReader):
        self.spectrum_data = spectrum_data
        self.spectrum_metadata = spectrum_metadata

    @property
    def spectra(self) -> pd.DataFrame:
        return self.spectrum_metadata.spectra

    @property
    def scans(self) -> pd.DataFrame:
        return self.spectrum_metadata.scans

