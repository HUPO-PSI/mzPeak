use std::fmt::Debug;
use std::sync::Arc;

use arrow::array::{
    Array, ArrayRef, BooleanArray, Float32Array, Float64Array, Int8Array, Int16Array, Int32Array,
    Int64Array, UInt8Array, UInt16Array, UInt32Array, UInt64Array,
};
use mzdata::spectrum::ArrayType;
use mzpeaks::CoordinateRange;
use mzpeaks::coordinate::SimpleInterval;
use mzpeaks::prelude::HasProximity;
use parquet::arrow::arrow_reader::ArrowReaderBuilder;
use parquet::file::metadata::ParquetMetaData;

use parquet::{
    self,
    arrow::arrow_reader::{RowSelection, RowSelector},
    // file::page_index::index::Index as ParquetTypedIndex,
    file::page_index::column_index::{
        ColumnIndexMetaData as ParquetTypedIndex, PrimitiveColumnIndex,
    },
    schema::types::SchemaDescriptor,
};

use mzdata::mzpeaks::coordinate::Span1D;
use serde::{Deserialize, Serialize};

use crate::BufferContext;
use crate::buffer_descriptors::{ArrayIndex, BufferFormat};

pub fn parquet_column(schema: &SchemaDescriptor, column: &str) -> Option<usize> {
    let mut column_ix: Option<usize> = None;
    for (i, col) in schema.columns().iter().enumerate() {
        if col.path().string() == column {
            column_ix = Some(i);
            break;
        }
    }
    column_ix
}

#[derive(Debug, Default, Clone, Copy, Serialize, Deserialize)]
pub struct PageIndexEntry<T> {
    pub row_group_i: usize,
    pub page_i: usize,
    pub min: T,
    pub max: T,
    pub start_row: i64,
    pub end_row: i64,
}

impl<T> PageIndexEntry<T> {
    pub fn row_len(&self) -> i64 {
        self.end_row - self.start_row
    }
}

impl<T: HasProximity> Span1D for PageIndexEntry<T> {
    type DimType = T;

    fn start(&self) -> Self::DimType {
        self.min
    }

    fn end(&self) -> Self::DimType {
        self.max
    }
}

impl<T: HasProximity> PageIndexType<T> for PageIndexEntry<T> {
    fn start_row(&self) -> i64 {
        self.start_row
    }

    fn end_row(&self) -> i64 {
        self.end_row
    }

    fn row_group(&self) -> usize {
        self.row_group_i
    }
}

/// An abstraction built atop the Parquet Page Index to support point and interval
/// queries to find which row groups and pages contain values of interest, implying
/// which rows actually need to be loaded.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct PageIndex<T: HasProximity>(Vec<PageIndexEntry<T>>)
where
    PageIndexEntry<T>: PageIndexType<T>;

impl<T: HasProximity> IntoIterator for PageIndex<T>
where
    PageIndexEntry<T>: PageIndexType<T>,
{
    type Item = PageIndexEntry<T>;

    type IntoIter = std::vec::IntoIter<PageIndexEntry<T>>;

    fn into_iter(self) -> Self::IntoIter {
        self.0.into_iter()
    }
}

impl<T: HasProximity + Debug> PageIndex<T>
where
    PageIndexEntry<T>: PageIndexType<T>,
{
    pub fn get(&self, index: usize) -> Option<&PageIndexEntry<T>> {
        self.0.get(index)
    }

    pub fn iter(&self) -> std::slice::Iter<'_, PageIndexEntry<T>> {
        self.0.iter()
    }

    pub fn first(&self) -> Option<&PageIndexEntry<T>> {
        self.0.first()
    }

    pub fn last(&self) -> Option<&PageIndexEntry<T>> {
        self.0.last()
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn sort(&mut self) {
        self.0.sort_by(|a, b| {
            a.min
                .partial_cmp(&b.min)
                .unwrap()
                .then_with(|| a.max.partial_cmp(&b.max).unwrap())
        });
    }

    pub fn row_selection_is_not_null(&self) -> RowSelection {
        let mut selectors = Vec::new();
        let mut last_row = 0;

        for page in self.iter() {
            if page.start_row() != last_row {
                selectors.push(RowSelector::skip((page.start_row() - last_row) as usize));
            }
            selectors.push(RowSelector::select(page.row_len() as usize));
            last_row = page.end_row();
        }
        selectors.into()
    }

    pub fn pages_not_null(&self) -> std::slice::Iter<'_, PageIndexEntry<T>> {
        self.iter()
    }

    pub fn row_selection_contains(&self, query: T) -> RowSelection {
        let mut selectors = Vec::new();
        let mut last_row = 0;
        for page in self.iter() {
            if page.start_row() != last_row {
                selectors.push(RowSelector::skip((page.start_row() - last_row) as usize));
            }
            if page.contains(&query) {
                selectors.push(RowSelector::select(page.row_len() as usize));
            } else {
                selectors.push(RowSelector::skip(page.row_len() as usize))
            }
            last_row = page.end_row();
        }

        selectors.into()
    }

    pub fn pages_contains(&self, query: T) -> impl Iterator<Item = &PageIndexEntry<T>> {
        self.iter().filter(move |p| p.contains(&query))
    }

    pub fn row_selection_overlaps<S: Span1D<DimType = T> + Debug>(
        &self,
        query: &S,
    ) -> RowSelection {
        let mut selectors = Vec::new();
        let mut last_row = 0;
        for page in self.iter() {
            if page.start_row() != last_row {
                selectors.push(RowSelector::skip((page.start_row() - last_row) as usize));
            }
            if page.overlaps(&query) {
                selectors.push(RowSelector::select(page.row_len() as usize));
            } else {
                selectors.push(RowSelector::skip(page.row_len() as usize))
            }
            last_row = page.end_row();
        }
        selectors.into()
    }

    pub fn pages_overlaps(
        &self,
        query: &impl Span1D<DimType = T>,
    ) -> impl Iterator<Item = &PageIndexEntry<T>> {
        self.iter().filter(move |p| p.overlaps(query))
    }

    pub fn pages_to_row_selection<'a>(
        &'a self,
        it: &'a [PageIndexEntry<T>],
        mut last_row: i64,
    ) -> RowSelection {
        let mut selectors = Vec::new();
        for page in it {
            if page.start_row() != last_row {
                selectors.push(RowSelector::skip((page.start_row() - last_row) as usize));
            }
            selectors.push(RowSelector::select(page.row_len() as usize));
            last_row = page.end_row();
        }
        selectors.into()
    }
}

/// A generic interface to a page index entry.
///
/// It requires [`Span1D`] over the type stored in the index.
pub trait PageIndexType<T>: Span1D<DimType = T> {
    /// Get the first row the page covers
    fn start_row(&self) -> i64;

    /// Get the last row the page covers
    fn end_row(&self) -> i64;

    /// The row group that this page belongs to
    fn row_group(&self) -> usize;

    /// Create a [`Span1D`] implementation over the page rows for this entry
    fn page_span(&self) -> SimpleInterval<i64> {
        SimpleInterval::new(self.start_row(), self.end_row())
    }

    /// The number of rows this page spans
    fn row_len(&self) -> i64 {
        self.end_row() - self.start_row()
    }
}

pub type TimeIndexPage = PageIndexEntry<f32>;
pub type PointMZIndexPage = PageIndexEntry<f64>;
pub type PointIonMobilityIndexPage = PageIndexEntry<f64>;
pub type PointSpectrumIndexPage = PageIndexEntry<u64>;

#[derive(Debug, Default, Clone, Copy)]
struct PageMinMaxBounds<T: Copy> {
    min: Option<T>,
    max: Option<T>,
}

impl<T: Copy> PageMinMaxBounds<T> {
    fn new(min: Option<T>, max: Option<T>) -> Self {
        Self { min, max }
    }

    fn from_index(column_index: &PrimitiveColumnIndex<T>) -> impl Iterator<Item = Self> {
        column_index
            .min_values_iter()
            .zip(column_index.max_values_iter())
            .map(|(min, max)| Self::new(min.copied(), max.copied()))
    }

    fn min(&self) -> Option<T> {
        self.min
    }

    fn max(&self) -> Option<T> {
        self.max
    }
}

macro_rules! read_pages {
    ($rg:ident, $i:ident, $native_index:expr, $vtype:ty, $pages:ident, $total_rows:ident, $offset_list:ident) => {
        for (page_i, (q, offset)) in PageMinMaxBounds::from_index($native_index)
            .zip($offset_list.page_locations().iter())
            .enumerate()
        {
            if q.min().is_none() {
                continue;
            }
            let min = q.min().unwrap() as $vtype;
            let max = q.max().unwrap() as $vtype;
            let start_row = offset.first_row_index + $total_rows;
            let end_row =
                if let Some(next_loc) = $offset_list.page_locations().get(page_i + 1) {
                    next_loc.first_row_index + $total_rows
                } else {
                    $rg.num_rows() + $total_rows
                };
            $pages.push(PageIndexEntry::<$vtype> {
                row_group_i: $i,
                page_i: page_i,
                min,
                max,
                start_row,
                end_row,
            })
        }
    };
}

macro_rules! read_numeric_page_index {
    ($metadata:expr, $pq_schema:expr, $column_path:expr, $type:ty) => {{
        let column_ix = parquet_column($pq_schema, $column_path)?;

        let rg_meta = $metadata.row_groups();
        let column_offset_index = $metadata.offset_index()?;
        let column_index = $metadata.column_index()?;

        let mut total_rows = 0;
        let mut pages = Vec::new();
        for (i, (rg, (offset_list, idx_list))) in rg_meta
            .iter()
            .zip(column_offset_index.iter().zip(column_index.iter()))
            .enumerate()
        {
            let idx_list = &idx_list[column_ix];
            let offset_list = &offset_list[column_ix];

            match idx_list {
                $crate::reader::index::ParquetTypedIndex::FLOAT(native_index) => {
                    read_pages!(rg, i, native_index, $type, pages, total_rows, offset_list);
                }
                $crate::reader::index::ParquetTypedIndex::DOUBLE(native_index) => {
                    read_pages!(rg, i, native_index, $type, pages, total_rows, offset_list);
                }
                $crate::reader::index::ParquetTypedIndex::INT32(native_index) => {
                    read_pages!(rg, i, native_index, $type, pages, total_rows, offset_list);
                }
                $crate::reader::index::ParquetTypedIndex::INT64(native_index) => {
                    read_pages!(rg, i, native_index, $type, pages, total_rows, offset_list);
                }
                tp => {
                    panic!("Wrong type of index! {tp:?}");
                }
            }
            total_rows += rg.num_rows();
        }
        Some(PageIndex(pages))
    }};
    ($reader:expr, $column_path:expr, $type:ty) => {{
        let metadata = $reader.metadata();
        let pq_schema = $reader.parquet_schema();

        read_numeric_page_index!(metadata, pq_schema, $column_path, $type)
    }};
}

/// Read a `f32` values from the page index for the specified path from
/// prepared metadata
pub fn read_f32_page_index_from(
    metadata: &Arc<ParquetMetaData>,
    pq_schema: &SchemaDescriptor,
    column_path: &str,
) -> Option<PageIndex<f32>> {
    read_numeric_page_index!(metadata, pq_schema, column_path, f32)
}

/// Read a `f64` values from the page index for the specified path from
/// prepared metadata
pub fn read_f64_page_index_from(
    metadata: &Arc<ParquetMetaData>,
    pq_schema: &SchemaDescriptor,
    column_path: &str,
) -> Option<PageIndex<f64>> {
    read_numeric_page_index!(metadata, pq_schema, column_path, f64)
}

/// Read a `i32` values from the page index for the specified path from
/// prepared metadata
pub fn read_i32_page_index_from(
    metadata: &Arc<ParquetMetaData>,
    pq_schema: &SchemaDescriptor,
    column_path: &str,
) -> Option<PageIndex<i32>> {
    read_numeric_page_index!(metadata, pq_schema, column_path, i32)
}

/// Read a `i64` values from the page index for the specified path from
/// prepared metadata
pub fn read_i64_page_index_from(
    metadata: &Arc<ParquetMetaData>,
    pq_schema: &SchemaDescriptor,
    column_path: &str,
) -> Option<PageIndex<i64>> {
    read_numeric_page_index!(metadata, pq_schema, column_path, i64)
}

/// Read a `u32` values from the page index for the specified path from
/// prepared metadata
pub fn read_u32_page_index_from(
    metadata: &Arc<ParquetMetaData>,
    pq_schema: &SchemaDescriptor,
    column_path: &str,
) -> Option<PageIndex<u32>> {
    read_numeric_page_index!(metadata, pq_schema, column_path, u32)
}

/// Read a `u64` values from the page index for the specified path from
/// prepared metadata
pub fn read_u64_page_index_from(
    metadata: &Arc<ParquetMetaData>,
    pq_schema: &SchemaDescriptor,
    column_path: &str,
) -> Option<PageIndex<u64>> {
    read_numeric_page_index!(metadata, pq_schema, column_path, u64)
}

/// Read a `u8` values from the page index for the specified path from
/// prepared metadata
pub fn read_u8_page_index_from(
    metadata: &Arc<ParquetMetaData>,
    pq_schema: &SchemaDescriptor,
    column_path: &str,
) -> Option<PageIndex<u8>> {
    read_numeric_page_index!(metadata, pq_schema, column_path, u8)
}

/// Read a `i8` values from the page index for the specified path from
/// prepared metadata
pub fn read_i8_page_index_from(
    metadata: &Arc<ParquetMetaData>,
    pq_schema: &SchemaDescriptor,
    column_path: &str,
) -> Option<PageIndex<i8>> {
    read_numeric_page_index!(metadata, pq_schema, column_path, i8)
}

pub trait SpanDynNumeric: Span1D
where
    Self::DimType: num_traits::NumCast,
{
    fn contains_dy_iter<'a>(
        &'a self,
        array: &'a ArrayRef,
    ) -> impl Iterator<Item = Option<bool>> + 'a {
        let n = array.len();
        let it = 0..n;

        macro_rules! span_dyn_impl {
            ($raw_ty:ty, $arr_ty:ty) => {{
                let start = <$raw_ty as num_traits::NumCast>::from(self.start()).unwrap();
                let end = <$raw_ty as num_traits::NumCast>::from(self.end()).unwrap();
                let span = SimpleInterval::new(start, end);
                let array: &$arr_ty = array.as_any().downcast_ref().unwrap();
                let closure: Box<dyn Fn(usize) -> Option<bool>> = Box::new(move |i| {
                    if array.is_valid(i) {
                        Some(span.contains(&array.value(i)))
                    } else {
                        None
                    }
                });
                it.map(closure)
            }};
        }

        match array.data_type() {
            arrow::datatypes::DataType::Int8 => {
                span_dyn_impl!(i8, Int8Array)
            }
            arrow::datatypes::DataType::Int16 => {
                span_dyn_impl!(i16, Int16Array)
            }
            arrow::datatypes::DataType::Int32 => span_dyn_impl!(i32, Int32Array),
            arrow::datatypes::DataType::Int64 => span_dyn_impl!(i64, Int64Array),
            arrow::datatypes::DataType::UInt8 => span_dyn_impl!(u8, UInt8Array),
            arrow::datatypes::DataType::UInt16 => span_dyn_impl!(u16, UInt16Array),
            arrow::datatypes::DataType::UInt32 => span_dyn_impl!(u32, UInt32Array),
            arrow::datatypes::DataType::UInt64 => span_dyn_impl!(u64, UInt64Array),
            arrow::datatypes::DataType::Float32 => span_dyn_impl!(f32, Float32Array),
            arrow::datatypes::DataType::Float64 => span_dyn_impl!(f64, Float64Array),
            _ => {
                let f: Box<dyn Fn(usize) -> Option<bool>> = Box::new(|_| None);
                it.map(f)
            }
        }
    }

    fn overlaps_dy(&self, start_array: &ArrayRef, end_array: &ArrayRef) -> BooleanArray {
        macro_rules! overlaps_dyn_impl {
            ($raw_ty:ty, $arr_ty:ty) => {{
                let start = <$raw_ty as num_traits::NumCast>::from(self.start()).unwrap();
                let end = <$raw_ty as num_traits::NumCast>::from(self.end()).unwrap();
                let span = SimpleInterval::new(start, end);
                let start_array: &$arr_ty = start_array.as_any().downcast_ref().unwrap();
                let end_array: &$arr_ty = end_array.as_any().downcast_ref().unwrap();
                start_array
                    .iter()
                    .zip(end_array.iter())
                    .map(|(start, end)| -> Option<bool> {
                        let v = SimpleInterval::new(start?, end?);
                        Some(v.overlaps(&span))
                    })
                    .collect()
            }};
        }
        match start_array.data_type() {
            arrow::datatypes::DataType::Int8 => {
                overlaps_dyn_impl!(i8, Int8Array)
            }
            arrow::datatypes::DataType::Int16 => {
                overlaps_dyn_impl!(i16, Int16Array)
            }
            arrow::datatypes::DataType::Int32 => overlaps_dyn_impl!(i32, Int32Array),
            arrow::datatypes::DataType::Int64 => overlaps_dyn_impl!(i64, Int64Array),
            arrow::datatypes::DataType::UInt8 => overlaps_dyn_impl!(u8, UInt8Array),
            arrow::datatypes::DataType::UInt16 => overlaps_dyn_impl!(u16, UInt16Array),
            arrow::datatypes::DataType::UInt32 => overlaps_dyn_impl!(u32, UInt32Array),
            arrow::datatypes::DataType::UInt64 => overlaps_dyn_impl!(u64, UInt64Array),
            arrow::datatypes::DataType::Float32 => overlaps_dyn_impl!(f32, Float32Array),
            arrow::datatypes::DataType::Float64 => overlaps_dyn_impl!(f64, Float64Array),
            _ => BooleanArray::new_null(start_array.len()),
        }
    }

    fn contains_dy(&self, array: &ArrayRef) -> BooleanArray {
        macro_rules! span_dyn_impl {
            ($raw_ty:ty, $arr_ty:ty) => {{
                let start = <$raw_ty as num_traits::NumCast>::from(self.start()).unwrap();
                let end = <$raw_ty as num_traits::NumCast>::from(self.end()).unwrap();
                let span = SimpleInterval::new(start, end);
                let array: &$arr_ty = array.as_any().downcast_ref().unwrap();
                array.iter().map(|v| v.map(|v| span.contains(&v))).collect()
            }};
        }
        match array.data_type() {
            arrow::datatypes::DataType::Int8 => {
                span_dyn_impl!(i8, Int8Array)
            }
            arrow::datatypes::DataType::Int16 => {
                span_dyn_impl!(i16, Int16Array)
            }
            arrow::datatypes::DataType::Int32 => span_dyn_impl!(i32, Int32Array),
            arrow::datatypes::DataType::Int64 => span_dyn_impl!(i64, Int64Array),
            arrow::datatypes::DataType::UInt8 => span_dyn_impl!(u8, UInt8Array),
            arrow::datatypes::DataType::UInt16 => span_dyn_impl!(u16, UInt16Array),
            arrow::datatypes::DataType::UInt32 => span_dyn_impl!(u32, UInt32Array),
            arrow::datatypes::DataType::UInt64 => span_dyn_impl!(u64, UInt64Array),
            arrow::datatypes::DataType::Float32 => span_dyn_impl!(f32, Float32Array),
            arrow::datatypes::DataType::Float64 => span_dyn_impl!(f64, Float64Array),
            _ => BooleanArray::new_null(array.len()),
        }
    }
}

impl<T: PartialEq + PartialOrd + HasProximity> SpanDynNumeric for SimpleInterval<T> where
    <mzpeaks::coordinate::SimpleInterval<T> as mzdata::prelude::Span1D>::DimType:
        num_traits::NumCast
{
}
impl<T: PartialEq + PartialOrd + HasProximity> SpanDynNumeric for CoordinateRange<T> where
    <mzpeaks::coordinate::CoordinateRange<T> as mzdata::prelude::Span1D>::DimType:
        num_traits::NumCast
{
}

pub struct RangeIndex<'a, T: HasProximity> {
    start_index: &'a PageIndex<T>,
    end_index: &'a PageIndex<T>,
}

impl<'a, T: HasProximity + Debug> RangeIndex<'a, T> {
    pub fn new(start_index: &'a PageIndex<T>, end_index: &'a PageIndex<T>) -> Self {
        Self {
            start_index,
            end_index,
        }
    }

    pub fn row_selection_overlaps(&self, query: &impl Span1D<DimType = T>) -> RowSelection {
        let mut selectors = Vec::new();
        let mut last_row = 0;
        for (start_page, end_page) in self.start_index.iter().zip(self.end_index.iter()) {
            if start_page.start_row() != last_row {
                selectors.push(RowSelector::skip(
                    (start_page.start_row() - last_row) as usize,
                ));
            }

            let overlaps = start_page.contains(&query.start())
                || end_page.contains(&query.end())
                || SimpleInterval::new(start_page.start(), end_page.end()).overlaps(query);
            if overlaps {
                selectors.push(RowSelector::select(start_page.row_len() as usize));
            } else {
                selectors.push(RowSelector::skip(start_page.row_len() as usize))
            }
            last_row = start_page.end_row();
        }
        selectors.into()
    }
}

pub trait BasicQueryIndex {
    fn query_pages(&self, spectrum_index: u64) -> PageQuery;
    fn query_pages_overlaps(&self, index_range: &impl Span1D<DimType = u64>) -> PageQuery;

    fn primary_data_index(&self) -> &PageIndex<u64>;

    fn is_empty(&self) -> bool {
        self.primary_data_index().is_empty()
    }
    fn is_populated(&self) -> bool {
        !self.is_empty()
    }
}

pub trait BasicChunkQueryIndex: BasicQueryIndex {
    fn chunk_start_index(&self) -> &PageIndex<f64>;
    fn chunk_end_index(&self) -> &PageIndex<f64>;
}

// An internal trait to make allow abstracting over different storage strategies expose a uniform API
#[allow(unused)]
pub trait SpectrumQueryIndex: BasicQueryIndex {
    fn spectrum_data_index(&self) -> &PageIndex<u64> {
        self.primary_data_index()
    }

    fn index_overlaps(&self, index_range: &SimpleInterval<u64>) -> RowSelection;
    fn coordinate_overlaps(&self, mz_range: &SimpleInterval<f64>) -> RowSelection;
    fn ion_mobility_overlaps(&self, im_range: &SimpleInterval<f64>) -> RowSelection;
}

pub trait ChromatogramQueryIndex: BasicQueryIndex {
    fn query_chromatrogram_pages(&self, chromatogram_index: u64) -> PageQuery;
    fn query_chromatogram_pages_overlaps(
        &self,
        index_range: &impl Span1D<DimType = u64>,
    ) -> PageQuery;

    fn chromatogram_data_index(&self) -> &PageIndex<u64>;

    fn len(&self) -> usize {
        self.chromatogram_data_index().len()
    }
}

#[derive(Debug, Default, Clone)]
pub struct SpectrumPointIndex {
    pub spectrum_index: PageIndex<u64>,
    pub mz_index: PageIndex<f64>,
    pub im_index: PageIndex<f64>,
    pub time_index: Option<PageIndex<f32>>,
}

impl SpectrumPointIndex {
    pub fn new(
        spectrum_index: PageIndex<u64>,
        mz_index: PageIndex<f64>,
        im_index: PageIndex<f64>,
        time_index: Option<PageIndex<f32>>,
    ) -> Self {
        Self {
            spectrum_index,
            mz_index,
            im_index,
            time_index,
        }
    }

    pub fn from_reader<T>(
        spectrum_data_reader: &ArrowReaderBuilder<T>,
        spectrum_array_indices: &ArrayIndex,
    ) -> Self {
        let peak_pq_schema = spectrum_data_reader.parquet_schema();
        let mut this = Self::default();

        this.spectrum_index = read_u64_page_index_from(
            &spectrum_data_reader.metadata(),
            &peak_pq_schema,
            &format!(
                "{}.{}",
                spectrum_array_indices.prefix,
                BufferContext::Spectrum.index_name()
            ),
        )
        .unwrap_or_default();

        this.time_index = read_f32_page_index_from(
            spectrum_data_reader.metadata(),
            peak_pq_schema,
            &format!(
                "{}.{}",
                spectrum_array_indices.prefix,
                BufferContext::Spectrum.time_name()
            ),
        );

        for entry in spectrum_array_indices.iter() {
            if matches!(entry.array_type, ArrayType::MZArray) {
                this.mz_index = read_f64_page_index_from(
                    spectrum_data_reader.metadata(),
                    peak_pq_schema,
                    &entry.path,
                )
                .unwrap_or_default();
            } else if entry.is_ion_mobility() {
                this.im_index = read_f64_page_index_from(
                    spectrum_data_reader.metadata(),
                    peak_pq_schema,
                    &entry.path,
                )
                .unwrap_or_default();
            }
        }

        this
    }

    pub fn query_pages(&self, spectrum_index: u64) -> PageQuery {
        let mut rg_idx_acc = Vec::new();
        let mut pages: Vec<PageIndexEntry<u64>> = Vec::new();

        for page in self.spectrum_index.pages_contains(spectrum_index) {
            if !rg_idx_acc.contains(&page.row_group_i) {
                rg_idx_acc.push(page.row_group_i);
            }
            pages.push(*page);
        }
        PageQuery::new(rg_idx_acc, pages)
    }

    pub fn query_pages_overlaps(&self, index_range: &impl Span1D<DimType = u64>) -> PageQuery {
        let mut rg_idx_acc = Vec::new();
        let mut pages: Vec<PageIndexEntry<u64>> = Vec::new();

        for page in self.spectrum_index.pages_overlaps(index_range) {
            if !rg_idx_acc.contains(&page.row_group_i) {
                rg_idx_acc.push(page.row_group_i);
            }
            pages.push(*page);
        }
        PageQuery::new(rg_idx_acc, pages)
    }

    pub fn is_empty(&self) -> bool {
        self.spectrum_index.is_empty()
    }

    pub fn is_populated(&self) -> bool {
        !self.spectrum_index.is_empty()
    }
}

impl BasicQueryIndex for SpectrumPointIndex {
    fn query_pages(&self, spectrum_index: u64) -> PageQuery {
        self.query_pages(spectrum_index)
    }

    fn query_pages_overlaps(&self, index_range: &impl Span1D<DimType = u64>) -> PageQuery {
        self.query_pages_overlaps(index_range)
    }

    fn is_empty(&self) -> bool {
        self.is_empty()
    }

    fn is_populated(&self) -> bool {
        self.is_populated()
    }

    fn primary_data_index(&self) -> &PageIndex<u64> {
        &self.spectrum_index
    }
}

impl SpectrumQueryIndex for SpectrumPointIndex {
    fn coordinate_overlaps(&self, query: &SimpleInterval<f64>) -> RowSelection {
        self.mz_index.row_selection_overlaps(query)
    }

    fn ion_mobility_overlaps(&self, im_range: &SimpleInterval<f64>) -> RowSelection {
        self.im_index.row_selection_overlaps(im_range)
    }

    fn index_overlaps(&self, index_range: &SimpleInterval<u64>) -> RowSelection {
        self.spectrum_index.row_selection_overlaps(index_range)
    }

    fn spectrum_data_index(&self) -> &PageIndex<u64> {
        &self.spectrum_index
    }
}

#[derive(Debug, Default, Clone)]
pub struct SpectrumChunkIndex {
    pub spectrum_index: PageIndex<u64>,
    pub start_mz_index: PageIndex<f64>,
    pub end_mz_index: PageIndex<f64>,
    pub time_index: Option<PageIndex<f32>>,
}

impl SpectrumChunkIndex {
    pub fn new(
        spectrum_index: PageIndex<u64>,
        start_mz_index: PageIndex<f64>,
        end_mz_index: PageIndex<f64>,
        time_index: Option<PageIndex<f32>>,
    ) -> Self {
        Self {
            spectrum_index,
            start_mz_index,
            end_mz_index,
            time_index,
        }
    }

    pub fn from_reader<T>(
        spectrum_data_reader: &ArrowReaderBuilder<T>,
        spectrum_array_indices: &ArrayIndex,
    ) -> Self {
        let pq_schema = spectrum_data_reader.parquet_schema();
        let mut this = Self::default();

        this.spectrum_index = read_u64_page_index_from(
            spectrum_data_reader.metadata(),
            pq_schema,
            &format!("{}.spectrum_index", spectrum_array_indices.prefix),
        )
        .unwrap_or_default();
        this.time_index = read_f32_page_index_from(
            spectrum_data_reader.metadata(),
            pq_schema,
            &format!("{}.spectrum_time", spectrum_array_indices.prefix),
        );

        for entry in spectrum_array_indices.iter() {
            if matches!(entry.array_type, ArrayType::MZArray) {
                this.start_mz_index = read_f64_page_index_from(
                    spectrum_data_reader.metadata(),
                    pq_schema,
                    &format!("{}_chunk_start", entry.path),
                )
                .unwrap_or_default();
                this.end_mz_index = read_f64_page_index_from(
                    spectrum_data_reader.metadata(),
                    pq_schema,
                    &format!("{}_chunk_end", entry.path),
                )
                .unwrap_or_default();
            }
        }
        this
    }

    pub fn query_pages(&self, spectrum_index: u64) -> PageQuery {
        let mut rg_idx_acc = Vec::new();
        let mut pages: Vec<PageIndexEntry<u64>> = Vec::new();

        for page in self.spectrum_index.pages_contains(spectrum_index) {
            if !rg_idx_acc.contains(&page.row_group_i) {
                rg_idx_acc.push(page.row_group_i);
            }
            pages.push(*page);
        }
        PageQuery::new(rg_idx_acc, pages)
    }

    pub fn query_pages_overlaps(&self, index_range: &impl Span1D<DimType = u64>) -> PageQuery {
        let mut rg_idx_acc = Vec::new();
        let mut pages: Vec<PageIndexEntry<u64>> = Vec::new();

        for page in self.spectrum_index.pages_overlaps(index_range) {
            if !rg_idx_acc.contains(&page.row_group_i) {
                rg_idx_acc.push(page.row_group_i);
            }
            pages.push(*page);
        }
        PageQuery::new(rg_idx_acc, pages)
    }

    pub fn is_empty(&self) -> bool {
        self.spectrum_index.is_empty()
    }

    pub fn is_populated(&self) -> bool {
        !self.spectrum_index.is_empty()
    }
}

impl BasicQueryIndex for SpectrumChunkIndex {
    fn query_pages(&self, spectrum_index: u64) -> PageQuery {
        self.query_pages(spectrum_index)
    }

    fn query_pages_overlaps(&self, index_range: &impl Span1D<DimType = u64>) -> PageQuery {
        self.query_pages_overlaps(index_range)
    }

    fn is_empty(&self) -> bool {
        self.is_empty()
    }

    fn is_populated(&self) -> bool {
        self.is_populated()
    }

    fn primary_data_index(&self) -> &PageIndex<u64> {
        &self.spectrum_index
    }
}

impl BasicChunkQueryIndex for SpectrumChunkIndex {
    fn chunk_start_index(&self) -> &PageIndex<f64> {
        &self.start_mz_index
    }

    fn chunk_end_index(&self) -> &PageIndex<f64> {
        &self.end_mz_index
    }
}

impl SpectrumQueryIndex for SpectrumChunkIndex {
    fn coordinate_overlaps(&self, mz_range: &SimpleInterval<f64>) -> RowSelection {
        let chunk_range_idx = RangeIndex::new(&self.start_mz_index, &self.end_mz_index);
        chunk_range_idx.row_selection_overlaps(mz_range)
    }

    fn ion_mobility_overlaps(&self, _im_range: &SimpleInterval<f64>) -> RowSelection {
        RowSelection::from(vec![RowSelector::select(
            self.spectrum_index.iter().map(|p| p.row_len()).sum::<i64>() as usize,
        )])
    }

    fn index_overlaps(&self, index_range: &SimpleInterval<u64>) -> RowSelection {
        self.spectrum_index.row_selection_overlaps(index_range)
    }

    fn spectrum_data_index(&self) -> &PageIndex<u64> {
        &self.spectrum_index
    }
}


#[derive(Debug, Default, Clone)]
pub struct ChromatogramPointIndex {
    pub chromatogram_index: PageIndex<u64>,
    pub time_index: PageIndex<f64>,
}

impl ChromatogramQueryIndex for ChromatogramPointIndex {
    fn query_chromatrogram_pages(&self, chromatogram_index: u64) -> PageQuery {
        self.query_pages(chromatogram_index)
    }

    fn query_chromatogram_pages_overlaps(
        &self,
        index_range: &impl Span1D<DimType = u64>,
    ) -> PageQuery {
        self.query_pages_overlaps(index_range)
    }

    fn chromatogram_data_index(&self) -> &PageIndex<u64> {
        &self.chromatogram_index
    }
}

impl ChromatogramPointIndex {
    pub fn new(chromatogram_index: PageIndex<u64>, time_index: PageIndex<f64>) -> Self {
        Self {
            chromatogram_index,
            time_index,
        }
    }

    pub fn from_reader<T>(
        chromatogram_data_reader: &ArrowReaderBuilder<T>,
        chromatogram_array_indices: &ArrayIndex,
    ) -> Self {
        let pq_schema = chromatogram_data_reader.parquet_schema();
        let mut this = Self::default();

        this.chromatogram_index = read_u64_page_index_from(
            chromatogram_data_reader.metadata(),
            pq_schema,
            &format!(
                "{}.{}",
                chromatogram_array_indices.prefix,
                BufferContext::Chromatogram.index_name()
            ),
        )
        .unwrap_or_default();

        for entry in chromatogram_array_indices.iter() {
            if matches!(entry.array_type, ArrayType::TimeArray) {
                this.time_index = read_f64_page_index_from(
                    chromatogram_data_reader.metadata(),
                    pq_schema,
                    &entry.path,
                )
                .unwrap_or_default();
            }
        }

        this
    }

    pub fn query_pages(&self, chromatogram_index: u64) -> PageQuery {
        let mut rg_idx_acc = Vec::new();
        let mut pages: Vec<PageIndexEntry<u64>> = Vec::new();

        for page in self.chromatogram_index.pages_contains(chromatogram_index) {
            if !rg_idx_acc.contains(&page.row_group_i) {
                rg_idx_acc.push(page.row_group_i);
            }
            pages.push(*page);
        }
        PageQuery::new(rg_idx_acc, pages)
    }

    pub fn query_pages_overlaps(&self, index_range: &impl Span1D<DimType = u64>) -> PageQuery {
        let mut rg_idx_acc = Vec::new();
        let mut pages: Vec<PageIndexEntry<u64>> = Vec::new();

        for page in self.chromatogram_index.pages_overlaps(index_range) {
            if !rg_idx_acc.contains(&page.row_group_i) {
                rg_idx_acc.push(page.row_group_i);
            }
            pages.push(*page);
        }
        PageQuery::new(rg_idx_acc, pages)
    }

    pub fn is_empty(&self) -> bool {
        self.chromatogram_index.is_empty()
    }

    pub fn is_populated(&self) -> bool {
        !self.chromatogram_index.is_empty()
    }
}

impl BasicQueryIndex for ChromatogramPointIndex {
    fn query_pages(&self, spectrum_index: u64) -> PageQuery {
        self.query_chromatrogram_pages(spectrum_index)
    }

    fn query_pages_overlaps(&self, index_range: &impl Span1D<DimType = u64>) -> PageQuery {
        self.query_chromatogram_pages_overlaps(index_range)
    }

    fn is_empty(&self) -> bool {
        self.chromatogram_index.is_empty()
    }

    fn is_populated(&self) -> bool {
        self.is_populated()
    }

    fn primary_data_index(&self) -> &PageIndex<u64> {
        &self.chromatogram_index
    }
}

impl SpectrumQueryIndex for ChromatogramPointIndex {
    fn spectrum_data_index(&self) -> &PageIndex<u64> {
        &self.chromatogram_index
    }

    fn index_overlaps(&self, index_range: &SimpleInterval<u64>) -> RowSelection {
        self.chromatogram_index.row_selection_overlaps(index_range)
    }

    fn coordinate_overlaps(&self, _mz_range: &SimpleInterval<f64>) -> RowSelection {
        RowSelection::default()
    }

    fn ion_mobility_overlaps(&self, _im_range: &SimpleInterval<f64>) -> RowSelection {
        RowSelection::default()
    }
}

#[derive(Debug, Default, Clone)]
pub struct ChromatogramChunkIndex {
    pub chromatogram_index: PageIndex<u64>,
    pub time_start_index: PageIndex<f64>,
    pub time_end_index: PageIndex<f64>,
}

impl ChromatogramChunkIndex {
    pub fn new(
        chromatogram_index: PageIndex<u64>,
        time_start_index: PageIndex<f64>,
        time_end_index: PageIndex<f64>,
    ) -> Self {
        Self {
            chromatogram_index,
            time_start_index,
            time_end_index,
        }
    }

    pub fn from_reader<T>(
        chromatogram_data_reader: &ArrowReaderBuilder<T>,
        chromatogram_array_indices: &ArrayIndex,
    ) -> Self {
        let peak_pq_schema = chromatogram_data_reader.parquet_schema();
        let mut this = Self::default();

        this.chromatogram_index = read_u64_page_index_from(
            chromatogram_data_reader.metadata(),
            peak_pq_schema,
            &format!("{}.chromatogram_index", chromatogram_array_indices.prefix),
        )
        .unwrap_or_default();

        for entry in chromatogram_array_indices.iter() {
            if matches!(entry.array_type, ArrayType::TimeArray)
                && matches!(entry.buffer_format, BufferFormat::ChunkBoundsStart)
            {
                this.time_start_index = read_f64_page_index_from(
                    chromatogram_data_reader.metadata(),
                    peak_pq_schema,
                    &entry.path,
                )
                .unwrap_or_default();
            } else if matches!(entry.array_type, ArrayType::TimeArray)
                && matches!(entry.buffer_format, BufferFormat::ChunkBoundsEnd)
            {
                this.time_end_index = read_f64_page_index_from(
                    chromatogram_data_reader.metadata(),
                    peak_pq_schema,
                    &entry.path,
                )
                .unwrap_or_default();
            }
        }

        this
    }
}

impl BasicQueryIndex for ChromatogramChunkIndex {
    fn query_pages(&self, chromatogram_index: u64) -> PageQuery {
        let mut rg_idx_acc = Vec::new();
        let mut pages: Vec<PageIndexEntry<u64>> = Vec::new();

        for page in self.chromatogram_index.pages_contains(chromatogram_index) {
            if !rg_idx_acc.contains(&page.row_group_i) {
                rg_idx_acc.push(page.row_group_i);
            }
            pages.push(*page);
        }
        PageQuery::new(rg_idx_acc, pages)
    }

    fn query_pages_overlaps(&self, index_range: &impl Span1D<DimType = u64>) -> PageQuery {
        let mut rg_idx_acc = Vec::new();
        let mut pages: Vec<PageIndexEntry<u64>> = Vec::new();

        for page in self.chromatogram_index.pages_overlaps(index_range) {
            if !rg_idx_acc.contains(&page.row_group_i) {
                rg_idx_acc.push(page.row_group_i);
            }
            pages.push(*page);
        }
        PageQuery::new(rg_idx_acc, pages)
    }

    fn is_empty(&self) -> bool {
        self.chromatogram_index.is_empty()
    }

    fn is_populated(&self) -> bool {
        !self.chromatogram_index.is_empty()
    }

    fn primary_data_index(&self) -> &PageIndex<u64> {
        &self.chromatogram_index
    }
}

impl BasicChunkQueryIndex for ChromatogramChunkIndex {
    fn chunk_start_index(&self) -> &PageIndex<f64> {
        &self.time_start_index
    }

    fn chunk_end_index(&self) -> &PageIndex<f64> {
        &self.time_end_index
    }
}

impl ChromatogramQueryIndex for ChromatogramChunkIndex {
    fn query_chromatrogram_pages(&self, chromatogram_index: u64) -> PageQuery {
        self.query_pages(chromatogram_index)
    }

    fn query_chromatogram_pages_overlaps(
        &self,
        index_range: &impl Span1D<DimType = u64>,
    ) -> PageQuery {
        self.query_pages_overlaps(index_range)
    }

    fn chromatogram_data_index(&self) -> &PageIndex<u64> {
        &self.chromatogram_index
    }
}

#[derive(Debug, Clone)]
pub enum GenericDataIndex<T: BasicQueryIndex + Default, U: BasicQueryIndex + BasicChunkQueryIndex + Default> {
    Point(T),
    Chunk(U),
}

impl<T: BasicQueryIndex + Default, U: BasicQueryIndex + BasicChunkQueryIndex + Default> Default for GenericDataIndex<T, U> {
    fn default() -> Self {
        GenericDataIndex::Point(Default::default())
    }
}


impl<T: BasicQueryIndex + Default, U: BasicQueryIndex + BasicChunkQueryIndex + Default> GenericDataIndex<T, U> {
    pub fn is_point(&self) -> bool {
        matches!(self, Self::Point(_))
    }

    pub fn is_chunked(&self) -> bool {
        matches!(self, Self::Chunk(_))
    }

    pub fn as_point(&self) -> Option<&T> {
        match self {
            Self::Point(x) => Some(x),
            _ => None
        }
    }

    pub fn as_chunked(&self) -> Option<&U> {
        match self {
            Self::Chunk(x) => Some(x),
            _ => None
        }
    }
}

impl<T: BasicQueryIndex + Default, U: BasicQueryIndex + BasicChunkQueryIndex + Default> BasicQueryIndex for GenericDataIndex<T, U> {
    fn query_pages(&self, spectrum_index: u64) -> PageQuery {
        match self {
            Self::Point(s) => s.query_pages(spectrum_index),
            Self::Chunk(s) => s.query_pages(spectrum_index)
        }
    }

    fn query_pages_overlaps(&self, index_range: &impl Span1D<DimType = u64>) -> PageQuery {
        match self {
            Self::Point(s) => s.query_pages_overlaps(index_range),
            Self::Chunk(s) => s.query_pages_overlaps(index_range)
        }
    }

    fn primary_data_index(&self) -> &PageIndex<u64> {
        match self {
            Self::Point(s) => s.primary_data_index(),
            Self::Chunk(s) => s.primary_data_index()
        }
    }
}


pub type SpectrumDataIndex = GenericDataIndex<SpectrumPointIndex, SpectrumChunkIndex>;

impl SpectrumQueryIndex for SpectrumDataIndex {
    fn index_overlaps(&self, index_range: &SimpleInterval<u64>) -> RowSelection {
        match self {
            Self::Point(s) => s.index_overlaps(index_range),
            Self::Chunk(s) => s.index_overlaps(index_range)
        }
    }

    fn coordinate_overlaps(&self, mz_range: &SimpleInterval<f64>) -> RowSelection {
        match self {
            Self::Point(s) => s.coordinate_overlaps(mz_range),
            Self::Chunk(s) => s.coordinate_overlaps(mz_range)
        }
    }

    fn ion_mobility_overlaps(&self, im_range: &SimpleInterval<f64>) -> RowSelection {
        match self {
            Self::Point(s) => s.ion_mobility_overlaps(im_range),
            Self::Chunk(s) => s.ion_mobility_overlaps(im_range)
        }
    }
}


pub type ChromatogramDataIndex = GenericDataIndex<ChromatogramPointIndex, ChromatogramChunkIndex>;

impl ChromatogramQueryIndex for ChromatogramDataIndex {
    fn query_chromatrogram_pages(&self, chromatogram_index: u64) -> PageQuery {
        match self {
            Self::Point(s) => s.query_chromatrogram_pages(chromatogram_index),
            Self::Chunk(s) => s.query_chromatrogram_pages(chromatogram_index)
        }
    }

    fn query_chromatogram_pages_overlaps(
        &self,
        index_range: &impl Span1D<DimType = u64>,
    ) -> PageQuery {
        match self {
            Self::Point(s) => s.query_chromatogram_pages_overlaps(index_range),
            Self::Chunk(s) => s.query_chromatogram_pages_overlaps(index_range)
        }
    }

    fn chromatogram_data_index(&self) -> &PageIndex<u64> {
        match self {
            Self::Point(s) => s.chromatogram_data_index(),
            Self::Chunk(s) => s.chromatogram_data_index()
        }
    }
}


#[derive(Debug, Default, Clone)]
pub struct WavelengthSpectrumPointIndex {
    pub spectrum_index: PageIndex<u64>,
    pub wavelength_index: PageIndex<f64>,
    pub time_index: Option<PageIndex<f64>>,
}

impl WavelengthSpectrumPointIndex {
    pub fn new(
        spectrum_index: PageIndex<u64>,
        wavelength_index: PageIndex<f64>,
        time_index: Option<PageIndex<f64>>,
    ) -> Self {
        Self {
            spectrum_index,
            wavelength_index,
            time_index,
        }
    }

    pub fn len(&self) -> usize {
        self.spectrum_index.len()
    }

    pub fn from_reader<T>(data_reader: &ArrowReaderBuilder<T>, array_indices: &ArrayIndex) -> Self {
        let pq_schema = data_reader.parquet_schema();
        let mut this = Self::default();

        this.spectrum_index = read_u64_page_index_from(
            data_reader.metadata(),
            pq_schema,
            &format!(
                "{}.{}",
                array_indices.prefix,
                BufferContext::WavelengthSpectrum.index_name()
            ),
        )
        .unwrap_or_default();

        for entry in array_indices.iter() {
            if matches!(entry.array_type, ArrayType::WavelengthArray) {
                this.wavelength_index =
                    read_f64_page_index_from(data_reader.metadata(), pq_schema, &entry.path)
                        .unwrap_or_default();
            }
        }
        this
    }
}

impl BasicQueryIndex for WavelengthSpectrumPointIndex {
    fn query_pages(&self, spectrum_index: u64) -> PageQuery {
        let mut rg_idx_acc = Vec::new();
        let mut pages: Vec<PageIndexEntry<u64>> = Vec::new();

        for page in self.spectrum_index.pages_contains(spectrum_index) {
            if !rg_idx_acc.contains(&page.row_group_i) {
                rg_idx_acc.push(page.row_group_i);
            }
            pages.push(*page);
        }
        PageQuery::new(rg_idx_acc, pages)
    }

    fn query_pages_overlaps(&self, index_range: &impl Span1D<DimType = u64>) -> PageQuery {
        let mut rg_idx_acc = Vec::new();
        let mut pages: Vec<PageIndexEntry<u64>> = Vec::new();

        for page in self.spectrum_index.pages_overlaps(index_range) {
            if !rg_idx_acc.contains(&page.row_group_i) {
                rg_idx_acc.push(page.row_group_i);
            }
            pages.push(*page);
        }
        PageQuery::new(rg_idx_acc, pages)
    }

    fn is_empty(&self) -> bool {
        self.spectrum_index.is_empty()
    }

    fn is_populated(&self) -> bool {
        !self.spectrum_index.is_empty()
    }

    fn primary_data_index(&self) -> &PageIndex<u64> {
        &self.spectrum_index
    }
}

impl SpectrumQueryIndex for WavelengthSpectrumPointIndex {
    fn spectrum_data_index(&self) -> &PageIndex<u64> {
        &self.spectrum_index
    }

    fn index_overlaps(&self, index_range: &SimpleInterval<u64>) -> RowSelection {
        self.spectrum_index.row_selection_overlaps(index_range)
    }

    fn coordinate_overlaps(&self, mz_range: &SimpleInterval<f64>) -> RowSelection {
        self.wavelength_index.row_selection_overlaps(mz_range)
    }

    fn ion_mobility_overlaps(&self, _im_range: &SimpleInterval<f64>) -> RowSelection {
        RowSelection::default()
    }
}

#[derive(Debug, Default, Clone)]
pub struct WavelengthSpectrumChunkIndex {
    pub spectrum_index: PageIndex<u64>,
    pub wavelength_start_index: PageIndex<f64>,
    pub wavelength_end_index: PageIndex<f64>,
}

impl WavelengthSpectrumChunkIndex {
    pub fn new(
        spectrum_index: PageIndex<u64>,
        wavelength_start_index: PageIndex<f64>,
        wavelength_end_index: PageIndex<f64>,
    ) -> Self {
        Self {
            spectrum_index,
            wavelength_start_index,
            wavelength_end_index,
        }
    }

    pub fn len(&self) -> usize {
        self.spectrum_index.len()
    }

    pub fn from_reader<T>(data_reader: &ArrowReaderBuilder<T>, array_indices: &ArrayIndex) -> Self {
        let peak_pq_schema = data_reader.parquet_schema();
        let mut this = Self::default();

        this.spectrum_index = read_u64_page_index_from(
            data_reader.metadata(),
            peak_pq_schema,
            &format!("{}.spectrum_index", array_indices.prefix),
        )
        .unwrap_or_default();

        for entry in array_indices.iter() {
            if matches!(entry.array_type, ArrayType::TimeArray)
                && matches!(entry.buffer_format, BufferFormat::ChunkBoundsStart)
            {
                this.wavelength_start_index =
                    read_f64_page_index_from(data_reader.metadata(), peak_pq_schema, &entry.path)
                        .unwrap_or_default();
            } else if matches!(entry.array_type, ArrayType::TimeArray)
                && matches!(entry.buffer_format, BufferFormat::ChunkBoundsEnd)
            {
                this.wavelength_end_index =
                    read_f64_page_index_from(data_reader.metadata(), peak_pq_schema, &entry.path)
                        .unwrap_or_default();
            }
        }

        this
    }
}

impl BasicQueryIndex for WavelengthSpectrumChunkIndex {
    fn query_pages(&self, spectrum_index: u64) -> PageQuery {
        let mut rg_idx_acc = Vec::new();
        let mut pages: Vec<PageIndexEntry<u64>> = Vec::new();

        for page in self.spectrum_index.pages_contains(spectrum_index) {
            if !rg_idx_acc.contains(&page.row_group_i) {
                rg_idx_acc.push(page.row_group_i);
            }
            pages.push(*page);
        }
        PageQuery::new(rg_idx_acc, pages)
    }

    fn query_pages_overlaps(&self, index_range: &impl Span1D<DimType = u64>) -> PageQuery {
        let mut rg_idx_acc = Vec::new();
        let mut pages: Vec<PageIndexEntry<u64>> = Vec::new();

        for page in self.spectrum_index.pages_overlaps(index_range) {
            if !rg_idx_acc.contains(&page.row_group_i) {
                rg_idx_acc.push(page.row_group_i);
            }
            pages.push(*page);
        }
        PageQuery::new(rg_idx_acc, pages)
    }

    fn is_empty(&self) -> bool {
        self.spectrum_index.is_empty()
    }

    fn is_populated(&self) -> bool {
        !self.spectrum_index.is_empty()
    }

    fn primary_data_index(&self) -> &PageIndex<u64> {
        &self.spectrum_index
    }
}

impl BasicChunkQueryIndex for WavelengthSpectrumChunkIndex {
    fn chunk_start_index(&self) -> &PageIndex<f64> {
        &self.wavelength_start_index
    }

    fn chunk_end_index(&self) -> &PageIndex<f64> {
        &self.wavelength_end_index
    }
}

impl SpectrumQueryIndex for WavelengthSpectrumChunkIndex {
    fn coordinate_overlaps(&self, coordinate_range: &SimpleInterval<f64>) -> RowSelection {
        let chunk_range_idx =
            RangeIndex::new(&self.wavelength_start_index, &self.wavelength_end_index);
        chunk_range_idx.row_selection_overlaps(coordinate_range)
    }

    fn ion_mobility_overlaps(&self, _im_range: &SimpleInterval<f64>) -> RowSelection {
        RowSelection::default()
    }

    fn index_overlaps(&self, index_range: &SimpleInterval<u64>) -> RowSelection {
        self.spectrum_index.row_selection_overlaps(index_range)
    }

    fn spectrum_data_index(&self) -> &PageIndex<u64> {
        &self.spectrum_index
    }
}

pub type WavelengthSpectrumDataIndex = GenericDataIndex<WavelengthSpectrumPointIndex, WavelengthSpectrumChunkIndex>;

impl SpectrumQueryIndex for WavelengthSpectrumDataIndex {
    fn index_overlaps(&self, index_range: &SimpleInterval<u64>) -> RowSelection {
        match self {
            Self::Point(s) => s.index_overlaps(index_range),
            Self::Chunk(s) => s.index_overlaps(index_range)
        }
    }

    fn coordinate_overlaps(&self, mz_range: &SimpleInterval<f64>) -> RowSelection {
        match self {
            Self::Point(s) => s.coordinate_overlaps(mz_range),
            Self::Chunk(s) => s.coordinate_overlaps(mz_range)
        }
    }

    fn ion_mobility_overlaps(&self, im_range: &SimpleInterval<f64>) -> RowSelection {
        match self {
            Self::Point(s) => s.ion_mobility_overlaps(im_range),
            Self::Chunk(s) => s.ion_mobility_overlaps(im_range)
        }
    }
}


#[derive(Debug, Default, Clone)]
pub struct WavelengthSpectrumIndex {
    pub index: PageIndex<u64>,
    pub data_index: WavelengthSpectrumDataIndex,
}

impl WavelengthSpectrumIndex {
    pub fn new(
        index: PageIndex<u64>,
        data_index: WavelengthSpectrumDataIndex
    ) -> Self {
        Self {
            index,
            data_index,
        }
    }
}

#[derive(Debug, Default, Clone)]
pub struct SpectrumMetadataIndex {
    pub time_index: PageIndex<f64>,
    pub ms_level_index: PageIndex<u8>,
    pub index_index: PageIndex<u64>,
    pub scan_index: PageIndex<u64>,
    pub precursor_index: PageIndex<u64>,
    pub selected_ion_index: PageIndex<u64>,

    pub data_index: SpectrumDataIndex,
    pub peak_point_index: Option<SpectrumPointIndex>,
}

#[derive(Debug, Default, Clone)]
pub struct ChromatogramMetadataIndex {
    pub chromatogram_index_index: PageIndex<u64>,
    pub precursor_index: PageIndex<u64>,
    pub selected_ion_index: PageIndex<u64>,
}

impl ChromatogramMetadataIndex {
    pub fn new(
        chromatogram_index: PageIndex<u64>,
        precursor_index: PageIndex<u64>,
        selected_ion_index: PageIndex<u64>,
    ) -> Self {
        Self {
            chromatogram_index_index: chromatogram_index,
            precursor_index,
            selected_ion_index,
        }
    }
}

#[derive(Debug, Default, Clone)]
pub struct QueryIndex {
    pub spectrum: SpectrumMetadataIndex,

    pub chromatogram_index_index: PageIndex<u64>,
    pub chromatogram_precursor_index: PageIndex<u64>,
    pub chromatogram_selected_ion_index: PageIndex<u64>,

    pub chromatogram_data_index: ChromatogramDataIndex,

    pub wavelength_spectrum_index: Option<WavelengthSpectrumIndex>,
}


impl QueryIndex {
    /// Populate the indices for spectrum metadata
    pub fn populate_spectrum_metadata_indices<T>(
        &mut self,
        spectrum_metadata_reader: &ArrowReaderBuilder<T>,
    ) {
        let pq_schema = spectrum_metadata_reader.parquet_schema();

        self.spectrum.index_index = read_u64_page_index_from(
            spectrum_metadata_reader.metadata(),
            pq_schema,
            "spectrum.index",
        )
        .unwrap_or_default();
        self.spectrum.time_index = read_f64_page_index_from(
            spectrum_metadata_reader.metadata(),
            pq_schema,
            "spectrum.time",
        )
        .unwrap_or_default();
        self.spectrum.ms_level_index = read_u8_page_index_from(
            spectrum_metadata_reader.metadata(),
            pq_schema,
            "spectrum.ms_level",
        )
        .or_else(|| {
            read_u8_page_index_from(
                spectrum_metadata_reader.metadata(),
                pq_schema,
                "spectrum.MS_1000511_ms_level",
            )
        })
        .unwrap_or_default();
        self.spectrum.scan_index = read_u64_page_index_from(
            spectrum_metadata_reader.metadata(),
            pq_schema,
            "scan.spectrum_index",
        )
        .or_else(|| {
            read_u64_page_index_from(
                spectrum_metadata_reader.metadata(),
                pq_schema,
                "scan.source_index",
            )
        })
        .unwrap_or_default();
        self.spectrum.precursor_index = read_u64_page_index_from(
            spectrum_metadata_reader.metadata(),
            pq_schema,
            "precursor.spectrum_index",
        )
        .or_else(|| {
            read_u64_page_index_from(
                spectrum_metadata_reader.metadata(),
                pq_schema,
                "precursor.source_index",
            )
        })
        .unwrap_or_default();
        self.spectrum.selected_ion_index = read_u64_page_index_from(
            spectrum_metadata_reader.metadata(),
            pq_schema,
            "selected_ion.spectrum_index",
        )
        .or_else(|| {
            read_u64_page_index_from(
                spectrum_metadata_reader.metadata(),
                pq_schema,
                "selected_ion.source_index",
            )
        })
        .unwrap_or_default();
    }

    pub fn populate_chromatogram_metadata_indices<T>(
        &mut self,
        chromatogram_metadata_reader: &ArrowReaderBuilder<T>,
    ) {
        let pq_schema = chromatogram_metadata_reader.parquet_schema();

        self.chromatogram_index_index = read_u64_page_index_from(
            chromatogram_metadata_reader.metadata(),
            pq_schema,
            "chromatogram.index",
        )
        .unwrap_or_default();
        self.chromatogram_precursor_index = read_u64_page_index_from(
            chromatogram_metadata_reader.metadata(),
            pq_schema,
            "precursor.spectrum_index",
        )
        .unwrap_or_default();
        self.chromatogram_selected_ion_index = read_u64_page_index_from(
            chromatogram_metadata_reader.metadata(),
            pq_schema,
            "selected_ion.spectrum_index",
        )
        .unwrap_or_default();
    }

    pub fn populate_wavelength_spectrum_metadata_indices<T>(
        &mut self,
        metadata_reader: &ArrowReaderBuilder<T>
    ) {
        let pq_schema = metadata_reader.parquet_schema();

        let mut wavelength_index = self.wavelength_spectrum_index.take().unwrap_or_default();

        wavelength_index.index = read_u64_page_index_from(
            metadata_reader.metadata(),
            pq_schema,
            "spectrum.index",
        )
        .unwrap_or_default();

        self.wavelength_spectrum_index = Some(wavelength_index);
    }

    /// Populate the indices for spectrum signal data
    pub fn populate_spectrum_data_indices<T>(
        &mut self,
        spectrum_data_reader: &ArrowReaderBuilder<T>,
        spectrum_array_indices: &ArrayIndex,
    ) {
        if BufferFormat::Point.prefix() == spectrum_array_indices.prefix {
            self.spectrum.data_index = SpectrumDataIndex::Point(SpectrumPointIndex::from_reader(spectrum_data_reader, spectrum_array_indices));
        } else if BufferFormat::Chunk.prefix() == spectrum_array_indices.prefix {
            self.spectrum.data_index = SpectrumDataIndex::Chunk(SpectrumChunkIndex::from_reader(spectrum_data_reader, spectrum_array_indices));
        } else {
            log::error!("Prefix {} not recognized", spectrum_array_indices.prefix)
        }
    }

    pub fn populate_wavelength_spectrum_data_indices<T>(
        &mut self,
        data_reader: &ArrowReaderBuilder<T>,
        array_indices: &ArrayIndex,
    ) {

        let mut wavelength_index = self.wavelength_spectrum_index.take().unwrap_or_default();

        if BufferFormat::Point.prefix() == array_indices.prefix {
            wavelength_index.data_index = WavelengthSpectrumDataIndex::Point(WavelengthSpectrumPointIndex::from_reader(
                data_reader,
                array_indices,
            ));
        } else if BufferFormat::Chunk.prefix() == array_indices.prefix {
            wavelength_index.data_index = WavelengthSpectrumDataIndex::Chunk(WavelengthSpectrumChunkIndex::from_reader(
                data_reader,
                array_indices,
            ));
        } else {
            log::error!(
                "Wavelength prefix {} not recognized",
                array_indices.prefix
            )
        }
        self.wavelength_spectrum_index = Some(wavelength_index);
    }

    pub fn populate_chromatogram_data_indices<T>(
        &mut self,
        chromatogram_data_reader: &ArrowReaderBuilder<T>,
        chromatogram_array_indices: &ArrayIndex,
    ) {
        if BufferFormat::Point.prefix() == chromatogram_array_indices.prefix {
            self.chromatogram_data_index = ChromatogramDataIndex::Point(ChromatogramPointIndex::from_reader(
                chromatogram_data_reader,
                chromatogram_array_indices,
            ));
        } else if BufferFormat::Chunk.prefix() == chromatogram_array_indices.prefix {
            self.chromatogram_data_index = ChromatogramDataIndex::Chunk(ChromatogramChunkIndex::from_reader(
                chromatogram_data_reader,
                chromatogram_array_indices,
            ));
        } else {
            log::error!(
                "Chromatogram prefix {} not recognized",
                chromatogram_array_indices.prefix
            )
        }
    }

    pub fn query_pages(&self, spectrum_index: u64) -> PageQuery {
        self.spectrum.data_index.query_pages(spectrum_index)
    }
}

impl BasicQueryIndex for QueryIndex {
    fn query_pages(&self, spectrum_index: u64) -> PageQuery {
        self.spectrum.data_index.query_pages(spectrum_index)
    }

    fn query_pages_overlaps(&self, index_range: &impl Span1D<DimType = u64>) -> PageQuery {
        self.spectrum.data_index.query_pages_overlaps(index_range)
    }

    fn is_empty(&self) -> bool {
        self.spectrum.data_index.is_empty()
    }

    fn is_populated(&self) -> bool {
        self.spectrum.data_index.is_populated()
    }

    fn primary_data_index(&self) -> &PageIndex<u64> {
        self.spectrum.data_index.primary_data_index()
    }
}

impl SpectrumQueryIndex for QueryIndex {
    fn coordinate_overlaps(&self, mz_range: &SimpleInterval<f64>) -> RowSelection {
        self.spectrum.data_index.coordinate_overlaps(mz_range)
    }

    fn ion_mobility_overlaps(&self, im_range: &SimpleInterval<f64>) -> RowSelection {
        self.spectrum.data_index.ion_mobility_overlaps(im_range)
    }

    fn index_overlaps(&self, index_range: &SimpleInterval<u64>) -> RowSelection {
        self.spectrum.data_index.index_overlaps(index_range)
    }

    fn spectrum_data_index(&self) -> &PageIndex<u64> {
        self.spectrum.data_index.spectrum_data_index()
    }
}

impl ChromatogramQueryIndex for QueryIndex {
    fn query_chromatrogram_pages(&self, chromatogram_index: u64) -> PageQuery {
        self.chromatogram_data_index.query_chromatrogram_pages(chromatogram_index)
    }

    fn query_chromatogram_pages_overlaps(
        &self,
        index_range: &impl Span1D<DimType = u64>,
    ) -> PageQuery {
        self.chromatogram_data_index.query_chromatogram_pages_overlaps(index_range)
    }

    fn chromatogram_data_index(&self) -> &PageIndex<u64> {
        self.chromatogram_data_index.chromatogram_data_index()
    }
}

#[derive(Debug, Default)]
pub struct PageQuery {
    pub row_group_indices: Vec<usize>,
    pub pages: Vec<PageIndexEntry<u64>>,
}

impl PageQuery {
    pub fn new(row_group_indices: Vec<usize>, pages: Vec<PageIndexEntry<u64>>) -> Self {
        Self {
            row_group_indices,
            pages,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.pages.is_empty()
    }

    pub fn num_pages(&self) -> usize {
        self.pages.len()
    }

    pub fn num_row_groups(&self) -> usize {
        self.row_group_indices.len()
    }

    pub fn can_split(&self) -> bool {
        self.row_group_indices.len() > 5
    }

    // This looks correct but still needs work. "selection contains less than the number of selected rows"
    pub fn row_selection_overlaps(&self, query: &impl Span1D<DimType = u64>) -> RowSelection {
        let mut selectors = Vec::new();
        let mut last_row = 0;
        for page in self.pages.iter() {
            if page.start_row() != last_row {
                selectors.push(RowSelector::skip((page.start_row() - last_row) as usize));
            }
            if page.overlaps(&query) {
                selectors.push(RowSelector::select(page.row_len() as usize));
            } else {
                selectors.push(RowSelector::skip(page.row_len() as usize))
            }
            last_row = page.end_row();
        }
        selectors.into()
    }

    // This looks correct but still needs work. "selection contains less than the number of selected rows"
    pub fn split_row_groups(&mut self) -> Option<Self> {
        if !self.can_split() {
            return None;
        }
        let i = self.row_group_indices.len() / 2;
        if self.row_group_indices.get(i).is_some() {
            let split_row_groups = self.row_group_indices.split_off(i);
            let row_group_i = split_row_groups.first().copied().unwrap();
            let page_i = self
                .pages
                .iter()
                .position(|p| p.row_group() >= row_group_i)
                .unwrap();
            let split_pages = self.pages.split_off(page_i);
            Some(PageQuery::new(split_row_groups, split_pages))
        } else {
            None
        }
    }

    pub fn get_num_rows_to_skip_for_row_groups(&self, meta: &ParquetMetaData) -> u64 {
        let mut up_to_first_row = 0;
        for i in 0..self.row_group_indices[0] {
            let rg = meta.row_group(i);
            up_to_first_row += rg.num_rows();
        }
        up_to_first_row as u64
    }

    pub fn value_range(&self) -> Option<SimpleInterval<u64>> {
        if self.is_empty() {
            return None;
        }
        let start = self.pages.first().map(|p| p.min).unwrap();
        let end = self.pages.last().map(|p| p.max).unwrap();
        Some(SimpleInterval::new(start, end))
    }
}
