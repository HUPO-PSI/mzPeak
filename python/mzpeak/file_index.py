from typing import ClassVar
from dataclasses import dataclass, field
from enum import StrEnum
from collections.abc import MutableSequence


OTHER = "other"

SPECTRUM = "spectrum"
CHROMATOGRAM = "chromatogram"
WAVELENGTH_SPECTRUM = "wavelength spectrum"

DATA_ARRAYS = "data arrays"
METADATA = "metadata"
PEAKS = "peaks"
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
    Other = OTHER
    Proprietary = PROPRIETARY

    @classmethod
    def get(cls, value: str):
        try:
            return cls(value.replace("_", " "))
        except ValueError:
            return cls.Other


@dataclass(frozen=True)
class DataKind:
    tag: DataKindTag
    label: str | None = None

    DataArrays: ClassVar[DataKindTag] = DataKindTag.DataArrays
    Peaks: ClassVar[DataKindTag] = DataKindTag.Peaks
    Metadata: ClassVar[DataKindTag] = DataKindTag.Metadata
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
class FileEntry:
    name: str
    entity_type: EntityType
    data_kind: DataKind


    def as_data_kind(self) -> DataKind:
        return self.data_kind

    def as_entity_type(self) -> EntityType:
        return self.entity_type

    def to_json(self) -> dict:
        return {
            "name": self.name,
            "entity_type": str(self.entity_type),
            "data_kind": str(self.data_kind),
        }

    @classmethod
    def from_json(cls, data: dict) -> 'FileEntry':
        return cls(data["name"], EntityType.get(data["entity_type"]), DataKind.get(data["data_kind"]))

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

    def to_json(self) -> dict:
        return {
            "files": [v.to_json() for v in self.files],
            "metadata": self.metadata
        }

    @classmethod
    def from_json(cls, data: dict) -> 'FileIndex':
        files = [FileEntry.from_json(f) for f in data['files']]
        return cls(files, data['metadata'])


__all__ = ["FileIndex", "FileEntry", "EntityType", "DataKind"]