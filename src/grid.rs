use std::hash::Hash;

use mzdata::io::tdf::clamp_u32;
use mzdata::{
    curie,
    params::{CURIE, Param, ParamValue},
};

#[inline(always)]
fn param_list_to_floats(param: &Param) -> Option<impl Iterator<Item = Option<f64>> + '_> {
    match &param.value {
        mzdata::params::Value::String(_) => None,
        mzdata::params::Value::Float(_) => None,
        mzdata::params::Value::Int(_) => None,
        mzdata::params::Value::Buffer(_) => None,
        mzdata::params::Value::Boolean(_) => None,
        mzdata::params::Value::Empty => None,
        mzdata::params::Value::List(values) => Some(values.iter().map(|v| v.to_f64().ok())),
    }
}

pub trait GridModelLike {
    fn grid_type(&self) -> CURIE;
    fn to_index(&self, value: f64) -> u32;
    fn from_index(&self, index: u32) -> f64;
    fn parameters(&self) -> Vec<f64>;
    fn from_param(parameters: &Param) -> Option<Self>
    where
        Self: Sized;
}

#[derive(Debug, Clone, Copy, PartialEq, PartialOrd)]
pub struct LinearGrid {
    pub intercept: f64,
    pub slope: f64,
}

impl Eq for LinearGrid {}

impl Ord for LinearGrid {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.intercept.total_cmp(&other.intercept).then(self.slope.total_cmp(&other.slope))
    }
}

impl LinearGrid {
    pub const ACCESSION: CURIE = curie!(MS:1003824);

    pub fn new(intercept: f64, slope: f64) -> Self {
        Self { intercept, slope }
    }

    pub fn from_param(param: &Param) -> Option<Self> {
        if param.curie() != Some(Self::ACCESSION) {
            return None;
        }
        let mut vals = param_list_to_floats(param)?;
        let intercept = vals.next()??;
        let slope = vals.next()??;
        Some(Self::new(intercept, slope))
    }
}

impl GridModelLike for LinearGrid {
    fn grid_type(&self) -> CURIE {
        Self::ACCESSION
    }

    fn to_index(&self, value: f64) -> u32 {
        clamp_u32((value - self.intercept) / self.slope)
    }

    fn from_index(&self, index: u32) -> f64 {
        index as f64 * self.slope + self.intercept
    }

    fn parameters(&self) -> Vec<f64> {
        vec![self.intercept, self.slope]
    }

    fn from_param(parameters: &Param) -> Option<Self>
    where
        Self: Sized,
    {
        Self::from_param(parameters)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, PartialOrd)]
pub struct SquareRootLinearGrid {
    pub intercept: f64,
    pub slope: f64,
}

impl Eq for SquareRootLinearGrid {}

impl Ord for SquareRootLinearGrid {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.intercept.total_cmp(&other.intercept).then(self.slope.total_cmp(&other.slope))
    }
}

impl SquareRootLinearGrid {
    pub const ACCESSION: CURIE = curie!(MS:1003825);

    pub fn new(intercept: f64, slope: f64) -> Self {
        Self { intercept, slope }
    }

    pub fn from_param(param: &Param) -> Option<Self> {
        if param.curie() != Some(Self::ACCESSION) {
            return None;
        }
        let mut vals = param_list_to_floats(param)?;
        let intercept = vals.next()??;
        let slope = vals.next()??;
        Some(Self::new(intercept, slope))
    }
}

impl GridModelLike for SquareRootLinearGrid {
    fn grid_type(&self) -> CURIE {
        Self::ACCESSION
    }

    fn to_index(&self, value: f64) -> u32 {
        clamp_u32((value.sqrt() - self.intercept) / self.slope)
    }

    fn from_index(&self, index: u32) -> f64 {
        ((index as f64) * self.slope + self.intercept).powi(2)
    }

    fn parameters(&self) -> Vec<f64> {
        vec![self.intercept, self.slope]
    }

    fn from_param(parameters: &Param) -> Option<Self>
    where
        Self: Sized,
    {
        Self::from_param(parameters)
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TimsTofTimsLinearGrid2(mzdata::io::tdf::TimsCalibrationModel2);

impl Eq for TimsTofTimsLinearGrid2 {}

impl PartialOrd for TimsTofTimsLinearGrid2 {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for TimsTofTimsLinearGrid2 {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.0.c6.total_cmp(&other.0.c6).then(
            self.0.c7.total_cmp(&other.0.c7).then(
                self.0.offset.total_cmp(&other.0.offset).then(
                    self.0.slope.total_cmp(&other.0.slope)
                )
            )
        )
    }
}

impl From<mzdata::io::tdf::TimsCalibrationModel2> for TimsTofTimsLinearGrid2 {
    fn from(value: mzdata::io::tdf::TimsCalibrationModel2) -> Self {
        Self::new(value)
    }
}

impl TimsTofTimsLinearGrid2 {
    pub const ACCESSION: CURIE = curie!(MS:9999001);

    pub fn from_param(param: &Param) -> Option<Self> {
        let v = param.as_slice();
        let mut it = v.iter();
        let c6 = it.next()?.to_f64().ok()?;
        let c7 = it.next()?.to_f64().ok()?;
        let slope = it.next()?.to_f64().ok()?;
        let offset = it.next()?.to_f64().ok()?;
        Some(Self(mzdata::io::tdf::TimsCalibrationModel2::new(
            c6, c7, offset, slope,
        )))
    }

    pub fn new(model: mzdata::io::tdf::TimsCalibrationModel2) -> Self {
        Self(model)
    }
}

impl GridModelLike for TimsTofTimsLinearGrid2 {
    fn grid_type(&self) -> CURIE {
        Self::ACCESSION
    }

    fn to_index(&self, value: f64) -> u32 {
        use timsrust::converters::ConvertableDomain;
        mzdata::io::tdf::clamp_u32(self.0.invert(value))
    }

    fn from_index(&self, index: u32) -> f64 {
        use timsrust::converters::ConvertableDomain;
        self.0.convert(index)
    }

    fn parameters(&self) -> Vec<f64> {
        vec![self.0.c6, self.0.c7, self.0.slope, self.0.offset]
    }

    fn from_param(param: &Param) -> Option<Self> {
        Self::from_param(param)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, PartialOrd, Eq, Ord)]
pub enum GridEncoding {
    Linear(LinearGrid),
    SquareRootLinear(SquareRootLinearGrid),
    TimsTofTims2(TimsTofTimsLinearGrid2),
}

impl Hash for GridEncoding {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        core::mem::discriminant(self).hash(state);
        self.grid_type().hash(state);
        for val in self.parameters() {
            (val as i64).hash(state);
        }
    }
}

impl From<LinearGrid> for GridEncoding {
    fn from(v: LinearGrid) -> Self {
        Self::Linear(v)
    }
}

impl From<SquareRootLinearGrid> for GridEncoding {
    fn from(v: SquareRootLinearGrid) -> Self {
        Self::SquareRootLinear(v)
    }
}

macro_rules! grid_dp {
    ($d:ident, $r:ident, $e:expr) => {
        match $d {
            GridEncoding::Linear($r) => $e,
            GridEncoding::SquareRootLinear($r) => $e,
            GridEncoding::TimsTofTims2($r) => $e,
        }
    };
}

impl GridModelLike for GridEncoding {
    fn grid_type(&self) -> CURIE {
        grid_dp!(self, grid, grid.grid_type())
    }

    fn to_index(&self, value: f64) -> u32 {
        grid_dp!(self, grid, grid.to_index(value))
    }

    fn from_index(&self, index: u32) -> f64 {
        grid_dp!(self, grid, grid.from_index(index))
    }

    fn parameters(&self) -> Vec<f64> {
        grid_dp!(self, grid, grid.parameters())
    }

    fn from_param(parameters: &Param) -> Option<Self>
    where
        Self: Sized,
    {
        match parameters.curie()? {
            LinearGrid::ACCESSION => LinearGrid::from_param(parameters).map(Self::from),
            SquareRootLinearGrid::ACCESSION => {
                SquareRootLinearGrid::from_param(parameters).map(Self::from)
            }
            TimsTofTimsLinearGrid2::ACCESSION => {
                TimsTofTimsLinearGrid2::from_param(parameters).map(Self::from)
            }
            _ => None,
        }
    }
}

impl From<TimsTofTimsLinearGrid2> for GridEncoding {
    fn from(v: TimsTofTimsLinearGrid2) -> Self {
        Self::TimsTofTims2(v)
    }
}
