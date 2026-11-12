use std::marker::PhantomData;

use columnar::{Column, ColumnType, MonotonicallyMappableToU64};

use crate::collector::sort_key::NaturalComparator;
use crate::collector::{SegmentSortKeyComputer, SortKeyComputer};
use crate::fastfield::{FastFieldNotAvailableError, FastValue};
use crate::schema::{value_type_to_column_type, Schema};
use crate::{DocId, Score, SegmentReader};

/// The column types a fast value of type `T` can be sorted by.
const NUMERIC_COLUMN_TYPES: [ColumnType; 3] =
    [ColumnType::I64, ColumnType::U64, ColumnType::F64];

/// Sorts by a fast value (u64, i64, f64, bool).
///
/// The field must either appear explicitly in the schema, with the right type, and declared as
/// a fast field, or be a path within a fast JSON field (e.g. `attrs.due` for a JSON field
/// `attrs`).
///
/// If the field is multivalued, only the first value is considered.
///
/// Documents that do not have this value are still considered.
/// Their sort key will simply be `None`.
#[derive(Debug, Clone)]
pub struct SortByStaticFastValue<T: FastValue> {
    field: String,
    typ: PhantomData<T>,
}

impl<T: FastValue> SortByStaticFastValue<T> {
    /// Creates a new `SortByStaticFastValue` instance for the given field.
    pub fn for_field(column_name: impl ToString) -> SortByStaticFastValue<T> {
        Self {
            field: column_name.to_string(),
            typ: PhantomData,
        }
    }
}

impl<T: FastValue> SortKeyComputer for SortByStaticFastValue<T> {
    type Child = SortByFastValueSegmentSortKeyComputer<T>;
    type SortKey = Option<T>;
    type Comparator = NaturalComparator;

    fn check_schema(&self, schema: &Schema) -> crate::Result<()> {
        // A path within a fast JSON field (e.g. `attrs.due`) is not part of the schema, and its
        // type may vary from segment to segment. It is therefore validated when opening the
        // columns of each segment, in `segment_sort_key_computer`.
        let default_field_opt = if cfg!(feature = "quickwit") {
            schema.get_field("_dynamic").ok()
        } else {
            None
        };
        if schema
            .find_json_field_path(&self.field, default_field_opt)
            .is_some()
        {
            return Ok(());
        }
        // At the segment sort key computer level, we rely on the u64 representation.
        // The mapping is monotonic, so it is sufficient to compute our top-K docs.
        let field = schema.get_field(&self.field)?;
        let field_entry = schema.get_field_entry(field);
        if !field_entry.is_fast() {
            return Err(crate::TantivyError::SchemaError(format!(
                "Field `{}` is not a fast field.",
                self.field,
            )));
        }
        let schema_type = field_entry.field_type().value_type();
        if schema_type != T::to_type() {
            return Err(crate::TantivyError::SchemaError(format!(
                "Field `{}` is of type {schema_type:?}, not of the type {:?}.",
                self.field,
                T::to_type()
            )));
        }
        Ok(())
    }

    fn segment_sort_key_computer(
        &self,
        segment_reader: &SegmentReader,
    ) -> crate::Result<Self::Child> {
        let fast_fields = segment_reader.fast_fields();
        let column_type = value_type_to_column_type(T::to_type())
            .expect("fast values are always associated to a column type");
        // Prefer the column matching the requested type, and fall back to another numerical
        // column (e.g. the path is stored as `i64` in this segment, but as `f64` in another
        // one). Values are then converted to `T` so that they remain comparable across
        // segments.
        let whitelist: &[ColumnType] = if T::to_type().numerical_type().is_some() {
            &NUMERIC_COLUMN_TYPES
        } else {
            std::slice::from_ref(&column_type)
        };
        let columns_and_types = fast_fields.u64_lenient_for_type_all(Some(whitelist), &self.field)?;
        let sort_column_opt = columns_and_types
            .iter()
            .find(|(_, typ)| *typ == column_type)
            .or_else(|| columns_and_types.first())
            .map(|(column, typ)| (column.clone(), *typ));

        // A JSON path may be missing from this segment altogether: in that case, all documents
        // get `None` as their sort key. A missing regular fast field is an error.
        if sort_column_opt.is_none() && !fast_fields.is_json_path(&self.field) {
            return Err(FastFieldNotAvailableError {
                field_name: self.field.clone(),
            }
            .into());
        }
        Ok(SortByFastValueSegmentSortKeyComputer {
            sort_column_opt,
            typ: PhantomData,
        })
    }
}

pub struct SortByFastValueSegmentSortKeyComputer<T> {
    sort_column_opt: Option<(Column<u64>, ColumnType)>,
    typ: PhantomData<T>,
}

impl<T: FastValue> SegmentSortKeyComputer for SortByFastValueSegmentSortKeyComputer<T> {
    type SortKey = Option<T>;
    type SegmentSortKey = Option<u64>;
    type SegmentComparator = NaturalComparator;

    #[inline(always)]
    fn segment_sort_key(&mut self, doc: DocId, _score: Score) -> Self::SegmentSortKey {
        let (sort_column, column_type) = self.sort_column_opt.as_ref()?;
        let sort_key = sort_column.first(doc)?;
        Some(map_to_t_sort_key::<T>(sort_key, *column_type))
    }

    fn convert_segment_sort_key(&self, sort_key: Self::SegmentSortKey) -> Self::SortKey {
        sort_key.map(T::from_u64)
    }
}

/// Maps the raw `u64` representation of a value in a column of the given `column_type` to the
/// `u64` representation of the same value as a `T`.
///
/// This makes sort keys comparable across segments, even when the column type backing the
/// sorted path differs from one segment to another (e.g. `i64` in one segment, `f64` in
/// another). The conversion goes through `f64`, like the `OwnedValue` comparator does.
#[inline(always)]
fn map_to_t_sort_key<T: FastValue>(sort_key: u64, column_type: ColumnType) -> u64 {
    if value_type_to_column_type(T::to_type()) == Some(column_type) {
        // The raw value is already in `T`'s u64 representation.
        return sort_key;
    }
    let numerical_type = column_type.numerical_type();
    debug_assert!(
        T::to_type().numerical_type().is_some() && numerical_type.is_some(),
        "non-matching column types are only tolerated for numerical values"
    );
    let value_f64 = match numerical_type {
        Some(columnar::NumericalType::I64) => i64::from_u64(sort_key) as f64,
        Some(columnar::NumericalType::U64) => sort_key as f64,
        Some(columnar::NumericalType::F64) => f64::from_u64(sort_key),
        None => return sort_key,
    };
    match T::to_type() {
        crate::schema::Type::I64 => (value_f64 as i64).to_u64(),
        // Saturating cast: values outside of `u64`'s range (e.g. negative ones) clamp to the
        // nearest bound, like the `OwnedValue` comparisons of mixed numeric types do.
        crate::schema::Type::U64 => (value_f64 as u64).to_u64(),
        crate::schema::Type::F64 => value_f64.to_u64(),
        _ => sort_key,
    }
}
