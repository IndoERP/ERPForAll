//! The transaction's columnar record cache (design §5).
//!
//! Per model: a slot map from record id to a dense slot number, and per field a typed column vector
//! indexed by slot with bitsets `present` (a value is cached) and `dirty` (pending UPDATE). No
//! per-record objects.
//!
//! Invariants:
//! - every present slot has a data element (NULL writes resize the vector too);
//! - NULL has one representation per column: `None` in text/JSON columns, the `null` bitset in all
//!   others;
//! - invalidation never drops a dirty value: the public invalidation APIs flush first, and dropping
//!   one would silently lose a write, so it is refused (debug assertion, kept in release).
//!
//! Model and field ids are only obtained from the registry; the cache reports fields with pending
//! writes by their index in the model, which callers map back through the model's spec.

use std::sync::Arc;

use erp_fields::{
    Cell, Date, DateRange, DateTime, DatetimeRange, Decimal, Error, RecId, Result, Storage,
};
use erp_registry::{FieldId, ModelId};
use rustc_hash::FxHashMap;

use crate::bitset::BitSet;

#[derive(Debug, Clone)]
enum Data {
    Bool(BitSet),
    Int(Vec<i32>),
    Id(Vec<i64>),
    Float(Vec<f64>),
    Decimal(Vec<Decimal>),
    Str(Vec<Option<Arc<str>>>),
    Date(Vec<i32>),
    DateTime(Vec<i64>),
    Uuid(Vec<erp_fields::Uuid>),
    Xid8(Vec<erp_fields::Xid8>),
    TimestampTz(Vec<erp_fields::TimestampTz>),

    Json(Vec<Option<Arc<erp_fields::serde_json::Value>>>),
    /// x2many values: the ordered ids of every linked record, archived ones included (REL §4.10).
    Ids(Vec<Option<Arc<[RecId]>>>),
    /// `date_range` values.
    DateRange(Vec<Option<DateRange>>),
    /// `datetime_range` values.
    DatetimeRange(Vec<Option<DatetimeRange>>),
}

impl Data {
    fn new(storage: Storage) -> Self {
        match storage {
            Storage::Bool => Self::Bool(BitSet::default()),
            Storage::Int => Self::Int(Vec::new()),
            Storage::Id => Self::Id(Vec::new()),
            Storage::Float => Self::Float(Vec::new()),
            Storage::Decimal => Self::Decimal(Vec::new()),
            Storage::Str => Self::Str(Vec::new()),
            Storage::Date => Self::Date(Vec::new()),
            Storage::DateTime => Self::DateTime(Vec::new()),
            Storage::Uuid => Self::Uuid(Vec::new()),
            Storage::Xid8 => Self::Xid8(Vec::new()),
            Storage::TimestampTz => Self::TimestampTz(Vec::new()),
            Storage::Json => Self::Json(Vec::new()),
            Storage::Ids => Self::Ids(Vec::new()),
            Storage::DateRange(_) => Self::DateRange(Vec::new()),
            Storage::DatetimeRange(_) => Self::DatetimeRange(Vec::new()),
        }
    }

    fn kind(&self) -> &'static str {
        match self {
            Self::Bool(_) => "boolean",
            Self::Int(_) => "integer",
            Self::Id(_) => "id",
            Self::Float(_) => "float",
            Self::Decimal(_) => "numeric",
            Self::Str(_) => "text",
            Self::Date(_) => "date",
            Self::DateTime(_) => "datetime",
            Self::Uuid(_) => "uuid",
            Self::Xid8(_) => "xid8",
            Self::TimestampTz(_) => "timestamptz",
            Self::Json(_) => "json",
            Self::Ids(_) => "ids",
            Self::DateRange(_) => "date range",
            Self::DatetimeRange(_) => "datetime range",
        }
    }

    /// Make room for `slot`, so every present slot has an element.
    fn reserve(&mut self, slot: usize) {
        fn grow<T: Clone>(v: &mut Vec<T>, slot: usize, filler: T) {
            if v.len() <= slot {
                v.resize(slot + 1, filler);
            }
        }
        match self {
            Self::Bool(_) => {}
            Self::Int(v) => grow(v, slot, 0),
            Self::Id(v) => grow(v, slot, 0),
            Self::Float(v) => grow(v, slot, 0.0),
            Self::Decimal(v) => grow(v, slot, Decimal::ZERO),
            Self::Str(v) => grow(v, slot, None),
            Self::Date(v) => grow(v, slot, 0),
            Self::DateTime(v) => grow(v, slot, 0),
            Self::Uuid(v) => grow(v, slot, erp_fields::Uuid::NIL),
            Self::Xid8(v) => grow(v, slot, erp_fields::Xid8::from_u64(0)),
            Self::TimestampTz(v) => grow(v, slot, erp_fields::TimestampTz::UNIX_EPOCH),
            Self::Json(v) => grow(v, slot, None),
            Self::Ids(v) => grow(v, slot, None),
            Self::DateRange(v) => grow(v, slot, None),
            Self::DatetimeRange(v) => grow(v, slot, None),
        }
    }

    /// Room for slots `0..slots` exactly (no amortised doubling: the caller knows the size).
    fn reserve_slots(&mut self, slots: usize) {
        fn room<T>(v: &mut Vec<T>, slots: usize) {
            v.reserve_exact(slots.saturating_sub(v.len()));
        }
        match self {
            Self::Bool(b) => b.reserve(slots),
            Self::Int(v) | Self::Date(v) => room(v, slots),
            Self::Id(v) | Self::DateTime(v) => room(v, slots),
            Self::Float(v) => room(v, slots),
            Self::Decimal(v) => room(v, slots),
            Self::Str(v) => room(v, slots),
            Self::Uuid(v) => room(v, slots),
            Self::Xid8(v) => room(v, slots),
            Self::TimestampTz(v) => room(v, slots),
            Self::Json(v) => room(v, slots),
            Self::Ids(v) => room(v, slots),
            Self::DateRange(v) => room(v, slots),
            Self::DatetimeRange(v) => room(v, slots),
        }
    }

    /// Number of slots the data can cover without growing.
    fn capacity(&self) -> usize {
        match self {
            Self::Bool(b) => b.capacity_bits(),
            Self::Int(v) | Self::Date(v) => v.capacity(),
            Self::Id(v) | Self::DateTime(v) => v.capacity(),
            Self::Float(v) => v.capacity(),
            Self::Decimal(v) => v.capacity(),
            Self::Str(v) => v.capacity(),
            Self::Uuid(v) => v.capacity(),
            Self::Xid8(v) => v.capacity(),
            Self::TimestampTz(v) => v.capacity(),
            Self::Json(v) => v.capacity(),
            Self::Ids(v) => v.capacity(),
            Self::DateRange(v) => v.capacity(),
            Self::DatetimeRange(v) => v.capacity(),
        }
    }

    /// Drop the elements of slots `slots..`, keeping the allocation.
    fn cut_at(&mut self, slots: usize) {
        match self {
            Self::Bool(b) => b.cut_at(slots),
            Self::Int(v) | Self::Date(v) => v.truncate(slots),
            Self::Id(v) | Self::DateTime(v) => v.truncate(slots),
            Self::Float(v) => v.truncate(slots),
            Self::Decimal(v) => v.truncate(slots),
            Self::Str(v) => v.truncate(slots),
            Self::Uuid(v) => v.truncate(slots),
            Self::Xid8(v) => v.truncate(slots),
            Self::TimestampTz(v) => v.truncate(slots),
            Self::Json(v) => v.truncate(slots),
            Self::Ids(v) => v.truncate(slots),
            Self::DateRange(v) => v.truncate(slots),
            Self::DatetimeRange(v) => v.truncate(slots),
        }
    }

    /// NULL is `None` in the data (text, JSON, ids) rather than a bit in the `null` bitset.
    fn nulls_inline(&self) -> bool {
        matches!(
            self,
            Self::Str(_)
                | Self::Json(_)
                | Self::Ids(_)
                | Self::DateRange(_)
                | Self::DatetimeRange(_)
        )
    }
}

/// One field's values for all slots of a model.
#[derive(Debug, Clone)]
struct Column {
    data: Data,
    present: BitSet,
    /// Number of present slots (constant-time "is anything cached", used by marking).
    present_count: usize,
    /// Only used by columns whose data cannot hold NULL inline.
    null: BitSet,
    dirty: BitSet,
    /// To-compute set of a stored computed field (COMP §4.4), with its size.
    pending: BitSet,
    pending_count: usize,
}

impl Column {
    fn new(storage: Storage) -> Self {
        Self {
            data: Data::new(storage),
            present: BitSet::default(),
            present_count: 0,
            null: BitSet::default(),
            dirty: BitSet::default(),
            pending: BitSet::default(),
            pending_count: 0,
        }
    }

    fn set_pending(&mut self, slot: usize, on: bool) {
        if self.pending.get(slot) != on {
            self.pending.assign(slot, on);
            if on {
                self.pending_count += 1;
            } else {
                self.pending_count -= 1;
            }
        }
    }

    fn get(&self, slot: usize) -> Option<Cell> {
        if !self.present.get(slot) {
            return None;
        }
        if self.null.get(slot) {
            return Some(Cell::Null);
        }
        Some(match &self.data {
            Data::Bool(b) => Cell::Bool(b.get(slot)),
            Data::Int(v) => Cell::Int(*v.get(slot)?),
            Data::Id(v) => Cell::Id(*v.get(slot)?),
            Data::Float(v) => Cell::Float(*v.get(slot)?),
            Data::Decimal(v) => Cell::Decimal(*v.get(slot)?),
            Data::Str(v) => v.get(slot)?.clone().map_or(Cell::Null, Cell::Str),
            Data::Date(v) => Cell::Date(Date::from_days(*v.get(slot)?)),
            Data::DateTime(v) => Cell::DateTime(DateTime::from_micros(*v.get(slot)?)),
            Data::Uuid(v) => Cell::Uuid(*v.get(slot)?),
            Data::Xid8(v) => Cell::Xid8(*v.get(slot)?),
            Data::TimestampTz(v) => Cell::TimestampTz(*v.get(slot)?),
            Data::Json(v) => v.get(slot)?.clone().map_or(Cell::Null, Cell::Json),
            Data::Ids(v) => v.get(slot)?.clone().map_or(Cell::Null, Cell::Ids),
            Data::DateRange(v) => v.get(slot)?.map_or(Cell::Null, Cell::DateRange),
            Data::DatetimeRange(v) => v.get(slot)?.map_or(Cell::Null, Cell::DatetimeRange),
        })
    }

    fn set(&mut self, slot: usize, cell: Cell) -> Result<()> {
        self.data.reserve(slot);
        let is_null = cell.is_null();
        let inline = self.data.nulls_inline();
        match (&mut self.data, cell) {
            (Data::Str(v), Cell::Null) => v[slot] = None,
            (Data::Json(v), Cell::Null) => v[slot] = None,
            (Data::Ids(v), Cell::Null) => v[slot] = None,
            (Data::DateRange(v), Cell::Null) => v[slot] = None,
            (Data::DatetimeRange(v), Cell::Null) => v[slot] = None,
            (_, Cell::Null) => {}
            (Data::Bool(b), Cell::Bool(x)) => b.assign(slot, x),
            (Data::Int(v), Cell::Int(x)) => v[slot] = x,
            (Data::Id(v), Cell::Id(x)) => v[slot] = x,
            (Data::Float(v), Cell::Float(x)) => v[slot] = x,
            (Data::Decimal(v), Cell::Decimal(x)) => v[slot] = x,
            (Data::Str(v), Cell::Str(x)) => v[slot] = Some(x),
            (Data::Date(v), Cell::Date(x)) => v[slot] = x.days(),
            (Data::DateTime(v), Cell::DateTime(x)) => v[slot] = x.micros(),
            (Data::Uuid(v), Cell::Uuid(x)) => v[slot] = x,
            (Data::Xid8(v), Cell::Xid8(x)) => v[slot] = x,
            (Data::TimestampTz(v), Cell::TimestampTz(x)) => v[slot] = x,
            (Data::Json(v), Cell::Json(x)) => v[slot] = Some(x),
            (Data::Ids(v), Cell::Ids(x)) => v[slot] = Some(x),
            (Data::DateRange(v), Cell::DateRange(x)) => v[slot] = Some(x),
            (Data::DatetimeRange(v), Cell::DatetimeRange(x)) => v[slot] = Some(x),
            (data, cell) => {
                return Err(Error::programming(format!(
                    "cannot cache {cell:?} in a {} column",
                    data.kind()
                )));
            }
        }
        self.null.assign(slot, is_null && !inline);
        if !self.present.get(slot) {
            self.present.set(slot);
            self.present_count += 1;
        }
        Ok(())
    }

    /// Drop every slot from `slots` on, keeping the allocations; the caller checked that none of
    /// them is dirty or pending ([`ModelCache::kept_past`]).
    fn cut_at(&mut self, slots: usize) {
        self.present_count -= self.present.count_from(slots);
        self.pending_count -= self.pending.count_from(slots);
        self.present.cut_at(slots);
        self.null.cut_at(slots);
        self.dirty.cut_at(slots);
        self.pending.cut_at(slots);
        self.data.cut_at(slots);
    }

    /// Drop the value of `slot`; a dirty value is kept (see the module invariants).
    fn remove(&mut self, slot: usize) {
        let dirty = self.dirty.get(slot);
        debug_assert!(!dirty, "refusing to drop a dirty cache value; flush first");
        if dirty {
            return;
        }
        if self.present.get(slot) {
            self.present.clear(slot);
            self.present_count -= 1;
        }
        self.null.clear(slot);
        match &mut self.data {
            Data::Str(v) if slot < v.len() => v[slot] = None,
            Data::Json(v) if slot < v.len() => v[slot] = None,
            Data::Ids(v) if slot < v.len() => v[slot] = None,
            _ => {}
        }
    }
}

/// Where a fetched row goes, from [`ModelCache::fetched_slot`]: its slot, the slot count the rest
/// of the fetch is expected to need, and a cap on a column's growth.
#[derive(Debug, Clone, Copy)]
pub(crate) struct FetchSlot {
    slot: usize,
    want: usize,
    /// The model's slot count for an existing slot (no record lies past it); no cap
    /// (`usize::MAX`) for a new slot.
    limit: usize,
}

impl FetchSlot {
    /// The slot count to make room for in a column of capacity `capacity` that cannot hold the
    /// slot: what the rest of the fetch wants, but at least a quarter more than the capacity, so
    /// repeated fetches into a full model grow geometrically (a few reallocations, not one per
    /// fetch); then at most the limit, which only an existing slot sets below that.
    fn target(self, capacity: usize) -> usize {
        self.want
            .max(grown(capacity))
            .min(self.limit)
            .max(self.slot.saturating_add(1))
    }
}

/// `n` plus a quarter (at least 4): the smallest growth of a full id list, column or bitset
/// during a fetch, so growth stays geometric whatever the fetch's size.
fn grown(n: usize) -> usize {
    n.saturating_add((n / 4).max(4))
}

/// The rows a fetch is expected to hand over still, this one included, after `seen` rows: the
/// rest of the known count `expected`; past it, or when the count is not known (0), as many as
/// have been seen so far, at least this one. A long stream of unknown length so grows the id
/// list and the columns in proportion to what it has added (a few growths, an over-reservation
/// bounded by the stream's own rows), and a one-row fetch into a full model grows them by a
/// quarter (design note `perf-stream.md` §3.1 "As built").
pub(crate) fn rows_to_come(expected: usize, seen: usize) -> usize {
    if seen < expected {
        expected - seen
    } else {
        seen.max(1)
    }
}

/// A fill of one model's cache from rows of unknown number that name its records, many rows to a
/// record (the many2many comodel's `active`, one row per pair). The model is saved in the open
/// snapshot layers once, on the first row (review F5; never for an empty answer, review D3),
/// and the rows still to come are estimated from the records the fill has added, not from its
/// rows ([`rows_to_come`]): many pairs over a few comodel records reserve room for those records
/// only (review round 4 D1).
#[derive(Debug)]
pub(crate) struct StreamFill {
    model: ModelId,
    /// The writer, and the model's slot count when it was taken.
    started: Option<(ModelWriter, usize)>,
}

impl StreamFill {
    /// A fill of `model`; nothing touches the cache before the first row.
    pub(crate) fn new(model: ModelId) -> Self {
        Self {
            model,
            started: None,
        }
    }

    /// Cache the value of `field` for record `id`, decoded only when it is not cached yet (W9).
    ///
    /// # Errors
    ///
    /// As [`ModelCache::fill_fetched`].
    pub(crate) fn fill(
        &mut self,
        cache: &mut Cache,
        id: RecId,
        field: FieldId,
        storage: Storage,
        decode: impl FnOnce() -> Result<Cell>,
    ) -> Result<()> {
        let (writer, start) = match self.started {
            Some(started) => started,
            None => {
                let writer = cache.writer(self.model);
                let start = cache.write(writer).len();
                *self.started.insert((writer, start))
            }
        };
        let model = cache.write(writer);
        let added = model.len().saturating_sub(start);
        let at = model.fetched_slot(id, rows_to_come(0, added));
        model.fill_fetched(at, field, storage, decode)
    }
}

/// The cache of one model.
#[derive(Debug, Default, Clone)]
pub(crate) struct ModelCache {
    slot_of: FxHashMap<RecId, u32>,
    ids: Vec<RecId>,
    columns: Vec<Option<Column>>,
    /// Slots of new (in-memory) records.
    new_slots: Vec<u32>,
}

impl ModelCache {
    fn slot(&self, id: RecId) -> Option<usize> {
        self.slot_of.get(&id).map(|s| *s as usize)
    }

    fn slot_or_insert(&mut self, id: RecId) -> usize {
        if let Some(s) = self.slot(id) {
            return s;
        }
        let s = self.ids.len();
        self.ids.push(id);
        self.slot_of.insert(id, s as u32);
        if id.is_new() {
            self.new_slots.push(s as u32);
        }
        s
    }

    fn column(&self, field: FieldId) -> Option<&Column> {
        self.columns.get(field.index()).and_then(Option::as_ref)
    }

    fn column_mut(&mut self, field: FieldId, storage: Storage) -> &mut Column {
        let i = field.index();
        if self.columns.len() <= i {
            self.columns.resize_with(i + 1, || None);
        }
        self.columns[i].get_or_insert_with(|| Column::new(storage))
    }

    /// The slot of `id`, when the record has one (a slot outlives invalidation; only
    /// [`Cache::clear`], [`Cache::restore`], [`Cache::reset`] and [`Cache::cut_to_mark`] drop
    /// slots, and they move [`Cache::epoch`]).
    pub(crate) fn slot_of(&self, id: RecId) -> Option<u32> {
        self.slot_of.get(&id).copied()
    }

    /// The cached value of `field` at `slot` (from [`ModelCache::slot_of`]), or `None` when not
    /// cached: the per-record hash lookup of [`ModelCache::get`] is done once per record by the
    /// caller, not once per field.
    pub(crate) fn get_at(&self, field: FieldId, slot: u32) -> Option<Cell> {
        self.column(field)?.get(slot as usize)
    }

    /// The cached value, or `None` when not cached.
    pub(crate) fn get(&self, field: FieldId, id: RecId) -> Option<Cell> {
        let slot = self.slot(id)?;
        self.column(field)?.get(slot)
    }

    /// True when a value is cached.
    pub(crate) fn contains(&self, field: FieldId, id: RecId) -> bool {
        match (self.slot(id), self.column(field)) {
            (Some(slot), Some(col)) => col.present.get(slot),
            _ => false,
        }
    }

    /// The slot of a fetched record, created when it has none, with room made for the rest of the
    /// fetch so the cache grows once rather than doubling (perf-audit R16). `remaining` is the
    /// number of the fetch's rows still to come, this one included, or an estimate of it
    /// ([`rows_to_come`]); 0 (nothing known) is treated like 1.
    ///
    /// A new slot goes at the end: when the id list or the slot map is full, room is made at once
    /// for the remaining rows, but for at least a quarter more than the list holds (at least 4;
    /// the map rounds up to its bucket count, which at least doubles its buckets). This holds for
    /// a one-row or unknown-size fetch too, so many small fetches into a full model cost a few
    /// reallocations rather than one each, and the id list and the columns grow by a quarter,
    /// not by doubling (the slot map still doubles its buckets).
    /// A record that already has a slot reserves nothing here. The returned [`FetchSlot`] tells
    /// [`ModelCache::fill_fetched`] how far a column may need to reach: columns are dense by slot,
    /// so a value at slot `s` needs its column to cover `s`. After a new slot the rest of the fetch
    /// wants `slot + remaining`; an existing slot is never past the model's slot count, which caps
    /// its columns' growth.
    ///
    /// The count is the rows to come, not the new slots among them: when a row is new and the
    /// rows after it already have slots, the room made is too large by those rows. That
    /// over-reservation is bounded by the fetch's row count (a statement's, or a chunked read's)
    /// and, since it is only made when something is full, by the growth rule above.
    pub(crate) fn fetched_slot(&mut self, id: RecId, remaining: usize) -> FetchSlot {
        if let Some(slot) = self.slot(id) {
            return FetchSlot {
                slot,
                want: slot.saturating_add(remaining),
                limit: self.ids.len(),
            };
        }
        let len = self.ids.len();
        if len == self.ids.capacity() {
            self.ids.reserve_exact(remaining.max(grown(len) - len));
        }
        if self.slot_of.len() == self.slot_of.capacity() {
            self.slot_of.reserve(remaining.max(1));
        }
        let slot = self.slot_or_insert(id);
        FetchSlot {
            slot,
            want: slot.saturating_add(remaining),
            limit: usize::MAX,
        }
    }

    /// [`ModelCache::insert_fetched`] at `at` (from [`ModelCache::fetched_slot`]), with the value
    /// decoded only when nothing is cached there: a cell whose value is already cached costs no
    /// decode and no allocation (perf-audit W9, design note `perf-stream.md` §3.1).
    ///
    /// A column that must grow to hold the slot grows to what the rest of the fetch wants,
    /// `slot + remaining`, or to a quarter more than its capacity when that is more (so repeated
    /// fetches, one-row and unknown-size fetches, and records spread over a large model cost few
    /// growths); for an existing slot never past the model's slot count. A column long enough
    /// already (a re-fetch) is not touched; the `present` bitset follows the same rule.
    ///
    /// # Errors
    ///
    /// `decode`'s error; a value of the wrong kind for the column.
    pub(crate) fn fill_fetched(
        &mut self,
        at: FetchSlot,
        field: FieldId,
        storage: Storage,
        decode: impl FnOnce() -> Result<Cell>,
    ) -> Result<()> {
        let col = self.column_mut(field, storage);
        if col.present.get(at.slot) {
            return Ok(());
        }
        let cap = col.data.capacity();
        if cap <= at.slot {
            col.data.reserve_slots(at.target(cap));
        }
        let bits = col.present.capacity_bits();
        if bits <= at.slot {
            col.present.reserve(at.target(bits));
        }
        col.set(at.slot, decode()?)
    }

    /// Store a value fetched from the database, unless one is already cached: a pending (dirty)
    /// value is never replaced by the stale database value (ENV §4.6).
    pub(crate) fn insert_fetched(
        &mut self,
        field: FieldId,
        storage: Storage,
        id: RecId,
        cell: Cell,
    ) -> Result<()> {
        let slot = self.slot_or_insert(id);
        let col = self.column_mut(field, storage);
        if col.present.get(slot) {
            return Ok(());
        }
        col.set(slot, cell)
    }

    /// Set a value; `dirty` marks it for the next flush.
    pub(crate) fn set(
        &mut self,
        field: FieldId,
        storage: Storage,
        id: RecId,
        cell: Cell,
        dirty: bool,
    ) -> Result<()> {
        let slot = self.slot_or_insert(id);
        let col = self.column_mut(field, storage);
        col.set(slot, cell)?;
        col.dirty.assign(slot, dirty);
        Ok(())
    }

    /// The records whose value of `field` is cached, with that value.
    pub(crate) fn cached_values(&self, field: FieldId) -> Vec<(RecId, Cell)> {
        let Some(col) = self.column(field) else {
            return Vec::new();
        };
        col.present
            .ones()
            .filter_map(|s| Some((self.ids[s], col.get(s)?)))
            .collect()
    }

    /// True when some record has a cached value of `field`.
    pub(crate) fn has_values(&self, field: FieldId) -> bool {
        self.column(field).is_some_and(|c| c.present_count > 0)
    }

    /// The records whose value of `field` is a pending write, with that value.
    pub(crate) fn dirty_values(&self, field: FieldId) -> Vec<(RecId, Cell)> {
        let Some(col) = self.column(field) else {
            return Vec::new();
        };
        col.dirty
            .ones()
            .filter_map(|s| Some((self.ids[s], col.get(s)?)))
            .collect()
    }

    /// The new (in-memory) records with a cached value of `field`, with that value.
    pub(crate) fn new_values(&self, field: FieldId) -> Vec<(RecId, Cell)> {
        let Some(col) = self.column(field) else {
            return Vec::new();
        };
        self.new_slots
            .iter()
            .filter_map(|s| {
                let s = *s as usize;
                Some((self.ids[s], col.get(s)?))
            })
            .collect()
    }

    /// The number of records with a slot (an upper bound of the records with cached values).
    pub(crate) fn len(&self) -> usize {
        self.ids.len()
    }

    /// True when no value is cached (slots of invalidated records may remain).
    pub(crate) fn is_empty(&self) -> bool {
        !self.columns.iter().flatten().any(|c| c.present_count > 0)
    }

    /// The records with at least one cached value.
    pub(crate) fn cached_ids(&self) -> Vec<RecId> {
        let mut slots: Vec<usize> = self
            .columns
            .iter()
            .flatten()
            .flat_map(|c| c.present.ones())
            .collect();
        slots.sort_unstable();
        slots.dedup();
        slots.into_iter().map(|s| self.ids[s]).collect()
    }

    /// Pending writes: `(field index in the model, ids)` per field.
    pub(crate) fn dirty(&self) -> Vec<(usize, Vec<RecId>)> {
        self.columns
            .iter()
            .enumerate()
            .filter_map(|(i, c)| {
                let col = c.as_ref()?;
                if !col.dirty.any() {
                    return None;
                }
                Some((i, col.dirty.ones().map(|s| self.ids[s]).collect()))
            })
            .collect()
    }

    /// True when any value is dirty.
    pub(crate) fn has_dirty(&self) -> bool {
        self.columns.iter().flatten().any(|c| c.dirty.any())
    }

    /// Whether `id`'s value of `field` is dirty.
    pub(crate) fn is_dirty(&self, field: FieldId, id: RecId) -> bool {
        match (self.slot(id), self.column(field)) {
            (Some(slot), Some(col)) => col.dirty.get(slot),
            _ => false,
        }
    }

    /// Clear the dirty flags of `ids` for `field` (after their UPDATE).
    pub(crate) fn mark_clean(&mut self, field: FieldId, ids: &[RecId]) {
        let slots: Vec<usize> = ids.iter().filter_map(|id| self.slot(*id)).collect();
        if let Some(Some(col)) = self.columns.get_mut(field.index()) {
            for s in slots {
                col.dirty.clear(s);
            }
        }
    }

    /// Add `ids` to the to-compute set of `field`.
    pub(crate) fn mark_pending(&mut self, field: FieldId, storage: Storage, ids: &[RecId]) {
        let slots: Vec<usize> = ids.iter().map(|id| self.slot_or_insert(*id)).collect();
        let col = self.column_mut(field, storage);
        for s in slots {
            col.set_pending(s, true);
        }
    }

    /// Remove `ids` from the to-compute set of `field`.
    pub(crate) fn clear_pending(&mut self, field: FieldId, ids: &[RecId]) {
        let slots: Vec<usize> = ids.iter().filter_map(|id| self.slot(*id)).collect();
        if let Some(Some(col)) = self.columns.get_mut(field.index()) {
            for s in slots {
                col.set_pending(s, false);
            }
        }
    }

    /// The number of records to compute for `field`.
    pub(crate) fn pending_count(&self, field: FieldId) -> usize {
        self.column(field).map_or(0, |c| c.pending_count)
    }

    /// True when some record is to compute for `field`.
    pub(crate) fn has_pending(&self, field: FieldId) -> bool {
        self.column(field).is_some_and(|c| c.pending_count > 0)
    }

    /// Whether `id` is to compute for `field`.
    pub(crate) fn is_pending(&self, field: FieldId, id: RecId) -> bool {
        match (self.slot(id), self.column(field)) {
            (Some(slot), Some(col)) => col.pending.get(slot),
            _ => false,
        }
    }

    /// The records to compute for `field`, in slot order.
    pub(crate) fn pending_ids(&self, field: FieldId) -> Vec<RecId> {
        match self.column(field) {
            Some(col) if col.pending_count > 0 => col.pending.ones().map(|s| self.ids[s]).collect(),
            _ => Vec::new(),
        }
    }

    /// Field indexes that have records to compute.
    pub(crate) fn pending_fields(&self) -> Vec<usize> {
        self.columns
            .iter()
            .enumerate()
            .filter(|(_, c)| c.as_ref().is_some_and(|c| c.pending_count > 0))
            .map(|(i, _)| i)
            .collect()
    }

    /// What keeps the slots from `slots` on from being dropped: a new (in-memory) record, a
    /// pending write, a pending computation (checked in that order); `None` when they can go.
    fn kept_past(&self, slots: usize) -> Option<KeptBy> {
        if self.new_slots.iter().any(|s| *s as usize >= slots) {
            return Some(KeptBy::NewRecord);
        }
        for (field, column) in self.columns.iter().enumerate() {
            let Some(column) = column else { continue };
            if column.dirty.any_from(slots) {
                return Some(KeptBy::Dirty(field));
            }
        }
        for (field, column) in self.columns.iter().enumerate() {
            let Some(column) = column else { continue };
            if column.pending.any_from(slots) {
                return Some(KeptBy::Pending(field));
            }
        }
        None
    }

    /// Drop the slots from `slots` on: their ids leave the slot map (one removal each), the id
    /// list, the columns and their bitsets are cut back; every allocation is kept, so the next
    /// window refills them without allocating. Returns whether a slot was dropped. The caller
    /// checked [`ModelCache::kept_past`].
    fn cut_at(&mut self, slots: usize) -> bool {
        if self.ids.len() <= slots {
            return false;
        }
        for id in self.ids.get(slots..).into_iter().flatten() {
            self.slot_of.remove(id);
        }
        self.ids.truncate(slots);
        for column in self.columns.iter_mut().flatten() {
            column.cut_at(slots);
        }
        true
    }

    /// Drop cached values of `fields` (all when `None`) for `ids` (all when `None`). Dirty values
    /// are kept (module invariants).
    pub(crate) fn invalidate(&mut self, fields: Option<&[FieldId]>, ids: Option<&[RecId]>) {
        let slots: Vec<usize> = match ids {
            Some(ids) => ids.iter().filter_map(|id| self.slot(*id)).collect(),
            None => (0..self.ids.len()).collect(),
        };
        let indexes: Vec<usize> = match fields {
            Some(fields) => fields.iter().map(|f| f.index()).collect(),
            None => (0..self.columns.len()).collect(),
        };
        for i in indexes {
            if let Some(Some(col)) = self.columns.get_mut(i) {
                for s in &slots {
                    col.remove(*s);
                }
            }
        }
    }
}

/// Why a stream window's slots cannot leave the cache ([`Cache::cut_to_mark`]); field indexes are
/// the field's index in its model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum KeptBy {
    /// A new (in-memory) record has a slot past the mark.
    NewRecord,
    /// A record past the mark has a pending write of this field.
    Dirty(usize),
    /// A record past the mark is still to compute for this field.
    Pending(usize),
}

/// What stopped [`Cache::cut_to_mark`]: an open snapshot layer, or a model whose slots past the
/// mark must stay.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CutRefused {
    /// A snapshot layer is open (an isolated scope): a restore would bring the slots back.
    Snapshot,
    /// The model at this index keeps slots past the mark.
    Kept { model: usize, by: KeptBy },
}

/// The record cache of a transaction, shared by all its environments.
///
/// Snapshots: [`Cache::snapshot`] opens a copy-on-write layer; the first mutable
/// access to a model after it saves a copy of that model's cache, and [`Cache::restore`] puts the
/// saved copies back, so the cache is exactly as it was when the layer opened. Every mutation goes
/// through [`Cache::model_mut`], [`Cache::write`] (which checks that the model was saved in every
/// layer open now) or [`Cache::clear`], which is what makes the layer complete. Only the models
/// touched inside the layer are copied.
#[derive(Debug, Default)]
pub(crate) struct Cache {
    models: Vec<ModelCache>,
    /// Open snapshot layers, innermost last.
    layers: Vec<SnapshotLayer>,
    /// Source of snapshot layer ids (never reused within a transaction).
    next_layer: u64,
    /// Moves whenever slots may have been dropped or renumbered ([`Cache::clear`],
    /// [`Cache::restore`], [`Cache::reset`]): slot numbers taken under one epoch stay valid while
    /// it lasts.
    epoch: u64,
    /// How many times every model was dropped ([`Cache::clear`], [`Cache::reset`]): a [`Mark`]
    /// taken before one counts nothing that is cached now.
    clears: u64,
}

/// The scope of a stream window in the cache ([`Cache::mark_into`]): the slot count of every
/// model, and the cache's clear count, when it was taken. A stream reuses one for every window.
#[derive(Debug, Default)]
pub(crate) struct Mark {
    slots: Vec<u32>,
    clears: u64,
}

/// One snapshot layer: the number of models at its start and the models changed since then, as
/// they were.
#[derive(Debug)]
struct SnapshotLayer {
    id: u64,
    len: usize,
    saved: FxHashMap<usize, ModelCache>,
}

/// The right to write one model's cache without saving it again, from [`Cache::writer`]; checked
/// by [`Cache::write`] on every use.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ModelWriter {
    model: ModelId,
    /// [`Cache::next_layer`] when the model was saved.
    next_layer: u64,
    /// [`Cache::epoch`] when the model was saved.
    epoch: u64,
}

/// A snapshot layer opened by [`Cache::snapshot`], to pass to [`Cache::restore`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[must_use = "an open snapshot layer must be restored"]
pub(crate) struct SnapshotId(u64);

impl Cache {
    pub(crate) fn model(&self, model: ModelId) -> Option<&ModelCache> {
        self.models.get(model.index())
    }

    pub(crate) fn model_mut(&mut self, model: ModelId) -> &mut ModelCache {
        let i = model.index();
        if !self.layers.is_empty() {
            self.save_model(i);
        }
        if self.models.len() <= i {
            self.models.resize_with(i + 1, ModelCache::default);
        }
        &mut self.models[i]
    }

    /// [`Cache::model_mut`] once for a run of writes to one model (the rows of a statement,
    /// perf-stream review F5): the model is saved in every open snapshot layer now, and
    /// [`Cache::write`] then reaches it without the per-call layer bookkeeping while no layer
    /// was opened and no slot dropped since.
    pub(crate) fn writer(&mut self, model: ModelId) -> ModelWriter {
        self.model_mut(model);
        ModelWriter {
            model,
            next_layer: self.next_layer,
            epoch: self.epoch,
        }
    }

    /// The model of `writer` for writing. While no snapshot layer was opened
    /// ([`Cache::snapshot`] moves `next_layer`) and none restored nor the cache cleared (they
    /// move `epoch`), every open layer still holds the copy [`Cache::writer`] saved, so the model
    /// is returned directly; otherwise this is [`Cache::model_mut`].
    pub(crate) fn write(&mut self, writer: ModelWriter) -> &mut ModelCache {
        if writer.next_layer == self.next_layer && writer.epoch == self.epoch {
            let i = writer.model.index();
            if i < self.models.len() {
                return &mut self.models[i];
            }
        }
        self.model_mut(writer.model)
    }

    /// Save model `i` in every open layer that has not saved it yet (copy on first write).
    fn save_model(&mut self, i: usize) {
        for layer in &mut self.layers {
            if i < layer.len && !layer.saved.contains_key(&i) {
                let copy = self.models.get(i).cloned().unwrap_or_default();
                layer.saved.insert(i, copy);
            }
        }
    }

    /// Open a snapshot layer: until [`Cache::restore`], every model changed is saved first.
    pub(crate) fn snapshot(&mut self) -> SnapshotId {
        self.next_layer += 1;
        let id = self.next_layer;
        self.layers.push(SnapshotLayer {
            id,
            len: self.models.len(),
            saved: FxHashMap::default(),
        });
        SnapshotId(id)
    }

    /// Put the cache back as it was when `snapshot` was opened, and close that layer (and any
    /// layer opened inside it). Fails when the layer no longer exists (the transaction ended in
    /// between: commit or rollback reset the cache).
    pub(crate) fn restore(&mut self, snapshot: SnapshotId) -> Result<()> {
        let Some(pos) = self.layers.iter().position(|l| l.id == snapshot.0) else {
            return Err(Error::programming(
                "the cache snapshot no longer exists: the transaction was committed or rolled \
                 back inside the snapshot scope",
            ));
        };
        // inner layers close with it: the cache goes back to this layer's start
        self.layers.truncate(pos + 1);
        let Some(layer) = self.layers.pop() else {
            return Err(Error::programming("cache snapshot stack is empty"));
        };
        self.epoch = self.epoch.wrapping_add(1);
        self.models.truncate(layer.len);
        if self.models.len() < layer.len {
            self.models.resize_with(layer.len, ModelCache::default);
        }
        for (i, saved) in layer.saved {
            self.models[i] = saved;
        }
        Ok(())
    }

    /// The slot epoch (see the field): slot numbers read from [`ModelCache::slot_of`] are valid
    /// until it changes.
    pub(crate) fn epoch(&self) -> u64 {
        self.epoch
    }

    /// Whether a snapshot layer is open (an isolated scope runs).
    pub(crate) fn has_snapshot(&self) -> bool {
        !self.layers.is_empty()
    }

    /// The slot count of every model, into `mark` (refilled; one entry per model the cache
    /// knows), and the clear count: the window scope of a stream (design note `perf-stream.md`
    /// §3.5). `mark` is the stream's own buffer, reused for every window.
    pub(crate) fn mark_into(&self, mark: &mut Mark) {
        mark.slots.clear();
        // a slot number fits a `u32` (`slot_of` stores them so)
        mark.slots
            .extend(self.models.iter().map(|m| m.ids.len() as u32));
        mark.clears = self.clears;
    }

    /// Drop every slot added since `mark` was taken ([`Cache::mark_into`]): models the mark does
    /// not know lose all their slots, and so does every model when the cache was cleared since
    /// (a savepoint rollback inside a stream's loop: what is cached now came after the mark).
    /// Slots below the mark keep their values; every allocation is kept. The epoch moves when a
    /// slot was dropped, as for [`Cache::clear`].
    ///
    /// Nothing is dropped when a snapshot layer is open, or when a model keeps a new record, a
    /// pending write or a pending computation past the mark: the caller reports it (a stream
    /// flushes first, so only new records and computations of new records are left to report).
    pub(crate) fn cut_to_mark(&mut self, mark: &Mark) -> std::result::Result<(), CutRefused> {
        if self.has_snapshot() {
            return Err(CutRefused::Snapshot);
        }
        let cleared = mark.clears != self.clears;
        let floor = |i: usize| {
            if cleared {
                0
            } else {
                mark.slots.get(i).map_or(0, |s| *s as usize)
            }
        };
        for (i, model) in self.models.iter().enumerate() {
            if let Some(by) = model.kept_past(floor(i)) {
                return Err(CutRefused::Kept { model: i, by });
            }
        }
        let mut dropped = false;
        for (i, model) in self.models.iter_mut().enumerate() {
            dropped |= model.cut_at(floor(i));
        }
        if dropped {
            self.epoch = self.epoch.wrapping_add(1);
        }
        Ok(())
    }

    /// Number of slots of `model` (records with a slot, cached or invalidated).
    #[cfg(feature = "test-faults")]
    pub(crate) fn slots(&self, model: ModelId) -> usize {
        self.model(model).map_or(0, ModelCache::len)
    }

    /// Number of models the innermost snapshot layer saved so far.
    #[cfg(test)]
    fn saved_models(&self) -> usize {
        self.layers.last().map_or(0, |l| l.saved.len())
    }

    pub(crate) fn get(&self, model: ModelId, field: FieldId, id: RecId) -> Option<Cell> {
        self.model(model)?.get(field, id)
    }

    pub(crate) fn contains(&self, model: ModelId, field: FieldId, id: RecId) -> bool {
        self.model(model).is_some_and(|m| m.contains(field, id))
    }

    /// Indexes (in the registry) of the models that have pending writes.
    pub(crate) fn dirty_models(&self) -> Vec<usize> {
        self.models
            .iter()
            .enumerate()
            .filter(|(_, m)| m.has_dirty())
            .map(|(i, _)| i)
            .collect()
    }

    /// Whether any value is dirty.
    pub(crate) fn has_dirty(&self) -> bool {
        self.models.iter().any(ModelCache::has_dirty)
    }

    /// `(model index, field index)` of every field with records to compute.
    pub(crate) fn pending_fields(&self) -> Vec<(usize, usize)> {
        self.models
            .iter()
            .enumerate()
            .flat_map(|(m, mc)| mc.pending_fields().into_iter().map(move |f| (m, f)))
            .collect()
    }

    /// Drop every value, dirty ones included (savepoint rollback). Open snapshot layers save the
    /// models first, so a restore still brings them back.
    pub(crate) fn clear(&mut self) {
        if !self.layers.is_empty() {
            for i in 0..self.models.len() {
                self.save_model(i);
            }
        }
        self.epoch = self.epoch.wrapping_add(1);
        self.clears = self.clears.wrapping_add(1);
        self.models.clear();
    }

    /// Drop everything, snapshot layers included (end of transaction, `transaction.clear()`).
    pub(crate) fn reset(&mut self) {
        self.epoch = self.epoch.wrapping_add(1);
        self.clears = self.clears.wrapping_add(1);
        self.models.clear();
        self.layers.clear();
    }
}

#[cfg(test)]
mod tests {
    use erp_registry::{AddonDescriptor, FieldDecl, ModelDecl, RegistryBuilder};

    use super::*;

    static T: AddonDescriptor = AddonDescriptor {
        name: "t",
        depends: &["base"],
        exact_only: false,
    };
    static M: ModelDecl = ModelDecl::new("t", "t.m").fields(
        const {
            &[
                FieldDecl::char("name"),
                FieldDecl::char("email"),
                FieldDecl::integer("number"),
                FieldDecl::boolean("flag"),
                FieldDecl::float("x"),
                FieldDecl::date("day"),
                FieldDecl::json("doc"),
            ]
        },
    );

    /// Field ids by name, from a real registry (ids cannot be forged).
    fn fields() -> impl Fn(&str) -> FieldId {
        let registry = RegistryBuilder::new([&T]).with_decls([&M]).build().unwrap();
        let ids: Vec<(String, FieldId)> = registry
            .model("t.m")
            .unwrap()
            .fields
            .iter()
            .map(|f| (f.name.to_owned(), f.id))
            .collect();
        move |name| ids.iter().find(|(n, _)| n == name).unwrap().1
    }

    fn id(i: i64) -> RecId {
        RecId::from_real(i).unwrap()
    }

    /// A registry with `t.m`: its model id and a field id lookup.
    fn model_and_fields() -> (ModelId, impl Fn(&str) -> FieldId) {
        let registry = RegistryBuilder::new([&T]).with_decls([&M]).build().unwrap();
        let spec = registry.model("t.m").unwrap();
        let model = spec.id;
        let ids: Vec<(String, FieldId)> = spec
            .fields
            .iter()
            .map(|f| (f.name.to_owned(), f.id))
            .collect();
        (model, move |name: &str| {
            ids.iter().find(|(n, _)| n == name).unwrap().1
        })
    }

    /// perf-stream §3.5: the slots added after a mark leave the slot map, the id list, the
    /// columns and their bitsets; the slots below keep their values; the allocations stay; the
    /// epoch moves, so slot numbers taken before are not trusted again.
    #[test]
    fn cut_to_mark_drops_the_slots_of_a_window() {
        let (model, field) = model_and_fields();
        let (name, number, flag) = (field("name"), field("number"), field("flag"));
        let mut cache = Cache::default();
        let fill = |cache: &mut Cache, from: i64, to: i64| {
            for i in from..to {
                let m = cache.model_mut(model);
                m.insert_fetched(name, Storage::Str, id(i), Cell::Str(format!("n{i}").into()))
                    .unwrap();
                let value = if i % 2 == 0 {
                    Cell::Null
                } else {
                    Cell::Int(i as i32)
                };
                m.insert_fetched(number, Storage::Int, id(i), value)
                    .unwrap();
                m.insert_fetched(flag, Storage::Bool, id(i), Cell::Bool(i % 3 == 0))
                    .unwrap();
            }
        };
        fill(&mut cache, 1, 4);
        let mut mark = Mark::default();
        cache.mark_into(&mut mark);
        assert_eq!(
            mark.slots.len(),
            model.index() + 1,
            "one entry per model the cache knows"
        );
        assert_eq!(mark.slots[model.index()], 3);
        for round in 0..3 {
            fill(&mut cache, 100, 200);
            let mc = cache.model(model).unwrap();
            assert_eq!(mc.len(), 103);
            let capacity = mc.ids.capacity();
            let epoch = cache.epoch();
            cache.cut_to_mark(&mark).unwrap();
            assert_ne!(cache.epoch(), epoch, "round {round}");
            let mc = cache.model(model).unwrap();
            assert_eq!(mc.len(), 3);
            assert_eq!(
                mc.ids.capacity(),
                capacity,
                "round {round}: the id list keeps its room"
            );
            assert_eq!(mc.slot_of(id(150)), None);
            assert!(!mc.contains(name, id(150)));
            assert_eq!(mc.get(name, id(2)), Some(Cell::Str("n2".into())));
            assert_eq!(mc.get(number, id(2)), Some(Cell::Null));
            assert_eq!(mc.get(number, id(3)), Some(Cell::Int(3)));
            assert_eq!(mc.get(flag, id(3)), Some(Cell::Bool(true)));
            assert_eq!(mc.cached_ids(), [id(1), id(2), id(3)]);
            let col = mc.column(name).unwrap();
            assert_eq!(col.present_count, 3);
        }
        // a slot dropped and fetched again gets a fresh slot and its value
        fill(&mut cache, 150, 151);
        assert_eq!(cache.model(model).unwrap().slot_of(id(150)), Some(3));
        assert_eq!(cache.get(model, number, id(150)), Some(Cell::Null));
        // nothing past the mark: nothing dropped, the epoch stays
        cache.cut_to_mark(&mark).unwrap();
        let epoch = cache.epoch();
        cache.cut_to_mark(&mark).unwrap();
        assert_eq!(cache.epoch(), epoch);
        // a model the mark does not know loses every slot
        cache.cut_to_mark(&Mark::default()).unwrap();
        assert_eq!(cache.model(model).unwrap().len(), 0);
        // after a clear (a savepoint rollback), everything cached came after the mark
        fill(&mut cache, 1, 4);
        cache.mark_into(&mut mark);
        cache.clear();
        fill(&mut cache, 1, 3);
        cache.cut_to_mark(&mark).unwrap();
        assert_eq!(cache.model(model).unwrap().len(), 0);
    }

    /// A new record, a pending write or a pending computation past the mark stops the cut (in
    /// that order), before anything is dropped; so does an open snapshot layer.
    #[test]
    fn cut_to_mark_refuses_what_must_stay() {
        let (model, field) = model_and_fields();
        let (name, number) = (field("name"), field("number"));
        let mut cache = Cache::default();
        cache
            .model_mut(model)
            .insert_fetched(name, Storage::Str, id(1), Cell::Str("a".into()))
            .unwrap();
        let mut mark = Mark::default();
        cache.mark_into(&mut mark);
        let m = cache.model_mut(model);
        m.set(number, Storage::Int, id(2), Cell::Int(5), true)
            .unwrap();
        m.mark_pending(name, Storage::Str, &[id(3)]);
        let refused = |cache: &mut Cache, mark: &Mark| match cache.cut_to_mark(mark) {
            Err(CutRefused::Kept { model: m, by }) => {
                assert_eq!(m, model.index());
                by
            }
            other => panic!("expected a refusal, got {other:?}"),
        };
        assert_eq!(refused(&mut cache, &mark), KeptBy::Dirty(number.index()));
        assert_eq!(cache.model(model).unwrap().len(), 3, "nothing dropped");
        cache.model_mut(model).mark_clean(number, &[id(2)]);
        assert_eq!(refused(&mut cache, &mark), KeptBy::Pending(name.index()));
        cache.model_mut(model).clear_pending(name, &[id(3)]);
        let new = RecId::new_unique(1).unwrap();
        cache
            .model_mut(model)
            .set(name, Storage::Str, new, Cell::Str("new".into()), false)
            .unwrap();
        assert_eq!(refused(&mut cache, &mark), KeptBy::NewRecord);
        let snapshot = cache.snapshot();
        assert_eq!(cache.cut_to_mark(&mark), Err(CutRefused::Snapshot));
        cache.restore(snapshot).unwrap();
    }

    #[test]
    fn fetch_never_overwrites_dirty_values() {
        let f = fields()("number");
        let mut m = ModelCache::default();
        m.set(f, Storage::Int, id(7), Cell::Int(42), true).unwrap();
        m.insert_fetched(f, Storage::Int, id(7), Cell::Int(1))
            .unwrap();
        m.insert_fetched(f, Storage::Int, id(8), Cell::Null)
            .unwrap();
        assert_eq!(m.get(f, id(7)), Some(Cell::Int(42)));
        assert_eq!(m.get(f, id(8)), Some(Cell::Null));
        assert_eq!(m.dirty(), vec![(f.index(), vec![id(7)])]);
        m.mark_clean(f, &[id(7)]);
        assert!(!m.has_dirty());
    }

    #[test]
    fn invalidate_by_field_and_ids() {
        let field = fields();
        let (name, email) = (field("name"), field("email"));
        let mut m = ModelCache::default();
        for i in [1, 2] {
            m.set(name, Storage::Str, id(i), Cell::Str("n".into()), false)
                .unwrap();
            m.set(email, Storage::Str, id(i), Cell::Str("e".into()), false)
                .unwrap();
        }
        m.invalidate(Some(&[name]), None);
        m.invalidate(Some(&[email]), Some(&[id(1)]));
        assert!(!m.contains(name, id(1)) && !m.contains(name, id(2)));
        assert!(!m.contains(email, id(1)));
        assert!(m.contains(email, id(2)));
        m.invalidate(None, None);
        assert!(!m.contains(email, id(2)));
    }

    #[test]
    fn null_writes_keep_columns_consistent() {
        let field = fields();
        let mut m = ModelCache::default();
        // a NULL far past the end, then a value before it: both readable
        m.set(field("number"), Storage::Int, id(10), Cell::Int(1), false)
            .unwrap();
        m.set(field("number"), Storage::Int, id(11), Cell::Null, false)
            .unwrap();
        m.set(field("number"), Storage::Int, id(11), Cell::Int(5), false)
            .unwrap();
        assert_eq!(m.get(field("number"), id(11)), Some(Cell::Int(5)));
        m.set(field("name"), Storage::Str, id(3), Cell::Null, false)
            .unwrap();
        assert_eq!(m.get(field("name"), id(3)), Some(Cell::Null));
        m.set(
            field("name"),
            Storage::Str,
            id(3),
            Cell::Str("x".into()),
            false,
        )
        .unwrap();
        assert_eq!(m.get(field("name"), id(3)), Some(Cell::Str("x".into())));
    }

    #[test]
    #[cfg(not(debug_assertions))]
    fn invalidation_keeps_dirty_values() {
        let f = fields()("number");
        let mut m = ModelCache::default();
        m.set(f, Storage::Int, id(1), Cell::Int(3), true).unwrap();
        m.invalidate(None, None);
        assert_eq!(m.get(f, id(1)), Some(Cell::Int(3)));
    }

    /// Two model ids from a real registry (ids cannot be forged).
    fn two_models() -> (ModelId, ModelId) {
        let registry = RegistryBuilder::new([&T]).with_decls([&M]).build().unwrap();
        let models = registry.models();
        (models[0].id, models[models.len() - 1].id)
    }

    /// Fetch `ids` into `m` as a statement of `expected` rows does: one `fetched_slot` per row
    /// with the rows still to come, then the values of `fields`. Returns how many times the id
    /// list, then each field's column, changed capacity during the fetch.
    fn fetch(
        m: &mut ModelCache,
        ids: &[RecId],
        expected: usize,
        fields: &[(FieldId, Storage)],
    ) -> Vec<u32> {
        let caps = |m: &ModelCache| -> Vec<usize> {
            let cols = fields
                .iter()
                .map(|(f, _)| m.column(*f).map_or(0, |c| c.data.capacity()));
            std::iter::once(m.ids.capacity()).chain(cols).collect()
        };
        let mut last = caps(m);
        let mut growths = vec![0u32; last.len()];
        for (n, id) in ids.iter().enumerate() {
            let at = m.fetched_slot(*id, expected.saturating_sub(n));
            for (field, storage) in fields {
                let cell = match storage {
                    Storage::Int => Cell::Int(1),
                    Storage::Float => Cell::Float(1.0),
                    other => panic!("no test value for {other:?}"),
                };
                m.fill_fetched(at, *field, *storage, || Ok(cell)).unwrap();
            }
            let now = caps(m);
            for (g, (a, b)) in growths.iter_mut().zip(last.iter().zip(&now)) {
                if a != b {
                    *g += 1;
                }
            }
            last = now;
        }
        growths
    }

    /// Reviews F1, D2, A1 and N9: a fetch makes room for what it adds. Into an empty model the id
    /// list and the column grow once (counted) to exactly the row count; into a full model of S
    /// slots the list and the columns grow once (counted), from S to between S + N and
    /// 1.25 (S + N), not to about 2 S; a
    /// re-fetch of records that have slots reserves nothing; a new column fetched for a few
    /// records of a large model covers up to their highest slot only.
    #[test]
    #[ignore = "DEFERRED_PERFORMANCE: milestone-end product cost; semantic coverage runs in ordinary CI"]
    fn a_fetch_reserves_what_it_adds() {
        const S: i64 = 100_000;
        const N: i64 = 1_000;
        let field = fields();
        let (num, x) = (field("number"), field("x"));
        let first: Vec<RecId> = (1..=S).map(id).collect();
        let mut m = ModelCache::default();
        let growths = fetch(&mut m, &first, first.len(), &[(num, Storage::Int)]);
        // empty model: one exact growth
        assert_eq!(growths, [1, 1]);
        assert_eq!(m.ids.capacity(), S as usize);
        assert_eq!(m.column(num).unwrap().data.capacity(), S as usize);
        let map = m.slot_of.capacity();
        assert!(map >= S as usize);

        // populated model (S slots, at capacity), N new records with both columns: the list and
        // every column grow once, to at most a quarter over S + N (the geometric floor), not to
        // about 2 S; the map does not grow (it had room)
        let new: Vec<RecId> = (S + 1..=S + N).map(id).collect();
        assert_eq!(m.ids.capacity(), S as usize, "full before");
        assert_eq!(m.column(num).unwrap().data.capacity(), S as usize);
        let growths = fetch(
            &mut m,
            &new,
            new.len(),
            &[(num, Storage::Int), (x, Storage::Float)],
        );
        assert_eq!(growths, [1, 1, 1], "the list and each column grow once");
        let total = (S + N) as usize;
        let ceiling = total + total / 4;
        let within = |cap: usize| (total..=ceiling).contains(&cap);
        assert!(within(m.ids.capacity()), "{}", m.ids.capacity());
        let ids = m.ids.capacity();
        let read = m.column(num).unwrap().data.capacity();
        assert!(within(read), "{read}");
        assert!(within(m.column(x).unwrap().data.capacity()));
        assert!(m.column(x).unwrap().present.capacity_bits() <= ceiling + 64);
        assert_eq!(m.slot_of.capacity(), map);

        // a re-fetch of records that all have slots, into a column long enough: nothing more
        m.invalidate(Some(&[num]), None);
        let all: Vec<RecId> = (1..=S + N).map(id).collect();
        fetch(&mut m, &all, all.len(), &[(num, Storage::Int)]);
        assert_eq!(m.ids.capacity(), ids);
        assert_eq!(m.column(num).unwrap().data.capacity(), read);

        // a new column for the 10 records at the lowest slots: room for those slots only
        let mut m2 = ModelCache::default();
        fetch(&mut m2, &first, first.len(), &[(num, Storage::Int)]);
        fetch(&mut m2, &first[..10], 10, &[(x, Storage::Float)]);
        assert!(m2.column(x).unwrap().data.capacity() <= 64);
        // the same for the 10 at the highest slots: the column covers up to the last slot, once
        fetch(
            &mut m2,
            &first[first.len() - 10..],
            10,
            &[(x, Storage::Float)],
        );
        assert_eq!(m2.column(x).unwrap().data.capacity(), S as usize);
        assert_eq!(m2.ids.capacity(), S as usize);
    }

    /// Review A1: many small fetches into a full model reallocate the id list, a column and its
    /// bitset a few times (geometric growth), not once per fetch: 1,000 fetches of 2 new records
    /// into 100,000 slots.
    #[test]
    #[ignore = "DEFERRED_PERFORMANCE: milestone-end product cost; semantic coverage runs in ordinary CI"]
    fn small_fetches_into_a_full_model_grow_geometrically() {
        const S: i64 = 100_000;
        let num = fields()("number");
        let first: Vec<RecId> = (1..=S).map(id).collect();
        let mut m = ModelCache::default();
        fetch(&mut m, &first, first.len(), &[(num, Storage::Int)]);
        assert_eq!(m.ids.capacity(), m.ids.len(), "the model starts full");
        let caps = |m: &ModelCache| {
            let col = m.column(num).unwrap();
            [
                m.ids.capacity(),
                col.data.capacity(),
                col.present.capacity_bits(),
            ]
        };
        let mut last = caps(&m);
        let mut changes = [0u32; 3];
        for k in 0..1_000 {
            let pair = [id(S + 1 + 2 * k), id(S + 2 + 2 * k)];
            fetch(&mut m, &pair, 2, &[(num, Storage::Int)]);
            let now = caps(&m);
            for (i, (a, b)) in last.iter().zip(&now).enumerate() {
                if a != b {
                    changes[i] += 1;
                    // every growth is geometric: at least a quarter (a linear step such as
                    // `remaining + 1000` would fail here)
                    assert!(*b >= grown(*a), "capacity {i} grew from {a} to {b}");
                }
            }
            last = now;
        }
        assert_eq!(m.ids.len(), (S + 2_000) as usize);
        // 2,000 new slots on 100,000: one growth by a quarter covers them; allow a few
        assert!(changes.iter().all(|c| *c <= 3), "{changes:?}");
    }

    /// Reviews N4, N6 and N7: a one-row or unknown-size fetch into a full model grows the id
    /// list, a column and its bitset by a quarter, not by doubling; into an empty model the first
    /// row makes room for 4 and 16 single rows cost 4 growths, not 16.
    #[test]
    #[ignore = "DEFERRED_PERFORMANCE: milestone-end product cost; semantic coverage runs in ordinary CI"]
    fn one_row_and_unknown_size_fetches_grow_by_a_quarter() {
        // a multiple of 64, so the bitset is full too
        const S: i64 = 102_400;
        let num = fields()("number");

        let mut m = ModelCache::default();
        let mut growths = [0u32; 2];
        for k in 1..=16 {
            let before = [
                m.ids.capacity(),
                m.column(num).map_or(0, |c| c.data.capacity()),
            ];
            let at = m.fetched_slot(id(k), 0);
            m.fill_fetched(at, num, Storage::Int, || Ok(Cell::Int(1)))
                .unwrap();
            let after = [m.ids.capacity(), m.column(num).unwrap().data.capacity()];
            if k == 1 {
                assert!(after.iter().all(|c| *c >= 4), "{after:?}");
            }
            for (g, (a, b)) in growths.iter_mut().zip(before.iter().zip(&after)) {
                if a != b {
                    *g += 1;
                }
            }
        }
        assert!(growths.iter().all(|g| *g <= 4), "{growths:?}");

        let first: Vec<RecId> = (1..=S).map(id).collect();
        for remaining in [1, 0] {
            let mut m = ModelCache::default();
            fetch(&mut m, &first, first.len(), &[(num, Storage::Int)]);
            let full = S as usize;
            assert_eq!(m.ids.capacity(), full, "the model starts full");
            assert_eq!(m.column(num).unwrap().present.capacity_bits(), full);
            let at = m.fetched_slot(id(S + 1), remaining);
            m.fill_fetched(at, num, Storage::Int, || Ok(Cell::Int(1)))
                .unwrap();
            let quarter = full + full / 4;
            assert_eq!(m.ids.capacity(), quarter, "remaining = {remaining}");
            let col = m.column(num).unwrap();
            assert_eq!(col.data.capacity(), quarter, "remaining = {remaining}");
            let bits = col.present.capacity_bits();
            assert!(bits >= grown(full) && bits < quarter + 64, "{bits}");
        }
    }

    /// Review round 4 D1: a stream fill reserves for the records it adds, not for its rows:
    /// 100,000 rows over the records of a full model, then 20,000 rows over 20 new records, grow
    /// the model by a quarter once (with an estimate from the rows it would reserve 100,000).
    #[test]
    #[ignore = "DEFERRED_PERFORMANCE: milestone-end product cost; semantic coverage runs in ordinary CI"]
    fn a_stream_fill_reserves_for_new_records_not_rows() {
        const S: i64 = 1_024;
        let num = fields()("number");
        let (m0, _) = two_models();
        let mut c = Cache::default();
        let first: Vec<RecId> = (1..=S).map(id).collect();
        fetch(c.model_mut(m0), &first, first.len(), &[(num, Storage::Int)]);
        assert_eq!(c.model_mut(m0).ids.capacity(), S as usize, "full before");
        let mut fill = StreamFill::new(m0);
        let mut decoded = 0u32;
        let mut row = |c: &mut Cache, id: RecId| {
            fill.fill(c, id, num, Storage::Int, || {
                decoded += 1;
                Ok(Cell::Int(2))
            })
            .unwrap();
        };
        for k in 0..100_000 {
            row(&mut c, id(1 + k % S));
        }
        for k in 0..20_000 {
            row(&mut c, id(S + 1 + k % 20));
        }
        assert_eq!(decoded, 20, "cached values are not decoded again (W9)");
        let m = c.model_mut(m0);
        assert_eq!(m.len(), (S + 20) as usize);
        let full = S as usize;
        assert_eq!(m.ids.capacity(), grown(full));
        assert_eq!(m.column(num).unwrap().data.capacity(), grown(full));
    }

    // Same actual fetch operations as the milestone tests; no capacity sampling.
    fn fetch_values(
        m: &mut ModelCache,
        ids: &[RecId],
        expected: usize,
        fields: &[(FieldId, Storage)],
    ) {
        for (n, id) in ids.iter().enumerate() {
            let at = m.fetched_slot(*id, expected.saturating_sub(n));
            for (field, storage) in fields {
                let cell = match storage {
                    Storage::Int => Cell::Int(1),
                    Storage::Float => Cell::Float(1.0),
                    other => panic!("no test value for {other:?}"),
                };
                m.fill_fetched(at, *field, *storage, || Ok(cell)).unwrap();
            }
        }
    }

    fn assert_int_values(m: &ModelCache, field: FieldId, first: i64, last: i64, value: i32) {
        for record in first..=last {
            assert_eq!(
                m.get(field, id(record)),
                Some(Cell::Int(value)),
                "record {record}"
            );
        }
    }

    #[test]
    fn fetches_preserve_cached_values() {
        const S: i64 = 100_000;
        const N: i64 = 1_000;
        let field = fields();
        let (num, x) = (field("number"), field("x"));
        let first: Vec<RecId> = (1..=S).map(id).collect();
        let mut m = ModelCache::default();
        fetch_values(&mut m, &first, first.len(), &[(num, Storage::Int)]);
        assert_int_values(&m, num, 1, S, 1);
        let new: Vec<RecId> = (S + 1..=S + N).map(id).collect();
        fetch_values(
            &mut m,
            &new,
            new.len(),
            &[(num, Storage::Int), (x, Storage::Float)],
        );
        assert_eq!(m.len(), (S + N) as usize);
        assert_int_values(&m, num, 1, S + N, 1);
        for record in S + 1..=S + N {
            assert_eq!(m.get(x, id(record)), Some(Cell::Float(1.0)));
        }
        m.invalidate(Some(&[num]), None);
        assert_eq!(m.get(num, id(1)), None);
        let all: Vec<RecId> = (1..=S + N).map(id).collect();
        fetch_values(&mut m, &all, all.len(), &[(num, Storage::Int)]);
        assert_int_values(&m, num, 1, S + N, 1);
        let mut m2 = ModelCache::default();
        fetch_values(&mut m2, &first, first.len(), &[(num, Storage::Int)]);
        fetch_values(&mut m2, &first[..10], 10, &[(x, Storage::Float)]);
        fetch_values(
            &mut m2,
            &first[first.len() - 10..],
            10,
            &[(x, Storage::Float)],
        );
        assert_int_values(&m2, num, 1, S, 1);
        for record in (1..=10).chain(S - 9..=S) {
            assert_eq!(m2.get(x, id(record)), Some(Cell::Float(1.0)));
        }
        assert_eq!(m2.get(x, id(S / 2)), None);
    }

    #[test]
    fn small_fetches_preserve_all_records() {
        const S: i64 = 100_000;
        let num = fields()("number");
        let first: Vec<RecId> = (1..=S).map(id).collect();
        let mut m = ModelCache::default();
        fetch_values(&mut m, &first, first.len(), &[(num, Storage::Int)]);
        for k in 0..1_000 {
            let pair = [id(S + 1 + 2 * k), id(S + 2 + 2 * k)];
            fetch_values(&mut m, &pair, 2, &[(num, Storage::Int)]);
        }
        assert_eq!(m.len(), (S + 2_000) as usize);
        assert_int_values(&m, num, 1, S + 2_000, 1);
    }

    #[test]
    fn one_row_and_unknown_size_fetches_preserve_values() {
        const S: i64 = 102_400;
        let num = fields()("number");
        let mut m = ModelCache::default();
        for k in 1..=16 {
            fetch_values(&mut m, &[id(k)], 0, &[(num, Storage::Int)]);
        }
        assert_eq!(m.len(), 16);
        assert_int_values(&m, num, 1, 16, 1);
        let first: Vec<RecId> = (1..=S).map(id).collect();
        for remaining in [1, 0] {
            let mut m = ModelCache::default();
            fetch_values(&mut m, &first, first.len(), &[(num, Storage::Int)]);
            fetch_values(&mut m, &[id(S + 1)], remaining, &[(num, Storage::Int)]);
            assert_eq!(m.len(), (S + 1) as usize);
            assert_int_values(&m, num, 1, S + 1, 1);
        }
    }

    #[test]
    fn stream_fill_preserves_cached_and_new_values() {
        const S: i64 = 1_024;
        let num = fields()("number");
        let (m0, _) = two_models();
        let mut c = Cache::default();
        let first: Vec<RecId> = (1..=S).map(id).collect();
        fetch_values(c.model_mut(m0), &first, first.len(), &[(num, Storage::Int)]);
        let mut fill = StreamFill::new(m0);
        for k in 0..100_000 {
            fill.fill(&mut c, id(1 + k % S), num, Storage::Int, || {
                Ok(Cell::Int(2))
            })
            .unwrap();
        }
        for k in 0..20_000 {
            fill.fill(&mut c, id(S + 1 + k % 20), num, Storage::Int, || {
                Ok(Cell::Int(2))
            })
            .unwrap();
        }
        let m = c.model_mut(m0);
        assert_eq!(m.len(), (S + 20) as usize);
        assert_int_values(m, num, 1, S, 1);
        assert_int_values(m, num, S + 1, S + 20, 2);
    }

    /// Review N4: a known count is spent row by row; past it, or without one, the estimate is
    /// the rows seen so far (at least one), so a lone row asks for one slot and a stream doubles
    /// its own share.
    #[test]
    fn rows_to_come_spends_a_known_count_then_follows_the_stream() {
        assert_eq!(rows_to_come(3, 0), 3);
        assert_eq!(rows_to_come(3, 2), 1);
        assert_eq!(rows_to_come(3, 3), 3);
        assert_eq!(rows_to_come(3, 7), 7);
        assert_eq!(rows_to_come(0, 0), 1);
        assert_eq!(rows_to_come(0, 1), 1);
        assert_eq!(rows_to_come(0, 500), 500);
    }

    /// Review F5: a writer saves its model once in the open layers; a layer opened or restored
    /// after it makes the next write go through `model_mut` again, so every layer still restores
    /// exactly.
    #[test]
    fn a_writer_saves_once_and_follows_later_layers() {
        let num = fields()("number");
        let (m0, _) = two_models();
        let mut c = Cache::default();
        c.model_mut(m0)
            .set(num, Storage::Int, id(1), Cell::Int(1), false)
            .unwrap();
        let outer = c.snapshot();
        let w = c.writer(m0);
        assert_eq!(c.saved_models(), 1, "saved when the writer is taken");
        c.write(w)
            .insert_fetched(num, Storage::Int, id(2), Cell::Int(2))
            .unwrap();
        let inner = c.snapshot();
        c.write(w)
            .set(num, Storage::Int, id(1), Cell::Int(9), false)
            .unwrap();
        assert_eq!(c.saved_models(), 1, "the later layer saved the model first");
        c.restore(inner).unwrap();
        assert_eq!(c.get(m0, num, id(1)), Some(Cell::Int(1)));
        assert_eq!(c.get(m0, num, id(2)), Some(Cell::Int(2)));
        // after a restore the writer still writes, through `model_mut`
        c.write(w)
            .insert_fetched(num, Storage::Int, id(3), Cell::Int(3))
            .unwrap();
        assert_eq!(c.get(m0, num, id(3)), Some(Cell::Int(3)));
        c.restore(outer).unwrap();
        assert!(!c.contains(m0, num, id(2)) && !c.contains(m0, num, id(3)));
        assert_eq!(c.get(m0, num, id(1)), Some(Cell::Int(1)));
    }

    /// a snapshot layer brings back values, dirty flags, to-compute sets and models
    /// that did not exist; reads copy nothing; nested layers close with their parent; a layer
    /// survives a savepoint-style clear but not the end of the transaction.
    #[test]
    fn snapshot_restores_exactly() {
        let field = fields();
        let (num, name) = (field("number"), field("name"));
        let (m0, m1) = two_models();
        let mut c = Cache::default();
        c.model_mut(m0)
            .set(num, Storage::Int, id(1), Cell::Int(1), true)
            .unwrap();
        c.model_mut(m0)
            .set(name, Storage::Str, id(2), Cell::Str("a".into()), false)
            .unwrap();
        let snap = c.snapshot();
        assert_eq!(c.get(m0, num, id(1)), Some(Cell::Int(1)));
        assert_eq!(c.saved_models(), 0, "reads copy nothing");
        c.model_mut(m0)
            .set(num, Storage::Int, id(1), Cell::Int(9), true)
            .unwrap();
        c.model_mut(m0).mark_clean(num, &[id(1)]);
        c.model_mut(m0).mark_pending(name, Storage::Str, &[id(3)]);
        c.model_mut(m0)
            .insert_fetched(name, Storage::Str, id(4), Cell::Str("db".into()))
            .unwrap();
        assert_eq!(c.saved_models(), 1, "one copy per model, on first write");
        c.model_mut(m1)
            .set(num, Storage::Int, id(5), Cell::Int(5), true)
            .unwrap();
        let inner = c.snapshot();
        c.model_mut(m0).invalidate(Some(&[name]), None);
        c.clear();
        c.restore(snap).unwrap();
        let mc = c.model(m0).unwrap();
        assert_eq!(mc.get(num, id(1)), Some(Cell::Int(1)));
        assert!(mc.is_dirty(num, id(1)));
        assert_eq!(mc.get(name, id(2)), Some(Cell::Str("a".into())));
        assert!(!mc.is_pending(name, id(3)));
        assert!(!mc.contains(name, id(4)));
        assert!(c.model(m1).is_none_or(|m| !m.has_dirty() && m.is_empty()));
        assert!(c.layers.is_empty());
        assert_eq!(
            c.restore(inner).unwrap_err().kind(),
            erp_fields::ErrorKind::Programming
        );
        let snap = c.snapshot();
        c.reset();
        assert!(c.restore(snap).is_err());
    }

    #[test]
    fn typed_columns_round_trip() {
        let field = fields();
        let mut m = ModelCache::default();
        let cases = [
            ("flag", Storage::Bool, Cell::Bool(true)),
            ("x", Storage::Float, Cell::Float(0.1 + 0.2)),
            ("day", Storage::Date, Cell::Date(Date::from_days(19_000))),
            (
                "doc",
                Storage::Json,
                Cell::Json(Arc::new(erp_fields::serde_json::json!([1]))),
            ),
        ];
        for (name, storage, cell) in cases {
            m.set(field(name), storage, id(1), cell.clone(), false)
                .unwrap();
            assert_eq!(m.get(field(name), id(1)), Some(cell));
        }
        let err = m
            .set(field("flag"), Storage::Bool, id(1), Cell::Int(1), false)
            .unwrap_err();
        assert_eq!(err.kind(), erp_fields::ErrorKind::Programming);
    }
}

#[cfg(test)]
mod native_fields_tests {
    #![allow(clippy::unwrap_used, reason = "tests")]
    use super::*;
    use erp_fields::{TimestampTz, Uuid, Xid8};

    #[test]
    fn native_fields_cache_preserves_absent_null_values_and_clone() {
        for (storage, value) in [
            (Storage::Uuid, Cell::Uuid(Uuid::NIL)),
            (Storage::Xid8, Cell::Xid8(Xid8::from_u64(u64::MAX))),
            (
                Storage::TimestampTz,
                Cell::TimestampTz(TimestampTz::UNIX_EPOCH),
            ),
        ] {
            let mut column = Column::new(storage);
            assert_eq!(column.get(0), None);
            column.set(2, value.clone()).unwrap();
            assert_eq!(column.get(0), None);
            assert_eq!(column.get(2), Some(value.clone()));
            let checkpoint = column.clone();
            column.set(2, Cell::Null).unwrap();
            assert_eq!(column.get(2), Some(Cell::Null));
            assert_eq!(checkpoint.get(2), Some(value));
            column.remove(2);
            assert_eq!(column.get(2), None);
            let error = column
                .set(2, Cell::Str(Arc::from("wrong type")))
                .unwrap_err();
            assert_eq!(error.kind(), erp_fields::ErrorKind::Programming);
            assert!(error.message().contains(column.data.kind()));
            assert_eq!(column.get(2), None);
        }
    }
}
