use arrow::{
    array::{
        Array, ArrayRef, AsArray, BooleanArray, Float32Array, Float64Array, Int32Array, Int64Array,
        StructArray, UInt32Array, UInt64Array,
    },
    datatypes::{DataType, UInt64Type},
};
use mzdata::params::{CURIE, ParamBuilder as MemParamBuilder, ParamDescribed, Unit};

use crate::{
    param::{MetadataColumn, MetadataMapping, PathOrCURIE},
    reader::visitor::{
        CompoundIndexVisitor, DoubleIndexed, OffsetCollection,
        hierarchical::OffsetCollectionSubGroup,
    },
};

use super::ParameterVisitor;
use super::curie_array_visitor::*;

// TODO: Use the parsed CURIE to also get the correct term name.
// Figure out how to do proper context piping to let this use a
// faster local cache.
#[inline(always)]
fn string_to_curie_or_value(value: &str, builder: MemParamBuilder) -> MemParamBuilder {
    if let Ok(acc) = value.parse::<CURIE>() {
        builder.curie(acc)
    } else {
        builder.value(value)
    }
}


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

pub trait VisitorBuilderBase<'a, T> {
    fn iter_instances(&mut self) -> OffsetCollection<'_, T>;

    fn metadata_map(&self) -> &'a MetadataMapping;

    fn visit_as_param(
        &mut self,
        spec_arr: &StructArray,
        index: usize,
        metacol: Option<&MetadataColumn>,
        name: Option<&str>,
    ) where
        T: ParamDescribed,
    {
        let arr = spec_arr.column(index);
        if arr.null_count() == arr.len() {
            return;
        }

        let (name, unit, accession, term_marker) = if let Some(metacol) = metacol {
            let unit = extract_unit!(metacol, spec_arr);
            let accession: Option<CURIE> = metacol.accession;
            (metacol.name.as_str(), unit, accession, metacol.term_marker)
        } else if let Some(name) = name {
            (name, Default::default(), None, false)
        } else {
            panic!("One of `metacol` or `name` must be defined")
        };

        macro_rules! convert {
            ($arr:ident) => {
                if term_marker {
                    for (i, descr) in self.iter_instances() {
                        if $arr.is_null(i) {
                            continue;
                        };
                        let mut p = mzdata::Param::builder();
                        p = p.name(&name);
                        if let Some(acc) = accession {
                            p = p.curie(acc)
                        }
                        descr.add_param(p.build());
                    }
                } else {
                    for (i, descr) in self.iter_instances() {
                        if $arr.is_null(i) {
                            continue;
                        };
                        let mut p = mzdata::Param::builder();
                        p = p.name(&name).value($arr.value(i)).unit(unit.value(i));
                        if let Some(acc) = accession {
                            p = p.curie(acc)
                        }
                        descr.add_param(p.build());
                    }
                }
            };
            ($arr:ident, $value_name:ident => $filter_exp:expr, $value_exp:expr) => {
                if term_marker {
                    for (i, descr) in self.iter_instances() {
                        if $arr.is_null(i) {
                            continue;
                        };
                        let $value_name = $arr.value(i);
                        if !$filter_exp {
                            continue;
                        }
                        let mut p = mzdata::Param::builder();
                        p = p.name(&name);
                        if let Some(acc) = accession {
                            p = p.curie(acc)
                        }
                        p = $value_exp($value_name, p);

                        descr.add_param(p.build());
                    }
                } else {
                    for (i, descr) in self.iter_instances() {
                        if $arr.is_null(i) {
                            continue;
                        };
                        let mut p = mzdata::Param::builder();
                        p = p.name(&name).value($arr.value(i)).unit(unit.value(i));
                        if let Some(acc) = accession {
                            p = p.curie(acc)
                        }
                        descr.add_param(p.build());
                    }
                }
            };
        }

        match arr.data_type() {
            DataType::Int32 => {
                let arr: &Int32Array = spec_arr.column(index).as_primitive();
                convert!(arr);
            }
            DataType::Int64 => {
                let arr: &Int64Array = spec_arr.column(index).as_primitive();
                convert!(arr);
            }
            DataType::Float32 => {
                let arr: &Float32Array = spec_arr.column(index).as_primitive();
                convert!(arr);
            }
            DataType::Float64 => {
                let arr: &Float64Array = spec_arr.column(index).as_primitive();
                convert!(arr);
            }
            DataType::Boolean => {
                let arr: &BooleanArray = arr.as_boolean();
                convert!(arr, value => { value }, |value: bool, builder: MemParamBuilder| { builder.value(value) });
            }
            DataType::Utf8 => {
                if let Some(arr) = spec_arr.column(index).as_string_opt::<i64>() {
                    convert!(arr, value => { true }, string_to_curie_or_value);
                } else if let Some(arr) = spec_arr.column(index).as_string_opt::<i32>() {
                    convert!(arr, value => { true }, string_to_curie_or_value);
                } else {
                    panic!(
                        "Unsupported data type: {:?} for {metacol:?}",
                        spec_arr.column(index).data_type()
                    );
                }
            }
            DataType::UInt32 => {
                let arr: &UInt32Array = spec_arr.column(index).as_primitive();
                convert!(arr);
            }
            DataType::UInt64 => {
                let arr: &UInt64Array = spec_arr.column(index).as_primitive();
                convert!(arr);
            }
            _ => {
                todo!("Unsupported data type {:?}", arr.data_type())
            }
        }
    }

    fn visit_parameters(&mut self, struct_arr: &StructArray, skip_params: &[CURIE])
    where
        T: ParamDescribed,
    {
        let params_array = struct_arr.column_by_name("parameters").unwrap();
        macro_rules! process {
            ($params_array:expr) => {
                for (i, descr) in self.iter_instances() {
                    let params = $params_array.value(i);
                    let params = ParameterVisitor::new(params.as_struct()).build();

                    for p in params {
                        if let Some(acc) = p.curie() {
                            if !skip_params.contains(&acc) {
                                descr.add_param(p);
                            }
                        } else {
                            descr.add_param(p);
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
}

pub trait VisitorBuilder3<'a, T>: VisitorBuilderBase<'a, DoubleIndexed<T>>
where
    DoubleIndexed<T>: CompoundIndexVisitor<T>,
{
    fn visit_precursor_index(&mut self, spec_arr: &StructArray, index: usize) {
        let arr = spec_arr.column(index).as_primitive::<UInt64Type>();
        for (i, descr) in self.iter_instances() {
            if arr.is_null(i) {
                continue;
            };
            let mut v = arr.value(i);
            let _ = descr.secondary_index_mut().insert(&mut v);
        }
    }
}

pub trait SubGroupVisitor<'a, S: 'a, T: 'a> {
    fn iter_instances_subgroup<'b>(&'b mut self) -> OffsetCollectionSubGroup<'b, S, T>;

    fn visit_as_param_subgroup(
        &mut self,
        arr: &ArrayRef,
        metacol: Option<&MetadataColumn>,
        name: Option<&str>,
    ) where
        T: ParamDescribed,
    {
        if arr.null_count() == arr.len() {
            return;
        }

        let (name, unit, accession, term_marker) = if let Some(metacol) = metacol {
            let unit = UnitCollection::singular(match metacol.unit {
                PathOrCURIE::CURIE(c) => Unit::from_curie(&c),
                _ => Unit::default(),
            });
            let accession: Option<CURIE> = metacol.accession;
            (metacol.name.as_str(), unit, accession, metacol.term_marker)
        } else if let Some(name) = name {
            (name, Default::default(), None, false)
        } else {
            panic!("One of `metacol` or `name` must be defined")
        };

        macro_rules! convert {
            ($arr:ident) => {
                if term_marker {
                    for (i, descr) in self.iter_instances_subgroup() {
                        if $arr.is_null(i) {
                            continue;
                        };
                        let mut p = mzdata::Param::builder();
                        p = p.name(&name);
                        if let Some(acc) = accession {
                            p = p.curie(acc)
                        }
                        descr.add_param(p.build());
                    }
                } else {
                    for (i, descr) in self.iter_instances_subgroup() {
                        if $arr.is_null(i) {
                            continue;
                        };
                        let mut p = mzdata::Param::builder();
                        p = p.name(&name).value($arr.value(i)).unit(unit.value(i));
                        if let Some(acc) = accession {
                            p = p.curie(acc)
                        }
                        descr.add_param(p.build());
                    }
                }
            };
            ($arr:ident, $value_name:ident => $filter_exp:expr, $value_exp:expr) => {
                if term_marker {
                    for (i, descr) in self.iter_instances_subgroup() {
                        if $arr.is_null(i) {
                            continue;
                        };
                        let $value_name = $arr.value(i);
                        if !$filter_exp {
                            continue;
                        }
                        let mut p = mzdata::Param::builder();
                        p = p.name(&name);
                        if let Some(acc) = accession {
                            p = p.curie(acc)
                        }
                        p = $value_exp($value_name, p);

                        descr.add_param(p.build());
                    }
                } else {
                    for (i, descr) in self.iter_instances_subgroup() {
                        if $arr.is_null(i) {
                            continue;
                        };
                        let mut p = mzdata::Param::builder();
                        p = p.name(&name).value($arr.value(i)).unit(unit.value(i));
                        if let Some(acc) = accession {
                            p = p.curie(acc)
                        }
                        descr.add_param(p.build());
                    }
                }
            };
        }

        match arr.data_type() {
            DataType::Int32 => {
                let arr: &Int32Array = arr.as_primitive();
                convert!(arr);
            }
            DataType::Int64 => {
                let arr: &Int64Array = arr.as_primitive();
                convert!(arr);
            }
            DataType::Float32 => {
                let arr: &Float32Array = arr.as_primitive();
                convert!(arr);
            }
            DataType::Float64 => {
                let arr: &Float64Array = arr.as_primitive();
                convert!(arr);
            }
            DataType::Boolean => {
                let arr: &BooleanArray = arr.as_boolean();
                convert!(arr, value => { value }, |value: bool, builder: MemParamBuilder| { builder.value(value) });
            }
            DataType::Utf8 => {
                if let Some(arr) = arr.as_string_opt::<i64>() {
                    convert!(arr, value => { true }, string_to_curie_or_value);
                } else if let Some(arr) = arr.as_string_opt::<i32>() {
                    convert!(arr, value => { true }, string_to_curie_or_value);
                } else {
                    panic!(
                        "Unsupported data type: {:?} for {metacol:?}",
                        arr.data_type()
                    );
                }
            }
            DataType::UInt32 => {
                let arr: &UInt32Array = arr.as_primitive();
                convert!(arr);
            }
            DataType::UInt64 => {
                let arr: &UInt64Array = arr.as_primitive();
                convert!(arr);
            }
            _ => {
                todo!("Unsupported data type {:?}", arr.data_type())
            }
        }
    }

    fn visit_parameters_subgroup(&mut self, params_array: &ArrayRef, skip_params: &[CURIE])
    where
        T: ParamDescribed,
    {
        macro_rules! process {
            ($params_array:expr) => {
                for (i, descr) in self.iter_instances_subgroup() {
                    let params = $params_array.value(i);
                    let params = ParameterVisitor::new(params.as_struct()).build();

                    for p in params {
                        if let Some(acc) = p.curie() {
                            if !skip_params.contains(&acc) {
                                descr.add_param(p);
                            }
                        } else {
                            descr.add_param(p);
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
}
