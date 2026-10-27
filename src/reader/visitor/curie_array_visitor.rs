#![allow(dead_code)]

use arrow::{array::{Array, ArrayRef, AsArray, LargeStringArray, StringArray, StructArray, UInt8Array, UInt32Array}, buffer::NullBuffer, datatypes::{Float64Type, Int64Type}};
use mzdata::params::{CURIE, ControlledVocabulary, Unit};


pub struct CURIEStrArray<'a>(&'a StringArray);

impl<'a> From<&'a StringArray> for CURIEStrArray<'a> {
    fn from(value: &'a StringArray) -> Self {
        Self(value)
    }
}

impl<'a> CURIEStrArray<'a> {
    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    #[inline(always)]
    pub fn is_null(&self, index: usize) -> bool {
        self.0.is_null(index)
    }

    #[inline(always)]
    pub fn value(&self, index: usize) -> Option<CURIE> {
        if self.is_null(index) {
            None
        } else {
            Some(self.0.value(index).parse().unwrap())
        }
    }

    pub fn iter(&self) -> impl Iterator<Item = Option<CURIE>> {
        self.0.iter().map(|v| v.map(|v| v.parse().unwrap()))
    }
}

pub struct LargeCURIEStrArray<'a>(&'a LargeStringArray);

impl<'a> From<&'a LargeStringArray> for LargeCURIEStrArray<'a> {
    fn from(value: &'a LargeStringArray) -> Self {
        Self(value)
    }
}

impl<'a> LargeCURIEStrArray<'a> {
    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    #[inline(always)]
    pub fn is_null(&self, index: usize) -> bool {
        self.0.is_null(index)
    }

    #[inline(always)]
    pub fn value(&self, index: usize) -> Option<CURIE> {
        if self.is_null(index) {
            None
        } else {
            Some(self.0.value(index).parse().unwrap())
        }
    }

    pub fn iter(&self) -> impl Iterator<Item = Option<CURIE>> {
        self.0.iter().map(|v| v.map(|v| v.parse().unwrap()))
    }
}

/// A deprecated struct-based encoding. To be removed before
/// first stable release.
pub struct CURIEStructArray<'a> {
    cv_id: &'a UInt8Array,
    accession: &'a UInt32Array,
    null: Option<&'a NullBuffer>,
}

impl<'a> From<&'a StructArray> for CURIEStructArray<'a> {
    fn from(value: &'a StructArray) -> Self {
        Self::from_struct_array(value)
    }
}

impl<'a> CURIEStructArray<'a> {
    fn new(
        cv_id: &'a UInt8Array,
        accession: &'a UInt32Array,
        null: Option<&'a NullBuffer>,
    ) -> Self {
        Self {
            cv_id,
            accession,
            null,
        }
    }

    pub fn len(&self) -> usize {
        self.cv_id.len()
    }

    pub fn is_empty(&self) -> bool {
        self.cv_id.is_empty()
    }

    pub fn from_struct_array(series: &'a StructArray) -> Self {
        Self::new(
            series.column(0).as_primitive(),
            series.column(1).as_primitive(),
            series.nulls(),
        )
    }

    #[inline(always)]
    pub fn is_null(&self, index: usize) -> bool {
        self.cv_id.is_null(index)
            || self.accession.is_null(index)
            || self.null.is_some_and(|v| v.is_null(index))
    }

    #[inline(always)]
    pub fn value(&self, index: usize) -> Option<CURIE> {
        if self.is_null(index) {
            None
        } else {
            Some(CURIE::new(
                match self.cv_id.value(index) {
                    1 => ControlledVocabulary::MS,
                    2 => ControlledVocabulary::UO,
                    x => panic!("Bad old CV ID {x}"),
                },
                self.accession.value(index),
            ))
        }
    }
}

pub enum AnyCURIEArray<'a> {
    Struct(CURIEStructArray<'a>),
    String(CURIEStrArray<'a>),
    LargeString(LargeCURIEStrArray<'a>),
}

impl<'a> TryFrom<&'a ArrayRef> for AnyCURIEArray<'a> {
    type Error = ();

    fn try_from(value: &'a ArrayRef) -> Result<Self, Self::Error> {
        if let Some(arr) = value.as_struct_opt() {
            Ok(Self::from_struct_array(arr))
        } else {
            if let Some(arr) = value.as_string_opt::<i32>() {
                Ok(Self::String(CURIEStrArray(arr)))
            } else if let Some(arr) = value.as_string_opt::<i64>() {
                Ok(Self::LargeString(LargeCURIEStrArray(arr)))
            } else {
                panic!("Unsupported data type: {:?}", value.data_type());
            }
        }
    }
}

impl<'a> AnyCURIEArray<'a> {
    pub fn from_struct_array(array: &'a StructArray) -> Self {
        Self::Struct(CURIEStructArray::from_struct_array(array))
    }

    pub fn len(&self) -> usize {
        match self {
            AnyCURIEArray::Struct(curiearray) => curiearray.len(),
            AnyCURIEArray::String(curiestr_array) => curiestr_array.len(),
            AnyCURIEArray::LargeString(curiestr_array) => curiestr_array.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn to_vec(&self) -> Vec<CURIE> {
        match self {
            AnyCURIEArray::Struct(curiearray) => {
                let n = curiearray.len();
                let mut acc: Vec<_> = Vec::with_capacity(n);
                for i in 0..n {
                    acc.push(curiearray.value(i).unwrap())
                }
                acc
            }
            AnyCURIEArray::String(curiestr_array) => {
                let n = curiestr_array.len();
                let mut acc: Vec<_> = Vec::with_capacity(n);
                for i in 0..n {
                    acc.push(curiestr_array.value(i).unwrap())
                }
                acc
            }
            AnyCURIEArray::LargeString(curiestr_array) => {
                let n = curiestr_array.len();
                let mut acc: Vec<_> = Vec::with_capacity(n);
                for i in 0..n {
                    acc.push(curiestr_array.value(i).unwrap())
                }
                acc
            }
        }
    }

    #[inline(always)]
    pub fn is_null(&self, index: usize) -> bool {
        match self {
            AnyCURIEArray::Struct(curiearray) => curiearray.is_null(index),
            AnyCURIEArray::String(curiestr_array) => curiestr_array.is_null(index),
            AnyCURIEArray::LargeString(curiestr_array) => curiestr_array.is_null(index),
        }
    }

    #[inline(always)]
    pub fn value(&self, index: usize) -> Option<CURIE> {
        match self {
            AnyCURIEArray::Struct(curiearray) => curiearray.value(index),
            AnyCURIEArray::String(curiestr_array) => curiestr_array.value(index),
            AnyCURIEArray::LargeString(curiestr_array) => curiestr_array.value(index),
        }
    }
}

impl<'a> From<CURIEStructArray<'a>> for AnyCURIEArray<'a> {
    fn from(value: CURIEStructArray<'a>) -> Self {
        Self::Struct(value)
    }
}

impl<'a> From<CURIEStrArray<'a>> for AnyCURIEArray<'a> {
    fn from(value: CURIEStrArray<'a>) -> Self {
        Self::String(value)
    }
}

impl<'a> From<LargeCURIEStrArray<'a>> for AnyCURIEArray<'a> {
    fn from(value: LargeCURIEStrArray<'a>) -> Self {
        Self::LargeString(value)
    }
}

pub struct UnitArray<'a>(AnyCURIEArray<'a>);

impl<'a> From<&'a ArrayRef> for UnitArray<'a> {
    fn from(value: &'a ArrayRef) -> Self {
        Self(AnyCURIEArray::try_from(value).unwrap())
    }
}

impl<'a> From<CURIEStructArray<'a>> for UnitArray<'a> {
    fn from(value: CURIEStructArray<'a>) -> Self {
        Self(value.into())
    }
}

impl<'a> From<&'a StructArray> for UnitArray<'a> {
    fn from(value: &'a StructArray) -> Self {
        Self(AnyCURIEArray::from_struct_array(value))
    }
}

impl<'a> UnitArray<'a> {
    pub fn from_struct_array(unit_series: &'a StructArray) -> Self {
        Self(AnyCURIEArray::from_struct_array(unit_series))
    }

    #[inline(always)]
    pub fn value(&self, index: usize) -> Unit {
        self.0
            .value(index)
            .map(|v| Unit::from_curie(&v))
            .unwrap_or_default()
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.len() == 0
    }

    pub fn is_null(&self, index: usize) -> bool {
        self.0.is_null(index)
    }
}

/// A single unit that is used for all values in a column
pub struct UnitScalar(Unit);

impl UnitScalar {
    #[inline(always)]
    fn value(&self, _index: usize) -> Unit {
        self.0
    }
}

/// A generic strategy for mapping units across rows of another column
pub enum UnitCollection<'a> {
    Array(UnitArray<'a>),
    Scalar(UnitScalar),
}

impl<'a> Default for UnitCollection<'a> {
    fn default() -> Self {
        Self::unknown()
    }
}

impl<'a> UnitCollection<'a> {
    #[inline(always)]
    pub fn value(&self, index: usize) -> Unit {
        match self {
            UnitCollection::Array(unit_series) => unit_series.value(index),
            UnitCollection::Scalar(unit_series_singular) => unit_series_singular.value(index),
        }
    }

    pub fn series(unit_series: &'a StructArray) -> Self {
        Self::Array(UnitArray::from_struct_array(unit_series))
    }

    pub fn singular(unit: Unit) -> Self {
        Self::Scalar(UnitScalar(unit))
    }

    pub fn unknown() -> Self {
        Self::Scalar(UnitScalar(Unit::Unknown))
    }
}


pub struct ParameterVisitor<'a> {
    root: &'a StructArray,
    destination: Vec<mzdata::Param>,
}

impl<'a> ParameterVisitor<'a> {
    pub fn new(root: &'a StructArray) -> Self {
        Self {
            root,
            destination: Vec::new(),
        }
    }

    pub fn build(mut self) -> Vec<mzdata::Param> {
        let n = self.root.len();
        self.destination.resize(n, Default::default());

        if let Some(name) = self.root.column_by_name("name") {
            if let Some(arr) = name.as_string_opt::<i64>() {
                for (i, v) in arr.iter().enumerate() {
                    self.destination[i].name = v.unwrap().to_string();
                }
            } else if let Some(arr) = name.as_string_opt::<i32>() {
                for (i, v) in arr.iter().enumerate() {
                    self.destination[i].name = v.unwrap().to_string();
                }
            } else {
                panic!("Unsupported data type {:?}", name.data_type());
            }
        }
        if let Some(curie) = self
            .root
            .column_by_name("curie")
            .or_else(|| self.root.column_by_name("accession"))
        {
            let arr = AnyCURIEArray::try_from(curie).unwrap();
            for i in 0..n {
                if let Some(v) = arr.value(i) {
                    let d = &mut self.destination[i];
                    d.accession = Some(v.accession);
                    d.controlled_vocabulary = Some(v.controlled_vocabulary);
                }
            }
        }
        if let Some(unit) = self.root.column_by_name("unit") {
            let arr = UnitArray::from(unit);
            for i in 0..n {
                self.destination[i].unit = arr.value(i);
            }
        }
        if let Some(values) = self.root.column_by_name("value") {
            let values = values.as_struct();
            if let Some(ints) = values.column_by_name("integer") {
                if ints.null_count() != n {
                    let ints = ints.as_primitive::<Int64Type>();
                    for i in 0..n {
                        if ints.is_valid(i) {
                            self.destination[i].value = mzdata::params::Value::Int(ints.value(i));
                        }
                    }
                }
            }
            if let Some(ints) = values.column_by_name("float") {
                if ints.null_count() != n {
                    let ints = ints.as_primitive::<Float64Type>();
                    for i in 0..n {
                        if ints.is_valid(i) {
                            self.destination[i].value = mzdata::params::Value::Float(ints.value(i));
                        }
                    }
                }
            }
            if let Some(ints) = values.column_by_name("string") {
                if ints.null_count() != n {
                    if let Some(ints) = ints.as_string_opt::<i64>() {
                        for i in 0..n {
                            if ints.is_valid(i) {
                                self.destination[i].value =
                                    mzdata::params::Value::String(ints.value(i).to_string());
                            }
                        }
                    } else if let Some(ints) = ints.as_string_opt::<i32>() {
                        for i in 0..n {
                            if ints.is_valid(i) {
                                self.destination[i].value =
                                    mzdata::params::Value::String(ints.value(i).to_string());
                            }
                        }
                    } else {
                        panic!("Unsupported data type {:?}", ints.data_type());
                    }
                }
            }
            if let Some(ints) = values.column_by_name("boolean") {
                if ints.null_count() != n {
                    let ints = ints.as_boolean();
                    for i in 0..n {
                        if ints.is_valid(i) {
                            self.destination[i].value =
                                mzdata::params::Value::Boolean(ints.value(i));
                        }
                    }
                }
            }
        }
        self.destination
    }
}
