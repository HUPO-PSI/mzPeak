import re
import logging

from dataclasses import dataclass, field
from numbers import Number
from typing import Any, Callable, Generic, Mapping, TypeVar, Iterator

import numpy as np
import pandas as pd
import pyarrow as pa

from .file_index import MetadataColumn

logger = logging.getLogger(__name__)
logger.addHandler(logging.NullHandler())

TRACE = logging.DEBUG - 5
logging.addLevelName(TRACE, "TRACE")

Q = TypeVar("Q", bound=Number)


class Span(Generic[Q]):
    start: Q
    end: Q

    def __init__(self, start, end):
        self.start = start
        self.end = end

    def __contains__(self, val: Q) -> bool:
        return self.start <= val and val <= self.end

    def overlaps(self, other: "Span[Q]") -> bool:
        return self.end >= other.start and other.end >= self.start

    def size(self):
        return self.end - self.start

    def __repr__(self) -> str:
        return f"{self.__class__.__name__}({self.start}, {self.end})"


DTYPES = {
    "MS:1000519": np.int32,
    "MS:1000521": np.float32,
    "MS:1000522": np.int64,
    "MS:1000523": np.float64,
}


def _slice_to_range(slice_val: slice, n: int) -> range:
    start = slice_val.start or 0
    end = slice_val.stop or n
    return range(start, end)


NOT_ALLOWED_IN_COLNAME_PATTERN = re.compile("[^a-zA-Z0-9_\\\\-]+")

PERMITTED_CV_NAMES = ('MS', 'UO', )


def inflect_cv_name(accession: str, name: str) -> str:
    parts = [accession.replace(":", "_")]
    parts.append(NOT_ALLOWED_IN_COLNAME_PATTERN.sub("_", name))
    return "_".join(parts)


@dataclass(frozen=True)
class ColumnName:
    accession: str | None
    name: str
    is_unit: bool | str = field(default=False)

    def has_unit_curie(self) -> bool:
        return isinstance(self.is_unit, str)

    def is_unit_column(self) -> bool:
        return self.is_unit and isinstance(self.is_unit, bool)

    def __iter__(self):
        yield self.accession
        yield self.name
        yield self.is_unit


def parse_inflected_cv_name(name: str) -> ColumnName:
    tokens = iter(name.split("_"))
    try:
        prefix = next(tokens)
        accession = next(tokens)
        rest = '_'.join(tokens)
    except StopIteration:
        return ColumnName(None, name, False)

    if not rest or prefix not in PERMITTED_CV_NAMES:
        return ColumnName(None, name, False)

    curie = f"{prefix}:{accession}"

    if rest.endswith("_unit"):
        return ColumnName(curie, rest, True)

    if "_unit_" in rest:
        try:
            (rest, unit) = rest.rsplit("_unit_", 1)
            (unit_cv, unit_accession) = unit.split("_", 1)
            unit_curie = f"{unit_cv}:{unit_accession}"
            return ColumnName(curie, rest, unit_curie)
        except ValueError:
            pass

    return ColumnName(curie, rest, False)


class MappingProxy(object):
    """An object that proxies :meth:`__getitem__` to another object which is loaded lazily through a callable :attr:`loader`."""

    def __init__(self, loader):
        assert callable(loader)
        self.loader = loader
        self.mapping = None

    @property
    def metadata(self):
        """The metadata forwarded from the wrapped object."""
        self._ensure_mapping()
        return self.mapping.metadata

    def _ensure_mapping(self):
        if self.mapping is None:
            self.mapping = self.loader()

    def __getitem__(self, key):
        self._ensure_mapping()
        return self.mapping[key]

    def get(self, key, default=None):
        self._ensure_mapping()
        if self.mapping is None:
            raise ImportError(
                "Failed to load controlled vocabulary. "
                "Please ensure 'psims' is installed: pip install psims"
            )
        return self.mapping.get(key, default)


def _lazy_load_psims():
    try:
        from psims.controlled_vocabulary.controlled_vocabulary import load_psims
        logger.debug("Loading PSI-MS controlled vocabulary")
        cv = load_psims()
    except Exception:  # pragma: no cover
        cv = None
    return cv


def _lazy_load_uo():
    try:
        from psims.controlled_vocabulary.controlled_vocabulary import load_uo

        logger.debug("Loading UO controlled vocabulary")
        cv = load_uo()
    except Exception:  # pragma: no cover
        cv = None
    return cv


CV_PSIMS = MappingProxy(_lazy_load_psims)
CV_UO = MappingProxy(_lazy_load_uo)


class OntologyMapper:
    cv_psims: Mapping[str, Any]
    cv_uo: Mapping[str, Any]
    overrides: dict[str, str]

    def __init__(self, cv_psims=CV_PSIMS, cv_uo=CV_UO, overrides: dict[str, str] | None = None):
        self.cv_psims = cv_psims
        self.cv_uo = cv_uo
        self.overrides = overrides or {}

    def with_overrides(self, overrides: dict[str, str] | None = None):
        return self.__class__(
            self.cv_psims,
            self.cv_uo,
            overrides
        )

    def with_overrides_from(self, metacolumns: list[MetadataColumn]):
        return self.with_overrides({
            c.path[-1]: c.name for c in metacolumns
        })

    def __getitem__(self, value: str):
        colname = parse_inflected_cv_name(value)
        accession, name, _unit = colname
        suffix = ' unit' if colname.is_unit_column() else ''
        if accession is None:
            alt_name = name.replace("_", " ").replace("mz", 'm/z')
            alt_term = self.cv_psims.get(alt_name)
            if alt_term and alt_name not in ('id', 'index', 'name', 'activation', 'precursor'):
                logger.log(TRACE, 'Mapped %r to %s|%s', name, alt_term['id'], alt_term['name'])
                return alt_term['name']
            return self.overrides.get(name, name) + suffix
        cv_id = accession.split(":")[0]
        if cv_id == "MS":
            term = self.cv_psims[accession]
            logger.log(TRACE, "Mapped %r to %s|%s", name, term["id"], term["name"])
            return term["name"] + suffix
        elif cv_id == "UO":
            term = self.cv_uo[accession]
            logger.log(TRACE, "Mapped %r to %s|%s", name, term["id"], term["name"])
            return term["name"] + suffix
        else:
            logger.warning("Unknown prefix %r from %r", cv_id, value)
            return self.overrides.get(name, name) + suffix

    def __call__(self, value: str):
        return self[value]

    def clean_column_names(self, df: pd.DataFrame):
        df.columns = df.columns.map(self)
        return df

    def clean_schema(self, table: pa.Table) -> pa.Table:
        blocks = []
        fields = []
        for f, block in zip(table.schema, table):
            chunks = []
            clean_f = None
            for chunk in block.chunks:
                node = _NameCleaningNode.from_array(f, chunk, self)
                clean_f, clean_chunk = node.clean()
                chunks.append(clean_chunk)
            fields.append(clean_f)
            blocks.append(chunks)

        chunks = []
        for block in zip(*blocks):
            chunks.append(pa.StructArray.from_arrays(block, fields=fields))
        return pa.Table.from_struct_array(
            pa.chunked_array(chunks)
        )


@dataclass
class _NameCleaningNode:
    """
    A helper type for doing recursive Arrow schema column renaming.

    Attributes
    ----------
    field : pa.Field or None
        The field object with the name and type for this column.
    array : pa.Array
        The actual data stored in this column. This may be a pa.StructArray which will itself
        have multiple arrays under it.
    mapper : Callable[[str], str]
        The renaming mapping table to use to update names
    children : list of _NameCleaningNode
        The sub-arrays of this array, nested columns used to handle the recursive case
    """
    field: pa.Field
    array: pa.Array
    mapper: Callable[[str], str]
    children: list["_NameCleaningNode"] = field(default_factory=list)

    def __post_init__(self):
        if self.field is not None:
            self.field = self.field.with_name(self.mapper(self.field.name))
            if self.children:
                if self.is_struct():
                    new_fields = [f.field for f in self.children]
                    self.field = self.field.with_type(pa.struct(new_fields))
            # elif self.is_list():
            #     self.field = self.field.with_type(pa.list_(self.children[0].field))
            #     self.array.type.value_field = self.field.value_field
            # elif self.is_large_list():
            #     self.field = self.field.with_type(pa.large_list(self.children[0].field))
            #     self.array.type.value_field = self.field.value_field

    def is_struct(self) -> bool:
        if not self.field:
            return False
        return isinstance(self.field.type, pa.StructType)

    def is_list(self) -> bool:
        if not self.field:
            return False
        return isinstance(self.field.type, pa.ListType)

    def is_large_list(self) -> bool:
        if not self.field:
            return False
        return isinstance(self.field.type, pa.LargeListType)

    @classmethod
    def from_array(cls, field: pa.Field, array: pa.Array, mapper: OntologyMapper):
        '''The main entry point'''
        if isinstance(array.type, pa.StructType):
            return cls.from_struct_array(field, array, mapper)
        elif isinstance(array.type, (pa.ListType, pa.LargeListType)):
            return cls.from_list_array(field, array, mapper)
        return cls(field, array, mapper)

    @classmethod
    def from_struct_array(
        cls, field: pa.Field, arrays: pa.StructArray, mapper: OntologyMapper
    ):
        nodes = []
        for f, a in zip(arrays.type.fields, arrays.flatten()):
            nodes.append(cls.from_array(f, a, mapper))
        return cls(field, arrays, mapper, nodes)

    @classmethod
    def from_list_array(
        cls, field: pa.Field, arrays: pa.ListArray, mapper: OntologyMapper
    ):
        nodes = []
        nodes.append(
            cls.from_array(field.type.value_field, arrays.values, mapper)
        )
        return cls(field, arrays, mapper, nodes)

    def clean(self):
        if self.is_struct() or self.field is None:
            fields = []
            arrays = []
            for node in self.children:
                f, a = node.clean()
                fields.append(f)
                arrays.append(a)
            return (self.field, pa.StructArray.from_arrays(arrays, fields=fields))
        # elif self.is_list():
        #     f, a = self.children[0].clean()
        #     return self.field, pa.ListArray.from_arrays(self.array.offsets, a)
        # elif self.is_large_list():
        #     f, a = self.children[0].clean()
        #     return self.field, pa.ListArray.from_arrays(self.array.offsets, a)
        else:
            return (self.field, self.array)

    @classmethod
    def clean_table(cls, table: pa.Table, mapper: Callable[[str], str]):
        blocks = []
        fields = []
        for f, block in zip(table.schema, table):
            chunks = []
            clean_f = None
            for chunk in block.chunks:
                node = cls.from_array(f, chunk, mapper)
                clean_f, clean_chunk = node.clean()
                chunks.append(clean_chunk)
            fields.append(clean_f)
            blocks.append(chunks)

        chunks = []
        for block in zip(*blocks):
            chunks.append(pa.StructArray.from_arrays(block, fields=fields))
        return pa.Table.from_struct_array(
            pa.chunked_array(chunks)
        )


T = TypeVar('T')


class _PeekableIter(Generic[T], Iterator[tuple[int, T]]):
    _peek: tuple[int, T] | None
    inner: Iterator[tuple[int, T]]

    def __init__(self, inner: Iterator[tuple[int, T]], peek: bool=True):
        self.inner = inner
        self._peek = None
        if peek:
            self.peek()

    def peek(self) -> tuple[int, T] | None:
        if self._peek is None:
            try:
                self._peek = next(self.inner)
            except StopIteration:
                return None
        return self._peek

    def __next__(self) -> tuple[int, T]:
        if self._peek is not None:
            val = self._peek
            try:
                self._peek = next(self.inner)
            except StopIteration:
                self._peek = None
            return val
        return next(self.inner)

    def __repr__(self):
        return f"{self.__class__.__name__}({self.inner}, {self._peek[0] if self._peek else '<done or new>'})"


class _SeekableMixin(Generic[T]):
    def seek(self, index: int) -> bool:
        n = self.peek()
        if n is None:
            raise StopIteration()
        if n[0] > index:
            raise ValueError("Cannot rewind iterator")
        if index == n[0]:
            return True
        else:
            next(self)
            while True:
                n = self.peek()
                if not n:
                    raise StopIteration()
                if n[0] == index:
                    return True
                if n[0] > index:
                    return False
                next(self)

    def index(self) -> int | None:
        peeked = self.peek()
        if peeked:
            return peeked[0]

    def at_or_before_index(self, index: int) -> bool:
        at = self.index()
        if at is not None:
            return at <= index
        return False

    def at_index(self, index: int) -> bool:
        return self.index() == index


class _SeekableIter(_PeekableIter[T], _SeekableMixin[T]):
    def __repr__(self):
        return f"{self.__class__.__name__}({self.inner}, {self._peek[0] if self._peek else '<done>'})"


__all__ = [
    "Span",
    "inflect_cv_name",
    "_slice_to_range",
    "parse_inflected_cv_name",
    "OntologyMapper",
    "_SeekableIter",
    "_PeekableIter",
]