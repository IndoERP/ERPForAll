//! Row abstraction for import (`load`) without matrix copies (PERF.LOAD-STREAM).

use std::marker::PhantomData;

use erp_fields::{Error, Result, Value};

/// A row-major source of string cells for import extraction.
pub trait RowSource<'a>: Send + Sync {
    /// Number of rows.
    fn len(&self) -> usize;

    /// Whether there are no rows.
    fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Number of columns in `row`.
    fn row_len(&self, row: usize) -> usize;

    /// Cell at `(row, col)` as string slice with lifetime `'a`.
    fn cell(&self, row: usize, col: usize) -> &'a str;
}

impl<'a, R: RowSource<'a> + ?Sized> RowSource<'a> for &R {
    #[inline]
    fn len(&self) -> usize {
        (**self).len()
    }

    #[inline]
    fn row_len(&self, row: usize) -> usize {
        (**self).row_len(row)
    }

    #[inline]
    fn cell(&self, row: usize, col: usize) -> &'a str {
        (**self).cell(row, col)
    }
}

impl<'a> RowSource<'a> for &'a [Vec<String>] {
    #[inline]
    fn len(&self) -> usize {
        <[Vec<String>]>::len(self)
    }

    #[inline]
    fn row_len(&self, row: usize) -> usize {
        self[row].len()
    }

    #[inline]
    fn cell(&self, row: usize, col: usize) -> &'a str {
        &self[row][col]
    }
}

impl<'a> RowSource<'a> for &'a [crate::csv::CsvRow] {
    #[inline]
    fn len(&self) -> usize {
        <[crate::csv::CsvRow]>::len(self)
    }

    #[inline]
    fn row_len(&self, row: usize) -> usize {
        self[row].values().len()
    }

    #[inline]
    fn cell(&self, row: usize, col: usize) -> &'a str {
        &self[row].values()[col]
    }
}

impl<'a> RowSource<'a> for &'a [&'a [&'a str]] {
    #[inline]
    fn len(&self) -> usize {
        <[&[&str]]>::len(self)
    }

    #[inline]
    fn row_len(&self, row: usize) -> usize {
        self[row].len()
    }

    #[inline]
    fn cell(&self, row: usize, col: usize) -> &'a str {
        self[row][col]
    }
}

/// A view into a subset of rows and columns of an underlying [`RowSource`].
pub struct MappedRowView<'s, 'a> {
    pub(crate) source: &'s (dyn RowSource<'a> + 's),
    pub(crate) row_indices: &'s [usize],
    pub(crate) col_indices: &'s [usize],
    pub(crate) _marker: PhantomData<&'a str>,
}

impl<'s, 'a> RowSource<'a> for MappedRowView<'s, 'a> {
    #[inline]
    fn len(&self) -> usize {
        self.row_indices.len()
    }

    #[inline]
    fn row_len(&self, _row: usize) -> usize {
        self.col_indices.len()
    }

    #[inline]
    fn cell(&self, row: usize, col: usize) -> &'a str {
        let real_row = self.row_indices[row];
        let real_col = self.col_indices[col];
        self.source.cell(real_row, real_col)
    }
}

/// Bounded view over JSON-2 RPC `Value::List` rows and string cells.
#[derive(Clone, Copy, Debug)]
pub struct ValueRows<'a> {
    rows: &'a [Value],
}

impl<'a> ValueRows<'a> {
    /// Validate JSON-2 data array shape and cell types before extracting.
    pub fn validate(data: &'a [Value], max_rows: usize) -> Result<Self> {
        if data.len() > max_rows {
            return Err(Error::validation(format!(
                "load received {} rows; at most {max_rows} are accepted per call",
                data.len()
            )));
        }
        for (row_idx, row) in data.iter().enumerate() {
            let Value::List(cells) = row else {
                return Err(Error::validation(format!(
                    "load row {row_idx} must be an array"
                )));
            };
            for (col_idx, cell) in cells.iter().enumerate() {
                if !matches!(cell, Value::Str(_)) {
                    return Err(Error::validation(format!(
                        "load cell at row {row_idx}, column {col_idx} must be a string"
                    )));
                }
            }
        }
        Ok(Self { rows: data })
    }
}

impl<'a> RowSource<'a> for ValueRows<'a> {
    #[inline]
    fn len(&self) -> usize {
        self.rows.len()
    }

    #[inline]
    fn row_len(&self, row: usize) -> usize {
        match &self.rows[row] {
            Value::List(cells) => cells.len(),
            _ => 0,
        }
    }

    #[inline]
    fn cell(&self, row: usize, col: usize) -> &'a str {
        match &self.rows[row] {
            Value::List(cells) => match &cells[col] {
                Value::Str(s) => s.as_ref(),
                _ => unreachable!("ValueRows::validate admits only string cells"),
            },
            _ => unreachable!("ValueRows::validate admits only array rows; row_len is 0 otherwise"),
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "tests")]
mod tests {
    use super::*;

    #[test]
    fn row_source_for_vec_strings() {
        let data = vec![vec!["a".to_owned(), "b".to_owned()], vec!["c".to_owned()]];
        let src: &[Vec<String>] = &data;
        assert_eq!(src.len(), 2);
        assert_eq!(src.row_len(0), 2);
        assert_eq!(src.row_len(1), 1);
        assert_eq!(src.cell(0, 0), "a");
        assert_eq!(src.cell(0, 1), "b");
        assert_eq!(src.cell(1, 0), "c");
    }

    #[test]
    fn row_source_for_str_slices() {
        let data: &[&[&str]] = &[&["x", "y"], &["z"]];
        assert_eq!(data.len(), 2);
        assert_eq!(data.row_len(0), 2);
        assert_eq!(data.row_len(1), 1);
        assert_eq!(data.cell(0, 0), "x");
        assert_eq!(data.cell(0, 1), "y");
        assert_eq!(data.cell(1, 0), "z");
    }

    #[test]
    fn mapped_row_view() {
        let data: &[&[&str]] = &[
            &["r0c0", "r0c1", "r0c2"],
            &["r1c0", "r1c1", "r1c2"],
            &["r2c0", "r2c1", "r2c2"],
        ];
        let rows = [0, 2];
        let cols = [2, 0];
        let view = MappedRowView {
            source: &data as &dyn RowSource<'_>,
            row_indices: &rows,
            col_indices: &cols,
            _marker: PhantomData,
        };
        assert_eq!(view.len(), 2);
        assert_eq!(view.row_len(0), 2);
        assert_eq!(view.cell(0, 0), "r0c2");
        assert_eq!(view.cell(0, 1), "r0c0");
        assert_eq!(view.cell(1, 0), "r2c2");
        assert_eq!(view.cell(1, 1), "r2c0");
    }

    #[test]
    fn value_rows_validation_and_access() {
        let data = vec![
            Value::list([Value::str("hello"), Value::str("world")]),
            Value::list([Value::str("foo"), Value::str("bar")]),
        ];
        let vr = ValueRows::validate(&data, 10).unwrap();
        assert_eq!(vr.len(), 2);
        assert_eq!(vr.row_len(0), 2);
        assert_eq!(vr.cell(0, 0), "hello");
        assert_eq!(vr.cell(0, 1), "world");
        assert_eq!(vr.cell(1, 0), "foo");
        assert_eq!(vr.cell(1, 1), "bar");

        let err_cap = ValueRows::validate(&data, 1).unwrap_err();
        assert_eq!(
            err_cap.message(),
            "load received 2 rows; at most 1 are accepted per call"
        );

        let bad_row = vec![Value::Int(42)];
        let err_row = ValueRows::validate(&bad_row, 10).unwrap_err();
        assert_eq!(err_row.message(), "load row 0 must be an array");

        let bad_cell = vec![Value::list([Value::Int(42)])];
        let err_cell = ValueRows::validate(&bad_cell, 10).unwrap_err();
        assert_eq!(
            err_cell.message(),
            "load cell at row 0, column 0 must be a string"
        );
    }
}
