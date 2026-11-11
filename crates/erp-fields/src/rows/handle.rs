//! Handles into a batch (`Value::Row`, `Value::Rows`) and the read-only dict view over rows and
//! maps.

use std::borrow::Borrow;
use std::fmt;
use std::hash::{DefaultHasher, Hash, Hasher};

use super::Rows;
use super::column::Children;
use crate::{Error, Result, Value, ValueMap};

/// One row of a batch: the batch's `Arc` and a row position, 16 bytes. Reads like a dict
/// through [`RowRef::get`] or [`Value::dict`]; equal to and hashed like a [`Value::Map`] with the
/// same entries.
#[derive(Clone)]
pub struct RowRef {
    rows: Rows,
    row: u32,
}

impl RowRef {
    /// Row `row` of `rows`; the caller checked `row < rows.len()`.
    pub(crate) fn new(rows: Rows, row: u32) -> Self {
        Self { rows, row }
    }

    /// The batch the row belongs to.
    #[must_use]
    pub fn rows(&self) -> &Rows {
        &self.rows
    }

    /// The row's position in [`RowRef::rows`].
    #[must_use]
    pub fn index(&self) -> usize {
        self.row as usize
    }

    /// The value of `key`.
    ///
    /// # Errors
    /// `Programming` for a key the layout lacks or a key absent in this row.
    pub fn get(&self, key: &str) -> Result<Value> {
        self.rows.get(self.index(), key)
    }

    /// The value of `key`, `Ok(None)` when the row lacks it.
    ///
    /// # Errors
    /// `Programming` for a key the layout lacks.
    pub fn get_opt(&self, key: &str) -> Result<Option<Value>> {
        self.rows.get_opt(self.index(), key)
    }

    /// Whether the row has `key`.
    #[must_use]
    pub fn contains(&self, key: &str) -> bool {
        self.rows
            .layout()
            .position(key)
            .and_then(|pos| self.rows.column(pos))
            .is_some_and(|col| !col.is_absent(self.index()))
    }

    /// How many keys the row has.
    #[must_use]
    pub fn len(&self) -> usize {
        let row = self.index();
        (0..self.rows.layout().len())
            .filter(|pos| self.rows.column(*pos).is_some_and(|c| !c.is_absent(row)))
            .count()
    }

    /// Whether the row has no key.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The keys the row has, in layout order.
    pub fn keys(&self) -> impl Iterator<Item = &str> + '_ {
        let row = self.index();
        self.rows
            .layout()
            .keys()
            .enumerate()
            .filter(move |(pos, _)| self.rows.column(*pos).is_some_and(|c| !c.is_absent(row)))
            .map(|(_, key)| key)
    }

    /// The row's entries, in layout order.
    pub fn iter(&self) -> impl Iterator<Item = Result<(&str, Value)>> + '_ {
        let row = self.index();
        self.rows
            .layout()
            .keys()
            .enumerate()
            .filter_map(move |(pos, key)| match self.rows.get_at(row, pos) {
                Ok(Some(value)) => Some(Ok((key, value))),
                // the row lacks this key: not an entry of the dict
                Ok(None) => None,
                Err(e) => Some(Err(e)),
            })
    }

    /// The row as a [`ValueMap`]: one owned key per entry plus a table (about 8 allocations for
    /// 5 keys). For edge code and legacy callers; bulk paths read the batch.
    ///
    /// # Errors
    /// Only on an internal inconsistency of the batch.
    pub fn to_map(&self) -> Result<ValueMap> {
        let mut map = ValueMap::with_capacity(self.rows.layout().len());
        for entry in self.iter() {
            let (key, value) = entry?;
            map.insert(key.to_owned(), value);
        }
        Ok(map)
    }
}

/// Rows of a batch: a whole batch, or the children of one parent row (an x2many value). Equal
/// to and hashed like a [`Value::List`] of maps.
#[derive(Clone)]
pub struct RowsRef(Source);

#[derive(Clone)]
enum Source {
    All(Rows),
    Children {
        level: Children,
        start: u32,
        end: u32,
    },
}

impl RowsRef {
    /// The positions `start..end` of a child level; the caller took them from its offsets.
    pub(crate) fn children(level: Children, start: u32, end: u32) -> Self {
        Self(Source::Children { level, start, end })
    }

    /// How many rows.
    #[must_use]
    pub fn len(&self) -> usize {
        match &self.0 {
            Source::All(rows) => rows.len(),
            Source::Children { start, end, .. } => end.saturating_sub(*start) as usize,
        }
    }

    /// Whether there is no row.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The batch the rows live in.
    #[must_use]
    pub fn rows(&self) -> &Rows {
        match &self.0 {
            Source::All(rows) => rows,
            Source::Children { level, .. } => level.child(),
        }
    }

    /// The positions of the rows in [`RowsRef::rows`], in order.
    ///
    /// # Errors
    /// Only on an internal inconsistency of the child level.
    pub fn positions(&self) -> Result<ChildPositions<'_>> {
        match &self.0 {
            Source::All(rows) => Ok(ChildPositions::range(0..rows.len())),
            Source::Children { level, start, end } => {
                let range = *start as usize..*end as usize;
                match &level.0.at {
                    None => Ok(ChildPositions::range(range)),
                    Some(at) => at.get(range).map(ChildPositions::at).ok_or_else(|| {
                        Error::programming(
                            "child positions past the child level \
                                 (internal error in erp_fields::rows)",
                        )
                    }),
                }
            }
        }
    }

    /// The `n`-th row.
    ///
    /// # Errors
    /// `Programming` when `n` is out of range.
    pub fn get(&self, n: usize) -> Result<RowRef> {
        let len = self.len();
        let row = self
            .positions()?
            .nth(n)
            .ok_or_else(|| super::column::out_of_range(n, len))?;
        self.rows().row(row)
    }

    /// The rows as handles.
    ///
    /// # Errors
    /// Only on an internal inconsistency of the child level.
    pub fn iter(&self) -> Result<impl Iterator<Item = RowRef> + '_> {
        let rows = self.rows();
        // a position is below the batch's length, which is at most `MAX_ROWS` < `u32::MAX`
        // (checked when the batch was built), so the cast keeps it exactly
        Ok(self
            .positions()?
            .map(move |row| RowRef::new(rows.clone(), row as u32)))
    }
}

impl From<Rows> for RowsRef {
    fn from(rows: Rows) -> Self {
        Self(Source::All(rows))
    }
}

impl From<Rows> for Value {
    fn from(rows: Rows) -> Self {
        Self::Rows(RowsRef::from(rows))
    }
}

impl From<RowRef> for Value {
    fn from(row: RowRef) -> Self {
        Self::Row(row)
    }
}

/// Positions of rows in a batch ([`RowsRef::positions`], [`Rows::child_positions`]).
#[derive(Debug, Clone)]
pub struct ChildPositions<'a>(Positions<'a>);

#[derive(Debug, Clone)]
enum Positions<'a> {
    Range(std::ops::Range<usize>),
    At(std::slice::Iter<'a, u32>),
}

impl<'a> ChildPositions<'a> {
    pub(crate) fn range(range: std::ops::Range<usize>) -> Self {
        Self(Positions::Range(range))
    }

    pub(crate) fn at(at: &'a [u32]) -> Self {
        Self(Positions::At(at.iter()))
    }
}

impl Iterator for ChildPositions<'_> {
    type Item = usize;

    fn next(&mut self) -> Option<usize> {
        match &mut self.0 {
            Positions::Range(r) => r.next(),
            Positions::At(it) => it.next().map(|p| *p as usize),
        }
    }

    fn nth(&mut self, n: usize) -> Option<usize> {
        match &mut self.0 {
            Positions::Range(r) => r.nth(n),
            Positions::At(it) => it.nth(n).map(|p| *p as usize),
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        match &self.0 {
            Positions::Range(r) => r.size_hint(),
            Positions::At(it) => it.size_hint(),
        }
    }
}

impl ExactSizeIterator for ChildPositions<'_> {}

/// ```
/// use std::sync::Arc;
/// use erp_fields::{Value, ValueMap};
/// let map: ValueMap = [("id".to_owned(), Value::Int(7))].into_iter().collect();
/// let value = Value::Map(Arc::new(map));
/// let dict = value.dict().unwrap();
/// assert_eq!(dict.get("id").unwrap(), Value::Int(7));
/// assert!(!dict.contains("name"));
/// ```
#[derive(Clone, Copy)]
pub struct DictRef<'a>(Dict<'a>);

#[derive(Clone, Copy)]
enum Dict<'a> {
    Map(&'a ValueMap),
    Row(&'a RowRef),
}

impl<'a> DictRef<'a> {
    pub(crate) fn map(map: &'a ValueMap) -> Self {
        Self(Dict::Map(map))
    }

    pub(crate) fn row(row: &'a RowRef) -> Self {
        Self(Dict::Row(row))
    }

    /// The value of `key`.
    pub fn get(&self, key: &str) -> Result<Value> {
        match self.0 {
            Dict::Map(map) => map.get(key).cloned().ok_or_else(|| {
                let keys: Vec<&str> = map.keys().map(String::as_str).take(20).collect();
                Error::programming(format!(
                    "dictionary has no key '{key}' (keys: {})",
                    if keys.is_empty() {
                        "none".to_owned()
                    } else {
                        keys.join(", ")
                    }
                ))
            }),
            Dict::Row(row) => row.get(key),
        }
    }

    /// The value of `key`, `Ok(None)` when the dict lacks it. A row still refuses a key its
    /// layout does not have.
    ///
    /// # Errors
    /// `Programming` for a key a row's layout lacks.
    pub fn get_opt(&self, key: &str) -> Result<Option<Value>> {
        match self.0 {
            Dict::Map(map) => Ok(map.get(key).cloned()),
            Dict::Row(row) => row.get_opt(key),
        }
    }

    /// Whether the dict has `key`.
    #[must_use]
    pub fn contains(&self, key: &str) -> bool {
        match self.0 {
            Dict::Map(map) => map.contains_key(key),
            Dict::Row(row) => row.contains(key),
        }
    }

    /// How many keys.
    #[must_use]
    pub fn len(&self) -> usize {
        match self.0 {
            Dict::Map(map) => map.len(),
            Dict::Row(row) => row.len(),
        }
    }

    /// Whether there is no key.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The keys in order.
    pub fn keys(&self) -> impl Iterator<Item = &'a str> + 'a {
        let (map, row) = match self.0 {
            Dict::Map(map) => (Some(map.keys().map(String::as_str)), None),
            Dict::Row(row) => (None, Some(row.keys())),
        };
        map.into_iter().flatten().chain(row.into_iter().flatten())
    }

    /// The entries in order.
    pub fn iter(&self) -> impl Iterator<Item = Result<(&'a str, Value)>> + 'a {
        let (map, row) = match self.0 {
            Dict::Map(map) => (
                Some(map.iter().map(|(k, v)| Ok((k.as_str(), v.clone())))),
                None,
            ),
            Dict::Row(row) => (None, Some(row.iter())),
        };
        map.into_iter().flatten().chain(row.into_iter().flatten())
    }
}

// ---------------------------------------------------------------- equality, hash, display

/// A row equals a map with the same entries (in any order), as two maps do.
pub(crate) fn row_eq_map(row: &RowRef, map: &ValueMap) -> bool {
    row.len() == map.len()
        && map
            .iter()
            .all(|(k, v)| matches!(row.get_opt(k), Ok(Some(ref value)) if value == v))
}

pub(crate) fn row_eq_row(a: &RowRef, b: &RowRef) -> bool {
    if a.rows.same_batch(&b.rows) && a.row == b.row {
        return true;
    }
    a.len() == b.len()
        && a.iter().all(|entry| match entry {
            Ok((k, v)) => matches!(b.get_opt(k), Ok(Some(ref value)) if *value == v),
            Err(_) => false,
        })
}

pub(crate) fn rows_eq_list(rows: &RowsRef, list: &[Value]) -> bool {
    rows.len() == list.len()
        && rows.iter().is_ok_and(|mut it| {
            it.by_ref()
                .zip(list)
                .all(|(row, item)| Value::Row(row) == *item)
        })
}

pub(crate) fn rows_eq_rows(a: &RowsRef, b: &RowsRef) -> bool {
    if a.len() != b.len() {
        return false;
    }
    match (a.iter(), b.iter()) {
        (Ok(x), Ok(y)) => x.zip(y).all(|(r, s)| row_eq_row(&r, &s)),
        _ => false,
    }
}

/// The order-independent hash of a dict's entries, shared by `Map` and `Row`.
pub(crate) fn hash_entries<'k, V: Borrow<Value>, H: Hasher>(
    entries: impl Iterator<Item = (&'k str, V)>,
    state: &mut H,
) {
    let mut len: usize = 0;
    let mut sum: u64 = 0;
    for (k, v) in entries {
        let mut h = DefaultHasher::new();
        k.hash(&mut h);
        v.borrow().hash(&mut h);
        sum = sum.wrapping_add(h.finish());
        len += 1;
    }
    len.hash(state);
    sum.hash(state);
}

pub(crate) fn hash_row<H: Hasher>(row: &RowRef, state: &mut H) {
    // `RowRef::iter` fails only when a column is shorter than its batch. That cannot happen: the
    // builder checks every column against the batch's length, mutation never changes a length
    // (`RowsMut` cannot replace a child level), and a handle's row is below the length of the
    // batch it holds, which is immutable while the handle shares it. `Hash` cannot report an
    // error, so the impossible case is caught by the debug assertion in tests.
    let entries = row.iter().map_while(|entry| {
        debug_assert!(entry.is_ok(), "inconsistent row batch: {entry:?}");
        entry.ok()
    });
    hash_entries(entries, state);
}

pub(crate) fn hash_rows<H: Hasher>(rows: &RowsRef, state: &mut H) {
    rows.len().hash(state);
    // `RowsRef::iter` fails only for child positions past their level. That cannot happen: a
    // child level's offsets and indices are checked against its parents and its child batch when
    // it is built, and `RowsMut` cannot replace a level afterwards. `Hash` cannot report an
    // error, so the impossible case is caught by the debug assertion in tests (the length above
    // is still hashed, so two such handles never hash alike by accident of an empty body).
    let it = rows.iter();
    debug_assert!(
        it.is_ok(),
        "inconsistent child level: {:?}",
        it.as_ref().err()
    );
    if let Ok(it) = it {
        for row in it {
            Value::Row(row).hash(state);
        }
    }
}

pub(crate) fn fmt_row(row: &RowRef, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    f.write_str("{")?;
    for (i, entry) in row.iter().enumerate() {
        if i > 0 {
            f.write_str(", ")?;
        }
        match entry {
            Ok((k, v)) => write!(f, "'{k}': {v}")?,
            Err(e) => write!(f, "<{}>", e.message())?,
        }
    }
    f.write_str("}")
}

/// Every row, untruncated, as `List` prints every item: a large batch prints in full (keep it out
/// of debug-level logs, or log its length).
pub(crate) fn fmt_rows(rows: &RowsRef, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    f.write_str("[")?;
    match rows.iter() {
        Ok(it) => {
            for (i, row) in it.enumerate() {
                if i > 0 {
                    f.write_str(", ")?;
                }
                fmt_row(&row, f)?;
            }
        }
        Err(e) => write!(f, "<{}>", e.message())?,
    }
    f.write_str("]")
}

/// The row's entries as a dict, untruncated.
impl fmt::Debug for RowRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Row(")?;
        fmt_row(self, f)?;
        f.write_str(")")
    }
}

/// Every row with every entry, untruncated (as a `List`): a large batch prints in full, so log
/// its length rather than the batch at debug level.
impl fmt::Debug for RowsRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Rows(")?;
        fmt_rows(self, f)?;
        f.write_str(")")
    }
}

impl PartialEq for RowRef {
    fn eq(&self, other: &Self) -> bool {
        row_eq_row(self, other)
    }
}

impl PartialEq for RowsRef {
    fn eq(&self, other: &Self) -> bool {
        rows_eq_rows(self, other)
    }
}
