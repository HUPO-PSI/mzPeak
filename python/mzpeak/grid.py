from dataclasses import dataclass
from typing import Any, ClassVar, Protocol, overload

import numpy as np
from numpy import typing as npt


@dataclass
class GridLike(Protocol):
    name: ClassVar[str]
    accession: ClassVar[str]

    @overload
    def from_index(self, index: int) -> float: ...

    @overload
    def from_index(self, index: npt.NDArray[np.uint32]) -> npt.NDArray[np.float64]: ...

    @overload
    def to_index(self, index: float) -> int: ...

    @overload
    def to_index(self, index: npt.NDArray[np.float64]) -> npt.NDArray[np.uint32]: ...

    def parameters(self) -> npt.NDArray[np.float64]: ...

    def __call__(self, value: int | npt.NDArray[np.uint32]):
        return self.from_index(value)

    def to_param(self) -> dict[str, Any]:
        return {
            "name": self.name,
            "accession": self.accession,
            "value": self.parameters().tolist()
        }


@dataclass
class LinearGrid(GridLike):
    intercept: float
    slope: float

    name: ClassVar[str] = "linear grid interpolation"
    accession: ClassVar[str] = "MS:1003824"

    def parameters(self):
        return np.array([self.intercept, self.slope])

    def from_index(
        self, index: int | npt.NDArray[np.uint32]
    ) -> float | npt.NDArray[np.float64]:
        value = self.intercept + index * self.slope
        if isinstance(value, np.ndarray):
            return value.astype(np.float64)
        return value

    def to_index(
        self, value: float | npt.NDArray[np.float64]
    ) -> int | npt.NDArray[np.uint32]:
        value = (value - self.intercept) / self.slope
        if isinstance(value, np.ndarray):
            return value.astype(np.uint32)
        return int(value)


@dataclass
class SquareRootLinearGrid(GridLike):
    intercept: float
    slope: float

    name: ClassVar[str] = "square root grid interpolation"
    accession: ClassVar[str] = "MS:1003825"

    def parameters(self):
        return np.array([self.intercept, self.slope])

    def from_index(
        self, index: int | npt.NDArray[np.uint32]
    ) -> float | npt.NDArray[np.float64]:
        value = (self.intercept + index * self.slope) ** 2
        if isinstance(value, np.ndarray):
            return value.astype(np.float64)
        return value

    def to_index(
        self, value: float | npt.NDArray[np.float64]
    ) -> int | npt.NDArray[np.uint32]:
        value = (np.sqrt(value) - self.intercept) / self.slope
        if isinstance(value, np.ndarray):
            return (value + 0.5).astype(np.uint32)
        return int(value + 0.5)


@dataclass
class TimsLinearGrid2(GridLike):
    accession: ClassVar[str] = "MS:9999001"

    c6: float
    c7: float
    intercept: float
    slope: float

    def from_index(self, value: int | npt.NDArray[np.uint32]):
        return 1.0 / (self.c6 + self.c7 / (self.intercept + self.slope * value))

    def to_index(self, index: float | npt.NDArray[np.float64]):
        d = (1.0 / index) - self.c6
        return ((self.c7 / d) - self.intercept) / self.slope

    def parameters(self):
        return np.array([self.c6, self.c7, self.intercept, self.slope])


def grid_model_from(accession: str, parameters: npt.NDArray[np.float64]) -> GridLike:
    match accession:
        case LinearGrid.accession:
            return LinearGrid(*parameters)
        case SquareRootLinearGrid.accession:
            return SquareRootLinearGrid(*parameters)
        case TimsLinearGrid2.accession:
            return TimsLinearGrid2(*parameters)
        case _:
            raise KeyError(accession)
