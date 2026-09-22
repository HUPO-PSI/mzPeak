use std::collections::HashMap;
use std::hash::Hash;

use mzdata::io::tdf::{MzCalibrationModel2, TimsCalibrationModel2, clamp_u32};
use mzdata::params::{ParamDescribed, ParamLike};
use mzdata::spectrum::{ArrayType, BinaryArrayMap};
use mzdata::spectrum::bindata::{BuildArrayMapFrom, ByteArrayView};
use mzdata::{
    curie,
    params::{CURIE, Param, ParamValue},
};
use mzpeaks::{CentroidLike, DeconvolutedCentroidLike, MZLocated, MassLocated, Tolerance};

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

    fn from_parameters(grid_type: CURIE, parameters: &[f64]) -> Option<Self>
    where
        Self: Sized;

    fn error(&self, values: &[f64], ppm: bool) -> Vec<f64> {
        if ppm {
            values
                .iter()
                .copied()
                .map(|v| (v - self.from_index(self.to_index(v))) / v * 1e6)
                .collect()
        } else {
            values
                .iter()
                .copied()
                .map(|v| v - self.from_index(self.to_index(v)))
                .collect()
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, PartialOrd)]
pub struct LinearGrid {
    pub intercept: f64,
    pub slope: f64,
    pub scale: f64,
}

impl Eq for LinearGrid {}

impl Ord for LinearGrid {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.intercept
            .total_cmp(&other.intercept)
            .then(self.slope.total_cmp(&other.slope))
    }
}

impl LinearGrid {
    pub const ACCESSION: CURIE = curie!(MS:1003824);

    pub fn new(intercept: f64, slope: f64, scale: f64) -> Self {
        Self {
            intercept,
            slope,
            scale,
        }
    }

    pub fn from_param(param: &Param) -> Option<Self> {
        if param.curie() != Some(Self::ACCESSION) {
            return None;
        }
        let mut vals = param_list_to_floats(param)?;
        let intercept = vals.next()??;
        let slope = vals.next()??;
        let scale = vals.next().unwrap_or(Some(1.0))?;
        Some(Self::new(intercept, slope, scale))
    }

    pub fn fit(values: &[f64], low: f64, high: f64, scale: f64) -> Option<Self> {
        let slots = u32::MAX;
        let low = low * scale;
        let high = high * scale;
        let step_size = (high - low) / slots as f64;

        macro_rules! xy {
            ($v:ident vector) => {{
                let y = $v.map(|v| v * scale);
                let x = y.map(|y| (y - low) / step_size);
                (x, y)
            }};
            ($v:ident scalar) => {{
                let y = ($v * scale);
                let x = (y - low) / step_size;
                (x, y)
            }};
        }

        let mut ymean = [0.0; 4];
        let mut xmean = [0.0; 4];

        let (it, last) = values.as_chunks::<4>();
        for v in it {
            let (x, y) = xy!(v vector);
            for (y, yo) in y.into_iter().zip(ymean.iter_mut()) {
                *yo += y;
            }
            for (x, xo) in x.into_iter().zip(xmean.iter_mut()) {
                *xo += x;
            }
        }

        let mut ymean = ymean.into_iter().sum::<f64>();
        let mut xmean = xmean.into_iter().sum::<f64>();
        for v in last {
            let (x, y) = xy!(v scalar);
            ymean += y;
            xmean += x;
        }

        ymean = ymean / values.len() as f64;
        let xmean = xmean as f64 / values.len() as f64;

        let mut xdiff = [0.0; 4];
        let mut xydiff = [0.0; 4];

        for v in it {
            let (x, y) = xy!(v vector);
            let xd = x.map(|x| x - xmean);
            let yd = y.map(|y| y - ymean);

            for ((y, x), yo) in yd
                .into_iter()
                .zip(xd.iter().copied())
                .zip(xydiff.iter_mut())
            {
                *yo += y * x;
            }
            for (x, xo) in xd.into_iter().zip(xdiff.iter_mut()) {
                *xo += x.powi(2);
            }
        }

        let mut xydiff = xydiff.into_iter().sum::<f64>();
        let mut xdiff = xdiff.into_iter().sum::<f64>();

        for v in last {
            let (x, y) = xy!(v scalar);
            let xd = x - xmean;
            let yd = y - ymean;

            xydiff += xd * yd;
            xdiff += xd.powi(2);
        }

        let slope = xydiff / xdiff;
        let intercept = ymean - slope * xmean;
        let model = Self::new(intercept, slope, scale);

        if values.is_empty() {
            return Some(model);
        }
        let (min, max) = GridPolicy::minmax(values).unwrap();
        (model.from_index(model.to_index(min)) < model.from_index(model.to_index(max)))
            .then(|| model)
    }
}

impl GridModelLike for LinearGrid {
    fn grid_type(&self) -> CURIE {
        Self::ACCESSION
    }

    fn to_index(&self, value: f64) -> u32 {
        clamp_u32((value * self.scale - self.intercept) / self.slope)
    }

    fn from_index(&self, index: u32) -> f64 {
        (index as f64 * self.slope + self.intercept) / self.scale
    }

    fn parameters(&self) -> Vec<f64> {
        vec![self.intercept, self.slope, self.scale]
    }

    fn from_param(parameters: &Param) -> Option<Self>
    where
        Self: Sized,
    {
        Self::from_param(parameters)
    }

    fn from_parameters(grid_type: CURIE, parameters: &[f64]) -> Option<Self>
    where
        Self: Sized,
    {
        if parameters.len() > 3 || parameters.len() < 2 || grid_type != Self::ACCESSION {
            return None;
        }
        Some(Self::new(
            parameters[0],
            parameters[1],
            parameters.get(2).copied().unwrap_or(1.0),
        ))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, PartialOrd)]
pub struct SquareRootLinearGrid {
    pub intercept: f64,
    pub slope: f64,
    pub scale: f64,
}

impl Eq for SquareRootLinearGrid {}

impl Ord for SquareRootLinearGrid {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.intercept
            .total_cmp(&other.intercept)
            .then(self.slope.total_cmp(&other.slope))
            .then(self.scale.total_cmp(&other.scale))
    }
}

impl SquareRootLinearGrid {
    pub const ACCESSION: CURIE = curie!(MS:1003825);

    pub fn new(intercept: f64, slope: f64, scale: f64) -> Self {
        Self {
            intercept,
            slope,
            scale,
        }
    }

    pub fn from_param(param: &Param) -> Option<Self> {
        if param.curie() != Some(Self::ACCESSION) {
            return None;
        }
        let mut vals = param_list_to_floats(param)?;
        let intercept = vals.next()??;
        let slope = vals.next()??;
        let scale = vals.next().unwrap_or(Some(1.0))?;
        Some(Self::new(intercept, slope, scale))
    }

    pub fn fit(values: &[f64], low: f64, high: f64, scale: f64) -> Option<Self> {
        let slots = u32::MAX;
        let step_size = (high * scale - low * scale) / slots as f64;
        let low_sqrt = (low * scale).sqrt();

        let mut ymean = [0.0; 4];
        let mut xmean = [0.0; 4];

        macro_rules! xy {
            ($v:ident vector) => {{
                let y = $v.map(|v| v * scale).map(|v| v.sqrt());
                let x = y.map(|y| (y - low_sqrt) / step_size);
                (x, y)
            }};
            ($v:ident scalar) => {{
                let y = ($v * scale).sqrt();
                let x = (y - low_sqrt) / step_size;
                (x, y)
            }};
        }

        let (it, last) = values.as_chunks::<4>();
        for v in it {
            let (x, y) = xy!(v vector);
            for (y, yo) in y.into_iter().zip(ymean.iter_mut()) {
                *yo += y;
            }
            for (x, xo) in x.into_iter().zip(xmean.iter_mut()) {
                *xo += x;
            }
        }

        let mut ymean = ymean.into_iter().sum::<f64>();
        let mut xmean = xmean.into_iter().sum::<f64>();
        for v in last {
            let (x, y) = xy!(v scalar);
            ymean += y;
            xmean += x;
        }

        ymean = ymean / values.len() as f64;
        let xmean = xmean as f64 / values.len() as f64;

        let mut xdiff = [0.0; 4];
        let mut xydiff = [0.0; 4];

        for v in it {
            let (x, y) = xy!(v vector);
            let xd = x.map(|x| x - xmean);
            let yd = y.map(|y| y - ymean);

            for ((y, x), yo) in yd
                .into_iter()
                .zip(xd.iter().copied())
                .zip(xydiff.iter_mut())
            {
                *yo += y * x;
            }
            for (x, xo) in xd.into_iter().zip(xdiff.iter_mut()) {
                *xo += x.powi(2);
            }
        }

        let mut xydiff = xydiff.into_iter().sum::<f64>();
        let mut xdiff = xdiff.into_iter().sum::<f64>();

        for v in last {
            let (x, y) = xy!(v scalar);

            let xd = x - xmean;
            let yd = y - ymean;
            xydiff += xd * yd;
            xdiff += xd.powi(2);
        }

        let slope = xydiff / xdiff;
        let intercept = ymean - slope * xmean;
        let model = Self::new(intercept, slope, scale);
        if values.is_empty() {
            return Some(model);
        }

        let (min, max) = GridPolicy::minmax(values).unwrap();
        (model.from_index(model.to_index(min)) < model.from_index(model.to_index(max)))
            .then(|| model)
    }
}

impl GridModelLike for SquareRootLinearGrid {
    fn grid_type(&self) -> CURIE {
        Self::ACCESSION
    }

    fn to_index(&self, value: f64) -> u32 {
        clamp_u32(((value * self.scale).sqrt() - self.intercept) / self.slope)
    }

    fn from_index(&self, index: u32) -> f64 {
        ((index as f64) * self.slope + self.intercept).powi(2) / self.scale
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

    fn from_parameters(grid_type: CURIE, parameters: &[f64]) -> Option<Self>
    where
        Self: Sized,
    {
        if parameters.len() > 3 || parameters.len() < 2 || grid_type != Self::ACCESSION {
            return None;
        }
        Some(Self::new(
            parameters[0],
            parameters[1],
            parameters.get(2).copied().unwrap_or(1.0),
        ))
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TimsTofMzGrid2(mzdata::io::tdf::MzCalibrationModel2);

impl TimsTofMzGrid2 {
    pub fn new(mz_calibration_model2: mzdata::io::tdf::MzCalibrationModel2) -> Self {
        Self(mz_calibration_model2)
    }

    pub const ACCESSION: CURIE = curie!(MS:9999002);
}

impl From<mzdata::io::tdf::MzCalibrationModel2> for TimsTofMzGrid2 {
    fn from(value: mzdata::io::tdf::MzCalibrationModel2) -> Self {
        Self(value)
    }
}

impl Eq for TimsTofMzGrid2 {}

impl PartialOrd for TimsTofMzGrid2 {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for TimsTofMzGrid2 {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.0.model_type.cmp(&other.0.model_type).then(
            self.0.c0.total_cmp(&other.0.c0).then(
                self.0.beta.total_cmp(&other.0.beta).then(
                    self.0.c2.total_cmp(&other.0.c2).then(
                        self.0.c3.total_cmp(&other.0.c3).then(
                            self.0.c4.total_cmp(&other.0.c4).then(
                                self.0
                                    .digitizer_timebase
                                    .total_cmp(&other.0.digitizer_timebase)
                                    .then(
                                        self.0.digitizer_delay.total_cmp(&other.0.digitizer_delay),
                                    ),
                            ),
                        ),
                    ),
                ),
            ),
        )
    }
}

impl GridModelLike for TimsTofMzGrid2 {
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
        self.0
            .as_param()
            .value()
            .as_slice()
            .iter()
            .map(|v| v.to_f64().unwrap())
            .collect()
    }

    fn from_param(parameters: &Param) -> Option<Self>
    where
        Self: Sized,
    {
        let v = parameters.as_slice();
        let mut it = v.iter();
        let c0 = it.next()?.to_f64().ok()?;
        let beta = it.next()?.to_f64().ok()?;
        let c2 = it.next()?.to_f64().ok()?;
        let c3 = it.next()?.to_f64().ok()?;
        let c4 = it.next()?.to_f64().ok()?;
        let digitizer_timebase = it.next()?.to_f64().ok()?;
        let digitizer_delay = it.next()?.to_f64().ok()?;
        Some(Self(mzdata::io::tdf::MzCalibrationModel2::new(
            2,
            c0,
            beta,
            c2,
            c3,
            c4,
            digitizer_timebase,
            digitizer_delay,
        )))
    }

    fn from_parameters(grid_type: CURIE, parameters: &[f64]) -> Option<Self>
    where
        Self: Sized,
    {
        if parameters.len() != 7 || grid_type != Self::ACCESSION {
            return None;
        }
        Some(Self::new(MzCalibrationModel2::new(
            0,
            parameters[0],
            parameters[1],
            parameters[2],
            parameters[3],
            parameters[4],
            parameters[5],
            parameters[6],
        )))
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
                self.0
                    .offset
                    .total_cmp(&other.0.offset)
                    .then(self.0.slope.total_cmp(&other.0.slope)),
            ),
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
        let offset = it.next()?.to_f64().ok()?;
        let slope = it.next()?.to_f64().ok()?;
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

    fn from_parameters(grid_type: CURIE, parameters: &[f64]) -> Option<Self>
    where
        Self: Sized,
    {
        if parameters.len() != 4 || grid_type != Self::ACCESSION {
            return None;
        }
        Some(Self::new(TimsCalibrationModel2::new(
            parameters[0],
            parameters[1],
            parameters[2],
            parameters[3],
        )))
    }
}

impl From<TimsTofTimsLinearGrid2> for GridEncoding {
    fn from(v: TimsTofTimsLinearGrid2) -> Self {
        Self::TimsTofTims2(v)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, PartialOrd, Eq)]
pub enum GridEncoding {
    Linear(LinearGrid),
    SquareRootLinear(SquareRootLinearGrid),
    TimsTofTims2(TimsTofTimsLinearGrid2),
    TimsTofMzGrid2(TimsTofMzGrid2),
}

impl From<TimsTofMzGrid2> for GridEncoding {
    fn from(v: TimsTofMzGrid2) -> Self {
        Self::TimsTofMzGrid2(v)
    }
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
            GridEncoding::TimsTofMzGrid2($r) => $e,
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
            TimsTofMzGrid2::ACCESSION => TimsTofMzGrid2::from_param(parameters).map(Self::from),
            _ => None,
        }
    }

    fn from_parameters(grid_type: CURIE, parameters: &[f64]) -> Option<Self>
    where
        Self: Sized,
    {
        match grid_type {
            LinearGrid::ACCESSION => {
                LinearGrid::from_parameters(grid_type, parameters).map(Self::from)
            }
            SquareRootLinearGrid::ACCESSION => {
                SquareRootLinearGrid::from_parameters(grid_type, parameters).map(Self::from)
            }
            TimsTofTimsLinearGrid2::ACCESSION => {
                TimsTofTimsLinearGrid2::from_parameters(grid_type, parameters).map(Self::from)
            }
            TimsTofMzGrid2::ACCESSION => {
                TimsTofMzGrid2::from_parameters(grid_type, parameters).map(Self::from)
            }
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct GridPolicy {
    /// The kind of array this applies to
    pub array_type: ArrayType,
    /// Fall back to [`SquareRootLinearGrid`] instead of [`LinearGrid`]
    pub fallback_square_root_linear: bool,
    /// The maximum error to tolerate when using a fitted grid
    pub maximum_error_tolerance: Option<Tolerance>,
    #[serde(skip)]
    pub current_grid: Option<GridEncoding>,
}

impl GridPolicy {
    pub const fn new(
        array_type: ArrayType,
        fallback_square_root_linear: bool,
        maximum_error_tolerance: Option<Tolerance>,
    ) -> Self {
        Self {
            array_type,
            fallback_square_root_linear,
            maximum_error_tolerance,
            current_grid: None,
        }
    }

    pub const fn quadratic(
        array_type: ArrayType,
        maximum_error_tolerance: Option<Tolerance>,
    ) -> Self {
        Self {
            array_type,
            fallback_square_root_linear: true,
            maximum_error_tolerance,
            current_grid: None,
        }
    }

    pub const fn linear(array_type: ArrayType, maximum_error_tolerance: Option<Tolerance>) -> Self {
        Self {
            array_type,
            fallback_square_root_linear: false,
            maximum_error_tolerance,
            current_grid: None,
        }
    }

    pub fn find_grid_model_param<P: ParamDescribed>(p: &P) -> Option<GridEncoding> {
        p.params()
            .iter()
            .find_map(|par| GridEncoding::from_param(par))
    }

    pub fn minmax(values: &[f64]) -> Option<(f64, f64)> {
        if values.is_empty() {
            return None;
        }
        Some(
            values
                .iter()
                .fold((f64::INFINITY, f64::NEG_INFINITY), |(min, max), v| {
                    (min.min(*v), max.max(*v))
                }),
        )
    }

    fn padding(low: f64, high: f64) -> f64 {
        ((high - low) * 0.05).min(5.0).max(0.0)
    }

    pub fn model_from_array_map(&self, arrays: &BinaryArrayMap, scale: Option<f64>) -> Option<GridEncoding> {
        arrays.get(&self.array_type).and_then(|v| {
            if let Some(model) = Self::find_grid_model_param(v) {
                return Some(model)
            }
            let v = v.to_f64().ok()?;
            let (low, high) = Self::minmax(&v).unwrap();
            let pad = Self::padding(low, high);
            self.model_from(&v, (low - pad).max(0.0), high + pad, scale)
        })
    }

    pub fn model_from_peaks<
        C: CentroidLike + BuildArrayMapFrom,
        D: DeconvolutedCentroidLike + BuildArrayMapFrom,
    >(
        &self,
        peaks: mzdata::spectrum::RefPeakDataLevel<'_, C, D>,
        scale: Option<f64>,
    ) -> Option<GridEncoding> {
        if peaks.is_empty() {
            return None;
        }
        match peaks {
            mzdata::spectrum::RefPeakDataLevel::Missing
            | mzdata::spectrum::RefPeakDataLevel::RawData(_) => None,
            mzdata::spectrum::RefPeakDataLevel::Centroid(peak_set_vec) => {
                if self.array_type == ArrayType::MZArray {
                    let mzs: Vec<_> = peak_set_vec.iter().map(|p| p.mz()).collect();
                    let low = mzs.first().copied().unwrap();
                    let high = mzs.last().copied().unwrap();
                    let pad = Self::padding(low, high);
                    self.model_from(&mzs, (low - pad).max(0.0), high + pad, scale)
                } else {
                    let arrays = BuildArrayMapFrom::as_arrays(peak_set_vec.as_slice());
                    self.model_from_array_map(&arrays, scale)
                }
            }
            mzdata::spectrum::RefPeakDataLevel::Deconvoluted(peak_set_vec) => {
                if self.array_type == ArrayType::MZArray {
                    let mzs: Vec<_> = peak_set_vec
                        .iter()
                        .map(|p| mzdata::utils::mass_charge_ratio(p.neutral_mass(), p.charge()))
                        .collect();
                    let (low, high) = Self::minmax(&mzs).unwrap();
                    let pad = Self::padding(low, high);
                    self.model_from(&mzs, (low - pad).max(0.0), high + pad, scale)
                } else {
                    let arrays = BuildArrayMapFrom::as_arrays(peak_set_vec.as_slice());
                    self.model_from_array_map(&arrays, scale)
                }
            }
        }
    }

    pub fn model_from(
        &self,
        array: &[f64],
        low: f64,
        high: f64,
        scale: Option<f64>,
    ) -> Option<GridEncoding> {
        if array.is_empty() {
            return None;
        }
        let model: GridEncoding = if self.fallback_square_root_linear {
            SquareRootLinearGrid::fit(array, low, high, scale.unwrap_or(1.0))?.into()
        } else {
            LinearGrid::fit(array, low, high, scale.unwrap_or(1.0))?.into()
        };

        if let Some(thresh) = self.maximum_error_tolerance {
            let (passes, max_err) = match thresh {
                Tolerance::PPM(t) => {
                    let err = model.error(array, true);
                    let max_err = err.iter().copied().reduce(|a, b| a.max(b)).unwrap();
                    (t >= max_err, max_err)
                }
                Tolerance::Da(t) => {
                    let err = model.error(array, false);
                    let max_err = err.iter().copied().reduce(|a, b| a.max(b)).unwrap();
                    (t >= max_err, max_err)
                }
            };
            if passes {
                Some(model)
            } else {
                log::warn!("Failed to construct satisfactory model, error was {max_err}");
                None
            }
        } else {
            Some(model)
        }
    }

    pub fn current_grid(&self) -> Option<&GridEncoding> {
        self.current_grid.as_ref()
    }

    pub fn set_current_grid(&mut self, current_grid: Option<GridEncoding>) {
        self.current_grid = current_grid;
    }
}

pub type GridPolicyTable = HashMap<ArrayType, GridPolicy>;
