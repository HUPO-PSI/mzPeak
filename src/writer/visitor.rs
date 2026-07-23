use std::{collections::HashMap, fmt::Debug, sync::Arc};

use arrow::{
    array::{
        ArrayBuilder, ArrayRef, AsArray, BooleanBuilder, Float32Builder, Float64Builder,
        Int8Builder, Int32Builder, Int64Builder, LargeListBuilder, LargeStringBuilder, NullBuilder,
        StringBuilder, StructArray, UInt8Builder, UInt32Builder, UInt64Builder,
    },
    datatypes::{DataType, Field, FieldRef, Schema, SchemaRef},
};
use mzdata::{
    curie,
    params::{CURIE, Unit},
    prelude::*,
    spectrum::{
        ArrayType, Chromatogram, RefPeakDataLevel, ScanPolarity, SignalContinuity,
        SpectrumDescription,
    },
};

use crate::{
    constants::{CHROMATOGRAM, PRECURSOR, SCAN, SELECTED_ION, SPECTRUM},
    param::MetadataColumn,
    spectrum::AuxiliaryArray,
    writer::{base::EntryMetadataDerivedFromData, builder::SpectrumFieldVisitors},
};

pub trait VisitorBase: Debug {
    /// Whether this visitor will be flattened into the enclosing group or stored as a group node itself
    fn flatten(&self) -> bool {
        false
    }

    /// Get the list of top-level fields for this visitor
    fn fields(&self) -> Vec<FieldRef>;

    /// Convert [`Self::fields`] into a [`SchemaRef`]
    fn schema(&self) -> SchemaRef {
        Arc::new(Schema::new(self.fields()))
    }

    /// Append nulls to all the columns
    fn append_null(&mut self);

    /// Convert [`Self::fields`] into a [`DataType::Struct`]
    fn as_struct_type(&self) -> DataType {
        DataType::Struct(self.fields().into())
    }

    /// Get metadata mappings for the columns of this visitor
    fn map_metadata_columns(&self) -> Vec<MetadataColumn>;
}

macro_rules! metacol {
    ($name:literal, $path:expr, $accession:expr) => {
        MetadataColumn::new(
            $name.into(),
            $path.into_iter().map(|v| v.into()).collect(),
            Some($accession),
        )
    };
    ($name:literal, $path:expr, $accession:expr, $unit:expr) => {
        MetadataColumn::new(
            $name.into(),
            $path.into_iter().map(|v| v.into()).collect(),
            Some($accession),
        )
        .with_unit($unit.to_curie().unwrap())
    };
}

macro_rules! finish_extra {
    ($self:ident, $arrays:ident) => {
        for e in $self.extra.iter_mut() {
            if e.flatten() {
                let arr = e.finish();
                let arr = arr.as_struct();
                $arrays.extend_from_slice(arr.columns());
            } else {
                $arrays.push(e.finish());
            }
        }
    };
}

macro_rules! finish_cloned_extra {
    ($self:ident, $arrays:ident) => {
        for e in $self.extra.iter() {
            if e.flatten() {
                let arr = e.finish_cloned();
                let arr = arr.as_struct();
                $arrays.extend_from_slice(arr.columns());
            } else {
                $arrays.push(e.finish_cloned());
            }
        }
    };
}


/// A set of shared behaviors for consuming references of a type and building an Arrow (sub-)structure from them
pub trait StructVisitor<T>: VisitorBase {
    /// Visit an instance of `T` and append its relevant values to columns.
    fn append_value(&mut self, item: &T) -> bool;

    /// As [`Self::append_value`] but calls [`VisitorBase::append_null`] if `item` is `None`
    fn append_option(&mut self, item: Option<&T>) -> bool {
        if let Some(item) = item {
            self.append_value(item)
        } else {
            self.append_null();
            false
        }
    }

    /// Report a [`CURIE`] that has been handled by this visitor and should not be handled again
    /// later, if applicable.
    fn associated_curie_to_skip(&self) -> Option<CURIE> {
        None
    }
}

/// An intersection of [`StructVisitor`], [`VisitorBase`], and [`arrow::array::ArrayBuilder`]
pub trait StructVisitorBuilder<T>: StructVisitor<T> + ArrayBuilder + VisitorBase {}

impl<T, U> StructVisitorBuilder<T> for U where U: StructVisitor<T> + ArrayBuilder {}

macro_rules! field {
    ($name:expr, $typeexpr:expr) => {
        Arc::new(Field::new($name, $typeexpr, true))
    };
    ($name:expr, $typeexpr:expr, $nullable:expr) => {
        Arc::new(Field::new($name, $typeexpr, $nullable))
    };
}

macro_rules! finish_it {
    ($builder:expr) => {
        Arc::new($builder.finish()) as ArrayRef
    };
}

macro_rules! finish_cloned {
    ($builder:expr) => {
        Arc::new($builder.finish_cloned()) as ArrayRef
    };
}

macro_rules! anyways {
    () => {
        fn as_any(&self) -> &dyn std::any::Any {
            self
        }

        fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
            self
        }

        fn into_box_any(self: Box<Self>) -> Box<dyn std::any::Any> {
            self
        }
    };
}

/// Inflect a controlled vocabulary term to a Parquet-compatible column name.
///
/// This involves `${cv}_${accession}_${formatted_name}` where formatted name
/// is the name of the term with all non alphanumeric characters are replaced
/// with '_'.
pub fn inflect_cv_term_to_column_name(curie: CURIE, name: &str, unit: Option<CURIE>) -> String {
    let cv_part = curie.to_string().replace(":", "_");
    let mut buffer = String::with_capacity(name.len() + cv_part.len() + 5);
    buffer.push_str("opt_");
    buffer.push_str(&cv_part);
    buffer.push('_');
    for c in name.replace("m/z", "mz").chars() {
        if c.is_alphanumeric() || c == '_' || c == '-' {
            buffer.push(c);
        } else {
            buffer.push('_');
        }
    }
    if let Some(unit) = unit {
        buffer.push_str("_unit_");
        buffer.push_str(unit.to_string().replace(":", "_").as_str());
    }
    buffer
}

/// A [`StructVisitor`] that will match an [`mzdata::Param`] by [`CURIE`]
/// and map it to a new column, and then prevent the same `CURIE` from being
/// reused again elsewhere for that object.
pub struct CustomBuilderFromParameter {
    accession: CURIE,
    name: String,
    value: Box<dyn ArrayBuilder>,
    field: FieldRef,
    unit: Option<CURIEBuilder>,
    fixed_unit: Option<Unit>,
}

impl Debug for CustomBuilderFromParameter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CustomBuilderFromParameter")
            .field("accession", &self.accession)
            .field("value", &"...")
            .field("field", &self.field)
            .field("unit", if self.unit.is_some() { &"yes" } else { &"no" })
            .finish()
    }
}

impl CustomBuilderFromParameter {
    /// Declare that the column has a separate unit column, not a single shared unit encoded in the metadata mapping
    pub fn with_unit_field(mut self) -> Self {
        self.unit = Some(CURIEBuilder::default());
        self
    }

    /// Declare a constant unit for the column
    pub fn with_unit_fixed(mut self, unit: Option<CURIE>) -> CustomBuilderFromParameter {
        self.fixed_unit = unit.as_ref().map(Unit::from_curie);
        self
    }

    /// Set the name of the column
    pub fn with_name(mut self, name: &str) -> Self {
        self.name = name.to_string();
        self.field = Arc::new(self.field.as_ref().clone().with_name(name));
        self
    }

    /// The [`CURIE`] of the [`Param`] this matches.
    pub fn accession(&self) -> CURIE {
        self.accession
    }

    /// Create a new [`CustomBuilderFromParameter`] for the specified CURIE with the requested Arrow [`DataType`]
    pub fn from_spec(curie: CURIE, name: &str, dtype: DataType) -> Self {
        let original_name = name.to_string();
        let name = inflect_cv_term_to_column_name(curie, name, None);
        let field = field!(name, dtype.clone());
        let unit = None;
        match dtype {
            DataType::Null => Self {
                accession: curie,
                name: original_name,
                field,
                value: Box::new(NullBuilder::new()),
                unit,
                fixed_unit: None,
            },
            DataType::Boolean => Self {
                accession: curie,
                name: original_name,
                field,
                value: Box::new(BooleanBuilder::new()),
                unit,
                fixed_unit: None,
            },
            DataType::Int64 => Self {
                accession: curie,
                name: original_name,
                field,
                value: Box::new(Int64Builder::new()),
                unit,
                fixed_unit: None,
            },
            DataType::UInt32 => Self {
                accession: curie,
                name: original_name,
                field,
                value: Box::new(UInt32Builder::new()),
                unit,
                fixed_unit: None,
            },
            DataType::Int32 => Self {
                accession: curie,
                name: original_name,
                field,
                value: Box::new(Int32Builder::new()),
                unit,
                fixed_unit: None,
            },
            DataType::Float64 => Self {
                accession: curie,
                name: original_name,
                field,
                value: Box::new(Float64Builder::new()),
                unit,
                fixed_unit: None,
            },
            DataType::LargeUtf8 => Self {
                accession: curie,
                name: original_name,
                field,
                value: Box::new(LargeStringBuilder::new()),
                unit,
                fixed_unit: None,
            },
            _ => unimplemented!("{dtype:?} is not supported by CustomBuilderFromParameter"),
        }
    }
}

impl VisitorBase for CustomBuilderFromParameter {
    fn flatten(&self) -> bool {
        self.unit.is_some()
    }

    fn fields(&self) -> Vec<FieldRef> {
        if let Some(unit) = self.unit.as_ref() {
            vec![
                self.field.clone(),
                field!(format!("{}_unit", self.field.name()), unit.as_struct_type()),
            ]
        } else {
            vec![self.field.clone()]
        }
    }

    fn append_null(&mut self) {
        if let Some(unit) = self.unit.as_mut() {
            unit.append_null();
        }

        match self.field.data_type() {
            DataType::Null => {
                self.value
                    .as_any_mut()
                    .downcast_mut::<NullBuilder>()
                    .unwrap()
                    .append_empty_value();
            }
            DataType::Boolean => {
                self.value
                    .as_any_mut()
                    .downcast_mut::<BooleanBuilder>()
                    .unwrap()
                    .append_null();
            }
            DataType::Int32 => {
                self.value
                    .as_any_mut()
                    .downcast_mut::<Int32Builder>()
                    .unwrap()
                    .append_null();
            }
            DataType::UInt32 => {
                self.value
                    .as_any_mut()
                    .downcast_mut::<UInt32Builder>()
                    .unwrap()
                    .append_null();
            }
            DataType::Int64 => {
                self.value
                    .as_any_mut()
                    .downcast_mut::<Int64Builder>()
                    .unwrap()
                    .append_null();
            }
            DataType::Float64 => {
                self.value
                    .as_any_mut()
                    .downcast_mut::<Float64Builder>()
                    .unwrap()
                    .append_null();
            }
            DataType::LargeUtf8 => {
                self.value
                    .as_any_mut()
                    .downcast_mut::<LargeStringBuilder>()
                    .unwrap()
                    .append_null();
            }
            _ => panic!("Unsupported value type {:?}", self.field.data_type()),
        }
    }

    fn map_metadata_columns(&self) -> Vec<MetadataColumn> {
        let mut columns = Vec::new();
        let f = MetadataColumn::new(
            self.name.clone(),
            vec![self.field.name().to_string()],
            Some(self.accession()),
        );
        if self.unit.is_some() {
            columns.push(f.with_unit(vec![format!("{}_unit", self.field.name())]));
            columns.push(MetadataColumn::new(
                self.name.clone() + " unit",
                vec![format!("{}_unit", self.field.name())],
                None,
            ));
        } else if let Some(unit) = self.fixed_unit.and_then(|u| u.to_curie()) {
            columns.push(f.with_unit(unit));
        } else {
            columns.push(f);
        }
        columns
    }
}

impl<T> StructVisitor<T> for CustomBuilderFromParameter
where
    T: ParamDescribed,
{
    fn append_value(&mut self, item: &T) -> bool {
        if let Some(val) = item.get_param_by_curie(&self.accession) {
            match self.field.data_type() {
                DataType::Null => {
                    self.value
                        .as_any_mut()
                        .downcast_mut::<NullBuilder>()
                        .unwrap()
                        .append_empty_value();
                }
                DataType::Boolean => {
                    self.value
                        .as_any_mut()
                        .downcast_mut::<BooleanBuilder>()
                        .unwrap()
                        .append_option(val.to_bool().ok());
                }
                DataType::UInt32 => {
                    self.value
                        .as_any_mut()
                        .downcast_mut::<UInt32Builder>()
                        .unwrap()
                        .append_option(val.to_u64().ok().and_then(|v| v.try_into().ok()));
                }
                DataType::Int32 => {
                    self.value
                        .as_any_mut()
                        .downcast_mut::<Int32Builder>()
                        .unwrap()
                        .append_option(val.to_i32().ok().and_then(|v| v.try_into().ok()));
                }
                DataType::Int64 => {
                    self.value
                        .as_any_mut()
                        .downcast_mut::<Int64Builder>()
                        .unwrap()
                        .append_option(val.to_i64().ok());
                }
                DataType::Float64 => {
                    self.value
                        .as_any_mut()
                        .downcast_mut::<Float64Builder>()
                        .unwrap()
                        .append_option(val.to_f64().ok());
                }
                DataType::LargeUtf8 => {
                    self.value
                        .as_any_mut()
                        .downcast_mut::<LargeStringBuilder>()
                        .unwrap()
                        .append_option(if val.is_empty() {
                            None
                        } else {
                            Some(val.value().to_string())
                        });
                }
                _ => panic!("Unsupported value type {:?}", self.field.data_type()),
            }

            if let Some(unit) = self.unit.as_mut() {
                unit.append_option(val.unit().to_curie().as_ref());
            }

            true
        } else {
            self.append_null();
            false
        }
    }

    fn associated_curie_to_skip(&self) -> Option<CURIE> {
        Some(self.accession)
    }
}

impl ArrayBuilder for CustomBuilderFromParameter {
    anyways!();

    fn len(&self) -> usize {
        self.value.len()
    }

    fn finish(&mut self) -> ArrayRef {
        let fields = self.fields();
        if let Some(unit) = self.unit.as_mut() {
            let arrays = vec![finish_it!(self.value), unit.finish()];
            Arc::new(StructArray::new(fields.into(), arrays, None))
        } else {
            self.value.finish()
        }
    }

    fn finish_cloned(&self) -> ArrayRef {
        let fields = self.fields();
        if let Some(unit) = self.unit.as_ref() {
            let arrays = vec![finish_cloned!(self.value), unit.finish_cloned()];
            Arc::new(StructArray::new(fields.into(), arrays, None))
        } else {
            self.value.finish_cloned()
        }
    }
}

/// A column that encodes [`CURIE`] as strings
#[derive(Debug, Default)]
pub struct CURIEBuilder(StringBuilder);

impl VisitorBase for CURIEBuilder {
    fn fields(&self) -> Vec<FieldRef> {
        vec![field!("accession", DataType::Utf8)]
    }

    fn append_null(&mut self) {
        self.0.append_null();
    }

    fn as_struct_type(&self) -> DataType {
        DataType::Utf8
    }

    fn map_metadata_columns(&self) -> Vec<MetadataColumn> {
        Vec::new()
    }
}

impl StructVisitor<mzdata::params::CURIE> for CURIEBuilder {
    fn append_value(&mut self, item: &mzdata::params::CURIE) -> bool {
        let item = item.to_string();
        self.0.append_value(&item);
        true
    }
}

impl StructVisitor<&str> for CURIEBuilder {
    fn append_value(&mut self, item: &&str) -> bool {
        let val: mzdata::params::CURIE = item.parse().unwrap();
        self.append_value(&val)
    }
}

impl ArrayBuilder for CURIEBuilder {
    fn len(&self) -> usize {
        self.0.len()
    }

    fn finish(&mut self) -> ArrayRef {
        Arc::new(self.0.finish())
    }

    fn finish_cloned(&self) -> ArrayRef {
        Arc::new(self.0.finish_cloned())
    }

    anyways!();
}

/// A builder for [`mzdata::params::Value`]-like types that only handles scalar values
#[derive(Debug, Default)]
pub struct ParamValueBuilder {
    integer: Int64Builder,
    float: Float64Builder,
    boolean: BooleanBuilder,
    string: LargeStringBuilder,
}

impl VisitorBase for ParamValueBuilder {
    fn fields(&self) -> Vec<FieldRef> {
        let fields = vec![
            Arc::new(Field::new("integer", DataType::Int64, true)),
            Arc::new(Field::new("float", DataType::Float64, true)),
            Arc::new(Field::new("string", DataType::LargeUtf8, true)),
            Arc::new(Field::new("boolean", DataType::Boolean, true)),
        ];
        fields
    }

    fn append_null(&mut self) {
        self.boolean.append_null();
        self.integer.append_null();
        self.string.append_null();
        self.float.append_null();
    }

    fn map_metadata_columns(&self) -> Vec<MetadataColumn> {
        Vec::new()
    }
}

impl StructVisitor<mzdata::params::Value> for ParamValueBuilder {
    fn append_value(&mut self, item: &mzdata::params::Value) -> bool {
        match item {
            mzdata::params::Value::String(v) => {
                self.string.append_value(v);
                self.integer.append_null();
                self.float.append_null();
                self.boolean.append_null();
                true
            }
            mzdata::params::Value::Float(v) => {
                self.string.append_null();
                self.integer.append_null();
                self.float.append_value(*v);
                self.boolean.append_null();
                true
            }
            mzdata::params::Value::Int(v) => {
                self.string.append_null();
                self.integer.append_value(*v);
                self.float.append_null();
                self.boolean.append_null();
                true
            }
            mzdata::params::Value::Buffer(_) => todo!(),
            mzdata::params::Value::Boolean(v) => {
                self.string.append_null();
                self.integer.append_null();
                self.float.append_null();
                self.boolean.append_value(*v);
                true
            }
            mzdata::params::Value::Empty => {
                self.string.append_null();
                self.integer.append_null();
                self.float.append_null();
                self.boolean.append_null();
                true
            }
            mzdata::params::Value::List(_values) => {
                unimplemented!()
            }
        }
    }
}

impl ArrayBuilder for ParamValueBuilder {
    fn len(&self) -> usize {
        self.string.len()
    }

    fn finish(&mut self) -> ArrayRef {
        let fields = self.fields();
        let arrays: Vec<ArrayRef> = vec![
            Arc::new(self.integer.finish()),
            Arc::new(self.float.finish()),
            Arc::new(self.string.finish()),
            Arc::new(self.boolean.finish()),
        ];
        Arc::new(StructArray::new(fields.into(), arrays, None))
    }

    fn finish_cloned(&self) -> ArrayRef {
        let fields = self.fields();
        let arrays: Vec<ArrayRef> = vec![
            Arc::new(self.integer.finish_cloned()),
            Arc::new(self.float.finish_cloned()),
            Arc::new(self.string.finish_cloned()),
            Arc::new(self.boolean.finish_cloned()),
        ];
        Arc::new(StructArray::new(fields.into(), arrays, None))
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self as &dyn std::any::Any
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self as &mut dyn std::any::Any
    }

    fn into_box_any(self: Box<Self>) -> Box<dyn std::any::Any> {
        self as Box<dyn std::any::Any>
    }
}


/// A builder for [`mzdata::params::Param`]-like types
#[derive(Debug, Default)]
pub struct ParamBuilder {
    value: ParamValueBuilder,
    curie: CURIEBuilder,
    name: LargeStringBuilder,
    unit: CURIEBuilder,
}

impl VisitorBase for ParamBuilder {
    fn fields(&self) -> Vec<FieldRef> {
        vec![
            field!("value", self.value.as_struct_type()),
            field!("accession", self.curie.as_struct_type()),
            field!("name", DataType::LargeUtf8),
            field!("unit", self.unit.as_struct_type()),
        ]
    }

    fn append_null(&mut self) {
        self.value.append_null();
        self.curie.append_null();
        self.name.append_null();
        self.unit.append_null();
    }

    fn map_metadata_columns(&self) -> Vec<MetadataColumn> {
        Vec::new()
    }
}

impl StructVisitor<mzdata::Param> for ParamBuilder {
    fn append_value(&mut self, item: &mzdata::Param) -> bool {
        self.curie.append_option(item.curie().as_ref());
        self.name.append_value(item.name());
        self.unit.append_option(item.unit.to_curie().as_ref());
        self.value.append_value(&item.value)
    }
}

impl ArrayBuilder for ParamBuilder {
    fn len(&self) -> usize {
        self.name.len()
    }

    fn finish(&mut self) -> ArrayRef {
        Arc::new(StructArray::new(
            self.fields().into(),
            vec![
                self.value.finish(),
                self.curie.finish(),
                Arc::new(self.name.finish()),
                self.unit.finish(),
            ],
            None,
        ))
    }

    fn finish_cloned(&self) -> ArrayRef {
        Arc::new(StructArray::new(
            self.fields().into(),
            vec![
                self.value.finish_cloned(),
                self.curie.finish_cloned(),
                Arc::new(self.name.finish_cloned()),
                self.unit.finish_cloned(),
            ],
            None,
        ))
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self as &dyn std::any::Any
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self as &mut dyn std::any::Any
    }

    fn into_box_any(self: Box<Self>) -> Box<dyn std::any::Any> {
        self as Box<dyn std::any::Any>
    }
}


#[derive(Debug, Default)]
pub struct ParamListBuilder(LargeListBuilder<ParamBuilder>);

impl ParamListBuilder {
    pub fn append_empty(&mut self) {
        self.0.append(true);
    }

    pub fn as_builder(&mut self) -> &mut LargeListBuilder<ParamBuilder> {
        &mut self.0
    }

    pub fn append_iter<'a, T: 'a>(&mut self, iter: impl IntoIterator<Item = &'a T>) -> bool
    where
        ParamBuilder: StructVisitor<T> + Sized,
    {
        let inner = self.0.values();
        for v in iter {
            inner.append_value(v);
        }
        self.0.append(true);
        true
    }
}

impl core::convert::AsMut<LargeListBuilder<ParamBuilder>> for ParamListBuilder {
    fn as_mut(&mut self) -> &mut LargeListBuilder<ParamBuilder> {
        &mut self.0
    }
}

impl ArrayBuilder for ParamListBuilder {
    fn len(&self) -> usize {
        self.0.len()
    }

    fn finish(&mut self) -> ArrayRef {
        Arc::new(self.0.finish())
    }

    fn finish_cloned(&self) -> ArrayRef {
        Arc::new(self.0.finish_cloned())
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self as &dyn std::any::Any
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self as &mut dyn std::any::Any
    }

    fn into_box_any(self: Box<Self>) -> Box<dyn std::any::Any> {
        self as Box<dyn std::any::Any>
    }
}

impl<T> StructVisitor<&[T]> for ParamListBuilder
where
    ParamBuilder: StructVisitor<T> + Sized,
{
    fn append_value(&mut self, item: &&[T]) -> bool {
        let inner = self.0.values();
        for v in item.into_iter() {
            inner.append_value(v);
        }
        self.0.append(true);
        true
    }
}

impl VisitorBase for ParamListBuilder {
    fn fields(&self) -> Vec<FieldRef> {
        vec![field!(
            "parameters",
            DataType::LargeList(field!("item", self.0.values_ref().as_struct_type()))
        )]
    }

    fn append_null(&mut self) {
        self.0.append_null();
    }

    fn map_metadata_columns(&self) -> Vec<MetadataColumn> {
        Vec::new()
    }
}

/// An inner builder for [`mzdata::spectrum::ScanWindow`]
#[derive(Debug, Default)]
pub struct ScanWindowBuilder {
    lower_limit: Float32Builder,
    upper_limit: Float32Builder,
    parameters: ParamListBuilder,
}

impl VisitorBase for ScanWindowBuilder {
    fn fields(&self) -> Vec<FieldRef> {
        let mut fields = vec![
            field!(
                "scan_window_lower_limit",
                DataType::Float32
            ),
            field!(
                "scan_window_upper_limit",
                DataType::Float32
            ),
        ];
        fields.extend(self.parameters.fields());
        fields
    }

    fn append_null(&mut self) {
        self.lower_limit.append_null();
        self.upper_limit.append_null();
        self.parameters.append_null();
    }

    fn map_metadata_columns(&self) -> Vec<MetadataColumn> {
        let fields = self.fields();
        vec![
            MetadataColumn::new(
                "scan window lower limit".into(),
                vec![fields[0].name().to_string()],
                Some(curie!(MS:1000501)),
            )
            .with_unit(Unit::MZ),
            MetadataColumn::new(
                "scan window upper limit".into(),
                vec![fields[1].name().to_string()],
                Some(curie!(MS:1000500)),
            )
            .with_unit(Unit::MZ),
        ]
    }
}

impl StructVisitor<mzdata::spectrum::ScanWindow> for ScanWindowBuilder {
    fn append_value(&mut self, item: &mzdata::spectrum::ScanWindow) -> bool {
        self.lower_limit.append_value(item.lower_bound);
        self.upper_limit.append_value(item.upper_bound);
        self.parameters.append_empty();
        true
    }
}

impl ArrayBuilder for ScanWindowBuilder {
    fn len(&self) -> usize {
        self.lower_limit.len()
    }

    fn finish(&mut self) -> ArrayRef {
        let fields = self.fields();
        let arrays = vec![
            finish_it!(self.lower_limit),
            finish_it!(self.upper_limit),
            self.parameters.finish(),
        ];

        Arc::new(StructArray::new(fields.into(), arrays, None))
    }

    fn finish_cloned(&self) -> ArrayRef {
        let fields = self.fields();
        let arrays = vec![
            Arc::new(self.lower_limit.finish_cloned()),
            Arc::new(self.upper_limit.finish_cloned()),
            self.parameters.finish_cloned(),
        ];

        Arc::new(StructArray::new(fields.into(), arrays, None))
    }

    anyways!();
}


/// A builder for [`mzdata::spectrum::ScanEvent`], mapping to the `scan` table. It carries two indices, the source's index (usually a spectrum),
/// and a unique scan index.
#[derive(Default, Debug)]
pub struct ScanBuilder {
    source_index: UInt64Builder,
    scan_index: UInt64Builder,
    scan_start_time: Float32Builder,
    preset_scan_configuration: UInt32Builder,
    filter_string: LargeStringBuilder,
    ion_injection_time: Float32Builder,
    ion_mobility_value: Float64Builder,
    ion_mobility_type: CURIEBuilder,
    instrument_configuration_ref: UInt32Builder,
    spectrum_reference: LargeStringBuilder,
    parameters: ParamListBuilder,
    scan_windows: LargeListBuilder<ScanWindowBuilder>,
    extra: Vec<Box<dyn StructVisitorBuilder<mzdata::spectrum::ScanEvent>>>,
    curies_to_mask: Vec<CURIE>,
}

impl ScanBuilder {
    pub fn extend_extra_fields(
        &mut self,
        iter: impl IntoIterator<Item = Box<dyn StructVisitorBuilder<mzdata::spectrum::ScanEvent>>>,
    ) {
        self.extra.extend(iter);
    }
}

impl VisitorBase for ScanBuilder {
    fn fields(&self) -> Vec<FieldRef> {
        let mut fields = vec![
            field!("source_index", DataType::UInt64),
            field!("scan_index", DataType::UInt64),
            field!(
                "scan_start_time",
                DataType::Float32
            ),
            field!("preset_scan_configuration", DataType::UInt32),
            field!("filter_string", DataType::LargeUtf8),
            field!(
                "ion_injection_time",
                DataType::Float32
            ),
            field!("ion_mobility_value", DataType::Float64),
            field!("ion_mobility_type", self.ion_mobility_type.as_struct_type()),
            field!("instrument_configuration_id", DataType::UInt32),
            field!("spectrum_reference", DataType::LargeUtf8),
        ];
        fields.extend(self.parameters.fields());
        fields.push(field!(
            "scan_windows",
            DataType::LargeList(field!(
                "item",
                self.scan_windows.values_ref().as_struct_type()
            ))
        ));
        for e in self.extra.iter() {
            fields.extend(e.fields());
        }
        fields
    }

    fn map_metadata_columns(&self) -> Vec<MetadataColumn> {
        let mut columns = Vec::new();
        let fields = self.fields();
        columns.push(metacol!(
            "scan start time",
            [fields[2].name()],
            curie!(MS:1000016),
            Unit::Minute
        ));
        columns.push(metacol!(
            "preset scan configuration",
            [fields[3].name()],
            curie!(MS:1000616)
        ));
        columns.push(metacol!(
            "filter string",
            [fields[4].name()],
            curie!(MS:1000512)
        ));
        columns.push(metacol!(
            "ion injection time",
            [fields[5].name()],
            curie!(MS:1000927),
            Unit::Millisecond
        ));
        for mut f in self.scan_windows.values_ref().map_metadata_columns() {
            f.path.insert(0, "scan_windows".into());
            columns.push(f);
        }
        for e in self.extra.iter() {
            columns.extend(e.map_metadata_columns());
        }
        columns
    }

    fn append_null(&mut self) {
        self.source_index.append_null();
        self.scan_index.append_null();
        self.scan_start_time.append_null();
        self.preset_scan_configuration.append_null();
        self.filter_string.append_null();
        self.ion_injection_time.append_null();
        self.ion_mobility_value.append_null();
        self.ion_mobility_type.append_null();
        self.instrument_configuration_ref.append_null();
        self.spectrum_reference.append_null();
        self.parameters.append_null();
        self.scan_windows.append_null();
        for e in self.extra.iter_mut() {
            e.append_null();
        }
    }
}

const BUILTIN_SCAN_PARAMS: &[CURIE] = &[curie!(MS:1000512), curie!(MS:1000616)];

impl StructVisitor<(u64, u64, &mzdata::spectrum::ScanEvent)> for ScanBuilder {
    fn append_value(&mut self, item: &(u64, u64, &mzdata::spectrum::ScanEvent)) -> bool {
        let (si, sci, item) = item;
        self.source_index.append_value(*si);
        self.scan_index.append_value(*sci);
        self.scan_start_time.append_value(item.start_time as f32);
        self.preset_scan_configuration.append_option(
            item.scan_configuration()
                .map(|i| i.to_u64().unwrap() as u32),
        );
        self.filter_string
            .append_option(item.filter_string().as_deref());
        self.ion_injection_time.append_value(item.injection_time);
        self.ion_mobility_value.append_option(item.ion_mobility());
        self.ion_mobility_type
            .append_option(item.ion_mobility_type().and_then(|v| v.curie()).as_ref());
        self.instrument_configuration_ref
            .append_value(item.instrument_configuration_id);
        self.spectrum_reference
            .append_option(item.spectrum_reference.as_ref());

        let val = self.scan_windows.values();
        for window in item.scan_windows.iter() {
            val.append_value(window);
        }
        self.scan_windows.append(true);

        self.curies_to_mask.extend_from_slice(BUILTIN_SCAN_PARAMS);
        for e in self.extra.iter_mut() {
            if e.append_value(item) {
                self.curies_to_mask.extend(e.associated_curie_to_skip());
            }
        }
        self.parameters
            .append_iter(item.params().iter().filter(|p| {
                if let Some(c) = p.curie() {
                    !self.curies_to_mask.contains(&c)
                } else {
                    true
                }
            }));
        self.curies_to_mask.clear();
        true
    }
}

impl ArrayBuilder for ScanBuilder {
    fn len(&self) -> usize {
        self.source_index.len()
    }

    fn finish(&mut self) -> ArrayRef {
        let schema = self.fields();
        let mut arrays: Vec<ArrayRef> = vec![
            finish_it!(self.source_index),
            finish_it!(self.scan_index),
            finish_it!(self.scan_start_time),
            finish_it!(self.preset_scan_configuration),
            finish_it!(self.filter_string),
            finish_it!(self.ion_injection_time),
            finish_it!(self.ion_mobility_value),
            self.ion_mobility_type.finish(),
            finish_it!(self.instrument_configuration_ref),
            finish_it!(self.spectrum_reference),
            self.parameters.finish(),
            finish_it!(self.scan_windows),
        ];
        finish_extra!(self, arrays);
        Arc::new(StructArray::new(schema.into(), arrays, None))
    }

    fn finish_cloned(&self) -> ArrayRef {
        let schema = self.fields();
        let mut arrays: Vec<ArrayRef> = vec![
            finish_cloned!(self.source_index),
            finish_cloned!(self.scan_index),
            finish_cloned!(self.scan_start_time),
            finish_cloned!(self.preset_scan_configuration),
            finish_cloned!(self.filter_string),
            finish_cloned!(self.ion_injection_time),
            finish_cloned!(self.ion_mobility_value),
            self.ion_mobility_type.finish_cloned(),
            finish_cloned!(self.instrument_configuration_ref),
            finish_cloned!(self.spectrum_reference),
            self.parameters.finish_cloned(),
            finish_cloned!(self.scan_windows),
        ];
        finish_cloned_extra!(self, arrays);
        Arc::new(StructArray::new(schema.into(), arrays, None))
    }

    anyways!();
}

#[derive(Default, Debug)]
pub struct IsolationWindowBuilder {
    target: Float32Builder,
    lower_bound: Float32Builder,
    upper_bound: Float32Builder,
    parameters: ParamListBuilder,
}

impl ArrayBuilder for IsolationWindowBuilder {
    anyways!();

    fn len(&self) -> usize {
        self.target.len()
    }

    fn finish(&mut self) -> ArrayRef {
        let schema = self.fields();
        let arrays: Vec<ArrayRef> = vec![
            finish_it!(self.target),
            finish_it!(self.lower_bound),
            finish_it!(self.upper_bound),
            self.parameters.finish(),
        ];

        Arc::new(StructArray::new(schema.into(), arrays, None))
    }

    fn finish_cloned(&self) -> ArrayRef {
        let schema = self.fields();
        let arrays: Vec<ArrayRef> = vec![
            finish_cloned!(self.target),
            finish_cloned!(self.lower_bound),
            finish_cloned!(self.upper_bound),
            self.parameters.finish_cloned(),
        ];

        Arc::new(StructArray::new(schema.into(), arrays, None))
    }
}

impl StructVisitor<mzdata::spectrum::IsolationWindow> for IsolationWindowBuilder {
    fn append_value(&mut self, item: &mzdata::spectrum::IsolationWindow) -> bool {
        match item.flags {
            mzdata::spectrum::IsolationWindowState::Unknown => {
                self.lower_bound.append_null();
                self.upper_bound.append_null();
                self.target.append_null();
            }
            mzdata::spectrum::IsolationWindowState::Offset => {
                self.target.append_value(item.target);
                self.lower_bound.append_value(item.lower_bound);
                self.upper_bound.append_value(item.upper_bound);
            }
            mzdata::spectrum::IsolationWindowState::Complete
            | mzdata::spectrum::IsolationWindowState::Explicit => {
                self.target.append_value(item.target);
                self.lower_bound
                    .append_value(item.target - item.lower_bound);
                self.upper_bound
                    .append_value(item.upper_bound - item.target);
            }
        }
        self.parameters.append_empty();
        true
    }
}

impl VisitorBase for IsolationWindowBuilder {
    fn fields(&self) -> Vec<FieldRef> {
        let mut fields = vec![
            field!("isolation_window_target", DataType::Float32),
            field!("isolation_window_lower_offset", DataType::Float32),
            field!("isolation_window_upper_offset", DataType::Float32),
        ];
        fields.extend(self.parameters.fields());
        fields
    }

    fn append_null(&mut self) {
        self.target.append_null();
        self.lower_bound.append_null();
        self.upper_bound.append_null();
        self.parameters.append_null();
    }

    fn map_metadata_columns(&self) -> Vec<MetadataColumn> {
        let fields = self.fields();
        vec![
            metacol!(
                "isolation window target m/z",
                [fields[0].name().to_string()],
                curie!(MS:1000827),
                Unit::MZ
            ),
            metacol!(
                "isolation window lower offset",
                [fields[1].name().to_string()],
                curie!(MS:1000828),
                Unit::MZ
            ),
            metacol!(
                "isolation window upper offset",
                [fields[2].name().to_string()],
                curie!(MS:1000829),
                Unit::MZ
            ),
        ]
    }
}

#[derive(Default, Debug)]
pub struct ActivationBuilder {
    parameters: ParamListBuilder,
    extra: Vec<Box<dyn StructVisitorBuilder<mzdata::spectrum::Activation>>>,
    curies_to_mask: Vec<CURIE>,
}

impl ArrayBuilder for ActivationBuilder {
    anyways!();

    fn len(&self) -> usize {
        self.parameters.len()
    }

    fn finish(&mut self) -> ArrayRef {
        let fields = self.fields();
        let mut arrays = vec![self.parameters.finish()];

        for e in self.extra.iter_mut() {
            arrays.push(e.finish());
        }
        Arc::new(StructArray::new(fields.into(), arrays, None))
    }

    fn finish_cloned(&self) -> ArrayRef {
        let fields = self.fields();
        let mut arrays = vec![self.parameters.finish_cloned()];

        for e in self.extra.iter() {
            arrays.push(e.finish_cloned());
        }
        Arc::new(StructArray::new(fields.into(), arrays, None))
    }
}

impl StructVisitor<mzdata::spectrum::Activation> for ActivationBuilder {
    fn append_value(&mut self, item: &mzdata::spectrum::Activation) -> bool {
        let params = self.parameters.as_mut().values();
        for method in item.methods() {
            let par: mzdata::Param = method.to_param().into();
            params.append_value(&par);
        }

        let energy = mzdata::Param::builder()
            .name("collision energy")
            .curie(mzdata::curie!(MS:1000045))
            .value(item.energy)
            .unit(Unit::Electronvolt)
            .build();

        for e in self.extra.iter_mut() {
            if e.append_value(item) {
                self.curies_to_mask.extend(e.associated_curie_to_skip());
            }
        }

        self.parameters
            .append_iter(item.params().iter().chain([&energy]).filter(|p| {
                if let Some(c) = p.curie() {
                    !self.curies_to_mask.contains(&c)
                } else {
                    true
                }
            }));
        self.curies_to_mask.clear();
        true
    }
}

impl VisitorBase for ActivationBuilder {
    fn fields(&self) -> Vec<FieldRef> {
        let mut fields = self.parameters.fields();
        for e in self.extra.iter() {
            fields.extend(e.fields());
        }
        fields
    }

    fn append_null(&mut self) {
        self.parameters.append_null();
        self.extra.iter_mut().for_each(|e| e.append_null());
    }

    fn map_metadata_columns(&self) -> Vec<MetadataColumn> {
        self.extra
            .iter()
            .map(|e| e.map_metadata_columns())
            .flatten()
            .collect()
    }
}

#[derive(Default, Debug)]
pub struct PrecursorBuilder {
    source_index: UInt64Builder,
    precursor_index: UInt64Builder,
    precursor_id: LargeStringBuilder,
    isolation_window: IsolationWindowBuilder,
    activation: ActivationBuilder,
}

impl PrecursorBuilder {
    pub fn extend_extra_activation_fields(
        &mut self,
        iter: impl IntoIterator<Item = Box<dyn StructVisitorBuilder<mzdata::spectrum::Activation>>>,
    ) {
        self.activation.extra.extend(iter);
    }
}

impl ArrayBuilder for PrecursorBuilder {
    fn len(&self) -> usize {
        self.source_index.len()
    }

    fn finish(&mut self) -> ArrayRef {
        let fields = self.fields();

        let arrays = vec![
            finish_it!(self.source_index),
            finish_it!(self.precursor_index),
            finish_it!(self.precursor_id),
            self.isolation_window.finish(),
            self.activation.finish(),
        ];

        Arc::new(StructArray::new(fields.into(), arrays, None))
    }

    fn finish_cloned(&self) -> ArrayRef {
        let fields = self.fields();

        let arrays = vec![
            finish_cloned!(self.source_index),
            finish_cloned!(self.precursor_index),
            finish_cloned!(self.precursor_id),
            self.isolation_window.finish_cloned(),
            self.activation.finish_cloned(),
        ];

        Arc::new(StructArray::new(fields.into(), arrays, None))
    }

    anyways!();
}

impl StructVisitor<(u64, Option<u64>, &mzdata::spectrum::Precursor)> for PrecursorBuilder {
    fn append_value(&mut self, item: &(u64, Option<u64>, &mzdata::spectrum::Precursor)) -> bool {
        let (i, j, item) = item;
        self.source_index.append_value(*i);
        self.precursor_index.append_option(*j);
        self.precursor_id.append_option(item.precursor_id.as_ref());
        self.isolation_window.append_value(&item.isolation_window);
        self.activation.append_value(&item.activation);
        true
    }
}

impl VisitorBase for PrecursorBuilder {
    fn fields(&self) -> Vec<FieldRef> {
        vec![
            field!("source_index", DataType::UInt64),
            field!("precursor_index", DataType::UInt64),
            field!("precursor_id", DataType::LargeUtf8),
            field!("isolation_window", self.isolation_window.as_struct_type()),
            field!("activation", self.activation.as_struct_type()),
        ]
    }

    fn append_null(&mut self) {
        self.source_index.append_null();
        self.precursor_index.append_null();
        self.precursor_id.append_null();
        self.isolation_window.append_null();
        self.activation.append_null();
    }

    fn map_metadata_columns(&self) -> Vec<MetadataColumn> {
        let mut cols = Vec::new();
        for mut col in self.isolation_window.map_metadata_columns() {
            col.path.insert(0, "isolation_window".into());
            cols.push(col);
        }
        for mut col in self.activation.map_metadata_columns() {
            col.path.insert(0, "activation".into());
            cols.push(col);
        }
        cols
    }
}

#[derive(Default, Debug)]
pub struct SelectedIonBuilder {
    source_index: UInt64Builder,
    precursor_index: UInt64Builder,
    selected_ion_mz: Float64Builder,
    charge_state: Int32Builder,
    intensity: Float32Builder,
    ion_mobility: Float64Builder,
    ion_mobility_type: CURIEBuilder,
    parameters: ParamListBuilder,
    extra: Vec<Box<dyn StructVisitorBuilder<mzdata::spectrum::SelectedIon>>>,
    curies_to_mask: Vec<CURIE>,
}

impl SelectedIonBuilder {
    pub fn extend_extra_fields(
        &mut self,
        iter: impl IntoIterator<Item = Box<dyn StructVisitorBuilder<mzdata::spectrum::SelectedIon>>>,
    ) {
        self.extra.extend(iter);
    }
}

impl ArrayBuilder for SelectedIonBuilder {
    fn len(&self) -> usize {
        self.source_index.len()
    }

    fn finish(&mut self) -> ArrayRef {
        let fields = self.fields();

        let mut arrays = vec![
            finish_it!(self.source_index),
            finish_it!(self.precursor_index),
            finish_it!(self.selected_ion_mz),
            finish_it!(self.charge_state),
            finish_it!(self.intensity),
            finish_it!(self.ion_mobility),
            self.ion_mobility_type.finish(),
            self.parameters.finish(),
        ];

        finish_extra!(self, arrays);

        Arc::new(StructArray::new(fields.into(), arrays, None))
    }

    fn finish_cloned(&self) -> ArrayRef {
        let fields = self.fields();

        let mut arrays = vec![
            finish_cloned!(self.source_index),
            finish_cloned!(self.precursor_index),
            finish_cloned!(self.selected_ion_mz),
            finish_cloned!(self.charge_state),
            finish_cloned!(self.intensity),
            finish_cloned!(self.ion_mobility),
            self.ion_mobility_type.finish_cloned(),
            self.parameters.finish_cloned(),
        ];

        finish_cloned_extra!(self, arrays);

        Arc::new(StructArray::new(fields.into(), arrays, None))
    }

    anyways!();
}

impl StructVisitor<(u64, Option<u64>, &mzdata::spectrum::SelectedIon)> for SelectedIonBuilder {
    fn append_value(&mut self, item: &(u64, Option<u64>, &mzdata::spectrum::SelectedIon)) -> bool {
        let (i, j, item) = item;
        self.source_index.append_value(*i);
        self.precursor_index.append_option(*j);
        self.selected_ion_mz.append_value(item.mz);
        self.charge_state.append_option(item.charge());
        self.intensity.append_value(item.intensity);

        if let Some(im_val) = item.ion_mobility_type() {
            self.ion_mobility.append_value(im_val.to_f64().unwrap());
            let c = im_val.curie();
            self.ion_mobility_type.append_option(c.as_ref());
            self.curies_to_mask.extend(c);
        } else {
            self.ion_mobility.append_null();
            self.ion_mobility_type.append_null();
        };

        for e in self.extra.iter_mut() {
            if e.append_value(item) {
                self.curies_to_mask.extend(e.associated_curie_to_skip());
            }
        }
        self.parameters
            .append_iter(item.params().iter().filter(|p| {
                if let Some(c) = p.curie() {
                    !self.curies_to_mask.contains(&c)
                } else {
                    true
                }
            }));
        self.curies_to_mask.clear();
        true
    }
}

impl VisitorBase for SelectedIonBuilder {
    fn fields(&self) -> Vec<FieldRef> {
        let mut fields = vec![
            field!("source_index", DataType::UInt64),
            field!("precursor_index", DataType::UInt64),
            field!("selected_ion_mz", DataType::Float64),
            field!("charge_state", DataType::Int32),
            field!("intensity", DataType::Float32),
            field!("ion_mobility_value", DataType::Float64),
            field!("ion_mobility_type", self.ion_mobility_type.as_struct_type()),
        ];
        fields.extend(self.parameters.fields());
        for e in self.extra.iter() {
            fields.extend(e.fields());
        }
        fields
    }

    fn append_null(&mut self) {
        self.source_index.append_null();
        self.precursor_index.append_null();
        self.selected_ion_mz.append_null();
        self.charge_state.append_null();
        self.intensity.append_null();
        self.ion_mobility.append_null();
        self.ion_mobility_type.append_null();
        self.parameters.append_null();
        for e in self.extra.iter_mut() {
            e.append_null();
        }
    }

    fn map_metadata_columns(&self) -> Vec<MetadataColumn> {
        let mut cols = Vec::new();
        let fields = self.fields();
        cols.extend([
            metacol!("selected ion m/z", [fields[2].name()], curie!(MS:1000744), Unit::MZ),
            metacol!("charge state", [fields[3].name()], curie!(MS:1000041)),
            metacol!("intensity", [fields[4].name()], curie!(MS:1000042), Unit::DetectorCounts),
        ]);
        for e in self.extra.iter() {
            cols.extend(e.map_metadata_columns());
        }
        cols
    }
}

#[derive(Debug)]
pub struct AuxiliaryArrayBuilder {
    data: LargeListBuilder<UInt8Builder>,
    name: ParamBuilder,
    data_type: CURIEBuilder,
    compression: CURIEBuilder,
    unit: CURIEBuilder,
    parameters: ParamListBuilder,
    data_processing_ref: LargeStringBuilder,
}

impl Default for AuxiliaryArrayBuilder {
    fn default() -> Self {
        Self {
            data: LargeListBuilder::new(UInt8Builder::new()).with_field(field!(
                "item",
                DataType::UInt8,
                false
            )),
            name: Default::default(),
            data_type: Default::default(),
            compression: Default::default(),
            unit: Default::default(),
            parameters: Default::default(),
            data_processing_ref: Default::default(),
        }
    }
}

impl VisitorBase for AuxiliaryArrayBuilder {
    fn fields(&self) -> Vec<FieldRef> {
        vec![
            field!(
                "data",
                DataType::LargeList(field!("item", DataType::UInt8, false))
            ),
            field!("name", self.name.as_struct_type()),
            field!("data_type", self.data_type.as_struct_type()),
            field!("compression", self.compression.as_struct_type()),
            field!("unit", self.unit.as_struct_type()),
            field!(
                "parameters",
                DataType::LargeList(field!(
                    "item",
                    self.parameters.0.values_ref().as_struct_type()
                ))
            ),
            field!("data_processing_id", DataType::LargeUtf8),
        ]
    }

    fn append_null(&mut self) {
        self.data.append_null();
        self.name.append_null();
        self.data_type.append_null();
        self.compression.append_null();
        self.unit.append_null();
        self.parameters.append_null();
        self.data_processing_ref.append_null();
    }

    fn map_metadata_columns(&self) -> Vec<MetadataColumn> {
        Vec::new()
    }
}

impl StructVisitor<AuxiliaryArray> for AuxiliaryArrayBuilder {
    fn append_value(&mut self, item: &AuxiliaryArray) -> bool {
        self.data.values().append_slice(&item.data);
        self.data.append(true);
        self.name.append_value(&item.name);
        self.data_type.append_value(&item.data_type);
        self.compression.append_value(&item.compression);
        self.unit.append_option(item.unit.as_ref());
        self.parameters.append_value(&item.parameters.as_slice());
        self.data_processing_ref
            .append_option(item.data_processing_ref.as_ref());
        true
    }
}

impl ArrayBuilder for AuxiliaryArrayBuilder {
    anyways!();

    fn len(&self) -> usize {
        self.name.len()
    }

    fn finish(&mut self) -> ArrayRef {
        let fields = self.fields();

        let arrays: Vec<ArrayRef> = vec![
            finish_it!(self.data),
            self.name.finish(),
            self.data_type.finish(),
            self.compression.finish(),
            self.unit.finish(),
            finish_it!(self.parameters),
            finish_it!(self.data_processing_ref),
        ];

        Arc::new(StructArray::new(fields.into(), arrays, None))
    }

    fn finish_cloned(&self) -> ArrayRef {
        let fields = self.fields();

        let arrays: Vec<ArrayRef> = vec![
            finish_cloned!(self.data),
            self.name.finish_cloned(),
            self.data_type.finish_cloned(),
            self.compression.finish_cloned(),
            self.unit.finish_cloned(),
            finish_cloned!(self.parameters),
            finish_cloned!(self.data_processing_ref),
        ];

        Arc::new(StructArray::new(fields.into(), arrays, None))
    }
}

#[derive(Debug)]
pub enum SpectrumVisitor {
    Description(Box<dyn StructVisitorBuilder<SpectrumDescription>>),
}

impl ArrayBuilder for SpectrumVisitor {
    anyways!();

    fn len(&self) -> usize {
        match self {
            Self::Description(builder) => builder.len(),
        }
    }

    fn finish(&mut self) -> ArrayRef {
        match self {
            Self::Description(builder) => builder.finish(),
        }
    }

    fn finish_cloned(&self) -> ArrayRef {
        match self {
            Self::Description(builder) => builder.finish_cloned(),
        }
    }
}

impl StructVisitor<SpectrumDescription> for SpectrumVisitor {
    fn append_value(&mut self, item: &SpectrumDescription) -> bool {
        match self {
            Self::Description(builder) => builder.append_value(item),
        }
    }
}

impl VisitorBase for SpectrumVisitor {
    fn flatten(&self) -> bool {
        match self {
            SpectrumVisitor::Description(struct_visitor_builder) => {
                struct_visitor_builder.flatten()
            }
        }
    }

    fn fields(&self) -> Vec<FieldRef> {
        match self {
            SpectrumVisitor::Description(struct_visitor_builder) => struct_visitor_builder.fields(),
        }
    }

    fn append_null(&mut self) {
        match self {
            SpectrumVisitor::Description(struct_visitor_builder) => {
                struct_visitor_builder.append_null()
            }
        }
    }

    fn map_metadata_columns(&self) -> Vec<MetadataColumn> {
        match self {
            Self::Description(builder) => builder.map_metadata_columns()
        }
    }
}

#[derive(Default, Debug)]
pub struct SpectrumDetailsBuilder {
    index: UInt64Builder,
    id: LargeStringBuilder,
    ms_level: UInt8Builder,
    time: Float64Builder,
    polarity: Int8Builder,
    spectrum_representation: CURIEBuilder,
    spectrum_type: CURIEBuilder,
    lowest_observed_mz: Float64Builder,
    highest_observed_mz: Float64Builder,
    number_of_data_points: UInt64Builder,
    number_of_peaks: UInt64Builder,
    base_peak_mz: Float64Builder,
    base_peak_intensity: Float32Builder,
    total_ion_current: Float32Builder,
    data_processing_ref: LargeStringBuilder,
    parameters: ParamListBuilder,
    auxiliary_arrays: LargeListBuilder<AuxiliaryArrayBuilder>,
    number_of_auxiliary_arrays: UInt32Builder,
    mz_delta_model: LargeListBuilder<Float64Builder>,
    extra: Vec<SpectrumVisitor>,

    curies_to_mask: Vec<mzdata::params::CURIE>,
}

impl SpectrumDetailsBuilder {
    pub fn extend_extra_fields(&mut self, iter: impl IntoIterator<Item = SpectrumVisitor>) {
        self.extra.extend(iter);
    }
}

impl VisitorBase for SpectrumDetailsBuilder {
    fn fields(&self) -> Vec<FieldRef> {
        let mut fields = vec![
            field!("index", DataType::UInt64),
            field!("id", DataType::LargeUtf8),
            field!("ms_level", DataType::UInt8),
            field!("time", DataType::Float64),
            field!("scan_polarity", DataType::Int8),
            field!("spectrum_representation", self.spectrum_representation.as_struct_type()),
            field!("spectrum_type", self.spectrum_type.as_struct_type()),
            field!("lowest_observed_mz", DataType::Float64),
            field!("highest_observed_mz", DataType::Float64),
            field!("number_of_data_points", DataType::UInt64),
            field!("number_of_peaks", DataType::UInt64),
            field!("base_peak_mz", DataType::Float64),
            field!("base_peak_intensity", DataType::Float32),
            field!("total_ion_current", DataType::Float32),
            field!("data_processing_id", DataType::LargeUtf8),
            field!(
                "parameters",
                DataType::LargeList(field!(
                    "item",
                    self.parameters.0.values_ref().as_struct_type()
                ))
            ),
            field!(
                "auxiliary_arrays",
                DataType::LargeList(field!(
                    "item",
                    self.auxiliary_arrays.values_ref().as_struct_type()
                ))
            ),
            field!("number_of_auxiliary_arrays", DataType::UInt32),
            field!(
                "mz_delta_model",
                DataType::LargeList(field!("item", DataType::Float64))
            ),
        ];

        for e in self.extra.iter() {
            fields.extend(e.fields());
        }
        fields
    }

    fn append_null(&mut self) {
        self.index.append_null();
        self.id.append_null();
        self.ms_level.append_null();
        self.time.append_null();
        self.polarity.append_null();
        self.spectrum_representation.append_null();
        self.spectrum_type.append_null();
        self.lowest_observed_mz.append_null();
        self.highest_observed_mz.append_null();
        self.number_of_data_points.append_null();
        self.number_of_peaks.append_null();
        self.base_peak_mz.append_null();
        self.base_peak_intensity.append_null();
        self.total_ion_current.append_null();
        self.data_processing_ref.append_null();
        self.parameters.append_null();
        self.auxiliary_arrays.append_null();
        self.number_of_auxiliary_arrays.append_null();
        self.mz_delta_model.append_null();
        for e in self.extra.iter_mut() {
            e.append_null();
        }
    }

    fn map_metadata_columns(&self) -> Vec<MetadataColumn> {
        let mut cols = Vec::new();
        let fields = self.fields();
        cols.extend([
            metacol!("ms level", [fields[2].name()], curie!(MS:1000511)),
            metacol!("scan polarity", [fields[4].name()], curie!(MS:1000465)),
            metacol!("spectrum representation", [fields[5].name()], curie!(MS:1000525)),
            metacol!("spectrum type", [fields[6].name()], curie!(MS:1000559)),
            metacol!("lowest observed m/z", [fields[7].name()], curie!(MS:1000528), Unit::MZ),
            metacol!("highest observed m/z", [fields[8].name()], curie!(MS:1000527), Unit::MZ),
            metacol!("number of data points", [fields[9].name()], curie!(MS:1003060)),
            metacol!("number of peaks", [fields[10].name()], curie!(MS:1003059)),
            metacol!("base peak m/z", [fields[11].name()], curie!(MS:1000504), Unit::MZ),
            metacol!("base peak intensity", [fields[12].name()], curie!(MS:1000505), Unit::DetectorCounts),
            metacol!("total ion current", [fields[13].name()], curie!(MS:1000285), Unit::DetectorCounts)
        ]);
        for e in self.extra.iter() {
            cols.extend(e.map_metadata_columns());
        }
        cols
    }
}

const BUILTIN_SPECTRUM_PARAMS: &[CURIE] = &[
    curie!(MS:1000504),
    curie!(MS:1000505),
    curie!(MS:1000285),
    curie!(MS:1000527),
    curie!(MS:1000528),
    curie!(MS:1000579),
    curie!(MS:1000580),
];

impl SpectrumDetailsBuilder {
    fn raw_summaries<
        C: CentroidLike,
        D: DeconvolutedCentroidLike,
        S: SpectrumLike<C, D> + 'static,
    >(
        &self,
        item: &S,
    ) -> mzdata::spectrum::SpectrumSummary {
        let summaries = item
            .raw_arrays()
            .map(|v| RefPeakDataLevel::<C, D>::RawData(v).fetch_summaries())
            .unwrap_or_else(|| item.peaks().fetch_summaries());
        summaries
    }

    fn peak_summaries<
        C: CentroidLike,
        D: DeconvolutedCentroidLike,
        S: SpectrumLike<C, D> + 'static,
    >(
        &self,
        item: &S,
    ) -> Option<mzdata::spectrum::SpectrumSummary> {
        let peaks = item.peaks();
        match &peaks {
            RefPeakDataLevel::Missing | RefPeakDataLevel::RawData(_) => None,
            RefPeakDataLevel::Centroid(_) | RefPeakDataLevel::Deconvoluted(_) => {
                Some(peaks.fetch_summaries())
            }
        }
    }

    pub fn append_value<
        C: CentroidLike,
        D: DeconvolutedCentroidLike,
        S: SpectrumLike<C, D> + 'static,
    >(
        &mut self,
        index: u64,
        item: &S,
        entry_derived: EntryMetadataDerivedFromData,
    ) -> bool {
        self.curies_to_mask.clear();

        let summaries = self.raw_summaries(item);
        let pk_summaries = self.peak_summaries(item);

        let n_pts = entry_derived
            .data_point_count
            .unwrap_or_else(|| summaries.len()) as u64;
        let n_pks = entry_derived
            .peak_count
            .or(pk_summaries.as_ref().map(|p| p.len()));
        let base_peak_mz = if n_pts > 0 {
            Some(summaries.base_peak.mz)
        } else {
            pk_summaries.as_ref().map(|s| s.base_peak.mz)
        };
        let base_peak_intensity = if n_pts > 0 {
            Some(summaries.base_peak.intensity)
        } else {
            pk_summaries.as_ref().map(|s| s.base_peak.intensity)
        };

        let spectrum_type = if let Some(v) = item
            .spectrum_type()
            .map(|t| crate::CURIE::from(t.to_param().curie().unwrap()))
        {
            v
        } else {
            match item.ms_level() {
                0 => {
                    log::warn!(
                        "Couldn't infer spectrum type from MS level, no explicit type specified. Defaulting to MS1 spectrum (MS:1000579)"
                    );
                    curie!(MS:1000579)
                }
                1 => curie!(MS:1000579),
                _ => curie!(MS:1000580),
            }
        };

        self.index.append_value(index);
        self.id.append_value(item.id());
        self.ms_level.append_value(item.ms_level());
        self.time.append_value(item.start_time());
        self.polarity.append_option(match item.polarity() {
            ScanPolarity::Positive => Some(1),
            ScanPolarity::Negative => Some(-1),
            ScanPolarity::Unknown => None,
        });
        self.spectrum_representation
            .append_option(match item.signal_continuity() {
                mzdata::spectrum::SignalContinuity::Unknown => None,
                mzdata::spectrum::SignalContinuity::Centroid => Some(&curie!(MS:1000127)),
                mzdata::spectrum::SignalContinuity::Profile => Some(&curie!(MS:1000128)),
            });

        self.spectrum_type.append_value(&spectrum_type);

        self.lowest_observed_mz.append_value(summaries.mz_range.0);
        self.highest_observed_mz.append_value(summaries.mz_range.1);

        self.base_peak_mz.append_option(base_peak_mz);
        self.base_peak_intensity.append_option(base_peak_intensity);
        self.total_ion_current.append_value(summaries.tic);
        match item.signal_continuity() {
            SignalContinuity::Unknown => {
                log::warn!(
                    "Signal continuity was unknown for {index} = {}, assuming profile",
                    item.id()
                );
                self.number_of_data_points.append_value(n_pts as u64);
                self.number_of_peaks.append_null();
            }
            SignalContinuity::Centroid => {
                let n = if n_pts == 0 {
                    n_pks.unwrap_or_default() as u64
                } else {
                    n_pts as u64
                };
                self.number_of_peaks.append_value(n);
                self.number_of_data_points.append_null();
            }
            SignalContinuity::Profile => {
                let pk_pts = pk_summaries.as_ref().map(|v| v.len() as u64);
                self.number_of_peaks.append_option(pk_pts);
                self.number_of_data_points.append_value(n_pts as u64);
            }
        }

        self.data_processing_ref.append_null();

        if let Some(arrays) = entry_derived.auxiliary_arrays.as_ref() {
            let b = self.auxiliary_arrays.values();
            for a in arrays {
                b.append_value(a);
            }
            self.auxiliary_arrays.append(true);
        } else {
            self.auxiliary_arrays.append_null();
        }

        self.number_of_auxiliary_arrays.append_value(
            entry_derived
                .auxiliary_arrays
                .map(|v| v.len())
                .unwrap_or_default() as u32,
        );

        match entry_derived.mz_delta_model {
            Some(params) => {
                self.mz_delta_model.values().append_slice(&params);
                self.mz_delta_model.append(true);
            }
            _ => {
                self.mz_delta_model.append_null();
            }
        };

        for e in self.extra.iter_mut() {
            match e {
                SpectrumVisitor::Description(builder) => {
                    if builder.append_value(item.description()) {
                        self.curies_to_mask
                            .extend(builder.associated_curie_to_skip());
                    }
                }
            }
        }

        self.curies_to_mask
            .extend_from_slice(BUILTIN_SPECTRUM_PARAMS);

        self.parameters
            .append_iter(item.params().iter().filter(|p| {
                if let Some(c) = p.curie() {
                    !self.curies_to_mask.contains(&c)
                } else {
                    true
                }
            }));
        self.curies_to_mask.clear();
        true
    }
}

impl ArrayBuilder for SpectrumDetailsBuilder {
    anyways!();

    fn len(&self) -> usize {
        self.index.len()
    }

    fn finish(&mut self) -> ArrayRef {
        let schema = self.fields();

        let mut arrays: Vec<ArrayRef> = vec![
            finish_it!(self.index),
            finish_it!(self.id),
            finish_it!(self.ms_level),
            finish_it!(self.time),
            finish_it!(self.polarity),
            self.spectrum_representation.finish(),
            self.spectrum_type.finish(),
            finish_it!(self.lowest_observed_mz),
            finish_it!(self.highest_observed_mz),
            finish_it!(self.number_of_data_points),
            finish_it!(self.number_of_peaks),
            finish_it!(self.base_peak_mz),
            finish_it!(self.base_peak_intensity),
            finish_it!(self.total_ion_current),
            finish_it!(self.data_processing_ref),
            self.parameters.finish(),
            finish_it!(self.auxiliary_arrays),
            finish_it!(self.number_of_auxiliary_arrays),
            finish_it!(self.mz_delta_model),
        ];

        finish_extra!(self, arrays);

        Arc::new(StructArray::new(schema.into(), arrays, None))
    }

    fn finish_cloned(&self) -> ArrayRef {
        let schema = self.fields();

        let mut arrays: Vec<ArrayRef> = vec![
            finish_cloned!(self.index),
            finish_cloned!(self.id),
            finish_cloned!(self.ms_level),
            finish_cloned!(self.time),
            finish_cloned!(self.polarity),
            self.spectrum_representation.finish_cloned(),
            self.spectrum_type.finish_cloned(),
            finish_cloned!(self.lowest_observed_mz),
            finish_cloned!(self.highest_observed_mz),
            finish_cloned!(self.number_of_data_points),
            finish_cloned!(self.number_of_peaks),
            finish_cloned!(self.base_peak_mz),
            finish_cloned!(self.base_peak_intensity),
            finish_cloned!(self.total_ion_current),
            finish_cloned!(self.data_processing_ref),
            self.parameters.finish_cloned(),
            finish_cloned!(self.auxiliary_arrays),
            finish_cloned!(self.number_of_auxiliary_arrays),
            finish_cloned!(self.mz_delta_model),
        ];

        finish_cloned_extra!(self, arrays);

        Arc::new(StructArray::new(schema.into(), arrays, None))
    }
}

#[derive(Default, Debug)]
pub struct SpectrumBuilder {
    spectrum_index_counter: u64,
    precursor_index_counter: u64,
    scan_index_counter: u64,
    pub(crate) spectrum: SpectrumDetailsBuilder,
    pub(crate) scan: ScanBuilder,
    pub(crate) precursor: PrecursorBuilder,
    pub(crate) selected_ion: SelectedIonBuilder,
    id_to_index: HashMap<String, u64>,
}

impl SpectrumBuilder {
    pub fn spectrum_metadata_columns(&self) -> Vec<MetadataColumn> {
        self.spectrum.map_metadata_columns()
    }

    pub fn scan_metadata_columns(&self) -> Vec<MetadataColumn> {
        self.scan.map_metadata_columns()
    }

    pub fn precursor_metadata_columns(&self) -> Vec<MetadataColumn> {
        self.precursor.map_metadata_columns()
    }

    pub fn selected_ion_metadata_columns(&self) -> Vec<MetadataColumn> {
        self.selected_ion.map_metadata_columns()
    }

    pub fn finish_spectrum(&mut self) -> ArrayRef {
        self.spectrum.finish()
    }

    pub fn finish_scan(&mut self) -> ArrayRef {
        self.scan.finish()
    }

    pub fn finish_precursor(&mut self) -> ArrayRef {
        self.precursor.finish()
    }

    pub fn finish_selected_ion(&mut self) -> ArrayRef {
        self.selected_ion.finish()
    }

    pub fn add_visitors_from(&mut self, visitors: SpectrumFieldVisitors) {
        self.spectrum.extend_extra_fields(visitors.spectrum_fields);
        self.scan.extend_extra_fields(visitors.spectrum_scan_fields);
        self.selected_ion
            .extend_extra_fields(visitors.spectrum_selected_ion_fields);
        self.precursor
            .extend_extra_activation_fields(visitors.spectrum_activation_fields);
    }

    pub fn add_imaging_position_visitors(&mut self) {
        let visitors: [Box<dyn StructVisitorBuilder<mzdata::spectrum::ScanEvent>>; _] = [
            CustomBuilderFromParameter::from_spec(
                mzdata::curie!(IMS:1000050),
                "position x",
                DataType::UInt32,
            ),
            CustomBuilderFromParameter::from_spec(
                mzdata::curie!(IMS:1000051),
                "position y",
                DataType::UInt32,
            ),
        ]
        .map(|v| Box::new(v) as Box<dyn StructVisitorBuilder<mzdata::spectrum::ScanEvent>>);
        self.scan.extend_extra_fields(visitors);
    }

    pub fn append_value<
        C: CentroidLike,
        D: DeconvolutedCentroidLike,
        S: SpectrumLike<C, D> + 'static,
    >(
        &mut self,
        item: &S,
        entry_derived_metadata: EntryMetadataDerivedFromData,
    ) -> bool {
        self.id_to_index
            .insert(item.id().to_string(), self.spectrum_index_counter);
        let out =
            self.spectrum
                .append_value(self.spectrum_index_counter, item, entry_derived_metadata);
        for s in item.acquisition().scans.iter() {
            self.scan
                .append_value(&(self.spectrum_index_counter, self.scan_index_counter, s));
            self.scan_index_counter += 1;
        }
        for precursor in item.precursor_iter() {
            let precursor_index = precursor
                .precursor_id
                .as_ref()
                .and_then(|s| self.id_to_index.get(s))
                .copied();
            self.precursor
                .append_value(&(self.spectrum_index_counter, precursor_index, precursor));
            for ion in precursor.iter() {
                self.selected_ion.append_value(&(
                    self.spectrum_index_counter,
                    precursor_index,
                    ion,
                ));
            }
            self.precursor_index_counter += 1;
        }
        self.spectrum_index_counter += 1;
        out
    }

    pub fn add_spectrum_param_field<T: StructVisitorBuilder<SpectrumDescription>>(
        &mut self,
        visitor: T,
    ) {
        self.spectrum
            .extra
            .push(SpectrumVisitor::Description(Box::new(visitor)));
    }

    pub fn add_selected_ion_field(
        &mut self,
        visitor: impl StructVisitorBuilder<mzdata::spectrum::SelectedIon>,
    ) {
        self.selected_ion.extra.push(Box::new(visitor));
    }

    pub fn add_scan_field(
        &mut self,
        visitor: impl StructVisitorBuilder<mzdata::spectrum::ScanEvent>,
    ) {
        self.scan.extra.push(Box::new(visitor));
    }

    pub fn add_activation_field<T: StructVisitorBuilder<mzdata::spectrum::Activation>>(
        &mut self,
        builder: Box<T>,
    ) {
        self.precursor.activation.extra.push(builder);
    }

    pub fn index_counter(&self) -> u64 {
        self.spectrum_index_counter
    }

    pub fn precursor_index_counter(&self) -> u64 {
        self.precursor_index_counter
    }

    pub fn scan_index_counter(&self) -> u64 {
        self.scan_index_counter
    }

    fn check_lengths_equal(&self) -> bool {
        let n = self
            .spectrum
            .len()
            .max(self.scan.len())
            .max(self.precursor.len())
            .max(self.selected_ion.len());
        self.spectrum.len() == n
            && self.scan.len() == n
            && self.precursor.len() == n
            && self.selected_ion.len() == n
    }

    pub fn equalize_lengths(&mut self) {
        let n = self
            .spectrum
            .len()
            .max(self.scan.len())
            .max(self.precursor.len())
            .max(self.selected_ion.len());

        while n > self.spectrum.len() {
            self.spectrum.append_null();
        }

        while n > self.scan.len() {
            self.scan.append_null();
        }

        while n > self.precursor.len() {
            self.precursor.append_null();
        }

        while n > self.selected_ion.len() {
            self.selected_ion.append_null();
        }
    }

    pub fn spectrum(&self) -> &SpectrumDetailsBuilder {
        &self.spectrum
    }

    pub fn scan(&self) -> &ScanBuilder {
        &self.scan
    }

    pub fn precursor(&self) -> &PrecursorBuilder {
        &self.precursor
    }

    pub fn selected_ion(&self) -> &SelectedIonBuilder {
        &self.selected_ion
    }
}

impl VisitorBase for SpectrumBuilder {
    fn fields(&self) -> Vec<FieldRef> {
        vec![
            field!(SPECTRUM, self.spectrum.as_struct_type()),
            field!(SCAN, self.scan.as_struct_type()),
            field!(PRECURSOR, self.precursor.as_struct_type()),
            field!(SELECTED_ION, self.selected_ion.as_struct_type()),
        ]
    }

    fn append_null(&mut self) {
        self.spectrum.append_null();
        self.scan.append_null();
        self.precursor.append_null();
        self.selected_ion.append_null();
    }

    fn map_metadata_columns(&self) -> Vec<MetadataColumn> {
        Vec::new()
    }
}

impl ArrayBuilder for SpectrumBuilder {
    anyways!();

    fn len(&self) -> usize {
        self.spectrum.len()
    }

    fn finish(&mut self) -> ArrayRef {
        self.equalize_lengths();

        let fields = self.fields();
        let arrays = vec![
            self.spectrum.finish(),
            self.scan.finish(),
            self.precursor.finish(),
            self.selected_ion.finish(),
        ];

        Arc::new(StructArray::new(fields.into(), arrays, None))
    }

    fn finish_cloned(&self) -> ArrayRef {
        let fields = self.fields();
        assert!(
            self.check_lengths_equal(),
            "Verify all facets are of equal length, call `equalize_lengths` first!"
        );
        let arrays = vec![
            self.spectrum.finish_cloned(),
            self.scan.finish_cloned(),
            self.precursor.finish_cloned(),
            self.selected_ion.finish_cloned(),
        ];
        Arc::new(StructArray::new(fields.into(), arrays, None))
    }
}

#[derive(Debug, Default)]
pub struct ChromatogramDetailsBuilder {
    index: UInt64Builder,
    id: LargeStringBuilder,
    polarity: Int8Builder,
    chromatogram_type: CURIEBuilder,

    data_processing_ref: LargeStringBuilder,
    parameters: ParamListBuilder,
    auxiliary_arrays: LargeListBuilder<AuxiliaryArrayBuilder>,
    number_of_auxiliary_arrays: UInt32Builder,
    number_of_data_points: UInt64Builder,

    extra: Vec<Box<dyn StructVisitorBuilder<Chromatogram>>>,
    curies_to_mask: Vec<CURIE>,
}

impl ChromatogramDetailsBuilder {
    fn append_value(
        &mut self,
        index: u64,
        item: &Chromatogram,
        auxiliary_arrays: Option<Vec<AuxiliaryArray>>,
    ) -> bool {
        self.index.append_value(index);
        self.id.append_value(item.id());
        self.polarity.append_value(match item.polarity() {
            ScanPolarity::Positive => 1,
            ScanPolarity::Negative => -1,
            ScanPolarity::Unknown => 0,
        });
        self.chromatogram_type
            .append_value(&item.chromatogram_type().to_curie());
        self.data_processing_ref.append_null();

        if let Some(aux_arrays) = auxiliary_arrays {
            self.number_of_auxiliary_arrays
                .append_value(aux_arrays.len() as u32);
            let b = self.auxiliary_arrays.values();
            for a in aux_arrays {
                b.append_value(&a);
            }
            self.auxiliary_arrays.append(true);
        } else {
            self.number_of_auxiliary_arrays.append_value(0);
            self.auxiliary_arrays.append_null();
        }

        let n_pts = item.time().map(|a| a.len() as u64).ok();
        self.number_of_data_points.append_option(n_pts);

        for e in self.extra.iter_mut() {
            if e.append_value(item) {
                self.curies_to_mask.extend(e.associated_curie_to_skip());
            }
        }
        self.parameters
            .append_iter(item.params().iter().filter(|p| {
                if let Some(c) = p.curie() {
                    !self.curies_to_mask.contains(&c)
                } else {
                    true
                }
            }));
        self.curies_to_mask.clear();
        true
    }
}

impl VisitorBase for ChromatogramDetailsBuilder {
    fn fields(&self) -> Vec<FieldRef> {
        let mut fields = vec![
            field!("index", DataType::UInt64),
            field!("id", DataType::LargeUtf8),
            field!("scan_polarity", DataType::Int8),
            field!("chromatogram_type", self.chromatogram_type.as_struct_type()),
            field!("data_processing_id", DataType::LargeUtf8),
            field!("number_of_data_points", DataType::UInt64),
        ];
        fields.extend(self.parameters.fields());
        fields.extend([
            field!(
                "auxiliary_arrays",
                DataType::LargeList(field!(
                    "item",
                    self.auxiliary_arrays.values_ref().as_struct_type()
                ))
            ),
            field!("number_of_auxiliary_arrays", DataType::UInt32),
        ]);
        for e in self.extra.iter() {
            fields.extend(e.fields())
        }
        fields
    }

    fn append_null(&mut self) {
        self.index.append_null();
        self.id.append_null();
        self.polarity.append_null();
        self.chromatogram_type.append_null();
        self.data_processing_ref.append_null();
        self.number_of_data_points.append_null();
        self.auxiliary_arrays.append_null();
        self.number_of_auxiliary_arrays.append_null();
        for e in self.extra.iter_mut() {
            e.append_null();
        }
    }

    fn map_metadata_columns(&self) -> Vec<MetadataColumn> {
        let mut cols = Vec::new();
        let fields = self.fields();

        cols.extend([
            metacol!("scan polarity", [fields[2].name()], curie!(MS:1000465)),
            metacol!("chromatogram type", [fields[3].name()], curie!(MS:1000626)),
            metacol!("number of data points", [fields[5].name()], curie!(MS:1003060)),
        ]);
        for e in self.extra.iter() {
            cols.extend(e.map_metadata_columns());
        }
        cols
    }
}

impl ArrayBuilder for ChromatogramDetailsBuilder {
    anyways!();

    fn len(&self) -> usize {
        self.index.len()
    }

    fn finish(&mut self) -> ArrayRef {
        let fields = self.fields();

        let mut arrays = vec![
            finish_it!(self.index),
            finish_it!(self.id),
            finish_it!(self.polarity),
            self.chromatogram_type.finish(),
            finish_it!(self.data_processing_ref),
            finish_it!(self.number_of_data_points),
            self.parameters.finish(),
            finish_it!(self.auxiliary_arrays),
            finish_it!(self.number_of_auxiliary_arrays),
        ];

        finish_extra!(self, arrays);

        Arc::new(StructArray::new(fields.into(), arrays, None))
    }

    fn finish_cloned(&self) -> ArrayRef {
        let fields = self.fields();

        let mut arrays = vec![
            finish_cloned!(self.index),
            finish_cloned!(self.id),
            finish_cloned!(self.polarity),
            self.chromatogram_type.finish_cloned(),
            finish_cloned!(self.data_processing_ref),
            finish_cloned!(self.number_of_data_points),
            self.parameters.finish_cloned(),
            finish_cloned!(self.auxiliary_arrays),
            finish_cloned!(self.number_of_auxiliary_arrays),
        ];

        finish_cloned_extra!(self, arrays);

        Arc::new(StructArray::new(fields.into(), arrays, None))
    }
}

#[derive(Default, Debug)]
pub struct ChromatogramBuilder {
    chromatogram_index_counter: u64,
    precursor_index_counter: u64,
    chromatogram: ChromatogramDetailsBuilder,
    precursor: PrecursorBuilder,
    selected_ion: SelectedIonBuilder,
    id_to_index: HashMap<String, u64>,
}

impl VisitorBase for ChromatogramBuilder {
    fn fields(&self) -> Vec<FieldRef> {
        vec![
            field!(CHROMATOGRAM, self.chromatogram.as_struct_type()),
            field!(PRECURSOR, self.precursor.as_struct_type()),
            field!(SELECTED_ION, self.selected_ion.as_struct_type()),
        ]
    }

    fn append_null(&mut self) {
        self.chromatogram.append_null();
        self.precursor.append_null();
        self.selected_ion.append_null();
    }

    fn map_metadata_columns(&self) -> Vec<MetadataColumn> {
        Vec::new()
    }
}

impl ChromatogramBuilder {
    pub fn chromatogram_metadata_columns(&self) -> Vec<MetadataColumn> {
        self.chromatogram.map_metadata_columns()
    }

    pub fn precursor_metadata_columns(&self) -> Vec<MetadataColumn> {
        self.precursor.map_metadata_columns()
    }

    pub fn selected_ion_metadata_columns(&self) -> Vec<MetadataColumn> {
        self.selected_ion.map_metadata_columns()
    }

    pub fn finish_chromatogram(&mut self) -> ArrayRef {
        self.chromatogram.finish()
    }

    pub fn finish_precursor(&mut self) -> ArrayRef {
        self.precursor.finish()
    }

    pub fn finish_selected_ion(&mut self) -> ArrayRef {
        self.selected_ion.finish()
    }


    pub fn precursor(&self) -> &PrecursorBuilder {
        &self.precursor
    }

    pub fn selected_ion(&self) -> &SelectedIonBuilder {
        &self.selected_ion
    }

    pub fn chromatogram(&self) -> &ChromatogramDetailsBuilder {
        &self.chromatogram
    }

    pub fn append_value(
        &mut self,
        item: &Chromatogram,
        auxiliary_arrays: Option<Vec<AuxiliaryArray>>,
    ) -> bool {
        self.id_to_index
            .insert(item.id().to_string(), self.chromatogram_index_counter);
        let out =
            self.chromatogram
                .append_value(self.chromatogram_index_counter, item, auxiliary_arrays);
        for precursor in item.precursor_iter() {
            let precursor_index = precursor
                .precursor_id
                .as_ref()
                .and_then(|s| self.id_to_index.get(s).copied());
            self.precursor.append_value(&(
                self.chromatogram_index_counter,
                precursor_index,
                precursor,
            ));
            for ion in precursor.iter() {
                self.selected_ion.append_value(&(
                    self.chromatogram_index_counter,
                    precursor_index,
                    ion,
                ));
            }
            self.precursor_index_counter += 1;
        }
        self.chromatogram_index_counter += 1;
        out
    }

    pub fn index_counter(&self) -> u64 {
        self.chromatogram_index_counter
    }

    pub fn precursor_index_counter(&self) -> u64 {
        self.precursor_index_counter
    }

    pub fn add_activation_field<T: StructVisitorBuilder<mzdata::spectrum::Activation>>(
        &mut self,
        builder: Box<T>,
    ) {
        self.precursor.activation.extra.push(builder);
    }
}

impl ArrayBuilder for ChromatogramBuilder {
    fn len(&self) -> usize {
        self.chromatogram.len()
    }

    fn finish(&mut self) -> ArrayRef {
        let fields = self.fields();
        let n = self
            .chromatogram
            .len()
            .max(self.precursor.len())
            .max(self.selected_ion.len());

        while n > self.chromatogram.len() {
            self.chromatogram.append_null();
        }

        while n > self.precursor.len() {
            self.precursor.append_null();
        }

        while n > self.selected_ion.len() {
            self.selected_ion.append_null();
        }

        let arrays = vec![
            self.chromatogram.finish(),
            self.precursor.finish(),
            self.selected_ion.finish(),
        ];

        Arc::new(StructArray::new(fields.into(), arrays, None))
    }

    fn finish_cloned(&self) -> ArrayRef {
        todo!()
    }

    anyways!();
}

#[derive(Debug, Default)]
pub struct WavelengthSpectrumDetailsBuilder {
    index: UInt64Builder,
    id: LargeStringBuilder,
    time: Float64Builder,
    spectrum_type: CURIEBuilder,
    spectrum_representation: CURIEBuilder,
    lowest_observed_wavelength: Float64Builder,
    highest_observed_wavelength: Float64Builder,
    number_of_data_points: UInt64Builder,
    base_peak_mz: Float64Builder,
    base_peak_intensity: Float32Builder,
    total_ion_current: Float32Builder,
    data_processing_ref: LargeStringBuilder,
    parameters: ParamListBuilder,
    auxiliary_arrays: LargeListBuilder<AuxiliaryArrayBuilder>,
    number_of_auxiliary_arrays: UInt32Builder,
    extra: Vec<SpectrumVisitor>,

    curies_to_mask: Vec<mzdata::params::CURIE>,
}

impl WavelengthSpectrumDetailsBuilder {
    pub fn extend_extra_fields(&mut self, iter: impl IntoIterator<Item = SpectrumVisitor>) {
        self.extra.extend(iter);
    }

    fn raw_summaries<
        C: CentroidLike,
        D: DeconvolutedCentroidLike,
        S: SpectrumLike<C, D> + 'static,
    >(
        &self,
        item: &S,
    ) -> mzdata::spectrum::SpectrumSummary {
        let mut summaries: mzdata::spectrum::SpectrumSummary = Default::default();
        if let Some(arrays) = item.raw_arrays() {
            if let Some(waves) = arrays
                .get(&ArrayType::WavelengthArray)
                .and_then(|v| v.to_f32().ok())
            {
                let intensities = arrays.intensities().unwrap();
                summaries.count = waves.len();

                if !waves.is_empty() && !intensities.is_empty() {
                    summaries.tic = 0.0;
                    for (i, int) in intensities.iter().copied().enumerate() {
                        if summaries.base_peak.intensity < int {
                            summaries.base_peak.intensity = int;
                            summaries.base_peak.mz = waves[i] as f64;
                        }
                        summaries.tic += int;
                    }
                    summaries.mz_range.0 = *waves.first().unwrap() as f64;
                    summaries.mz_range.1 = *waves.last().unwrap() as f64;
                }
            }
        }
        summaries
    }

    pub fn append_value<
        C: CentroidLike,
        D: DeconvolutedCentroidLike,
        S: SpectrumLike<C, D> + 'static,
    >(
        &mut self,
        index: u64,
        item: &S,
        auxiliary_arrays: Option<Vec<AuxiliaryArray>>,
    ) -> bool {
        self.curies_to_mask.clear();

        let summaries = self.raw_summaries(item);

        let n_pts = summaries.len();
        let base_peak_mz = if n_pts > 0 {
            Some(summaries.base_peak.mz)
        } else {
            None
        };
        let base_peak_intensity = if n_pts > 0 {
            Some(summaries.base_peak.intensity)
        } else {
            None
        };

        let spectrum_type = item
            .spectrum_type()
            .map(|t| crate::CURIE::from(t.to_param().curie().unwrap()));

        self.index.append_value(index);
        self.id.append_value(item.id());
        self.time.append_value(item.start_time());
        self.spectrum_type.append_option(spectrum_type.as_ref());
        self.spectrum_representation
            .append_value(&mzdata::curie!(MS:1000128));

        self.lowest_observed_wavelength
            .append_value(summaries.mz_range.0);
        self.highest_observed_wavelength
            .append_value(summaries.mz_range.1);

        self.base_peak_mz.append_option(base_peak_mz);
        self.base_peak_intensity.append_option(base_peak_intensity);
        self.total_ion_current.append_value(summaries.tic);
        self.number_of_data_points.append_value(n_pts as u64);

        self.data_processing_ref.append_null();

        if let Some(arrays) = auxiliary_arrays.as_ref() {
            let b = self.auxiliary_arrays.values();
            for a in arrays {
                b.append_value(a);
            }
            self.auxiliary_arrays.append(true);
        } else {
            self.auxiliary_arrays.append_null();
        }

        self.number_of_auxiliary_arrays
            .append_value(auxiliary_arrays.map(|v| v.len()).unwrap_or_default() as u32);

        for e in self.extra.iter_mut() {
            match e {
                SpectrumVisitor::Description(builder) => {
                    if builder.append_value(item.description()) {
                        self.curies_to_mask
                            .extend(builder.associated_curie_to_skip());
                    }
                }
            }
        }

        self.curies_to_mask
            .extend_from_slice(BUILTIN_SPECTRUM_PARAMS);

        self.parameters
            .append_iter(item.params().iter().filter(|p| {
                if let Some(c) = p.curie() {
                    !self.curies_to_mask.contains(&c)
                } else {
                    true
                }
            }));
        self.curies_to_mask.clear();
        true
    }
}

impl VisitorBase for WavelengthSpectrumDetailsBuilder {
    fn fields(&self) -> Vec<FieldRef> {
        let mut fields = vec![
            field!("index", DataType::UInt64),
            field!("id", DataType::LargeUtf8),
            field!("time", DataType::Float64),
            field!(
                "spectrum_type",
                self.spectrum_type.as_struct_type()
            ),
            field!(
                "spectrum_representation",
                self.spectrum_representation.as_struct_type()
            ),
            field!("lowest_observed_wavelength", DataType::Float64),
            field!(
                "highest_observed_wavelength",
                DataType::Float64
            ),
            field!("number_of_data_points", DataType::UInt64),
            field!(
                "lambda_max",
                DataType::Float64
            ),
            field!(
                "base_peak_intensity",
                DataType::Float32
            ),
            field!(
                "total_ion_current",
                DataType::Float32
            ),
            field!("data_processing_id", DataType::LargeUtf8),
            field!(
                "parameters",
                DataType::LargeList(field!(
                    "item",
                    self.parameters.0.values_ref().as_struct_type()
                ))
            ),
            field!(
                "auxiliary_arrays",
                DataType::LargeList(field!(
                    "item",
                    self.auxiliary_arrays.values_ref().as_struct_type()
                ))
            ),
            field!("number_of_auxiliary_arrays", DataType::UInt32),
        ];

        for e in self.extra.iter() {
            fields.extend(e.fields());
        }
        fields
    }

    fn append_null(&mut self) {
        self.index.append_null();
        self.id.append_null();
        self.time.append_null();
        self.spectrum_type.append_null();
        self.spectrum_representation.append_null();
        self.lowest_observed_wavelength.append_null();
        self.highest_observed_wavelength.append_null();
        self.number_of_data_points.append_null();
        self.base_peak_mz.append_null();
        self.base_peak_intensity.append_null();
        self.total_ion_current.append_null();
        self.data_processing_ref.append_null();
        self.parameters.append_null();
        self.auxiliary_arrays.append_null();
        self.number_of_auxiliary_arrays.append_null();
        for e in self.extra.iter_mut() {
            e.append_null();
        }
    }

    fn map_metadata_columns(&self) -> Vec<MetadataColumn> {
        let mut cols = Vec::new();
        let fields = self.fields();
        cols.extend([
            metacol!("spectrum type", [fields[3].name()], curie!(MS:1000559)),
            metacol!("spectrum representation", [fields[4].name()], curie!(MS:1000525)),
            metacol!("lowest observed wavelength", [fields[5].name()], curie!(MS:1000619), Unit::Nanometer),
            metacol!("highest observed wavelength", [fields[6].name()], curie!(MS:1000618), Unit::Nanometer),
            metacol!("number of data points", [fields[7].name()], curie!(MS:1003060)),
            metacol!("lambda max", [fields[8].name()], curie!(MS:1003812), Unit::Nanometer),
            metacol!("base peak intensity", [fields[9].name()], curie!(MS:1000505), Unit::DetectorCounts),
            metacol!("total ion current", [fields[10].name()], curie!(MS:1000285), Unit::DetectorCounts),
        ]);

        for e in self.extra.iter() {
            cols.extend(e.map_metadata_columns());
        }
        cols
    }
}

impl ArrayBuilder for WavelengthSpectrumDetailsBuilder {
    anyways!();

    fn len(&self) -> usize {
        self.index.len()
    }

    fn finish(&mut self) -> ArrayRef {
        let schema = self.fields();

        let mut arrays: Vec<ArrayRef> = vec![
            finish_it!(self.index),
            finish_it!(self.id),
            finish_it!(self.time),
            self.spectrum_type.finish(),
            self.spectrum_representation.finish(),
            finish_it!(self.lowest_observed_wavelength),
            finish_it!(self.highest_observed_wavelength),
            finish_it!(self.number_of_data_points),
            finish_it!(self.base_peak_mz),
            finish_it!(self.base_peak_intensity),
            finish_it!(self.total_ion_current),
            finish_it!(self.data_processing_ref),
            self.parameters.finish(),
            finish_it!(self.auxiliary_arrays),
            finish_it!(self.number_of_auxiliary_arrays),
        ];

        finish_extra!(self, arrays);

        Arc::new(StructArray::new(schema.into(), arrays, None))
    }

    fn finish_cloned(&self) -> ArrayRef {
        let schema = self.fields();

        let mut arrays: Vec<ArrayRef> = vec![
            finish_cloned!(self.index),
            finish_cloned!(self.id),
            finish_cloned!(self.time),
            self.spectrum_type.finish_cloned(),
            self.spectrum_representation.finish_cloned(),
            finish_cloned!(self.lowest_observed_wavelength),
            finish_cloned!(self.highest_observed_wavelength),
            finish_cloned!(self.number_of_data_points),
            finish_cloned!(self.base_peak_mz),
            finish_cloned!(self.base_peak_intensity),
            finish_cloned!(self.total_ion_current),
            finish_cloned!(self.data_processing_ref),
            self.parameters.finish_cloned(),
            finish_cloned!(self.auxiliary_arrays),
            finish_cloned!(self.number_of_auxiliary_arrays),
        ];

        finish_cloned_extra!(self, arrays);

        Arc::new(StructArray::new(schema.into(), arrays, None))
    }
}

#[derive(Default, Debug)]
pub struct WavelengthSpectrumBuilder {
    spectrum_index_counter: u64,
    scan_index_counter: u64,
    pub(crate) spectrum: WavelengthSpectrumDetailsBuilder,
    pub(crate) scan: ScanBuilder,
    id_to_index: HashMap<String, u64>,
}

impl WavelengthSpectrumBuilder {
    pub fn spectrum_metadata_columns(&self) -> Vec<MetadataColumn> {
        self.spectrum.map_metadata_columns()
    }

    pub fn scan_metadata_columns(&self) -> Vec<MetadataColumn> {
        self.scan.map_metadata_columns()
    }

    pub fn finish_spectrum(&mut self) -> ArrayRef {
        self.spectrum.finish()
    }

    pub fn finish_scan(&mut self) -> ArrayRef {
        self.scan.finish()
    }

    pub fn add_visitors_from(&mut self, visitors: SpectrumFieldVisitors) {
        self.spectrum.extend_extra_fields(visitors.spectrum_fields);
        self.scan.extend_extra_fields(visitors.spectrum_scan_fields);
    }

    pub fn append_value<
        C: CentroidLike,
        D: DeconvolutedCentroidLike,
        S: SpectrumLike<C, D> + 'static,
    >(
        &mut self,
        item: &S,
        auxiliary_arrays: Option<Vec<AuxiliaryArray>>,
    ) -> bool {
        self.id_to_index
            .insert(item.id().to_string(), self.spectrum_index_counter);
        let out = self
            .spectrum
            .append_value(self.spectrum_index_counter, item, auxiliary_arrays);
        for s in item.acquisition().scans.iter() {
            self.scan
                .append_value(&(self.spectrum_index_counter, self.scan_index_counter, s));
            self.scan_index_counter += 1;
        }
        self.spectrum_index_counter += 1;
        out
    }

    pub fn add_spectrum_param_field<T: StructVisitorBuilder<SpectrumDescription>>(
        &mut self,
        visitor: T,
    ) {
        self.spectrum
            .extra
            .push(SpectrumVisitor::Description(Box::new(visitor)));
    }

    pub fn add_scan_field(
        &mut self,
        visitor: impl StructVisitorBuilder<mzdata::spectrum::ScanEvent>,
    ) {
        self.scan.extra.push(Box::new(visitor));
    }

    pub fn index_counter(&self) -> u64 {
        self.spectrum_index_counter
    }

    fn check_lengths_equal(&self) -> bool {
        let n = self.spectrum.len().max(self.scan.len());
        self.spectrum.len() == n && self.scan.len() == n
    }

    pub fn equalize_lengths(&mut self) {
        let n = self.spectrum.len().max(self.scan.len());

        while n > self.spectrum.len() {
            self.spectrum.append_null();
        }

        while n > self.scan.len() {
            self.scan.append_null();
        }
    }

    pub fn spectrum(&self) -> &WavelengthSpectrumDetailsBuilder {
        &self.spectrum
    }

    pub fn scan(&self) -> &ScanBuilder {
        &self.scan
    }
}

impl VisitorBase for WavelengthSpectrumBuilder {
    fn fields(&self) -> Vec<FieldRef> {
        vec![
            field!(SPECTRUM, self.spectrum.as_struct_type()),
            field!(SCAN, self.scan.as_struct_type()),
        ]
    }

    fn append_null(&mut self) {
        self.spectrum.append_null();
        self.scan.append_null();
    }

    fn map_metadata_columns(&self) -> Vec<MetadataColumn> {
        Vec::new()
    }
}

impl ArrayBuilder for WavelengthSpectrumBuilder {
    anyways!();

    fn len(&self) -> usize {
        self.spectrum.len()
    }

    fn finish(&mut self) -> ArrayRef {
        self.equalize_lengths();

        let fields = self.fields();
        let arrays = vec![self.spectrum.finish(), self.scan.finish()];

        Arc::new(StructArray::new(fields.into(), arrays, None))
    }

    fn finish_cloned(&self) -> ArrayRef {
        let fields = self.fields();
        assert!(
            self.check_lengths_equal(),
            "Verify all facets are of equal length, call `equalize_lengths` first!"
        );
        let arrays = vec![self.spectrum.finish_cloned(), self.scan.finish_cloned()];
        Arc::new(StructArray::new(fields.into(), arrays, None))
    }
}

#[cfg(test)]
mod test {
    use crate::constants::SPECTRUM;

    use super::*;
    use arrow::{
        array::{Array, AsArray},
        datatypes::Float64Type,
    };
    use mzdata::{self, spectrum::DataArray};
    use std::io;

    #[test]
    fn test_auxiliary_array_visitor() {
        let mut dat = DataArray::from_name_type_size(
            &mzdata::spectrum::ArrayType::BaselineArray,
            mzdata::spectrum::BinaryDataArrayType::Float32,
            20,
        );
        for _ in 0..10 {
            dat.push(10.0f32).unwrap();
        }
        let aux = AuxiliaryArray::from_data_array(&dat).unwrap();
        let mut builder = AuxiliaryArrayBuilder::default();
        builder.append_value(&aux);
        let arrays = builder.finish();
        let arrays = arrays.as_struct();
        assert_eq!(arrays.len(), 1);
        let name = arrays.column_by_name("name").unwrap();
        let name = name.as_struct();
        let name = name
            .column_by_name("name")
            .unwrap()
            .as_string::<i64>()
            .value(0);
        assert_eq!("baseline array", name);

        let reader = crate::reader::visitor::AuxiliaryArrayVisitor::default();
        let rebuilt = reader.visit(arrays);
        let dup = &rebuilt[0];
        assert_eq!(dup.dtype(), dat.dtype());
        assert_eq!(dup.name(), dat.name());
        assert_eq!(dup.raw_len(), dat.raw_len());
    }

    #[test]
    fn test_custom_param_builder() {
        let mut builder = CustomBuilderFromParameter::from_spec(
            mzdata::curie!(MS:999999),
            "testfoo",
            DataType::Float64,
        );
        builder.append_null();
        builder.append_value(&vec![
            mzdata::Param::builder()
                .curie(mzdata::curie!(MS:999999))
                .name("testfoo")
                .value(100.0)
                .build(),
        ]);
        builder.finish();
        let mut builder = CustomBuilderFromParameter::from_spec(
            mzdata::curie!(MS:999999),
            "testfoo",
            DataType::LargeUtf8,
        );
        builder.append_null();
        builder.append_value(&vec![
            mzdata::Param::builder()
                .curie(mzdata::curie!(MS:999999))
                .name("testfoo")
                .value("bar")
                .build(),
        ]);
        builder.finish();
        let mut builder = CustomBuilderFromParameter::from_spec(
            mzdata::curie!(MS:999999),
            "testfoo",
            DataType::Boolean,
        );
        builder.append_null();
        builder.append_value(&vec![
            mzdata::Param::builder()
                .curie(mzdata::curie!(MS:999999))
                .name("testfoo")
                .value(true)
                .build(),
        ]);
        builder.finish();
    }

    #[test]
    fn test_build_spectra() -> io::Result<()> {
        let mut reader = mzdata::MZReader::open_path("small.mzML")?;
        let spec = reader.get_spectrum_by_index(2).unwrap();

        let mut builder = SpectrumBuilder::default();

        builder.add_spectrum_param_field(
            CustomBuilderFromParameter::from_spec(
                mzdata::curie!(MS:1000504),
                "base peak m/z",
                DataType::Float64,
            )
            .with_name("base_peak_mz_3")
            .with_unit_fixed(Unit::MZ.to_curie()),
        );
        builder.add_spectrum_param_field(
            CustomBuilderFromParameter::from_spec(
                mzdata::curie!(MS:1000504),
                "base peak m/z",
                DataType::Float64,
            )
            .with_unit_field()
            .with_name("base_peak_mz_2"),
        );

        let meta_map = builder.spectrum.map_metadata_columns();
        assert!(meta_map.iter().find(|v| v.name == "base_peak_mz_2").is_some());
        // Match the main column plus the two addons, but not the unit column
        assert_eq!(meta_map.iter().filter(|v| v.accession == Some(curie!(MS:1000504))).count(), 3);

        builder.append_value(&spec, Default::default());
        builder.equalize_lengths();
        builder.append_null();
        let arrays = builder.finish_cloned();
        let arrays = arrays.as_struct();
        let arrays = arrays.column_by_name(SPECTRUM).unwrap();
        let arrays = arrays.as_struct();

        let names = arrays.column_names();

        assert!(names.contains(&"base_peak_mz_2"));
        assert!(names.contains(&"base_peak_mz_2_unit"));
        assert!(names.contains(&"base_peak_mz_3"));

        let arr1 = arrays
            .column_by_name("base_peak_mz")
            .unwrap();
        let arr2 = arrays
            .column_by_name("base_peak_mz_3")
            .unwrap();
        let arr1 = arr1.as_primitive::<Float64Type>();
        let arr2 = arr2.as_primitive::<Float64Type>();

        let x1 = arr1.value(0);
        let x2 = arr2.value(0);
        let e = (x1 - x2).abs();

        // They are not identical because the former is computed from the data while the latter
        // is read as a cvParam from the input.
        assert!(e < 1e-5, "{x1} - {x2} = {e} > 1e-5");

        let arr3 = arrays
            .column_by_name("base_peak_mz_2_unit")
            .unwrap();
        assert_eq!(arr3.len(), 2);

        Ok(())
    }
}
