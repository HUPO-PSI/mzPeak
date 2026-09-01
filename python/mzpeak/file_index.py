from collections.abc import MutableSequence
from dataclasses import asdict, dataclass, field
from enum import StrEnum
from typing import TYPE_CHECKING, ClassVar

if TYPE_CHECKING:
    import pyarrow.parquet


OTHER = "other"

SPECTRUM = "spectrum"
CHROMATOGRAM = "chromatogram"
WAVELENGTH_SPECTRUM = "wavelength_spectrum"

DATA_ARRAYS = "data_arrays"
METADATA = "metadata"
PEAKS = "peaks"
SCANS = "scans"
PRECURSORS = "precursors"
SELECTED_IONS = "selected_ions"
PRODUCTS = "products"
PROPRIETARY = "proprietary"


class EntityTypeTag(StrEnum):
    Spectrum = SPECTRUM
    Chromatogram = CHROMATOGRAM
    WavelengthSpectrum = WAVELENGTH_SPECTRUM
    Other = OTHER

    @classmethod
    def get(cls, value: str):
        try:
            return cls(value)
        except ValueError:
            return cls.Other

    def __call__(self, *args, **kwargs):
        return EntityType.get(self)


@dataclass(frozen=True)
class EntityType:
    tag: EntityTypeTag
    label: str | None = None

    Spectrum: ClassVar[EntityTypeTag] = EntityTypeTag.Spectrum
    Chromatogram: ClassVar[EntityTypeTag] = EntityTypeTag.Chromatogram
    WavelengthSpectrum: ClassVar[EntityTypeTag] = EntityTypeTag.WavelengthSpectrum
    Other: ClassVar[EntityTypeTag] = EntityTypeTag.Other

    def __eq__(self, other: 'EntityType') -> bool:
        if other is None:
            return False
        elif not isinstance(other, EntityType):
            return self.tag == other or self.label == other
        elif self.tag == other.tag:
            if self.tag == EntityTypeTag.Other:
                return self.label == other.label
            else:
                return True
        return False

    def __ne__(self, other) -> bool:
        return not self == other

    @classmethod
    def get(cls, value: str):
        value = value.lower()
        tag = EntityTypeTag.get(value)
        if tag == EntityTypeTag.Other:
            return cls(tag, value)
        else:
            return cls(tag, None)

    def __str__(self):
        if self.label is None:
            return str(self.tag)
        else:
            return self.label


class DataKindTag(StrEnum):
    DataArrays = DATA_ARRAYS
    Peaks = PEAKS
    Metadata = METADATA
    Scans = SCANS
    Precursors = PRECURSORS
    SelectedIons = SELECTED_IONS
    Products = PRODUCTS
    Other = OTHER
    Proprietary = PROPRIETARY

    @classmethod
    def get(cls, value: str):
        try:
            return cls(value.replace(" ", "_"))
        except ValueError:
            return cls.Other

    def __call__(self, *args, **kwargs):
        return DataKind.get(self)

@dataclass(frozen=True)
class DataKind:
    tag: DataKindTag
    label: str | None = None

    DataArrays: ClassVar[DataKindTag] = DataKindTag.DataArrays
    Peaks: ClassVar[DataKindTag] = DataKindTag.Peaks
    Metadata: ClassVar[DataKindTag] = DataKindTag.Metadata
    Scans: ClassVar[DataKindTag] = DataKindTag.Scans
    Precursors: ClassVar[DataKindTag] = DataKindTag.Precursors
    SelectedIons: ClassVar[DataKindTag] = DataKindTag.SelectedIons
    Products: ClassVar[DataKindTag] = DataKindTag.Products
    Other: ClassVar[DataKindTag] = DataKindTag.Other
    Proprietary: ClassVar[DataKindTag] = DataKindTag.Proprietary

    def __eq__(self, other: "DataKind") -> bool:
        if other is None:
            return False
        elif not isinstance(other, DataKind):
            return self.tag == other or self.label == other
        elif self.tag == other.tag:
            if self.tag == DataKindTag.Other:
                return self.label == other.label
            else:
                return True
        return False

    def __ne__(self, other) -> bool:
        return not self == other

    @classmethod
    def get(cls, value: str):
        value = value.lower()
        tag = DataKindTag.get(value)
        if tag == DataKindTag.Other:
            return cls(tag, value)
        else:
            return cls(tag, None)

    def __str__(self):
        if self.label is None:
            return str(self.tag)
        else:
            return self.label

@dataclass
class MetadataColumn:
    name: str
    path: list[str]
    index: int | None = None
    accession: str | None = None
    unit: str | None = None
    term_marker: bool | None = None

    def __post_init__(self):
        if isinstance(self.path, str):
            self.path = self.path.split(".")

    def to_json(self):
        state = asdict(self)
        state['path'] = '.'.join(state['path'])
        state.pop("index", None)
        if self.unit is None:
            del state['unit']
        if not self.term_marker:
            del state['term_marker']
        if self.index is None:
            del state['index']
        if self.accession is None:
            del state['accession']
        return state

    def find_column(
        self, schema: "pyarrow.parquet.ParquetSchema"
    ) -> tuple[int, "pyarrow.parquet.ColumnChunkMetaData"] | None:
        col: pyarrow.parquet.ColumnChunkMetaData
        self_path = '.'.join(self.path)
        for (i, col) in enumerate(schema):
            if col.path == self_path:
                return (i, col)
        self_path_prefix = '.'.join(self.path[:-1])
        for (i, col) in enumerate(schema):
            if col.path.startswith(self_path_prefix) and col.path.split(".")[-1] == self.path[-1]:
                return (i, col)

@dataclass
class FileEntry:
    name: str
    entity_type: EntityType
    data_kind: DataKind
    column_mapping: list[MetadataColumn]
    parameters: list[dict]

    def as_data_kind(self) -> DataKind:
        return self.data_kind

    def as_entity_type(self) -> EntityType:
        return self.entity_type

    def to_json(self) -> dict:
        return {
            "name": self.name,
            "entity_type": str(self.entity_type),
            "data_kind": str(self.data_kind),
            "column_mapping": [c.to_json() for c in self.column_mapping],
            "parameters": self.parameters,
        }

    def mapping(self, name: str | None = None, accession: str | None = None) -> MetadataColumn | None:
        if name is None and accession is None:
            raise ValueError("one of `name` and `accession` must be provided")
        if name is not None:
            for col in self.column_mapping:
                if col.name == name:
                    return col
        if accession is not None:
            for col in self.column_mapping:
                if col.accession == accession:
                    return col

    def rename_columns(self, columns: list[str]) -> list[str]:
        name_map = {c.path[-1]: c.name for c in self.column_mapping}
        return [name_map.get(c, c) for c in columns]

    def renaming_map(self) -> dict[str, str]:
        return {c.path[-1]: c.name for c in self.column_mapping}

    @classmethod
    def from_json(cls, data: dict) -> 'FileEntry':
        return cls(
            data["name"],
            EntityType.get(data["entity_type"]),
            DataKind.get(data["data_kind"]),
            [MetadataColumn(**c) for c in data.get("column_mapping", [])],
            data.get("parameters", []),
        )

    def entry_type(self) -> tuple[EntityType, DataKind]:
        return (self.as_entity_type(), self.as_data_kind())


@dataclass
class FileIndex(MutableSequence[FileEntry]):
    FILE_NAME: ClassVar[str] = "mzpeak_index.json"

    files: list[FileEntry] = field(default_factory=list)
    metadata: dict[str, int | float | list | dict] = field(default_factory=dict)

    def __len__(self):
        return len(self.files)

    def __getitem__(self, i: int):
        return self.files[i]

    def __setitem__(self, i: int, value: FileEntry):
        self.files[i] = value

    def __delitem__(self, i: int):
        del self.files[i]

    def __iter__(self):
        return iter(self.files)

    def append(self, value: FileEntry):
        self.files.append(value)

    def remove(self, value: FileEntry):
        self.files.remove(value)

    def insert(self, i: int, value: FileEntry):
        self.files.insert(i, value)

    def find(self, entity_type: EntityTypeTag | EntityType, data_kind: DataKindTag | DataKind) -> FileEntry | None:
        """
        Find a :class:`FileEntry` by tags.

        Parameters
        ----------
        entity_type :
            The entity type to constrain the search to.
        data_kind :
            The kind of data to constrain the search to.

        Returns
        -------
        :class:`FileEntry` or :const:`None`
        """
        for f in self:
            if f.data_kind == data_kind and f.entity_type == entity_type:
                return f
        return None

    def to_json(self) -> dict:
        return {
            "files": [v.to_json() for v in self.files],
            "metadata": self.metadata
        }

    @classmethod
    def from_json(cls, data: dict) -> 'FileIndex':
        files = [FileEntry.from_json(f) for f in data['files']]
        return cls(files, data.get('metadata', {}))


__all__ = ["DataKind", "EntityType", "FileEntry", "FileIndex"]