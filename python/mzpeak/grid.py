from dataclasses import dataclass
from typing import Any, ClassVar, Protocol, Self, overload

import numpy as np
from numpy import typing as npt


@dataclass
class GridLike(Protocol):
    name: ClassVar[str]
    accession: ClassVar[str]

    @classmethod
    def fit(cls, values: npt.NDArray[np.float64], *args, **kwargs) -> Self: ...

    @overload
    def from_index(self, index: int) -> float: ...

    @overload
    def from_index(self, index: npt.NDArray[np.uint32]) -> npt.NDArray[np.float64]: ...

    @overload
    def to_index(self, value: float) -> int: ...

    @overload
    def to_index(self, value: npt.NDArray[np.float64]) -> npt.NDArray[np.uint32]: ...

    def parameters(self) -> npt.NDArray[np.float64]: ...

    def __call__(self, value: int | npt.NDArray[np.uint32]):
        return self.from_index(value)

    def to_param(self) -> dict[str, Any]:
        return {
            "name": self.name,
            "accession": self.accession,
            "value": self.parameters().tolist()
        }

    @staticmethod
    def from_param(accession: str, parameters: npt.NDArray[np.float64]) -> 'GridLike':
        return grid_model_from(accession, parameters)

    def error(self, values: npt.NDArray[np.float64]):
        yhat = self.from_index(self.to_index(values))
        err = values - yhat
        return err, np.median(np.abs(err)), np.max(np.abs(err))


@dataclass
class LinearGrid(GridLike):
    intercept: float
    slope: float
    scale: float = 1.0

    name: ClassVar[str] = "linear grid interpolation"
    accession: ClassVar[str] = "MS:1003824"

    @classmethod
    def fit(
        cls,
        values: npt.NDArray[np.float64],
        low: float,
        high: float,
        scale: float = 1.0,
    ):
        slots = np.iinfo(np.uint32).max
        step_size = (high * scale - low * scale) / slots
        values = values * scale
        ii = ((values - (low * scale)) / step_size).astype(np.uint32)
        X = np.stack([np.ones_like(ii), ii], -1)
        par = np.linalg.lstsq(X, values)[0]
        return cls(par[0], par[1])

    def parameters(self):
        return np.array([self.intercept, self.slope, self.scale])

    def from_index(
        self, index: int | npt.NDArray[np.uint32]
    ) -> float | npt.NDArray[np.float64]:
        value = (self.intercept + index * self.slope) / self.scale
        if isinstance(value, np.ndarray):
            return value.astype(np.float64)
        return value

    def to_index(
        self, value: float | npt.NDArray[np.float64]
    ) -> int | npt.NDArray[np.uint32]:
        value = (value * self.scale - self.intercept) / self.slope
        if isinstance(value, np.ndarray):
            return (value + 0.5).astype(np.uint32)
        return int(value + 0.5)


@dataclass
class SquareRootLinearGrid(GridLike):
    intercept: float
    slope: float
    scale: float = 1.0

    name: ClassVar[str] = "square root grid interpolation"
    accession: ClassVar[str] = "MS:1003825"

    @classmethod
    def fit(cls, values: npt.NDArray[np.float64], low: float, high: float, scale: float = 1.0):
        slots = np.iinfo(np.uint32).max
        step_size = (high * scale - low * scale) / slots
        values = np.sqrt(values * scale)
        ii = ((values - np.sqrt(low * scale)) / step_size).astype(np.uint32)
        X = np.stack([np.ones_like(ii), ii], -1)
        par = np.linalg.lstsq(X, values)[0]
        return cls(par[0], par[1], scale)

    def parameters(self):
        return np.array([self.intercept, self.slope, self.scale])

    def from_index(
        self, index: int | npt.NDArray[np.uint32]
    ) -> float | npt.NDArray[np.float64]:
        value = (self.intercept + index * self.slope) ** 2 / self.scale
        if isinstance(value, np.ndarray):
            return value.astype(np.float64)
        return value

    def to_index(
        self, value: float | npt.NDArray[np.float64]
    ) -> int | npt.NDArray[np.uint32]:
        value = (np.sqrt(value * self.scale) - self.intercept) / self.slope
        if isinstance(value, np.ndarray):
            return (value + 0.5).astype(np.uint32)
        return int(value + 0.5)



@dataclass
class BrukerTimsTOFTimsLinearGrid2(GridLike):
    accession: ClassVar[str] = "MS:9999001"

    c6: float
    c7: float
    intercept: float
    slope: float

    def from_index(self, value: int | npt.NDArray[np.uint32]):
        return 1.0 / (self.c6 + self.c7 / (self.intercept + self.slope * value))

    def to_index(self, value: float | npt.NDArray[np.float64]):
        d = (1.0 / value) - self.c6
        return ((self.c7 / d) - self.intercept) / self.slope

    def parameters(self):
        return np.array([self.c6, self.c7, self.intercept, self.slope])


@dataclass
class BrukerTimsTOFMzGrid2(GridLike):
    accession: ClassVar[str] = "MS:9999002"

    c0: float
    beta: float
    c2: float
    c3: float
    c4: float
    slope: float
    intercept: float

    def from_index(self, value: int | npt.NDArray[np.uint32]):
        tof = value * self.slope + self.intercept
        inner = tof - self.c0
        s0 = inner / self.beta
        if self.c3 != 0:
            s = s0
            for _ in range(8):
                f = self.c0 + self.beta * s + self.c2 * s * s + self.c3 * s * s * s - tof
                df = self.beta + 2.0 * self.c2 * s + 3.0 * self.c3 * s * s
                mask: np.ndarray = df != 0
                if np.all(~mask):
                    break
                else:
                    step = f[mask] / df[mask]
                    s[mask] -= step
            s0 = s
        elif self.c2 != 0:
            disc = self.beta**2 - 4.0 * self.c2 * (self.c0 - tof)
            mask = (disc < 0)
            disc[mask] = s0[mask]
            q = -0.5 * (self.beta + np.sqrt(disc))
            disc[~mask] = ((self.c0 - tof) / q)[~mask]
            s0 = disc
        return s0**2 - self.c4

    def to_index(self, value: float | npt.NDArray[np.float64]):
        lin = np.sqrt(value + self.c4)
        if np.isscalar(lin):
            lin = max(lin, 0.0)
        else:
            mask = lin < 0.0
            lin[mask] = 0.0
        tof = self.c0 + self.beta * lin + self.c2 * lin ** 2 + self.c3 * lin ** 3
        value = (tof - self.intercept) / self.slope
        if isinstance(value, np.ndarray):
            return (value + 0.5).astype(np.uint32)
        return int(value + 0.5)

    def parameters(self):
        return np.array([
            self.c0,
            self.beta,
            self.c2,
            self.c3,
            self.c4,
            self.intercept,
            self.slope
        ])


def grid_model_from(accession: str, parameters: npt.NDArray[np.float64]) -> GridLike:
    match accession:
        case LinearGrid.accession:
            return LinearGrid(*parameters)
        case SquareRootLinearGrid.accession:
            return SquareRootLinearGrid(*parameters)
        case BrukerTimsTOFTimsLinearGrid2.accession:
            return BrukerTimsTOFTimsLinearGrid2(*parameters)
        case _:
            raise KeyError(accession)
