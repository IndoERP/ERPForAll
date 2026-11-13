//! Per-registry-generation cache of statement texts (PERF.SQL-TEXT).
//!
//! Statements for `select_by_ids`, `select`, `select_ids_where`, and `update_from_arrays`
//! depend only on (model, columns, shape) on an immutable registry. Caching their SQL
//! text on the registry generation is meant to remove SQL construction allocations on warm
//! paths (by construction; not measured, DEFERRED_PERFORMANCE until milestone end).

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, PoisonError, RwLock};

use erp_fields::ColumnType;

use crate::spec::{FieldId, ModelId};

/// Query shape specification for cached `select` statements.
#[derive(Debug, Clone, Copy, Default)]
pub struct SelectShape<'a> {
    pub joins: Option<&'a str>,
    pub predicate: Option<&'a str>,
    pub order: Option<&'a str>,
    pub limit: Option<usize>,
    pub offset: Option<usize>,
}

/// Additional SQL shape for cached array updates.
///
/// Projection order is significant: callers decode returned columns positionally.
///
/// ```
/// use erp_registry::statement_cache::UpdateShape;
/// let shape = UpdateShape::new()
///     .expected(true)
///     .returning_changed(&["name", "active"]);
/// ```
#[derive(Debug, Clone, Copy, Default)]
#[non_exhaustive]
pub struct UpdateShape<'a> {
    bump: bool,
    expected: bool,
    returning_old: &'a [&'a str],
    returning_changed: &'a [&'a str],
    merge_json: &'a [&'a str],
    merge_company: &'a [&'a str],
}

impl<'a> UpdateShape<'a> {
    /// Plain update without additional clauses.
    pub fn new() -> Self {
        Self::default()
    }

    /// Bump the row version.
    #[must_use]
    pub fn bump(mut self, value: bool) -> Self {
        self.bump = value;
        self
    }

    /// Check expected row versions.
    #[must_use]
    pub fn expected(mut self, value: bool) -> Self {
        self.expected = value;
        self
    }

    /// Ordered OLD/NEW hook projections.
    #[must_use]
    pub fn returning_old(mut self, value: &'a [&'a str]) -> Self {
        self.returning_old = value;
        self
    }

    /// Ordered NULL-safe OLD/NEW difference projections.
    #[must_use]
    pub fn returning_changed(mut self, value: &'a [&'a str]) -> Self {
        self.returning_changed = value;
        self
    }

    /// Columns updated with null-preserving company patches.
    #[must_use]
    pub fn merge_company(mut self, value: &'a [&'a str]) -> Self {
        self.merge_company = value;
        self
    }

    /// Columns merged as JSONB deltas.
    #[must_use]
    pub fn merge_json(mut self, value: &'a [&'a str]) -> Self {
        self.merge_json = value;
        self
    }
}

#[derive(Debug, Clone)]
struct ByIdsEntry {
    fields: Box<[FieldId]>,
    sql: Arc<str>,
}

/// Per-model cached statement texts.
#[derive(Debug, Default)]
pub struct ModelStatementCache {
    select_by_ids: RwLock<Vec<ByIdsEntry>>,
    select: RwLock<Vec<SelectEntry>>,
    select_ids_where: RwLock<Vec<SelectIdsWhereEntry>>,
    update_from_arrays: RwLock<Vec<UpdateEntry>>,
    /// Entries dropped because a per-kind cap was reached (see [`Self::evictions`]).
    evictions: AtomicU64,
}

#[derive(Debug, Clone)]
struct SelectEntry {
    columns: Box<[FieldId]>,
    joins: Option<Arc<str>>,
    predicate: Option<Arc<str>>,
    order: Option<Arc<str>>,
    limit: Option<usize>,
    offset: Option<usize>,
    sql: Arc<str>,
}

#[derive(Debug, Clone)]
struct SelectIdsWhereEntry {
    columns: Box<[FieldId]>,
    joins: Arc<str>,
    predicate: Option<Arc<str>>,
    ids_param: usize,
    sql: Arc<str>,
}

#[derive(Debug, Clone)]
struct UpdateEntry {
    columns: Box<[(Box<str>, ColumnType)]>,
    extra: Box<[(Box<str>, Box<str>)]>,
    bump: bool,
    expected: bool,
    returning_old: Box<[Box<str>]>,
    returning_changed: Box<[Box<str>]>,
    merge_json: Box<[Box<str>]>,
    merge_company: Box<[Box<str>]>,
    sql: Arc<str>,
}

const MAX_SELECT_ENTRIES_PER_MODEL: usize = 128;
const MAX_WHERE_ENTRIES_PER_MODEL: usize = 64;
const MAX_UPDATE_ENTRIES_PER_MODEL: usize = 64;
const MAX_BY_IDS_ENTRIES_PER_MODEL: usize = 32;

impl ModelStatementCache {
    /// Number of entries evicted from this model's caches because a cap was reached. A value
    /// that keeps growing means the workload has more statement shapes than the cap and the
    /// cache is thrashing (every miss then pays the write lock and the build). The value is a
    /// snapshot (relaxed atomic), not synchronised with the cache contents.
    pub fn evictions(&self) -> u64 {
        self.evictions.load(Ordering::Relaxed)
    }

    fn note_eviction(&self) {
        self.evictions.fetch_add(1, Ordering::Relaxed);
    }

    /// Cached statement for `select_by_ids`.
    pub fn select_by_ids<F>(&self, fields: &[FieldId], build: F) -> Arc<str>
    where
        F: FnOnce() -> String,
    {
        {
            let entries = self
                .select_by_ids
                .read()
                .unwrap_or_else(PoisonError::into_inner);
            for entry in entries.iter() {
                if entry.fields.as_ref() == fields {
                    return Arc::clone(&entry.sql);
                }
            }
        }
        let sql: Arc<str> = Arc::from(build());
        let mut entries = self
            .select_by_ids
            .write()
            .unwrap_or_else(PoisonError::into_inner);
        for entry in entries.iter() {
            if entry.fields.as_ref() == fields {
                return Arc::clone(&entry.sql);
            }
        }
        if entries.len() >= MAX_BY_IDS_ENTRIES_PER_MODEL {
            entries.remove(0);
            self.note_eviction();
        }
        entries.push(ByIdsEntry {
            fields: Box::from(fields),
            sql: Arc::clone(&sql),
        });
        sql
    }

    /// Cached statement for `select`.
    pub fn select<F>(&self, columns: &[FieldId], shape: SelectShape<'_>, build: F) -> Arc<str>
    where
        F: FnOnce() -> String,
    {
        {
            let entries = self.select.read().unwrap_or_else(PoisonError::into_inner);
            for entry in entries.iter() {
                if entry.columns.as_ref() == columns
                    && entry.limit == shape.limit
                    && entry.offset == shape.offset
                    && entry.joins.as_deref() == shape.joins
                    && entry.predicate.as_deref() == shape.predicate
                    && entry.order.as_deref() == shape.order
                {
                    return Arc::clone(&entry.sql);
                }
            }
        }
        let sql: Arc<str> = Arc::from(build());
        let mut entries = self.select.write().unwrap_or_else(PoisonError::into_inner);
        for entry in entries.iter() {
            if entry.columns.as_ref() == columns
                && entry.limit == shape.limit
                && entry.offset == shape.offset
                && entry.joins.as_deref() == shape.joins
                && entry.predicate.as_deref() == shape.predicate
                && entry.order.as_deref() == shape.order
            {
                return Arc::clone(&entry.sql);
            }
        }
        if entries.len() >= MAX_SELECT_ENTRIES_PER_MODEL {
            entries.remove(0);
            self.note_eviction();
        }
        entries.push(SelectEntry {
            columns: Box::from(columns),
            joins: shape.joins.map(Arc::from),
            predicate: shape.predicate.map(Arc::from),
            order: shape.order.map(Arc::from),
            limit: shape.limit,
            offset: shape.offset,
            sql: Arc::clone(&sql),
        });
        sql
    }

    /// Cached statement for `select_ids_where`.
    pub fn select_ids_where<F>(
        &self,
        columns: &[FieldId],
        joins: &str,
        predicate: Option<&str>,
        ids_param: usize,
        build: F,
    ) -> Arc<str>
    where
        F: FnOnce() -> String,
    {
        {
            let entries = self
                .select_ids_where
                .read()
                .unwrap_or_else(PoisonError::into_inner);
            for entry in entries.iter() {
                if entry.columns.as_ref() == columns
                    && entry.ids_param == ids_param
                    && entry.joins.as_ref() == joins
                    && entry.predicate.as_deref() == predicate
                {
                    return Arc::clone(&entry.sql);
                }
            }
        }
        let sql: Arc<str> = Arc::from(build());
        let mut entries = self
            .select_ids_where
            .write()
            .unwrap_or_else(PoisonError::into_inner);
        for entry in entries.iter() {
            if entry.columns.as_ref() == columns
                && entry.ids_param == ids_param
                && entry.joins.as_ref() == joins
                && entry.predicate.as_deref() == predicate
            {
                return Arc::clone(&entry.sql);
            }
        }
        if entries.len() >= MAX_WHERE_ENTRIES_PER_MODEL {
            entries.remove(0);
            self.note_eviction();
        }
        entries.push(SelectIdsWhereEntry {
            columns: Box::from(columns),
            joins: Arc::from(joins),
            predicate: predicate.map(Arc::from),
            ids_param,
            sql: Arc::clone(&sql),
        });
        sql
    }

    /// Cached statement for `update_from_arrays`.
    pub fn update_from_arrays<F>(
        &self,
        columns: &[(&str, ColumnType)],
        extra: &[(&str, &str)],
        shape: UpdateShape<'_>,
        build: F,
    ) -> Arc<str>
    where
        F: FnOnce() -> String,
    {
        let UpdateShape {
            bump,
            expected,
            returning_old,
            returning_changed,
            merge_json,
            merge_company,
        } = shape;
        {
            let entries = self
                .update_from_arrays
                .read()
                .unwrap_or_else(PoisonError::into_inner);
            for entry in entries.iter() {
                if entry.bump == bump
                    && entry.expected == expected
                    && entry.extra.len() == extra.len()
                    && entry.columns.len() == columns.len()
                    && entry.returning_old.len() == returning_old.len()
                    && entry.returning_changed.len() == returning_changed.len()
                    && entry.merge_company.len() == merge_company.len()
                    && entry
                        .merge_company
                        .iter()
                        .zip(merge_company)
                        .all(|(a, b)| a.as_ref() == *b)
                    && entry.merge_json.len() == merge_json.len()
                    && entry
                        .columns
                        .iter()
                        .zip(columns)
                        .all(|(a, b)| a.0.as_ref() == b.0 && a.1 == b.1)
                    && entry
                        .extra
                        .iter()
                        .zip(extra)
                        .all(|(a, b)| a.0.as_ref() == b.0 && a.1.as_ref() == b.1)
                    && entry
                        .returning_old
                        .iter()
                        .zip(returning_old)
                        .all(|(a, b)| a.as_ref() == *b)
                    && entry
                        .returning_changed
                        .iter()
                        .zip(returning_changed)
                        .all(|(a, b)| a.as_ref() == *b)
                    && entry
                        .merge_json
                        .iter()
                        .zip(merge_json)
                        .all(|(a, b)| a.as_ref() == *b)
                {
                    return Arc::clone(&entry.sql);
                }
            }
        }
        let sql: Arc<str> = Arc::from(build());
        let mut entries = self
            .update_from_arrays
            .write()
            .unwrap_or_else(PoisonError::into_inner);
        for entry in entries.iter() {
            if entry.bump == bump
                && entry.expected == expected
                && entry.extra.len() == extra.len()
                && entry.columns.len() == columns.len()
                && entry.returning_old.len() == returning_old.len()
                && entry.returning_changed.len() == returning_changed.len()
                && entry.merge_company.len() == merge_company.len()
                && entry
                    .merge_company
                    .iter()
                    .zip(merge_company)
                    .all(|(a, b)| a.as_ref() == *b)
                && entry.merge_json.len() == merge_json.len()
                && entry
                    .columns
                    .iter()
                    .zip(columns)
                    .all(|(a, b)| a.0.as_ref() == b.0 && a.1 == b.1)
                && entry
                    .extra
                    .iter()
                    .zip(extra)
                    .all(|(a, b)| a.0.as_ref() == b.0 && a.1.as_ref() == b.1)
                && entry
                    .returning_old
                    .iter()
                    .zip(returning_old)
                    .all(|(a, b)| a.as_ref() == *b)
                && entry
                    .returning_changed
                    .iter()
                    .zip(returning_changed)
                    .all(|(a, b)| a.as_ref() == *b)
                && entry
                    .merge_json
                    .iter()
                    .zip(merge_json)
                    .all(|(a, b)| a.as_ref() == *b)
            {
                return Arc::clone(&entry.sql);
            }
        }
        if entries.len() >= MAX_UPDATE_ENTRIES_PER_MODEL {
            entries.remove(0);
            self.note_eviction();
        }
        entries.push(UpdateEntry {
            columns: columns
                .iter()
                .map(|(c, t)| (Box::from(*c), *t))
                .collect::<Vec<_>>()
                .into_boxed_slice(),
            extra: extra
                .iter()
                .map(|(k, v)| (Box::from(*k), Box::from(*v)))
                .collect::<Vec<_>>()
                .into_boxed_slice(),
            bump,
            expected,
            returning_old: returning_old
                .iter()
                .map(|c| Box::from(*c))
                .collect::<Vec<_>>()
                .into_boxed_slice(),
            returning_changed: returning_changed
                .iter()
                .map(|c| Box::from(*c))
                .collect::<Vec<_>>()
                .into_boxed_slice(),
            merge_company: merge_company
                .iter()
                .map(|c| Box::from(*c))
                .collect::<Vec<_>>()
                .into_boxed_slice(),
            merge_json: merge_json
                .iter()
                .map(|c| Box::from(*c))
                .collect::<Vec<_>>()
                .into_boxed_slice(),
            sql: Arc::clone(&sql),
        });
        sql
    }
}

/// The statement cache for a registry generation.
#[derive(Debug)]
pub struct StatementCache {
    models: Box<[ModelStatementCache]>,
}

impl StatementCache {
    /// Create a new cache for `model_count` models.
    pub(crate) fn new(model_count: usize) -> Self {
        let mut models = Vec::with_capacity(model_count);
        for _ in 0..model_count {
            models.push(ModelStatementCache::default());
        }
        Self {
            models: models.into_boxed_slice(),
        }
    }

    /// Access the cache of `model`. The id must come from the registry generation that owns
    /// this cache: ids carry no generation, so an id of another generation with a valid index
    /// would select another model's slot. Callers outside this crate go through
    /// `Registry::statement_cache_of`, which checks it in every build.
    pub(crate) fn model(&self, model: ModelId) -> &ModelStatementCache {
        assert!(
            model.index() < self.models.len(),
            "ModelId {} is out of range for a statement cache of {} models (id from another registry generation?)",
            model.index(),
            self.models.len()
        );
        &self.models[model.index()]
    }

    /// Total evictions over all models (see [`ModelStatementCache::evictions`]).
    pub fn evictions(&self) -> u64 {
        self.models.iter().map(ModelStatementCache::evictions).sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn update_cache_distinguishes_changed_projection_and_order() {
        let cache = ModelStatementCache::default();
        let columns = [("a", ColumnType::Int8)];
        for names in [
            &[][..],
            &["a"][..],
            &["b"][..],
            &["a", "b"][..],
            &["b", "a"][..],
        ] {
            let shape = UpdateShape::new().returning_changed(names);
            let expected = format!("projection={names:?}");
            let sql = cache.update_from_arrays(&columns, &[], shape, || expected.clone());
            assert_eq!(&*sql, expected);
            let hit = cache.update_from_arrays(&columns, &[], shape, || panic!("cache miss"));
            assert!(Arc::ptr_eq(&sql, &hit));
        }
    }
}
