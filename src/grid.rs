use mzdata::{params::{CURIE, Param, ParamValue}, curie};


pub trait GridModelLike {
    fn grid_type(&self) -> CURIE;
    fn to_index(&self, value: f64) -> u32;
    fn from_index(&self, index: u32) -> f64;
    fn parameters(&self) -> Vec<f64>;
    fn from_param(parameters: &Param) -> Option<Self> where Self: Sized;
}


#[derive(Debug, Clone, Copy)]
pub struct TimsTofTimsLinearGrid1(mzdata::io::tdf::TimsCalibrationModel1);

impl From<mzdata::io::tdf::TimsCalibrationModel1> for TimsTofTimsLinearGrid1 {
    fn from(value: mzdata::io::tdf::TimsCalibrationModel1) -> Self {
        Self::new(value)
    }
}

impl TimsTofTimsLinearGrid1 {
    pub const ACCESSION: CURIE = curie!(MS:1003824);

    pub fn from_param(param: &Param) -> Option<Self> {
        let v = param.as_slice();
        let mut it = v.iter();
        let c6 = it.next()?.to_f64().ok()?;
        let c7 = it.next()?.to_f64().ok()?;
        let slope = it.next()?.to_f64().ok()?;
        let offset = it.next()?.to_f64().ok()?;
        Some(Self(mzdata::io::tdf::TimsCalibrationModel1::new(c6, c7, offset, slope)))
    }

    pub fn new(model: mzdata::io::tdf::TimsCalibrationModel1) -> Self {
        Self(model)
    }
}

impl GridModelLike for TimsTofTimsLinearGrid1 {
    fn grid_type(&self) -> CURIE {
        curie!(MS:1003824)
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

#[derive(Debug, Clone, Copy)]
pub struct TimsTofMZGrid1(mzdata::io::tdf::MzCalibrationModel1);

impl From<mzdata::io::tdf::MzCalibrationModel1> for TimsTofMZGrid1 {
    fn from(value: mzdata::io::tdf::MzCalibrationModel1) -> Self {
        Self(value)
    }
}

impl GridModelLike for TimsTofMZGrid1 {
    fn grid_type(&self) -> CURIE {
        curie!(MS:1003825)
    }

    fn to_index(&self, value: f64) -> u32 {
        mzdata::io::tdf::clamp_u32(self.0.invert_f64(value))
    }

    fn from_index(&self, index: u32) -> f64 {
        use timsrust::converters::ConvertableDomain;
        self.0.convert(index)
    }

    fn parameters(&self) -> Vec<f64> {
        vec![self.0.c0, self.0.c1, self.0.digitizer_timebase, self.0.digitize_delay]
    }


    fn from_param(param: &Param) -> Option<Self> {
        Self::from_param(param)
    }
}

impl TimsTofMZGrid1 {
    pub const ACCESSION: CURIE = curie!(MS:1003825);

    pub fn new(mz_calibration_model1: mzdata::io::tdf::MzCalibrationModel1) -> Self {
        Self(mz_calibration_model1)
    }

    pub fn from_param(param: &Param) -> Option<Self> {
        let v = param.as_slice();
        let mut it = v.iter();
        let c0 = it.next()?.to_f64().ok()?;
        let c1 = it.next()?.to_f64().ok()?;
        let timebase = it.next()?.to_f64().ok()?;
        let delay = it.next()?.to_f64().ok()?;
        Some(Self(mzdata::io::tdf::MzCalibrationModel1::new(c0, c1, timebase, delay)))
    }
}

#[derive(Debug, Clone)]
pub enum GridEncoding {
    TimsTofMZ1(TimsTofMZGrid1),
    TimsTofTims1(TimsTofTimsLinearGrid1),
}

impl GridModelLike for GridEncoding {
    fn grid_type(&self) -> CURIE {
        match self {
            GridEncoding::TimsTofMZ1(tims_tof_mzgrid1) => tims_tof_mzgrid1.grid_type(),
            GridEncoding::TimsTofTims1(tims_tof_tims_linear_grid1) => tims_tof_tims_linear_grid1.grid_type(),
        }
    }

    fn to_index(&self, value: f64) -> u32 {
        match self {
            GridEncoding::TimsTofMZ1(tims_tof_mzgrid1) => tims_tof_mzgrid1.to_index(value),
            GridEncoding::TimsTofTims1(tims_tof_tims_linear_grid1) => tims_tof_tims_linear_grid1.to_index(value),
        }
    }

    fn from_index(&self, index: u32) -> f64 {
        match self {
            GridEncoding::TimsTofMZ1(tims_tof_mzgrid1) => tims_tof_mzgrid1.from_index(index),
            GridEncoding::TimsTofTims1(tims_tof_tims_linear_grid1) => tims_tof_tims_linear_grid1.from_index(index),
        }
    }

    fn parameters(&self) -> Vec<f64> {
        match self {
            GridEncoding::TimsTofMZ1(tims_tof_mzgrid1) => tims_tof_mzgrid1.parameters(),
            GridEncoding::TimsTofTims1(tims_tof_tims_linear_grid1) => tims_tof_tims_linear_grid1.parameters(),
        }
    }

    fn from_param(parameters: &Param) -> Option<Self> where Self: Sized {
        match parameters.curie()? {
            TimsTofMZGrid1::ACCESSION => TimsTofMZGrid1::from_param(parameters).map(Self::from),
            TimsTofTimsLinearGrid1::ACCESSION => TimsTofTimsLinearGrid1::from_param(parameters).map(Self::from),
            _ => None
        }
    }
}

impl From<TimsTofTimsLinearGrid1> for GridEncoding {
    fn from(v: TimsTofTimsLinearGrid1) -> Self {
        Self::TimsTofTims1(v)
    }
}

impl From<TimsTofMZGrid1> for GridEncoding {
    fn from(v: TimsTofMZGrid1) -> Self {
        Self::TimsTofMZ1(v)
    }
}