use arrow::{
    array::{Array, ArrayRef, AsArray, StructArray},
    datatypes::{
        DataType, Float32Type, Float64Type, Int8Type, Int32Type, Int64Type, UInt8Type, UInt32Type,
        UInt64Type,
    },
};

use mzdata::{
    curie,
    meta::{DissociationMethodTerm, SpectrumType},
    params::{CURIE, Unit},
    prelude::*,
    spectrum::{
        Activation, ArrayType, BinaryDataArrayType, ChromatogramDescription, ChromatogramType,
        DataArray, IsolationWindow, Precursor, ScanEvent, ScanPolarity, ScanWindow, SelectedIon,
        SpectrumDescription, bindata::BinaryCompressionType,
    },
};

use crate::{
    param::{MetadataColumn, MetadataMapping},
    reader::visitor::hierarchical::OffsetCollectionSubGroup,
};

mod curie_array_visitor;
pub(crate) use curie_array_visitor::{AnyCURIEArray, ParameterVisitor, UnitArray, UnitCollection};

mod hierarchical;
pub use hierarchical::{CompoundIndexVisitor, DoubleIndexed, Indexed, OffsetCollection};

mod generic;
pub use generic::{SubGroupVisitor, VisitorBuilder3, VisitorBuilderBase};

macro_rules! extract_unit {
    ($metacol:ident, $array:ident) => {{
        let units: UnitCollection<'_> = match &$metacol.unit {
            crate::param::PathOrCURIE::Path(items) => {
                if let Some(val) = $array.column_by_name(items.last().unwrap()) {
                    UnitCollection::series(val.as_struct())
                } else {
                    log::error!("The path {} did not exist", items.join("."));
                    UnitCollection::unknown()
                }
            }
            crate::param::PathOrCURIE::CURIE(curie) => {
                UnitCollection::singular(Unit::from_curie(&(*curie).into()))
            }
            crate::param::PathOrCURIE::None => UnitCollection::unknown(),
        };
        units
    }};
}

macro_rules! visit_source_index {
    ($self:ident, $spec_arr:ident, $index:ident) => {
        let arr = $spec_arr.column($index).as_primitive::<UInt64Type>();
        let mut offsets = Vec::with_capacity($self.descriptions.len());
        let mut j = 0;
        for (i, descr) in $self.descriptions.iter_mut().enumerate() {
            while arr.is_null($self.base_offset + i + j) {
                j += 1;
            }
            let val = arr.value($self.base_offset + i + j);
            offsets.push($self.base_offset + i + j);
            *descr.source_index_mut() = val;
        }
        $self.offsets = offsets
    };
}

pub struct MzSpectrumVisitor<'a> {
    pub(crate) descriptions: &'a mut [SpectrumDescription],
    pub(crate) metadata_map: &'a MetadataMapping,
    pub(crate) base_offset: usize,
    pub(crate) offsets: Vec<usize>,
}

impl<'a> VisitorBuilderBase<'a, SpectrumDescription> for MzSpectrumVisitor<'a> {
    fn iter_instances(&mut self) -> OffsetCollection<'_, SpectrumDescription> {
        OffsetCollection::new(self.descriptions, &self.offsets)
    }

    fn metadata_map(&self) -> &'a MetadataMapping {
        self.metadata_map
    }
}

impl<'a> MzSpectrumVisitor<'a> {
    pub fn new(
        descriptions: &'a mut [SpectrumDescription],
        metadata_map: &'a MetadataMapping,
        base_offset: usize,
    ) -> Self {
        Self {
            descriptions,
            metadata_map,
            base_offset,
            offsets: Vec::new(),
        }
    }

    fn visit_index(&mut self, spec_arr: &StructArray, index: usize) {
        let arr = spec_arr.column(index).as_primitive::<UInt64Type>();
        let mut offsets = Vec::with_capacity(self.descriptions.len());
        let mut j = 0;
        for (i, descr) in self.descriptions.iter_mut().enumerate() {
            while arr.is_null(self.base_offset + i + j) {
                j += 1;
            }
            let val = arr.value(self.base_offset + i + j);
            offsets.push(self.base_offset + i + j);
            descr.index = val as usize;
        }
        self.offsets = offsets
    }

    fn visit_id(&mut self, spec_arr: &StructArray, index: usize) {
        let arr = spec_arr.column(index);
        if let Some(arr) = arr.as_string_opt::<i64>() {
            for (i, descr) in self.iter_instances() {
                let val = arr.value(i);
                descr.id = val.to_string();
            }
        } else if let Some(arr) = arr.as_string_opt::<i32>() {
            for (i, descr) in self.iter_instances() {
                let val = arr.value(i);
                descr.id = val.to_string();
            }
        } else {
            panic!("Unsupported data type: {:?}", arr.data_type());
        }
    }

    fn visit_ms_level(&mut self, spec_arr: &StructArray, index: usize) {
        let arr = spec_arr.column(index);

        macro_rules! process {
            ($arr:expr) => {
                for (i, descr) in self.iter_instances() {
                    if arr.is_null(i) {
                        continue;
                    };
                    let ms_level_val = $arr.value(i);
                    descr.ms_level = ms_level_val as u8;
                }
                return
            };
        }

        if let Some(arr) = arr.as_primitive_opt::<UInt8Type>() {
            process!(arr);
        } else if let Some(arr) = arr.as_primitive_opt::<UInt8Type>() {
            process!(arr);
        } else if let Some(arr) = arr.as_primitive_opt::<UInt32Type>() {
            process!(arr);
        } else if let Some(arr) = arr.as_primitive_opt::<UInt64Type>() {
            process!(arr);
        } else if let Some(arr) = arr.as_primitive_opt::<Int8Type>() {
            process!(arr);
        } else if let Some(arr) = arr.as_primitive_opt::<Int32Type>() {
            process!(arr);
        } else if let Some(arr) = arr.as_primitive_opt::<Int64Type>() {
            process!(arr);
        }
    }

    fn visit_polarity(&mut self, spec_arr: &StructArray, index: usize) {
        let polarity_arr = spec_arr.column(index);
        macro_rules! downcast_run {
            ($($tp:ty)+) => {
                $(
                    if let Some(array) = polarity_arr.as_primitive_opt::<$tp>() {
                        for (i, descr) in self
                            .offsets
                            .iter()
                            .copied()
                            .zip(self.descriptions.iter_mut())
                        {
                            let polarity_val = array.value(i);
                            match polarity_val {
                                1 => descr.polarity = ScanPolarity::Positive,
                                -1 => descr.polarity = ScanPolarity::Negative,
                                _ => {
                                    descr.polarity = ScanPolarity::Unknown;
                                }
                            }
                        }
                        return;
                    }
                )+
            };
        }

        downcast_run!(
            Int8Type
            Int32Type
            Int32Type
            Int64Type
        );
    }

    fn visit_mz_signal_continuity(&mut self, spec_arr: &StructArray, index: usize) {
        let continuity_array = AnyCURIEArray::try_from(spec_arr.column(index)).unwrap();

        for (i, descr) in self.iter_instances() {
            if continuity_array.is_null(i) {
                continue;
            };
            let continuity_curie = continuity_array.value(i).unwrap();
            descr.signal_continuity = match continuity_curie {
                curie!(MS:1000525) => mzdata::spectrum::SignalContinuity::Unknown,
                curie!(MS:1000127) => mzdata::spectrum::SignalContinuity::Centroid,
                curie!(MS:1000128) => mzdata::spectrum::SignalContinuity::Profile,
                _ => todo!("Don't know how to deal with {continuity_curie}"),
            };
        }
    }

    fn visit_spectrum_type(&mut self, spec_arr: &StructArray, index: usize) {
        let spec_type_array = spec_arr.column(index);

        let curie_array = AnyCURIEArray::try_from(spec_type_array).unwrap();

        for (i, descr) in self.iter_instances() {
            let spec_type = curie_array
                .value(i)
                .and_then(|v| SpectrumType::from_accession(v.accession));
            if let Some(spec_type) = spec_type {
                descr.set_spectrum_type(spec_type);
            }
        }
    }

    fn visit_lowest_mz(&mut self, spec_arr: &StructArray, index: usize) {
        let arr = spec_arr.column(index);
        if arr.null_count() == arr.len() {
            return;
        }

        macro_rules! pack {
            ($arr:ident) => {
                for (i, descr) in self.iter_instances()
                {
                    if $arr.is_null(i) {
                        continue;
                    };
                    let p = mzdata::Param::builder()
                        .curie(mzdata::curie!(MS:1000528))
                        .name("lowest observed m/z")
                        .unit(Unit::MZ)
                        .value($arr.value(i))
                        .build();
                    descr.add_param(p);
                }
            };
        }

        if let Some(arr) = arr.as_primitive_opt::<Float64Type>() {
            pack!(arr);
        } else if let Some(arr) = arr.as_primitive_opt::<Float32Type>() {
            pack!(arr);
        } else {
            unimplemented!("{:?} for lowest m/z", arr.data_type())
        }
    }

    fn visit_highest_mz(&mut self, spec_arr: &StructArray, index: usize) {
        let arr = spec_arr.column(index);
        if arr.null_count() == arr.len() {
            return;
        }
        macro_rules! pack {
            ($arr:ident) => {
                for (i, descr) in self.iter_instances()
                {
                    if $arr.is_null(i) {
                        continue;
                    };
                    let p = mzdata::Param::builder()
                        .curie(mzdata::curie!(MS:1000527))
                        .name("highest observed m/z")
                        .unit(Unit::MZ)
                        .value($arr.value(i))
                        .build();
                    descr.add_param(p);
                }
            };
        }

        if let Some(arr) = arr.as_primitive_opt::<Float64Type>() {
            pack!(arr);
        } else if let Some(arr) = arr.as_primitive_opt::<Float32Type>() {
            pack!(arr);
        } else {
            unimplemented!("{:?} for lowest m/z", arr.data_type())
        }
    }

    fn visit_base_peak_mz(&mut self, spec_arr: &StructArray, index: usize) {
        let arr = spec_arr.column(index);
        if arr.null_count() == arr.len() {
            return;
        }

        macro_rules! pack {
            ($arr:ident) => {
                for (i, descr) in self.iter_instances()
                {
                    if $arr.is_null(i) {
                        continue;
                    };
                    let p = mzdata::Param::builder()
                        .curie(mzdata::curie!(MS:1000504))
                        .name("base peak m/z")
                        .unit(Unit::MZ)
                        .value($arr.value(i))
                        .build();
                    descr.add_param(p);
                }
            };
        }

        if let Some(arr) = arr.as_primitive_opt::<Float64Type>() {
            pack!(arr);
        } else if let Some(arr) = arr.as_primitive_opt::<Float32Type>() {
            pack!(arr);
        } else {
            unimplemented!("{:?} for lowest m/z", arr.data_type())
        }
    }

    fn visit_base_peak_intensity(
        &mut self,
        spec_arr: &StructArray,
        index: usize,
        metacol: &MetadataColumn,
    ) {
        let unit = extract_unit!(metacol, spec_arr);

        macro_rules! extract {
            ($arr:expr) => {
                for (i, descr) in self.offsets.iter().copied().zip(self.descriptions.iter_mut()) {
                    if $arr.is_null(i) { continue };
                    let p = mzdata::Param::builder()
                        .curie(mzdata::curie!(MS:1000505))
                        .name("base peak intensity")
                        .unit(unit.value(i))
                        .value($arr.value(i))
                        .build();
                    descr.add_param(p);
                }
            };
        }

        let arr = spec_arr.column(index);
        if arr.null_count() == arr.len() {
            return;
        }
        if let Some(arr) = arr.as_primitive_opt::<Float32Type>() {
            extract!(arr);
        } else if let Some(arr) = arr.as_primitive_opt::<Float64Type>() {
            extract!(arr);
        } else if let Some(arr) = arr.as_primitive_opt::<Int32Type>() {
            extract!(arr);
        } else {
            unimplemented!("{:?} not supported for {metacol:?}", arr.data_type())
        }
    }

    fn visit_total_ion_current(
        &mut self,
        spec_arr: &StructArray,
        index: usize,
        metacol: &MetadataColumn,
    ) {
        let unit = extract_unit!(metacol, spec_arr);
        macro_rules! extract {
            ($arr:expr) => {
                for (i, descr) in self.offsets.iter().copied().zip(self.descriptions.iter_mut()) {
                    if $arr.is_null(i) { continue };
                    let p = mzdata::Param::builder()
                        .curie(mzdata::curie!(MS:1000285))
                        .name("total ion current")
                        .unit(unit.value(i))
                        .value($arr.value(i))
                        .build();
                    descr.add_param(p);
                }
            };
        }

        let arr = spec_arr.column(index);

        if let Some(arr) = arr.as_primitive_opt::<Float32Type>() {
            extract!(arr);
        } else if let Some(arr) = arr.as_primitive_opt::<Float64Type>() {
            extract!(arr);
        } else if let Some(arr) = arr.as_primitive_opt::<Int32Type>() {
            extract!(arr);
        } else {
            unimplemented!("{:?} not supported for {metacol:?}", arr.data_type())
        }
    }

    pub fn visit(&mut self, spec_arr: &StructArray) -> usize {
        let names = spec_arr.column_names();
        let mut visited = vec![false; spec_arr.num_columns()];

        // Must visit the index first, to infer null spacing
        if let Some(i) = names.iter().position(|v| *v == "index") {
            self.visit_index(spec_arr, i);
            visited[i] = true;
        } else {
            log::error!("Spectrum arrays did not contain \"index\" column");
            panic!("Spectrum arrays did not contain \"index\" column: {names:?}");
        }

        if let Some(i) = names.iter().position(|v| *v == "id") {
            self.visit_id(spec_arr, i);
            visited[i] = true;
        } else {
            log::error!("Spectrum arrays did not contain \"id\" column");
        }
        const SKIP_PARAMS: [CURIE; 6] = [
            curie!(MS:1000505),
            curie!(MS:1000504),
            curie!(MS:1000257),
            curie!(MS:1000285),
            curie!(MS:1000527),
            curie!(MS:1000528),
        ];

        for (_, (index, colname)) in visited
            .iter()
            .copied()
            .zip(names.into_iter().enumerate())
            .filter(|(seen, _)| !seen)
        {
            log::trace!("Visiting spectrum {colname} ({index})");

            if let Some(colspec) = self.metadata_map().traverse::<&str>(&[], colname) {
                if let Some(accession) = colspec.accession {
                    match accession {
                        curie!(MS:1000511) => self.visit_ms_level(spec_arr, index),
                        curie!(MS:1000465) => self.visit_polarity(spec_arr, index),
                        curie!(MS:1000525) => self.visit_mz_signal_continuity(spec_arr, index),
                        curie!(MS:1000559) => self.visit_spectrum_type(spec_arr, index),
                        curie!(MS:1000528) => self.visit_lowest_mz(spec_arr, index),
                        curie!(MS:1000527) => self.visit_highest_mz(spec_arr, index),
                        curie!(MS:1000504) => self.visit_base_peak_mz(spec_arr, index),
                        curie!(MS:1000505) => {
                            self.visit_base_peak_intensity(spec_arr, index, colspec)
                        }
                        curie!(MS:1000285) => {
                            self.visit_total_ion_current(spec_arr, index, colspec)
                        }
                        curie!(MS:1003821) | curie!(MS:1003820) => {} // coordinate spacing model
                        curie!(MS:1003060) | curie!(MS:1003059) => {} // number of data points | number of peak
                        _ => self.visit_as_param(spec_arr, index, Some(colspec), None),
                    }
                } else {
                    self.visit_as_param(spec_arr, index, Some(colspec), None);
                }
            } else {
                match colname {
                    "time"
                    | "auxiliary_arrays"
                    | "number_of_auxiliary_arrays"
                    | "mz_delta_model"
                    | "coordinate_spacing_model" => {}
                    "polarity" => self.visit_polarity(spec_arr, index),
                    "spectrum_type" => self.visit_spectrum_type(spec_arr, index),
                    "mz_signal_continuity" => self.visit_mz_signal_continuity(spec_arr, index),
                    "ms_level" => self.visit_ms_level(spec_arr, index),
                    "lowest_observed_mz" => self.visit_lowest_mz(spec_arr, index),
                    "highest_observed_mz" => self.visit_highest_mz(spec_arr, index),
                    "number_of_data_points" | "number_of_peaks" => {}
                    "base_peak_mz" => self.visit_base_peak_mz(spec_arr, index),
                    "base_peak_intensity" => self.visit_base_peak_intensity(
                        spec_arr,
                        index,
                        &MetadataColumn::named("", Unit::DetectorCounts),
                    ),
                    "total_ion_current" => {
                        self.visit_total_ion_current(
                            spec_arr,
                            index,
                            &MetadataColumn::named("", Unit::DetectorCounts),
                        );
                    }
                    "parameters" => {
                        self.visit_parameters(spec_arr, &SKIP_PARAMS);
                    }
                    "data_processing_ref" | "data_processing_id" => {} // TODO: Add a slot for this in the `mzdata` data model
                    _ => {
                        log::trace!("Visited unspecified column {colname}");
                        // let metacol = MetadataColumn::new(
                        //     colname.to_string(),
                        //     vec![colname.to_string()],
                        //     None,
                        // );
                        self.visit_as_param(spec_arr, index, None, Some(&colname));
                    }
                }
            }
        }

        self.offsets.len()
    }
}

pub struct MzScanVisitor<'a> {
    pub(crate) descriptions: &'a mut [Indexed<ScanEvent>],
    pub(crate) metadata_map: &'a MetadataMapping,
    pub(crate) base_offset: usize,
    pub(crate) offsets: Vec<usize>,
}

impl<'a> VisitorBuilderBase<'a, Indexed<ScanEvent>> for MzScanVisitor<'a> {
    fn iter_instances(&mut self) -> OffsetCollection<'_, Indexed<ScanEvent>> {
        OffsetCollection::new(self.descriptions, &self.offsets)
    }

    fn metadata_map(&self) -> &'a MetadataMapping {
        self.metadata_map
    }
}

#[derive(Debug, Clone, Copy, Default)]
struct ScanWindowSchema {
    lower_limit: Option<usize>,
    upper_limit: Option<usize>,
    #[allow(unused)]
    unit: Option<CURIE>,
    parameters: Option<usize>,
}

impl ScanWindowSchema {
    fn has_window(&self) -> bool {
        self.lower_limit.is_some() && self.upper_limit.is_some()
    }

    fn limit_arrays<'a>(
        &self,
        scan_window_array: &'a StructArray,
    ) -> Option<(&'a std::sync::Arc<dyn Array>, &'a std::sync::Arc<dyn Array>)> {
        self.has_window().then(|| {
            let lower_limit_arr = scan_window_array.column(self.lower_limit.unwrap());
            let upper_limit_arr = scan_window_array.column(self.upper_limit.unwrap());
            (lower_limit_arr, upper_limit_arr)
        })
    }
}

impl<'a> MzScanVisitor<'a> {
    pub fn new(
        descriptions: &'a mut [Indexed<ScanEvent>],
        metadata_map: &'a MetadataMapping,
        base_offset: usize,
        offsets: Vec<usize>,
    ) -> Self {
        Self {
            descriptions,
            metadata_map,
            base_offset,
            offsets,
        }
    }

    fn visit_index(&mut self, spec_arr: &StructArray, index: usize) {
        visit_source_index!(self, spec_arr, index);
    }

    fn visit_instrument_configuration_ref(&mut self, spec_arr: &StructArray, index: usize) {
        macro_rules! pack {
            ($arr:ident) => {
                for (i, Indexed(_, descr)) in self
                    .offsets
                    .iter()
                    .copied()
                    .zip(self.descriptions.iter_mut())
                {
                    if $arr.is_null(i) {
                        continue;
                    };
                    descr.instrument_configuration_id = $arr.value(i) as u32;
                }
            };
        }

        let arr = spec_arr.column(index);

        if let Some(arr) = arr.as_primitive_opt::<UInt32Type>() {
            pack!(arr);
        } else if let Some(arr) = arr.as_primitive_opt::<UInt64Type>() {
            pack!(arr);
        } else if let Some(arr) = arr.as_primitive_opt::<Int32Type>() {
            pack!(arr);
        } else if let Some(arr) = arr.as_primitive_opt::<Int64Type>() {
            pack!(arr);
        } else {
            unimplemented!("{:?}", arr.data_type());
        }
    }

    fn visit_preset_scan_configuration(&mut self, spec_arr: &StructArray, index: usize) {
        macro_rules! pack {
            ($arr:ident) => {
                for (i, Indexed(_, descr)) in self
                    .offsets
                    .iter()
                    .copied()
                    .zip(self.descriptions.iter_mut())
                {
                    if $arr.is_null(i) {
                        continue;
                    };
                    let p = mzdata::Param::builder()
                        .name("preset scan configuration")
                        .curie(mzdata::curie!(MS:1000616))
                        .value($arr.value(i))
                        .build();
                    descr.add_param(p);
                }
            };
        }
        let arr = spec_arr.column(index);
        if arr.null_count() == arr.len() {
            return;
        }
        if let Some(arr) = arr.as_primitive_opt::<UInt32Type>() {
            pack!(arr);
        } else if let Some(arr) = arr.as_primitive_opt::<UInt64Type>() {
            pack!(arr);
        } else if let Some(arr) = arr.as_primitive_opt::<Int32Type>() {
            pack!(arr);
        } else if let Some(arr) = arr.as_primitive_opt::<Int64Type>() {
            pack!(arr);
        } else {
            unimplemented!("{:?}", arr.data_type());
        }
    }

    fn visit_filter_string(&mut self, spec_arr: &StructArray, index: usize) {
        macro_rules! pack {
            ($arr:ident) => {
                for (i, Indexed(_, descr)) in self
                    .offsets
                    .iter()
                    .copied()
                    .zip(self.descriptions.iter_mut())
                {
                    if $arr.is_null(i) {
                        continue;
                    };
                    let p = mzdata::Param::builder()
                        .name("filter string")
                        .curie(mzdata::curie!(MS:1000512))
                        .value($arr.value(i))
                        .build();
                    descr.add_param(p);
                }
            };
        }

        let arr = spec_arr.column(index);
        if arr.null_count() == arr.len() {
            return;
        }
        if let Some(arr) = arr.as_string_opt::<i32>() {
            pack!(arr);
        } else if let Some(arr) = arr.as_string_opt::<i64>() {
            pack!(arr);
        } else {
            unimplemented!("{:?}", arr.data_type());
        }
    }

    fn visit_scan_start_time(&mut self, spec_arr: &StructArray, index: usize, unit: Option<Unit>) {
        let arr = spec_arr.column(index);
        if arr.null_count() == arr.len() {
            return;
        }
        let unit = unit.unwrap_or(Unit::Minute);
        let scalar = match unit {
            Unit::Minute => 1.0,
            Unit::Second => 1.0 / 60.0,
            Unit::Millisecond => 1.0 / (60.0 * 1000.0),
            _ => {
                log::error!("A unit {unit} other than a time unit provided, defaulting to minutes");
                1.0
            }
        };
        macro_rules! pack {
            ($arr:ident) => {
                for (i, Indexed(_, descr)) in self
                    .offsets
                    .iter()
                    .copied()
                    .zip(self.descriptions.iter_mut())
                {
                    if $arr.is_null(i) {
                        continue;
                    };
                    descr.start_time = $arr.value(i) as f64 * scalar;
                }
            };
        }
        if let Some(arr) = arr.as_primitive_opt::<Float32Type>() {
            pack!(arr);
        } else if let Some(arr) = arr.as_primitive_opt::<Float64Type>() {
            pack!(arr);
        } else {
            todo!(
                "{:?} for {index} in {:?}",
                arr.data_type(),
                spec_arr.fields()
            );
        }
    }

    fn visit_injection_time(&mut self, spec_arr: &StructArray, index: usize) {
        let arr = spec_arr.column(index);
        if arr.null_count() == arr.len() {
            return;
        }
        macro_rules! pack {
            ($arr:ident) => {
                for (i, Indexed(_, descr)) in self
                    .offsets
                    .iter()
                    .copied()
                    .zip(self.descriptions.iter_mut())
                {
                    if $arr.is_null(i) {
                        continue;
                    };
                    descr.injection_time = $arr.value(i) as f32;
                }
            };
        }
        if let Some(arr) = arr.as_primitive_opt::<Float32Type>() {
            pack!(arr);
        } else if let Some(arr) = arr.as_primitive_opt::<Float64Type>() {
            pack!(arr);
        } else {
            todo!("{:?}", arr.data_type());
        }
    }

    fn visit_scan_windows_inner(
        scan_window_array: &StructArray,
        spec: Option<&ScanWindowSchema>,
    ) -> Vec<ScanWindow> {
        let (lower_limit_arr, upper_limit_arr) = if let Some(spec) = spec {
            if let Some((lower_limit_arr, upper_limit_arr)) = spec.limit_arrays(scan_window_array) {
                (lower_limit_arr, upper_limit_arr)
            } else {
                let lower_limit_arr = scan_window_array.column(0);
                let upper_limit_arr = scan_window_array.column(1);
                (lower_limit_arr, upper_limit_arr)
            }
        } else {
            let lower_limit_arr = scan_window_array.column(0);
            let upper_limit_arr = scan_window_array.column(1);
            (lower_limit_arr, upper_limit_arr)
        };
        let n = lower_limit_arr.len();
        let mut windows: Vec<ScanWindow> = Vec::with_capacity(n);
        windows.resize(n, Default::default());

        macro_rules! pack {
            ($dtype:ty) => {
                let lower_limit_arr = lower_limit_arr.as_primitive::<$dtype>();
                let upper_limit_arr = upper_limit_arr.as_primitive::<$dtype>();
                for (i, window) in windows.iter_mut().enumerate() {
                    window.lower_bound = lower_limit_arr.value(i) as f32;
                    window.upper_bound = upper_limit_arr.value(i) as f32;
                }
            };
        }

        if matches!(lower_limit_arr.data_type(), DataType::Float32) {
            pack!(Float32Type);
        } else if matches!(lower_limit_arr.data_type(), DataType::Float64) {
            pack!(Float64Type);
        } else {
            todo!("{:?} is not implemented", lower_limit_arr.data_type());
        }
        windows
    }

    fn visit_scan_windows(&mut self, spec_arr: &StructArray, index: usize) {
        let arr = spec_arr.column(index);
        macro_rules! pack {
            ($arr:ident, $spec:expr) => {
                for (i, Indexed(_, descr)) in self
                    .offsets
                    .iter()
                    .copied()
                    .zip(self.descriptions.iter_mut())
                {
                    let windows = $arr.value(i);
                    let windows = windows.as_struct();
                    descr.scan_windows = Self::visit_scan_windows_inner(windows, $spec);
                }
            };
        }

        let mut schema = ScanWindowSchema::default();
        macro_rules! window_schema_from {
            ($fields:expr, $node:expr) => {
                for (i, f) in $fields.iter().enumerate() {
                    if let Some(col) = $node.traverse::<&str>(&[], f.name()) {
                        match col.accession {
                            Some(curie!(MS:1000501)) => {
                                schema.lower_limit = Some(i);
                            }
                            Some(curie!(MS:1000500)) => {
                                schema.upper_limit = Some(i);
                            }
                            _ => {}
                        }
                    } else {
                        match f.name().as_str() {
                            "parameters" => {
                                schema.parameters = Some(i);
                            }
                            _ => {}
                        }
                    }
                }
            };
        }

        if let Some(node) = self.metadata_map.member("scan_windows") {
            if let Some(arr) = arr.as_list_opt::<i64>() {
                if let DataType::Struct(fields) = arr.value_type() {
                    window_schema_from!(fields, node);
                }
                pack!(arr, Some(&schema));
            } else if let Some(arr) = arr.as_list_opt::<i32>() {
                if let DataType::Struct(fields) = arr.value_type() {
                    window_schema_from!(fields, node);
                }
                pack!(arr, Some(&schema));
            } else {
                unimplemented!("{:?}", arr.data_type())
            }
        }
    }

    fn visit_ion_mobility(
        &mut self,
        spec_arr: &StructArray,
        ion_mobility_value_index: usize,
        ion_mobility_type_index: usize,
    ) {
        let arr = spec_arr.column(ion_mobility_value_index);
        if arr.null_count() == arr.len() {
            return;
        }
        let ion_mobility_types = spec_arr.column(ion_mobility_type_index);
        let ion_mobility_types = AnyCURIEArray::try_from(ion_mobility_types).unwrap();

        macro_rules! pack {
            ($arr:ident) => {
                        for (i, Indexed(_, descr)) in self
            .offsets
            .iter()
            .copied()
            .zip(self.descriptions.iter_mut())
        {
            if $arr.is_null(i) {
                continue;
            };
            let im_val = $arr.value(i);
            let im_tp = ion_mobility_types.value(i).unwrap();
            match im_tp {
                // ion mobility drift time
                curie!(MS:1002476) => descr.add_param(
                    mzdata::Param::builder()
                        .name("ion mobility drift time")
                        .curie(mzdata::curie!(MS:1002476))
                        .value(im_val)
                        .unit(Unit::Millisecond)
                        .build(),
                ),
                // inverse reduced ion mobility drift time
                curie!(MS:1002815) => {
                    descr.add_param(
                        mzdata::Param::builder()
                            .name("inverse reduced ion mobility drift time")
                            .curie(mzdata::curie!(MS:1002815))
                            .value(im_val)
                            .unit(Unit::VoltSecondPerSquareCentimeter)
                            .build()
                    )
                }
                // FAIMS compensation voltage
                curie!(MS:1001581) => {
                    descr.add_param(
                        mzdata::Param::builder()
                            .name("FAIMS compensation voltage")
                            .curie(mzdata::curie!(MS:1001581))
                            .value(im_val)
                            .unit(Unit::Volt)
                            .build()
                    )
                }
                // SELEXION compensation voltage
                curie!(MS:1003371) => {
                    descr.add_param(
                        mzdata::Param::builder()
                            .name("SELEXION compensation voltage")
                            .curie(mzdata::curie!(MS:1003371))
                            .value(im_val)
                            .unit(Unit::Volt)
                            .build()
                    )
                }
                _ => todo!("{im_tp} not supported yet"),
            }
        }
            };
        }

        if let Some(arr) = arr.as_primitive_opt::<Float64Type>() {
            pack!(arr);
        } else if let Some(arr) = arr.as_primitive_opt::<Float32Type>() {
            pack!(arr);
        } else {
            todo!("{:?} not supported for ion mobility", arr.data_type());
        }
    }

    fn visit_spectrum_reference(&mut self, spec_arr: &StructArray, index: usize) {
        let arr = spec_arr.column(index);
        if arr.null_count() == arr.len() {
            return;
        }
        macro_rules! pack {
            ($arr:ident) => {
                for (i, Indexed(_, descr)) in self
                    .offsets
                    .iter()
                    .copied()
                    .zip(self.descriptions.iter_mut())
                {
                    if $arr.is_null(i) {
                        continue;
                    };
                    descr.spectrum_reference = Some($arr.value(i).into());
                }
            };
        }
        if let Some(arr) = arr.as_string_opt::<i32>() {
            pack!(arr);
        } else if let Some(arr) = arr.as_string_opt::<i64>() {
            pack!(arr);
        } else {
            todo!("{:?}", arr.data_type());
        }
    }

    pub fn visit(&mut self, spec_arr: &StructArray) {
        let names = spec_arr.column_names();
        let mut visited = vec![false; spec_arr.num_columns()];
        // Must visit the index first, to infer null spacing
        if let Some(i) = names.iter().position(|v| *v == "source_index") {
            self.visit_index(spec_arr, i);
            visited[i] = true;
        } else {
            log::error!("Scan arrays did not contain \"source_index\" column");
            panic!("Scan arrays did not contain \"source_index\" column: {names:?}");
        }

        let mut ion_mobility_value_index = None;
        let mut ion_mobility_type_index = None;

        for (_, (index, colname)) in visited
            .iter()
            .zip(names.into_iter().enumerate())
            .filter(|(seen, _)| !**seen)
        {
            if let Some(colspec) = self.metadata_map().traverse::<&str>(&[], colname) {
                if let Some(accession) = colspec.accession {
                    match accession {
                        curie!(MS:1000512) => self.visit_filter_string(spec_arr, index),
                        curie!(MS:1000616) => self.visit_preset_scan_configuration(spec_arr, index),
                        curie!(MS:1000016) => {
                            let unit = match &colspec.unit {
                                crate::param::PathOrCURIE::Path(_items) => todo!(),
                                crate::param::PathOrCURIE::CURIE(curie) => {
                                    Some(Unit::from_curie(curie))
                                }
                                crate::param::PathOrCURIE::None => None,
                            };
                            self.visit_scan_start_time(spec_arr, index, unit);
                        }
                        curie!(MS:1000927) => self.visit_injection_time(spec_arr, index),
                        _ => self.visit_as_param(spec_arr, index, Some(colspec), None),
                    }
                } else {
                    self.visit_as_param(spec_arr, index, Some(colspec), None);
                }
            } else {
                log::trace!("Visiting scan {colname} ({index})");
                match colname {
                    "parameters" => self.visit_parameters(spec_arr, &[]),
                    "scan_index" => {}
                    "spectrum_reference" => self.visit_spectrum_reference(spec_arr, index),
                    "instrument_configuration_ref" | "instrument_configuration_id" => {
                        self.visit_instrument_configuration_ref(spec_arr, index)
                    }
                    "scan_windows" => self.visit_scan_windows(spec_arr, index),
                    "ion_mobility_value" => ion_mobility_value_index = Some(index),
                    "ion_mobility_type" => ion_mobility_type_index = Some(index),
                    "scan_start_time" => self.visit_scan_start_time(spec_arr, index, None),
                    "injection_time" => self.visit_injection_time(spec_arr, index),
                    "filter_string" => self.visit_filter_string(spec_arr, index),
                    "preset_scan_configuration" => {
                        self.visit_preset_scan_configuration(spec_arr, index)
                    }
                    _ => {
                        log::trace!("Visited unspecified column {colname}");
                        let metacol = MetadataColumn::new(
                            colname.to_string(),
                            vec![colname.to_string()],
                            None,
                        );
                        self.visit_as_param(spec_arr, index, Some(&metacol), Some(colname));
                    }
                }
            }
        }

        if let (Some(ion_mobility_value_index), Some(ion_mobility_type_index)) =
            (ion_mobility_value_index, ion_mobility_type_index)
        {
            self.visit_ion_mobility(spec_arr, ion_mobility_value_index, ion_mobility_type_index);
        }
    }
}


pub struct MzPrecursorVisitor<'a> {
    pub(crate) descriptions: &'a mut [DoubleIndexed<Precursor>],
    pub(crate) metadata_map: &'a MetadataMapping,
    pub(crate) base_offset: usize,
    pub(crate) offsets: Vec<usize>,
}

impl<'a> VisitorBuilderBase<'a, DoubleIndexed<Precursor>> for MzPrecursorVisitor<'a> {
    fn iter_instances(&mut self) -> OffsetCollection<'_, DoubleIndexed<Precursor>> {
        OffsetCollection::new(self.descriptions, &self.offsets)
    }

    fn metadata_map(&self) -> &'a MetadataMapping {
        self.metadata_map
    }
}

impl<'a> VisitorBuilder3<'a, Precursor> for MzPrecursorVisitor<'a> {}

impl<'a> SubGroupVisitor<'a, DoubleIndexed<Precursor>, IsolationWindow> for MzPrecursorVisitor<'a> {
    fn iter_instances_subgroup<'b>(
        &'b mut self,
    ) -> OffsetCollectionSubGroup<'b, DoubleIndexed<Precursor>, IsolationWindow> {
        let base = self.iter_instances();
        let f = |(i, descr): (usize, &'b mut DoubleIndexed<Precursor>)| {
            (i, descr.2.isolation_window_mut())
        };
        let f = Box::new(f);
        OffsetCollectionSubGroup::new(base, f)
    }
}

impl<'a> SubGroupVisitor<'a, DoubleIndexed<Precursor>, Activation> for MzPrecursorVisitor<'a> {
    fn iter_instances_subgroup<'b>(
        &'b mut self,
    ) -> OffsetCollectionSubGroup<'b, DoubleIndexed<Precursor>, Activation> {
        let base = self.iter_instances();
        let f =
            |(i, descr): (usize, &'b mut DoubleIndexed<Precursor>)| (i, descr.2.activation_mut());
        let f = Box::new(f);
        OffsetCollectionSubGroup::new(base, f)
    }
}

impl<'a> MzPrecursorVisitor<'a> {
    pub fn new(
        descriptions: &'a mut [DoubleIndexed<Precursor>],
        metadata_map: &'a MetadataMapping,
        base_offset: usize,
        offsets: Vec<usize>,
    ) -> Self {
        Self {
            descriptions,
            metadata_map,
            base_offset,
            offsets,
        }
    }

    fn visit_source_index(&mut self, spec_arr: &StructArray, index: usize) {
        visit_source_index!(self, spec_arr, index);
    }

    fn visit_precursor_id(&mut self, spec_arr: &StructArray, index: usize) {
        let arr = spec_arr.column(index);
        macro_rules! process {
            ($arr:expr) => {
                for (index, descr) in self.iter_instances() {
                    if $arr.is_null(index) {
                        continue;
                    }
                    descr.description_mut().precursor_id = Some($arr.value(index).to_string());
                }
            };
        }
        if let Some(arr) = arr.as_string_opt::<i64>() {
            process!(arr);
        } else if let Some(arr) = arr.as_string_opt::<i32>() {
            process!(arr);
        } else {
            panic!("Unsupported data type: {:?}", arr.data_type());
        }
    }

    // Isolation window columns

    fn visit_isolation_window_target(&mut self, arr: &ArrayRef, _col: &MetadataColumn) {
        macro_rules! process {
            ($arr:expr) => {
                for (offset, descr) in self.iter_instances() {
                    if $arr.is_null(offset) {
                        continue;
                    }
                    let descr = descr.description_mut();
                    descr.isolation_window.target = $arr.value(offset) as f32;
                    descr.isolation_window.flags = mzdata::spectrum::IsolationWindowState::Explicit;
                }
            };
        }
        if let Some(arr) = arr.as_primitive_opt::<Float32Type>() {
            process!(arr);
        } else if let Some(arr) = arr.as_primitive_opt::<Float64Type>() {
            process!(arr);
        } else {
            panic!("Unsupported data type: {:?}", arr.data_type());
        }
    }

    fn visit_isolation_window_parameters(
        &mut self,
        params_array: &ArrayRef,
        skip_params: &[CURIE],
    ) {
        macro_rules! process {
            ($params_array:expr) => {
                for (i, descr) in self.iter_instances() {
                    let params = $params_array.value(i);
                    let params = ParameterVisitor::new(params.as_struct()).build();

                    for p in params {
                        if let Some(acc) = p.curie() {
                            if !skip_params.contains(&acc) {
                                descr.description_mut().isolation_window.add_param(p);
                            }
                        } else {
                            descr.description_mut().isolation_window.add_param(p);
                        }
                    }
                }
            };
        }
        if let Some(params_array) = params_array.as_list_opt::<i64>() {
            process!(params_array);
        } else if let Some(params_array) = params_array.as_list_opt::<i32>() {
            process!(params_array);
        } else {
            panic!("{:?} not supported", params_array.data_type());
        }
    }

    fn visit_isolation_window_lower_offset(&mut self, arr: &ArrayRef, _col: &MetadataColumn) {
        macro_rules! process {
            ($arr:expr) => {
                for (offset, descr) in self.iter_instances() {
                    if $arr.is_null(offset) {
                        continue;
                    }
                    let descr = descr.description_mut();
                    descr.isolation_window.lower_bound =
                        descr.isolation_window.target - $arr.value(offset) as f32;
                    descr.isolation_window.flags = mzdata::spectrum::IsolationWindowState::Explicit;
                }
            };
        }
        if let Some(arr) = arr.as_primitive_opt::<Float32Type>() {
            process!(arr);
        } else if let Some(arr) = arr.as_primitive_opt::<Float64Type>() {
            process!(arr);
        } else {
            panic!("Unsupported data type: {:?}", arr.data_type());
        }
    }

    fn visit_isolation_window_upper_offset(&mut self, arr: &ArrayRef, _col: &MetadataColumn) {
        macro_rules! process {
            ($arr:expr) => {
                for (offset, descr) in self.iter_instances() {
                    if $arr.is_null(offset) {
                        continue;
                    }
                    let descr = descr.description_mut();
                    descr.isolation_window.upper_bound =
                        descr.isolation_window.target + $arr.value(offset) as f32;
                    descr.isolation_window.flags = mzdata::spectrum::IsolationWindowState::Explicit;
                }
            };
        }
        if let Some(arr) = arr.as_primitive_opt::<Float32Type>() {
            process!(arr);
        } else if let Some(arr) = arr.as_primitive_opt::<Float64Type>() {
            process!(arr);
        } else {
            panic!("Unsupported data type: {:?}", arr.data_type());
        }
    }

    fn visit_isolation_window(&mut self, spec_arr: &StructArray, index: usize) {
        let root = spec_arr.column(index).as_struct();
        let _m = MetadataMapping::default();
        let isolation_window_map = self.metadata_map().member("isolation_window").unwrap_or(&_m);
        // Find the isolation target column first, everything else is relative to it
        for (field, arr) in root.fields().iter().zip(root.columns()) {
            if let Some(col) = isolation_window_map.traverse::<&str>(&[], field.name().as_str()) {
                if let Some(accession) = col.accession {
                    match accession {
                        curie!(MS:1000827) => self.visit_isolation_window_target(arr, col),
                        _ => {}
                    }
                }
            }
        }
        for (field, arr) in root.fields().iter().zip(root.columns()) {
            if let Some(col) = isolation_window_map.traverse::<&str>(&[], field.name().as_str()) {
                if let Some(accession) = col.accession {
                    match accession {
                        curie!(MS:1000828) => self.visit_isolation_window_lower_offset(arr, col),
                        curie!(MS:1000829) => self.visit_isolation_window_upper_offset(arr, col),
                        curie!(MS:1000827) => {}
                        _ => {
                            <Self as SubGroupVisitor<
                                'a,
                                DoubleIndexed<Precursor>,
                                IsolationWindow,
                            >>::visit_as_param_subgroup(
                                &mut *self,
                                arr,
                                Some(col),
                                Some(field.name()),
                            );
                        }
                    }
                } else {
                    <Self as SubGroupVisitor<'a, DoubleIndexed<Precursor>, IsolationWindow>>::visit_as_param_subgroup(
                                &mut *self, arr, Some(col), Some(field.name()));
                }
            } else {
                match field.name().as_str() {
                    "parameters" => {
                        <Self as SubGroupVisitor<
                            'a,
                            DoubleIndexed<Precursor>,
                            IsolationWindow,
                        >>::visit_parameters_subgroup(
                            self, arr, &[]
                        );
                        self.visit_isolation_window_parameters(arr, &[]);
                    }
                    _ => {
                        <Self as SubGroupVisitor<
                            'a,
                            DoubleIndexed<Precursor>,
                            IsolationWindow,
                        >>::visit_as_param_subgroup(
                            self,
                            arr,
                            None,
                            Some(field.name().as_str()),
                        );
                    }
                }
            }
        }
    }

    // Activation columns

    fn visit_activation_energy(&mut self, arr: &ArrayRef, _col: &MetadataColumn) {
        macro_rules! process {
            ($arr:expr) => {
                for (offset, descr) in <Self as SubGroupVisitor<
                    'a,
                    DoubleIndexed<Precursor>,
                    Activation,
                >>::iter_instances_subgroup(self)
                {
                    if $arr.is_null(offset) {
                        continue;
                    }
                    descr.energy = $arr.value(offset) as f32;
                }
            };
        }
        if let Some(arr) = arr.as_primitive_opt::<Float32Type>() {
            process!(arr);
        } else if let Some(arr) = arr.as_primitive_opt::<Float64Type>() {
            process!(arr);
        } else {
            panic!("Unsupported data type: {:?}", arr.data_type());
        }
    }

    fn visit_activation_method(&mut self, arr: &ArrayRef, _col: &MetadataColumn) {
        if let Ok(arr) = AnyCURIEArray::try_from(arr) {
            for (offset, descr) in <Self as SubGroupVisitor<
                'a,
                DoubleIndexed<Precursor>,
                Activation,
            >>::iter_instances_subgroup(self)
            {
                if arr.is_null(offset) {
                    continue;
                }
                let v = arr.value(offset).unwrap();
                if let Some(v) = DissociationMethodTerm::from_curie(&v) {
                    descr.methods_mut().push(v);
                }
            }
        } else {
            panic!("Unsupported data type: {:?} for {_col:?}", arr.data_type());
        }
    }

    fn visit_activation(&mut self, spec_arr: &StructArray, index: usize) {
        let root = spec_arr.column(index).as_struct();
        let _m = MetadataMapping::default();
        let activation_map = self.metadata_map().member("activation").unwrap_or(&_m);
        for (field, arr) in root.fields().iter().zip(root.columns()) {
            if let Some(col) = activation_map.traverse::<&str>(&[], field.name().as_str()) {
                if let Some(accession) = col.accession {
                    match accession {
                        curie!(MS:1000045) => self.visit_activation_energy(arr, col),
                        curie!(MS:1000044) => self.visit_activation_method(arr, col),
                        _ => <Self as SubGroupVisitor<
                            'a,
                            DoubleIndexed<Precursor>,
                            Activation,
                        >>::visit_as_param_subgroup(
                            self, arr, Some(col), Some(field.name())
                        ),
                    }
                } else {
                    <Self as SubGroupVisitor<'a, DoubleIndexed<Precursor>, Activation>>::visit_as_param_subgroup(
                        self, arr, None, Some(field.name())
                    );
                }
            } else {
                match field.name().as_str() {
                    "parameters" => {
                        <Self as SubGroupVisitor<'a, DoubleIndexed<Precursor>, Activation>>::visit_parameters_subgroup(
                            self, arr, &[]);
                        self.visit_isolation_window_parameters(arr, &[]);
                    }
                    _ => <Self as SubGroupVisitor<'a, DoubleIndexed<Precursor>, Activation>>::visit_as_param_subgroup(
                        self, arr, None, Some(field.name())),
                }
            }
        }
    }

    pub fn visit(&mut self, spec_arr: &StructArray) {
        let names = spec_arr.column_names();
        let mut visited = vec![false; spec_arr.num_columns()];

        // Must visit the index first, to infer null spacing
        if let Some(i) = names.iter().position(|v| *v == "source_index") {
            self.visit_source_index(spec_arr, i);
            visited[i] = true;
        } else {
            log::error!("Precursor arrays did not contain \"source_index\" column");
            panic!("Precursor arrays missing \"source_index\" column: {names:?}");
        }

        if let Some(i) = names.iter().position(|v| *v == "precursor_index") {
            self.visit_precursor_index(spec_arr, i);
            visited[i] = true;
        } else {
            log::error!("Precursor arrays did not contain \"precursor_index\" column");
        }

        for (_, (index, colname)) in visited
            .into_iter()
            .zip(names.into_iter().enumerate())
            .filter(|(seen, _)| !seen)
        {
            log::trace!("Visiting precursor {colname} ({index})");
            match colname {
                "activation" => self.visit_activation(spec_arr, index),
                "isolation_window" => self.visit_isolation_window(spec_arr, index),
                "precursor_id" => self.visit_precursor_id(spec_arr, index),
                _ => {
                    // self.visit_as_param(spec_arr, index, None, Some(colname));
                }
            }
        }
    }
}

pub struct MzSelectedIonVisitor<'a> {
    pub(crate) descriptions: &'a mut [DoubleIndexed<SelectedIon>],
    pub(crate) metadata_map: &'a MetadataMapping,
    pub(crate) base_offset: usize,
    pub(crate) offsets: Vec<usize>,
}

impl<'a> VisitorBuilderBase<'a, DoubleIndexed<SelectedIon>> for MzSelectedIonVisitor<'a> {
    fn iter_instances(&mut self) -> OffsetCollection<'_, DoubleIndexed<SelectedIon>> {
        OffsetCollection::new(self.descriptions, &self.offsets)
    }

    fn metadata_map(&self) -> &'a MetadataMapping {
        self.metadata_map
    }
}

impl<'a> VisitorBuilder3<'a, SelectedIon> for MzSelectedIonVisitor<'a> {}

impl<'a> MzSelectedIonVisitor<'a> {
    pub fn new(
        descriptions: &'a mut [DoubleIndexed<SelectedIon>],
        metadata_map: &'a MetadataMapping,
        base_offset: usize,
        offsets: Vec<usize>,
    ) -> Self {
        Self {
            descriptions,
            metadata_map,
            base_offset,
            offsets,
        }
    }

    fn visit_source_index(&mut self, spec_arr: &StructArray, index: usize) {
        visit_source_index!(self, spec_arr, index);
    }

    fn visit_selected_ion_mz(&mut self, spec_arr: &StructArray, index: usize) {
        let arr = spec_arr.column(index);
        macro_rules! pack {
            ($arr:ident) => {
                for (i, descr) in self.iter_instances() {
                    if $arr.is_null(i) {
                        continue;
                    };
                    let descr = descr.description_mut();
                    descr.mz = $arr.value(i) as f64;
                }
            };
        }
        if let Some(arr) = arr.as_primitive_opt::<Float32Type>() {
            pack!(arr);
        } else if let Some(arr) = arr.as_primitive_opt::<Float64Type>() {
            pack!(arr);
        } else {
            todo!("{:?}", arr.data_type());
        }
    }

    fn visit_peak_intensity(&mut self, spec_arr: &StructArray, index: usize) {
        let arr = spec_arr.column(index);
        if arr.null_count() == arr.len() {
            return;
        }
        macro_rules! pack {
            ($arr:ident) => {
                for (i, descr) in self
                    .offsets
                    .iter()
                    .copied()
                    .zip(self.descriptions.iter_mut())
                {
                    if $arr.is_null(i) {
                        continue;
                    };
                    let descr = descr.description_mut();
                    descr.intensity = $arr.value(i) as f32;
                }
            };
        }
        if let Some(arr) = arr.as_primitive_opt::<Float32Type>() {
            pack!(arr);
        } else if let Some(arr) = arr.as_primitive_opt::<Float64Type>() {
            pack!(arr);
        } else {
            todo!("{:?}", arr.data_type());
        }
    }

    fn visit_charge(&mut self, spec_arr: &StructArray, index: usize) {
        let arr = spec_arr.column(index);
        if arr.null_count() == arr.len() {
            return;
        }
        macro_rules! pack {
            ($arr:ident) => {
                for (i, descr) in self
                    .offsets
                    .iter()
                    .copied()
                    .zip(self.descriptions.iter_mut())
                {
                    let descr = descr.description_mut();
                    if $arr.is_null(i) {
                        descr.charge = None;
                    } else {
                        descr.charge = Some($arr.value(i) as i32);
                    }
                }
            };
        }
        if let Some(arr) = arr.as_primitive_opt::<Float32Type>() {
            pack!(arr);
        } else if let Some(arr) = arr.as_primitive_opt::<Float64Type>() {
            pack!(arr);
        } else if let Some(arr) = arr.as_primitive_opt::<Int32Type>() {
            pack!(arr);
        } else if let Some(arr) = arr.as_primitive_opt::<Int64Type>() {
            pack!(arr);
        } else if let Some(arr) = arr.as_primitive_opt::<Int32Type>() {
            pack!(arr);
        } else if let Some(arr) = arr.as_primitive_opt::<UInt32Type>() {
            pack!(arr);
        } else if let Some(arr) = arr.as_primitive_opt::<UInt64Type>() {
            pack!(arr);
        } else {
            todo!("{:?}", arr.data_type());
        }
    }

    fn visit_ion_mobility(
        &mut self,
        spec_arr: &StructArray,
        ion_mobility_value_index: usize,
        ion_mobility_type_index: usize,
    ) {
        let arr = spec_arr.column(ion_mobility_value_index);
        if arr.null_count() == arr.len() {
            return;
        }

        let ion_mobility_types = spec_arr.column(ion_mobility_type_index);
        let ion_mobility_types = AnyCURIEArray::try_from(ion_mobility_types).unwrap();

        macro_rules! pack {
            ($arr:ident) => {
                        for (i, descr) in self
            .offsets
            .iter()
            .copied()
            .zip(self.descriptions.iter_mut())
        {
            if $arr.is_null(i) {
                continue;
            };
            let descr = descr.description_mut();
            let im_val = $arr.value(i);
            let im_tp = ion_mobility_types.value(i).unwrap();
            match im_tp {
                // ion mobility drift time
                curie!(MS:1002476) => descr.add_param(
                    mzdata::Param::builder()
                        .name("ion mobility drift time")
                        .curie(mzdata::curie!(MS:1002476))
                        .value(im_val)
                        .unit(Unit::Millisecond)
                        .build(),
                ),
                // inverse reduced ion mobility drift time
                curie!(MS:1002815) => {
                    descr.add_param(
                        mzdata::Param::builder()
                            .name("inverse reduced ion mobility drift time")
                            .curie(mzdata::curie!(MS:1002815))
                            .value(im_val)
                            .unit(Unit::VoltSecondPerSquareCentimeter)
                            .build()
                    )
                }
                // FAIMS compensation voltage
                curie!(MS:1001581) => {
                    descr.add_param(
                        mzdata::Param::builder()
                            .name("FAIMS compensation voltage")
                            .curie(mzdata::curie!(MS:1001581))
                            .value(im_val)
                            .unit(Unit::Volt)
                            .build()
                    )
                }
                // SELEXION compensation voltage
                curie!(MS:1003371) => {
                    descr.add_param(
                        mzdata::Param::builder()
                            .name("SELEXION compensation voltage")
                            .curie(mzdata::curie!(MS:1003371))
                            .value(im_val)
                            .unit(Unit::Volt)
                            .build()
                    )
                }
                _ => todo!("{im_tp} not supported yet"),
            }
        }
            };
        }

        if let Some(arr) = arr.as_primitive_opt::<Float64Type>() {
            pack!(arr);
        } else if let Some(arr) = arr.as_primitive_opt::<Float32Type>() {
            pack!(arr);
        } else {
            todo!("{:?} not supported for ion mobility", arr.data_type());
        }
    }

    pub fn visit(&mut self, spec_arr: &StructArray) {
        let names = spec_arr.column_names();
        let mut visited = vec![false; spec_arr.num_columns()];

        // Must visit the index first, to infer null spacing
        if let Some(i) = names.iter().position(|v| *v == "source_index") {
            self.visit_source_index(spec_arr, i);
            visited[i] = true;
        } else {
            log::error!("Precursor arrays did not contain \"source_index\" column");
            panic!("Precursor arrays did not contain \"source_index\" column: {names:?}");
        }

        if let Some(i) = names.iter().position(|v| *v == "precursor_index") {
            self.visit_precursor_index(spec_arr, i);
            visited[i] = true;
        } else {
            log::error!("Precursor arrays did not contain \"precursor_index\" column");
        }

        let mut ion_mobility_value_index = None;
        let mut ion_mobility_type_index = None;

        for (_, (index, colname)) in visited
            .into_iter()
            .zip(names.into_iter().enumerate())
            .filter(|(seen, _)| !seen)
        {
            if let Some(col) = self.metadata_map().traverse::<&str>(&[], colname) {
                if let Some(accession) = col.accession {
                    match accession {
                        curie!(MS:1000744) => self.visit_selected_ion_mz(spec_arr, index),
                        curie!(MS:1000041) => self.visit_charge(spec_arr, index),
                        curie!(MS:1000042) => self.visit_peak_intensity(spec_arr, index),
                        _ => self.visit_as_param(spec_arr, index, Some(col), None),
                    }
                } else {
                    panic!("undefined selected ion {col:?}")
                }
            } else {
                log::trace!("Visiting selected ion {colname} ({index})");
                match colname {
                    "parameters" => self.visit_parameters(spec_arr, &[]),
                    "ion_mobility_value" => ion_mobility_value_index = Some(index),
                    "ion_mobility_type" => ion_mobility_type_index = Some(index),
                    "selected_ion_mz" => self.visit_selected_ion_mz(spec_arr, index),
                    "charge_state" => self.visit_charge(spec_arr, index),
                    "intensity" => self.visit_peak_intensity(spec_arr, index),
                    "selected_ion_index" => {}
                    _ => {
                        log::trace!("Visited unspecified column {colname}");
                        self.visit_as_param(spec_arr, index, None, Some(colname));
                    }
                }
            }
        }

        if let (Some(ion_mobility_value_index), Some(ion_mobility_type_index)) =
            (ion_mobility_value_index, ion_mobility_type_index)
        {
            self.visit_ion_mobility(spec_arr, ion_mobility_value_index, ion_mobility_type_index);
        }
    }
}

pub struct MzChromatogramBuilder<'a> {
    pub(crate) descriptions: &'a mut [ChromatogramDescription],
    pub(crate) metadata_map: &'a MetadataMapping,
    pub(crate) base_offset: usize,
    pub(crate) offsets: Vec<usize>,
}

impl<'a> VisitorBuilderBase<'a, ChromatogramDescription> for MzChromatogramBuilder<'a> {
    fn iter_instances(&mut self) -> OffsetCollection<'_, ChromatogramDescription> {
        OffsetCollection::new(self.descriptions, &self.offsets)
    }

    fn metadata_map(&self) -> &'a MetadataMapping {
        self.metadata_map
    }
}

impl<'a> MzChromatogramBuilder<'a> {
    pub fn new(
        descriptions: &'a mut [ChromatogramDescription],
        metadata_map: &'a MetadataMapping,
        base_offset: usize,
    ) -> Self {
        Self {
            descriptions,
            metadata_map,
            base_offset,
            offsets: Vec::new(),
        }
    }

    fn visit_index(&mut self, chrom_arr: &StructArray, index: usize) {
        let arr = chrom_arr.column(index).as_primitive::<UInt64Type>();
        let mut offsets = Vec::with_capacity(self.descriptions.len());
        let mut j = 0;
        for (i, descr) in self.descriptions.iter_mut().enumerate() {
            while arr.is_null(self.base_offset + i + j) {
                j += 1;
            }
            let val = arr.value(self.base_offset + i + j);
            offsets.push(self.base_offset + i + j);
            descr.index = val as usize;
        }
        self.offsets = offsets
    }

    fn visit_id(&mut self, chrom_arr: &StructArray, index: usize) {
        macro_rules! process {
            ($idx_type:ty) => {
                if let Some(arr) = chrom_arr.column(index).as_string_opt::<$idx_type>() {
                    for (i, descr) in self
                        .offsets
                        .iter()
                        .copied()
                        .zip(self.descriptions.iter_mut())
                    {
                        let val = arr.value(i);
                        descr.id = val.to_string();
                    }
                    return;
                }
            };
        }
        process!(i64);
        process!(i32);
        panic!(
            "Unsupported data type {:?}",
            chrom_arr.column(index).data_type()
        );
    }

    fn visit_polarity(&mut self, chrom_arr: &StructArray, index: usize) {
        let arr = chrom_arr.column(index);
        macro_rules! downcast_run {
            ($($tp:ty)+) => {
                $(
                    if let Some(array) = arr.as_primitive_opt::<$tp>() {
                        for (i, descr) in self
                            .offsets
                            .iter()
                            .copied()
                            .zip(self.descriptions.iter_mut())
                        {
                            let polarity_val = array.value(i);
                            match polarity_val {
                                1 => descr.polarity = ScanPolarity::Positive,
                                -1 => descr.polarity = ScanPolarity::Negative,
                                _ => {
                                    descr.polarity = ScanPolarity::Unknown;
                                }
                            }
                        }
                        return;
                    }
                )+
            };
        }

        downcast_run!(
            Int8Type
            Int32Type
            Int32Type
            Int64Type
        );
    }

    fn visit_chromatogram_type(&mut self, spec_arr: &StructArray, index: usize) {
        let spec_type_array = AnyCURIEArray::try_from(spec_arr.column(index)).unwrap();
        for (i, descr) in self
            .offsets
            .iter()
            .copied()
            .zip(self.descriptions.iter_mut())
        {
            let chromatogram_type_curie = spec_type_array.value(i).unwrap();
            let chromatogram_type =
                ChromatogramType::from_accession(chromatogram_type_curie.accession);
            if let Some(chromatogram_type) = chromatogram_type {
                descr.chromatogram_type = chromatogram_type;
            }
        }
    }

    pub fn visit(&mut self, chrom_arr: &StructArray) -> usize {
        // Must visit the index first, to infer null spacing
        self.visit_index(chrom_arr, 0);
        self.visit_id(chrom_arr, 1);

        let names = chrom_arr.column_names();
        let mut visited = vec![false; chrom_arr.num_columns()];
        visited[0] = true;
        visited[1] = true;

        for (_, (index, colname)) in visited
            .into_iter()
            .zip(names.into_iter().enumerate())
            .filter(|(seen, _)| !seen)
        {
            if let Some(col) = self.metadata_map().traverse::<&str>(&[], colname) {
                if let Some(accession) = col.accession {
                    match accession {
                        curie!(MS:1000465) => self.visit_polarity(chrom_arr, index),
                        curie!(MS:1000626) => self.visit_chromatogram_type(chrom_arr, index), // chromatogram type
                        curie!(MS:1003060) => {} // number of data points
                        _ => self.visit_as_param(chrom_arr, index, Some(col), None),
                    }
                } else {
                    self.visit_as_param(chrom_arr, index, Some(col), None);
                }
            } else {
                match colname {
                    "number_of_auxiliary_arrays" | "auxiliary_arrays" => {}
                    "polarity" => self.visit_polarity(chrom_arr, index),
                    "chromatogram_type" => self.visit_chromatogram_type(chrom_arr, index),
                    "data_processing_ref" | "data_processing_id" => {}
                    "parameters" => {
                        self.visit_parameters(chrom_arr, &[]);
                    }
                    _ => {
                        log::trace!("Visited unspecified column {colname}");
                        self.visit_as_param(chrom_arr, index, None, Some(colname));
                    }
                }
            }
        }
        self.offsets.len()
    }
}

#[derive(Default, Debug)]
pub struct AuxiliaryArrayVisitor(Vec<DataArray>);

impl AuxiliaryArrayVisitor {
    fn visit_data(&mut self, index: usize, arrays: &StructArray) {
        let arr = arrays.column(index);
        let arr = arr.as_list::<i64>();
        for (i, item) in arr.iter().enumerate() {
            if let Some(item) = item {
                self.0[i].data = item.as_primitive::<UInt8Type>().values().to_vec();
            }
        }
    }

    fn visit_name(&mut self, index: usize, arrays: &StructArray) {
        let arr = arrays.column(index);
        let visitor = ParameterVisitor::new(arr.as_struct());
        let params = visitor.build();

        for (i, param) in params.into_iter().enumerate() {
            let accession = param.curie().unwrap();
            let val = ArrayType::from_accession(accession).unwrap();
            if matches!(val, ArrayType::NonStandardDataArray { name: _ }) {
                self.0[i].name = ArrayType::nonstandard(param.value.to_string());
            } else {
                self.0[i].name = val;
            }
        }
    }

    fn visit_data_type(&mut self, index: usize, arrays: &StructArray) {
        let arr = arrays.column(index);
        let visitor = AnyCURIEArray::try_from(arr).unwrap();

        for (i, da) in self.0.iter_mut().enumerate() {
            let val = visitor.value(i).unwrap();
            da.dtype = BinaryDataArrayType::from_accession(val).unwrap();
        }
    }

    fn visit_compression(&mut self, index: usize, arrays: &StructArray) {
        let arr = arrays.column(index);
        let visitor = AnyCURIEArray::try_from(arr).unwrap();

        for (i, da) in self.0.iter_mut().enumerate() {
            let val = visitor.value(i).unwrap();
            da.compression = BinaryCompressionType::from_accession(val).unwrap();
            if matches!(da.compression, BinaryCompressionType::NoCompression) {
                da.compression = BinaryCompressionType::Decoded;
            }
        }
    }

    fn visit_unit(&mut self, index: usize, arrays: &StructArray) {
        let arr = arrays.column(index);
        let visitor = UnitArray::from(arr);

        for (i, da) in self.0.iter_mut().enumerate() {
            let val = visitor.value(i);
            da.unit = val;
        }
    }

    fn visit_parameters(&mut self, index: usize, arrays: &StructArray) {
        let arr = arrays.column(index).as_list::<i64>();

        for (i, da) in self.0.iter_mut().enumerate() {
            if arr.is_null(i) {
                continue;
            }
            let val = arr.value(i);
            let val = val.as_struct();
            da.params_mut().extend(ParameterVisitor::new(val).build());
        }
    }

    fn visit_data_processing_id(&mut self, index: usize, arrays: &StructArray) {
        let arr = arrays.column(index);
        let arr = arr.as_string::<i64>();
        for (i, da) in self.0.iter_mut().enumerate() {
            if arr.is_null(i) {
                continue;
            }
            let val: Box<str> = arr.value(i).into();
            da.set_data_processing_reference(Some(val));
        }
    }

    pub fn visit(mut self, arrays: &StructArray) -> Vec<DataArray> {
        let n = arrays.len();
        self.0.resize(n, Default::default());
        let column_names = arrays.column_names();
        for (index, col_name) in column_names.iter().enumerate() {
            match *col_name {
                "data" => {
                    self.visit_data(index, arrays);
                }
                "name" => {
                    self.visit_name(index, arrays);
                }
                "data_type" => {
                    self.visit_data_type(index, arrays);
                }
                "compression" => {
                    self.visit_compression(index, arrays);
                }
                "unit" => {
                    self.visit_unit(index, arrays);
                }
                "parameters" => {
                    self.visit_parameters(index, arrays);
                }
                "data_processing_ref" | "data_processing_id" => {
                    self.visit_data_processing_id(index, arrays);
                }
                "items" => {}
                _ => {}
            }
        }

        self.0
    }
}
