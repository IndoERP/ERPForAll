//! Until the real `addons/base` exists (it then takes the
//! declaration over unchanged), it is declared by the [`ADDON`] `base_data` (depends on `base`): a
//! registry gets it by adding [`ADDON`] to its addon set and [`IR_MODEL_DATA`]
//! (`RegistryBuilder::with_decls`) or [`ir_model_data_decl`] (`HarnessBuilder::with_decls`) to
//! its declarations. It is not linked into every registry, so registries without the data layer
//! keep their exact table set. Every function here fails with a `Programming` error naming the fix
//! when the registry lacks the model.
//!
//! Public API for the data layer (/S2 and the import), on the readers' validated
//! [`XmlId`] (an id without module is the row with module `''`): [`lookup`],
//! [`xmlid_to_res_id`] and [`update_xmlids`]. Every function is one set-based statement (plus
//! flushing pending ORM writes first); [`update_xmlids`] writes with
//! `INSERT … ON CONFLICT (module, name) DO UPDATE`, so two transactions binding the same
//! external id never fail on the unique index (under REPEATABLE READ the later one gets a
//! serialization failure, which the unit of work retries).

use std::collections::{HashMap, HashSet};
use std::hash::{BuildHasher, Hasher};
use std::sync::Arc;

use erp_domain::{Domain, Operator, Value as DValue};
use erp_fields::RecId;
use erp_fields::{Cell, Error, ErrorKind, Result, Value};
use erp_orm::{BoxFuture, Env, RawVals, Records, Transaction, UnlinkHook};
use erp_registry::{
    AddonDescriptor, FieldDecl, ModelDecl, ModelSpec, StaticValue, TableObjectDecl, TableObjectKind,
};
use tokio_postgres::Row;

use crate::ast::XmlId;

/// Model name.
pub const MODEL: &str = "ir.model.data";

/// The addon declaring `ir.model.data` until `addons/base` takes it over.
pub static ADDON: AddonDescriptor = AddonDescriptor {
    name: "base_data",
    depends: &["base"],
    exact_only: false,
};

const FIELDS: &[FieldDecl] = &[
    FieldDecl::char("name")
        .string("External Identifier")
        .required(true),
    FieldDecl::char("model").string("Model Name").required(true),
    FieldDecl::char("module")
        .default(StaticValue::Str(""))
        .required(true),
    FieldDecl::many2one_reference("res_id", "model").string("Record ID"),
    FieldDecl::boolean("noupdate")
        .string("Non Updatable")
        .default(StaticValue::Bool(false)),
];

const TABLE_OBJECTS: &[TableObjectDecl] = &[
    TableObjectDecl {
        name: "_name_nospaces",
        kind: TableObjectKind::Constraint {
            definition: "CHECK(name NOT LIKE '% %')",
            message: "External IDs cannot contain spaces",
        },
    },
    TableObjectDecl {
        name: "_module_name_uniq_index",
        kind: TableObjectKind::UniqueIndex {
            definition: "(module, name)",
            message: "",
        },
    },
    TableObjectDecl {
        name: "_model_res_id_index",
        kind: TableObjectKind::Index {
            definition: "(model, res_id)",
        },
    },
];

/// The `ir.model.data` declaration (addon [`ADDON`]), as a value for builders that take
/// ownership.
pub const fn ir_model_data_decl() -> ModelDecl {
    ModelDecl::new("base_data", MODEL)
        .description("Model Data")
        .order("module, model, name")
        .fields(FIELDS)
        .table_objects(TABLE_OBJECTS)
        .at(file!(), line!())
}

/// The `ir.model.data` declaration (addon [`ADDON`]).
pub static IR_MODEL_DATA: ModelDecl = ir_model_data_decl();

/// The quoted table of `ir.model.data` in the environment's registry, or the error telling how to
/// add the model.
fn table(env: &Env) -> Result<String> {
    env.registry()
        .model(MODEL)
        .map(|spec| erp_sql::quote(&spec.table))
        .ok_or_else(|| {
            Error::programming(format!(
                "model '{MODEL}' is not in the registry: add the addon \
                 erp_data::model_data::ADDON and the declaration erp_data::IR_MODEL_DATA \
                 (erp_data::ir_model_data_decl())"
            ))
        })
}

/// The unlink hook that removes the external ids of deleted records: one
/// `DELETE FROM ir_model_data WHERE model = $1 AND res_id = ANY($2)` per `unlink` call, whatever
/// the number of records. It does nothing (no statement) when the registry has no `ir.model.data`
/// or when the deleted model is `ir.model.data` itself.
pub const UNLINK_HOOK: UnlinkHook = UnlinkHook::new(remove_xids_of_deleted);

fn remove_xids_of_deleted<'a>(
    tx: &'a mut Transaction,
    model: &'a str,
    ids: &'a [RecId],
) -> BoxFuture<'a, Result<()>> {
    Box::pin(async move {
        if model == MODEL || ids.is_empty() {
            return Ok(());
        }
        let Some(table) = tx.registry().model(MODEL).map(|spec| spec.table.clone()) else {
            return Ok(());
        };
        let res_ids: Vec<i64> = ids.iter().map(|id| id.get()).collect();
        let sql = format!(
            "DELETE FROM {} WHERE model = $1 AND res_id = ANY($2::int8[]) RETURNING id",
            erp_sql::quote(&table)
        );
        // pending writes to the rows first; the rows go behind the ORM cache, so drop it
        tx.flush().await?;
        let removed = tx.query(&sql, &[&model, &res_ids]).await?;
        if !removed.is_empty() {
            tx.invalidate_model(MODEL, None).await?;
        }
        Ok(())
    })
}

/// An `ir.model.data` row.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct XidRecord {
    /// `ir.model.data` id.
    pub id: i64,
    /// Model the external id points at.
    pub model: String,
    /// Record it points at (`None` when the row has no record).
    pub res_id: Option<i64>,
    /// `noupdate` flag.
    pub noupdate: bool,
}

/// A binding to write with [`update_xmlids`].
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct XidBinding {
    /// The external id.
    pub xid: XmlId,
    /// Target model.
    pub model: String,
    /// Target record (a positive database id).
    pub res_id: i64,
    pub noupdate: bool,
}

impl XidBinding {
    /// Bind `xid` to record `res_id` of `model`.
    pub fn new(xid: XmlId, model: impl Into<String>, res_id: i64, noupdate: bool) -> Self {
        Self {
            xid,
            model: model.into(),
            res_id,
            noupdate,
        }
    }

    /// Bind a qualified `xid` to record `res_id` of `model`.
    pub fn qualified(
        xid: crate::ast::QualifiedXmlId,
        model: impl Into<String>,
        res_id: i64,
        noupdate: bool,
    ) -> Self {
        Self::new(xid.into(), model, res_id, noupdate)
    }

    /// The qualified XML ID, if this binding's ID has a module part.
    pub fn qualified_xid(&self) -> Option<crate::ast::QualifiedXmlId> {
        self.xid.as_qualified()
    }
}

/// A cell of a row, with a `Database` error naming the column when its type is unexpected.
fn get<'a, T: tokio_postgres::types::FromSql<'a>>(row: &'a Row, idx: usize) -> Result<T> {
    row.try_get(idx).map_err(|e| {
        Error::new(
            ErrorKind::Database,
            format!("unexpected value in column {idx} of an ir_model_data statement: {e}"),
        )
    })
}

/// `(module, name)` of an external id as stored: an id without module has module `''` (the
/// column's default).
fn key(xid: &XmlId) -> (String, String) {
    if let Some(q) = xid.as_qualified() {
        (q.module().to_owned(), q.name().to_owned())
    } else {
        (String::new(), xid.name().to_owned())
    }
}

/// The requested ids by stored `(module, name)`, and the two parallel arrays to bind.
#[allow(
    clippy::type_complexity,
    reason = "private helper returning three related views"
)]
fn pairs(xids: &[XmlId]) -> (HashMap<(String, String), XmlId>, Vec<String>, Vec<String>) {
    let by_key: HashMap<(String, String), XmlId> =
        xids.iter().map(|x| (key(x), x.clone())).collect();
    let (modules, names) = by_key.keys().cloned().unzip();
    (by_key, modules, names)
}

/// The requested id of a returned row.
fn requested(
    by_key: &HashMap<(String, String), XmlId>,
    module: String,
    name: String,
) -> Result<XmlId> {
    by_key.get(&(module, name)).cloned().ok_or_else(|| {
        Error::internal("an ir_model_data lookup returned an id that was not asked for")
    })
}

/// Ids without a row are absent from the map.
pub async fn lookup(
    tx: &mut Transaction,
    env: &Env,
    xids: &[XmlId],
) -> Result<HashMap<XmlId, XidRecord>> {
    let mut found = HashMap::new();
    if xids.is_empty() {
        return Ok(found);
    }
    let imd = table(env)?;
    let (by_key, modules, names) = pairs(xids);
    tx.flush().await?;
    let sql = format!(
        "SELECT id, module, name, model, res_id, noupdate FROM {imd} \
         WHERE (module, name) IN (SELECT * FROM unnest($1::text[], $2::text[]))"
    );
    for row in tx.query(&sql, &[&modules, &names]).await? {
        let xid = requested(&by_key, get(&row, 1)?, get(&row, 2)?)?;
        found.insert(
            xid,
            XidRecord {
                id: get(&row, 0)?,
                model: get(&row, 3)?,
                res_id: get(&row, 4)?,
                noupdate: get::<Option<bool>>(&row, 5)?.unwrap_or(false),
            },
        );
    }
    Ok(found)
}

pub async fn xmlid_to_res_id(
    tx: &mut Transaction,
    env: &Env,
    xid: &XmlId,
) -> Result<Option<(String, i64)>> {
    let found = lookup(tx, env, std::slice::from_ref(xid)).await?;
    Ok(found
        .get(xid)
        .and_then(|row| row.res_id.map(|id| (row.model.clone(), id))))
}

/// The
/// same external id twice with different targets is an error; exact repeats are written once.
///
/// The statement `RETURNING`s the rows it inserted or re-pointed; [`update_xmlids_returning`]
/// hands them back and [`update_xmlids_cached`] feeds them to an [`XidCache`].
pub async fn update_xmlids(
    tx: &mut Transaction,
    env: &Env,
    bindings: &[XidBinding],
    update: bool,
) -> Result<()> {
    update_xmlids_returning(tx, env, bindings, update)
        .await
        .map(drop)
}

/// [`update_xmlids`] returning the rows the statement inserted, re-pointed or stamped as written in
/// this transaction (still one statement). A binding that is not in the result left its row as it
/// was: it was already written in this transaction, or the row is `noupdate` and `update` was
/// true.
pub async fn update_xmlids_returning(
    tx: &mut Transaction,
    env: &Env,
    bindings: &[XidBinding],
    update: bool,
) -> Result<Vec<(XmlId, XidRecord)>> {
    if bindings.is_empty() {
        return Ok(Vec::new());
    }
    let quoted = table(env)?;
    let mut seen: HashMap<&XmlId, &XidBinding> = HashMap::new();
    let mut rows = Vec::with_capacity(bindings.len());
    for binding in bindings {
        // a row of module '' would be an id nobody can name (`module.name` is the only form)
        if binding.xid.module().is_none_or(str::is_empty) {
            return Err(Error::programming(format!(
                "external id {} has no module: only qualified ids are stored",
                binding.xid
            )));
        }
        if binding.res_id <= 0 {
            return Err(Error::programming(format!(
                "external id {} bound to invalid record id {}",
                binding.xid, binding.res_id
            )));
        }
        match seen.get(&binding.xid) {
            Some(prev) if prev.model == binding.model && prev.res_id == binding.res_id => continue,
            Some(prev) => {
                return Err(Error::programming(format!(
                    "external id {} bound twice: to {}({}) and {}({})",
                    binding.xid, prev.model, prev.res_id, binding.model, binding.res_id
                )));
            }
            None => {
                seen.insert(&binding.xid, binding);
                rows.push(binding);
            }
        }
    }
    let modules: Vec<&str> = rows.iter().map(|b| b.xid.module().unwrap_or("")).collect();
    let names: Vec<&str> = rows.iter().map(|b| b.xid.name()).collect();
    let models: Vec<&str> = rows.iter().map(|b| b.model.as_str()).collect();
    let res_ids: Vec<i64> = rows.iter().map(|b| b.res_id).collect();
    let flags: Vec<bool> = rows.iter().map(|b| b.noupdate).collect();
    let guard = if update {
        format!(" AND NOT COALESCE({quoted}.noupdate, false)")
    } else {
        String::new()
    };
    let now = erp_sql::NOW_UTC;
    let sql = format!(
        "INSERT INTO {quoted} (module, name, model, res_id, noupdate, create_date, write_date) \
         SELECT m, n, mo, r, nu, {now}, {now} \
         FROM unnest($1::text[], $2::text[], $3::text[], $4::int8[], $5::bool[]) \
         AS t(m, n, mo, r, nu) ORDER BY m, n \
         ON CONFLICT (module, name) DO UPDATE \
         SET (model, res_id, write_date) = (EXCLUDED.model, EXCLUDED.res_id, {now}) \
         WHERE ({quoted}.res_id IS DISTINCT FROM EXCLUDED.res_id \
                OR {quoted}.model <> EXCLUDED.model \
                OR {quoted}.write_date IS DISTINCT FROM {now}){guard} \
         RETURNING id, module, name, model, res_id, noupdate"
    );
    tx.flush().await?;
    let returned = tx
        .query(&sql, &[&modules, &names, &models, &res_ids, &flags])
        .await?;
    let by_key: HashMap<(String, String), XmlId> =
        rows.iter().map(|b| (key(&b.xid), b.xid.clone())).collect();
    let mut out = Vec::with_capacity(returned.len());
    for row in &returned {
        let xid = requested(&by_key, get(row, 1)?, get(row, 2)?)?;
        out.push((
            xid,
            XidRecord {
                id: get(row, 0)?,
                model: get(row, 3)?,
                res_id: get(row, 4)?,
                noupdate: get::<Option<bool>>(row, 5)?.unwrap_or(false),
            },
        ));
    }
    // rows changed behind the ORM: drop cached values of the model
    tx.invalidate_model(MODEL, None).await?;
    Ok(out)
}

/// What a session knows about external ids: `(module, name)` to the stored row. Entries come from
/// lookups ([`lookup_cached`]) and from the rows a write statement returned
/// ([`update_xmlids_cached`]); an id that is absent from the cache is simply unknown, never
/// "missing". Whoever changes `ir.model.data` behind the cache (a hook, a method, a delete) must
/// [`XidCache::clear`] it; the load session does after a `<delete>`, a `<function>` and the
/// end-of-run cleanup. One case stays stale until the next file: an `unlink` that a `<record>`
/// write triggers (an x2many `(2, id)` or `(5,)` command) removes external ids behind the cache.
/// Only the module's code fills the cache
/// (`insert` is crate-private), with rows a statement read or returned.
#[derive(Debug, Default)]
pub struct XidCache {
    entries: HashMap<(String, String), XidRecord>,
}

impl XidCache {
    /// An empty cache.
    pub fn new() -> Self {
        Self::default()
    }

    /// The cached row of `xid`. Returns `None` if `xid` has no module part.
    pub fn get(&self, xid: &XmlId) -> Option<&XidRecord> {
        xid.module()?;
        self.entries.get(&key(xid))
    }

    /// The cached row of qualified `xid`.
    pub fn get_qualified(&self, xid: &crate::ast::QualifiedXmlId) -> Option<&XidRecord> {
        self.entries
            .get(&(xid.module().to_owned(), xid.name().to_owned()))
    }

    /// Remember `row` for `xid` (only rows a statement read or returned).
    pub(crate) fn insert(&mut self, xid: &XmlId, row: XidRecord) {
        self.entries.insert(key(xid), row);
    }

    /// Forget everything.
    pub fn clear(&mut self) {
        self.entries.clear();
    }

    /// Number of cached ids.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether nothing is cached.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// [`lookup`] through a cache: ids in the cache cost no statement, the others one statement
/// (none when all are cached); the rows found are cached. Ids without a row are absent from the
/// result and stay uncached.
pub async fn lookup_cached(
    tx: &mut Transaction,
    env: &Env,
    cache: &mut XidCache,
    xids: &[XmlId],
) -> Result<HashMap<XmlId, XidRecord>> {
    let mut found = HashMap::new();
    let mut missing = Vec::new();
    for xid in xids {
        match cache.get(xid) {
            Some(row) => {
                found.insert(xid.clone(), row.clone());
            }
            None => missing.push(xid.clone()),
        }
    }
    if missing.is_empty() {
        return Ok(found);
    }
    for (xid, row) in lookup(tx, env, &missing).await? {
        cache.insert(&xid, row.clone());
        found.insert(xid, row);
    }
    Ok(found)
}

/// [`update_xmlids`] that keeps `cache` in step, with no further statement: the rows the
/// statement returned replace their entries. A binding that came back unreturned left its row
/// alone (unchanged, or `noupdate` under `update`), so its entry, which was read from that row,
/// stays as it is; an id the cache never saw stays unknown.
pub async fn update_xmlids_cached(
    tx: &mut Transaction,
    env: &Env,
    cache: &mut XidCache,
    bindings: &[XidBinding],
    update: bool,
) -> Result<()> {
    for (xid, row) in update_xmlids_returning(tx, env, bindings, update).await? {
        cache.insert(&xid, row);
    }
    Ok(())
}

/// # Errors
/// `Missing`: `External ID not found in the system: m.none` when there is no row (or it has no
/// record id), `No record found for unique ID m.deleted. It may have been deleted.` when the row
/// points at a record that is gone, or `External ID not found in the system: bare` when the id has
/// no module part. With `raise_if_not_found` false they give `Ok(None)`. A row naming a model that
/// is not in the registry is a `Programming` error, never `None`.
pub async fn env_ref(
    tx: &mut Transaction,
    env: &Env,
    xid: &XmlId,
    raise_if_not_found: bool,
) -> Result<Option<Records>> {
    if xid.module().is_none() {
        return if raise_if_not_found {
            Err(Error::missing(format!(
                "External ID not found in the system: {xid}"
            )))
        } else {
            Ok(None)
        };
    }
    let row = lookup(tx, env, std::slice::from_ref(xid)).await?;
    let Some((model, res_id)) = row
        .get(xid)
        .and_then(|r| r.res_id.map(|id| (r.model.clone(), id)))
    else {
        return if raise_if_not_found {
            Err(Error::missing(format!(
                "External ID not found in the system: {xid}"
            )))
        } else {
            Ok(None)
        };
    };
    let record = env.records(&model)?.browse([res_id])?;
    let record = record.exists(tx).await?;
    if record.is_empty() {
        return if raise_if_not_found {
            Err(Error::missing(format!(
                "No record found for unique ID {xid}. It may have been deleted."
            )))
        } else {
            Ok(None)
        };
    }
    Ok(Some(record))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TargetRow {
    pub(crate) id: i64,
    pub(crate) model: String,
    pub(crate) res_id: Option<i64>,
    pub(crate) noupdate: bool,
    /// Whether the target row exists in the target's table.
    pub(crate) exists: bool,
}

/// [`lookup`] joined (outer) with the table of `target`.
pub(crate) async fn lookup_with_target(
    tx: &mut Transaction,
    target: &Records,
    xids: &[XmlId],
) -> Result<HashMap<XmlId, TargetRow>> {
    let mut found = HashMap::new();
    if xids.is_empty() {
        return Ok(found);
    }
    let imd = table(target.env())?;
    let table = erp_sql::quote(&target.spec().table);
    let (by_key, modules, names) = pairs(xids);
    tx.flush().await?;
    let sql = format!(
        "SELECT d.id, d.module, d.name, d.model, d.res_id, d.noupdate, r.id \
         FROM {imd} d LEFT JOIN {table} r ON d.res_id = r.id \
         WHERE (d.module, d.name) IN (SELECT * FROM unnest($1::text[], $2::text[]))"
    );
    for row in tx.query(&sql, &[&modules, &names]).await? {
        let xid = requested(&by_key, get(&row, 1)?, get(&row, 2)?)?;
        let target_id: Option<i64> = get(&row, 6)?;
        found.insert(
            xid,
            TargetRow {
                id: get(&row, 0)?,
                model: get(&row, 3)?,
                res_id: get(&row, 4)?,
                noupdate: get::<Option<bool>>(&row, 5)?.unwrap_or(false),
                exists: target_id.is_some(),
            },
        );
    }
    Ok(found)
}

pub(crate) async fn xids_to_records(
    tx: &mut Transaction,
    target: &Records,
    xids: &[XmlId],
) -> Result<HashMap<XmlId, (String, i64)>> {
    let mut found = HashMap::new();
    if xids.is_empty() {
        return Ok(found);
    }
    let imd = table(target.env())?;
    let table = erp_sql::quote(&target.spec().table);
    let (by_key, modules, names) = pairs(xids);
    tx.flush().await?;
    let sql = format!(
        "SELECT d.module, d.name, d.model, d.res_id \
         FROM {imd} d JOIN {table} r ON d.res_id = r.id \
         WHERE (d.module, d.name) IN (SELECT * FROM unnest($1::text[], $2::text[]))"
    );
    for row in tx.query(&sql, &[&modules, &names]).await? {
        let xid = requested(&by_key, get(&row, 0)?, get(&row, 1)?)?;
        found.insert(xid, (get(&row, 2)?, get(&row, 3)?));
    }
    Ok(found)
}

/// One record for [`load_records`]: its values, the external id to give it, and the flag a new
/// external id gets.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct LoadRecord {
    xml_id: Option<XmlId>,
    db_id: Option<i64>,
    values: RawVals,
    noupdate: bool,
}

impl LoadRecord {
    /// A record with `values`. `xml_id` is its external id (`None`: the record is created, or
    /// updated when [`LoadRecord::with_db_id`] names it); `noupdate` is the flag of the external
    /// ids created for it.
    pub fn new(xml_id: Option<XmlId>, values: RawVals, noupdate: bool) -> Self {
        Self {
            xml_id,
            db_id: None,
            values,
            noupdate,
        }
    }

    #[must_use]
    pub fn with_db_id(mut self, id: i64) -> Self {
        self.db_id = Some(id);
        self
    }

    /// The external id.
    pub fn xml_id(&self) -> Option<&XmlId> {
        self.xml_id.as_ref()
    }
}

/// With `update` true (a module update), a record whose external id is `noupdate` is
/// left alone, and a record with neither external id nor database id is refused.
///
/// The step list, and so the number of statements, does not depend on the batch size: one lookup
/// of the external ids, one `write` per record to update (cache-only until the flush), one
/// `create`, one upsert of the external ids.
///
/// The context key `import_file` makes creation under the external-id prefix of an existing
/// module an error.
///
/// # Errors
/// `Validation`: `Cannot update a record without specifying its id or xml_id`, and
/// `For external id m.x when trying to create/update a record of model a found record of
/// different model b (n)`; `UserError` for the import-file prefix rule; any ORM error of the
/// writes.
pub async fn load_records(
    tx: &mut Transaction,
    records: &Records,
    batch: Vec<LoadRecord>,
    update: bool,
) -> Result<Records> {
    load_records_with(tx, records, batch, update, None).await
}

/// [`load_records`] that also keeps `cache` in step with the external ids it wrote (the same
/// rows [`update_xmlids_cached`] would cache).
pub async fn load_records_cached(
    tx: &mut Transaction,
    records: &Records,
    batch: Vec<LoadRecord>,
    update: bool,
    cache: &mut XidCache,
) -> Result<Records> {
    load_records_with(tx, records, batch, update, Some(cache)).await
}

async fn load_records_with(
    tx: &mut Transaction,
    records: &Records,
    mut batch: Vec<LoadRecord>,
    update: bool,
    cache: Option<&mut XidCache>,
) -> Result<Records> {
    let spec = records.spec();
    // Validate every output target before an earlier row can update records or external ids.
    // Unadapted outputs stay database-owned. Flagged inputs reach the same checked create/write
    // adapter as direct callers; they never become assignments to generated columns.
    for name in batch.iter().flat_map(|item| {
        item.values
            .iter()
            .map(|(name, _)| name)
            .chain(item.values.iter_commands().map(|(name, _)| name))
    }) {
        if let Some(field) = spec.field(name)
            && field.is_generated()
            && !field.is_input_adapted()
        {
            return Err(Error::validation(format!(
                "Generated field '{}.{}' is read-only",
                spec.name, field.name
            )));
        }
    }
    let xids: Vec<XmlId> = batch.iter().filter_map(|r| r.xml_id.clone()).collect();
    let existing = lookup_with_target(tx, records, &xids).await?;
    let mut targets: Vec<Option<i64>> = vec![None; batch.len()];
    let mut to_create = Vec::new();
    let mut to_update = Vec::new();
    let mut dangling = Vec::new();
    for (k, item) in batch.iter().enumerate() {
        let Some(xid) = &item.xml_id else {
            if let Some(id) = item.db_id {
                targets[k] = Some(id);
                to_update.push(k);
            } else if !update {
                to_create.push(k);
            } else {
                return Err(Error::validation(
                    "Cannot update a record without specifying its id or xml_id",
                ));
            }
            continue;
        };
        match existing.get(xid) {
            None => to_create.push(k),
            Some(row) => {
                if row.model != spec.name {
                    return Err(Error::validation(format!(
                        "For external id {xid} when trying to create/update a record of \
                         model {} found record of different model {} ({})",
                        spec.name, row.model, row.id
                    )));
                }
                match row.res_id.filter(|_| row.exists) {
                    Some(res_id) => {
                        targets[k] = Some(res_id);
                        if !(update && row.noupdate) {
                            to_update.push(k);
                        }
                    }
                    None => {
                        dangling.push(row.id);
                        to_create.push(k);
                    }
                }
            }
        }
    }
    // a tracked batch (the data loader's `ir.ui.view` windows) tells each write which of its
    // records were written before it (`load::load_state`, review A-R1)
    let tracked = batch_names(records, &batch, &to_update, &to_create)?;
    let mut updated_to_check = Vec::new();
    for (written, &k) in to_update.iter().enumerate() {
        let id = targets[k]
            .ok_or_else(|| Error::internal("a record to update has no target id".to_owned()))?;
        let with_progress;
        let target = match &tracked {
            Some((names, positions)) => {
                if crate::load::load_state::CHECKED_VIEW_FIELDS
                    .iter()
                    .any(|f| batch[k].values.contains(f))
                {
                    updated_to_check.push(id);
                }
                with_progress = records.with_context([
                    (crate::load::load_state::BATCH_NAMES, names.clone()),
                    (crate::load::load_state::BATCH_POSITIONS, positions.clone()),
                    (
                        crate::load::load_state::BATCH_WRITTEN,
                        batch_written(written)?,
                    ),
                ]);
                &with_progress
            }
            None => records,
        };
        target
            .browse([id])?
            .write(tx, std::mem::take(&mut batch[k].values))
            .await?;
    }
    // batch-level upgrade check (review D-1): run one combined-arch
    // check per load window over the per-position load state after all updates have been written.
    if let Some((names, positions)) = &tracked
        && !updated_to_check.is_empty()
    {
        updated_to_check.sort_unstable();
        updated_to_check.dedup();
        let with_batch = records.with_context([
            (crate::load::load_state::BATCH_NAMES, names.clone()),
            (crate::load::load_state::BATCH_POSITIONS, positions.clone()),
            (
                crate::load::load_state::BATCH_WRITTEN,
                batch_written(to_update.len())?,
            ),
        ]);
        let to_check = with_batch.browse(updated_to_check)?;
        if !tx.has_method(records.model_id(), crate::load::load_state::CHECK_XML) {
            return Err(Error::invariant(format!(
                "model {} has tracked batch load state but does not implement {}",
                records.spec().name,
                crate::load::load_state::CHECK_XML
            )));
        }
        to_check
            .call::<(), ()>(tx, crate::load::load_state::CHECK_XML, ())
            .await?;
    }
    if !dangling.is_empty() {
        let imd = spec_of_model_data(records)?.name;
        records
            .env()
            .records(imd)?
            .sudo(true)
            .browse(dangling)?
            .unlink(tx)
            .await?;
    }
    if import_file(records.env())? {
        check_module_prefixes(tx, records, &batch, &to_create).await?;
    }
    if to_create.is_empty() {
        return finish_load(records, targets);
    }
    let vals: Vec<RawVals> = to_create
        .iter()
        .map(|&k| std::mem::take(&mut batch[k].values))
        .collect();
    let with_progress;
    let creator = match &tracked {
        Some((names, positions)) => {
            with_progress = records.with_context([
                (crate::load::load_state::BATCH_NAMES, names.clone()),
                (crate::load::load_state::BATCH_POSITIONS, positions.clone()),
                (
                    crate::load::load_state::BATCH_WRITTEN,
                    batch_written(to_update.len())?,
                ),
            ]);
            &with_progress
        }
        None => records,
    };
    let mut inherits_has_link: Vec<Vec<bool>> = Vec::with_capacity(spec.inherits.len());
    for &(_parent, link) in &spec.inherits {
        let link_field = spec.field_by_id(link);
        let has_link: Vec<bool> = vals.iter().map(|v| v.contains(link_field.name)).collect();
        inherits_has_link.push(has_link);
    }
    let created = creator.create(tx, vals).await?;
    let created_ids = created.ids();
    if created_ids.len() != to_create.len() {
        return Err(Error::internal(format!(
            "create returned {} records for {} values",
            created_ids.len(),
            to_create.len()
        )));
    }
    let mut bindings = Vec::new();
    for (&k, &id) in to_create.iter().zip(&created_ids) {
        targets[k] = Some(id);
        if let Some(xid) = &batch[k].xml_id {
            bindings.push(XidBinding::new(
                xid.clone(),
                spec.name,
                id,
                batch[k].noupdate,
            ));
        }
    }
    // `_inherits` parents created along with the records get `<xid>_<parent_model>`
    for (idx, &(parent, link)) in spec.inherits.iter().enumerate() {
        let link_field = spec.field_by_id(link);
        let parent_name = records.env().registry().model_by_id(parent).name;
        let has_link = &inherits_has_link[idx];
        let with_xid: Vec<(usize, usize, &XmlId)> = to_create
            .iter()
            .enumerate()
            .filter(|&(pos, _)| !has_link[pos])
            .filter_map(|(pos, k)| batch[*k].xml_id.as_ref().map(|x| (pos, *k, x)))
            .collect();
        if with_xid.is_empty() {
            continue;
        }
        let parents = created.cells(tx, link_field.name).await?;
        for (pos, k, xid) in with_xid {
            let parent_id = match parents.get(pos) {
                Some(Cell::Id(id)) => *id,
                other => {
                    return Err(Error::internal(format!(
                        "created {}({}) has no {} parent: {other:?}",
                        spec.name, created_ids[pos], parent_name
                    )));
                }
            };
            let name = format!("{}_{}", xid.name(), parent_name.replace('.', "_"));
            let parent_xid = XmlId::qualified(xid.module().unwrap_or(""), &name)
                .map_err(|e| Error::internal(e.to_string()))?;
            bindings.push(XidBinding::new(
                parent_xid,
                parent_name,
                parent_id,
                batch[k].noupdate,
            ));
        }
    }
    match cache {
        Some(cache) => {
            update_xmlids_cached(tx, records.env(), cache, &bindings, update).await?;
        }
        None => update_xmlids(tx, records.env(), &bindings, update).await?,
    }
    finish_load(records, targets)
}

/// The `load_state::BATCH_NAMES` and `load_state::BATCH_POSITIONS` values of a tracked batch
/// (`load_state::TRACK_BATCH` in the context; `None` otherwise), built once for the batch: the
/// names of the records' external ids of the context's `install_module`, sorted, each once, and
/// the write position (the updates, then the creations) of each name's first record.
///
/// # Errors
///
/// `Programming` for a tracked batch without `install_module` text in the context.
fn batch_names(
    records: &Records,
    batch: &[LoadRecord],
    to_update: &[usize],
    to_create: &[usize],
) -> Result<Option<(Value, Value)>> {
    let context = records.env().context();
    if context.get(crate::load::load_state::TRACK_BATCH).is_none() {
        return Ok(None);
    }
    let Some(Value::Str(module)) = context.get("install_module") else {
        return Err(Error::invariant(format!(
            "context['{}'] is set without install_module text",
            crate::load::load_state::TRACK_BATCH
        )));
    };
    // records of other modules, and records without external id, are never looked up by name
    let mut named: Vec<(&str, usize)> = Vec::with_capacity(to_update.len() + to_create.len());
    for (position, &k) in to_update.iter().chain(to_create).enumerate() {
        if let Some(xid) = &batch[k].xml_id
            && xid.module() == Some(&*module)
        {
            named.push((xid.name(), position));
        }
    }
    // by name, then position: the first of a name is its earliest write
    named.sort_unstable();
    named.dedup_by(|later, first| later.0 == first.0);
    // the readers binary-search the names: strictly increasing bytewise (`load_state`)
    debug_assert!(named.windows(2).all(|w| w[0].0 < w[1].0));
    let names: Arc<[Value]> = named.iter().map(|(name, _)| Value::str(name)).collect();
    let mut positions: Vec<Value> = Vec::with_capacity(named.len());
    for (_, position) in &named {
        positions.push(batch_written(*position)?);
    }
    Ok(Some((Value::List(names), Value::List(positions.into()))))
}

fn batch_written(count: usize) -> Result<Value> {
    i64::try_from(count)
        .map(Value::Int)
        .map_err(|_| Error::internal("a batch of more records than i64 counts"))
}

/// The batch's records in order, once every one has a target.
fn finish_load(records: &Records, targets: Vec<Option<i64>>) -> Result<Records> {
    let spec = records.spec();
    let ids: Vec<i64> = targets
        .into_iter()
        .map(|t| {
            t.ok_or_else(|| {
                Error::internal(format!(
                    "a record of '{}' was neither created nor found",
                    spec.name
                ))
            })
        })
        .collect::<Result<_>>()?;
    records.browse(ids)
}

/// A context flag in Python truthiness (`False` is the absent flag), a `Programming` error for a
/// value that is not a boolean.
fn import_file(env: &Env) -> Result<bool> {
    match env.context().get("import_file") {
        None | Some(Value::False) => Ok(false),
        Some(Value::Bool(b)) => Ok(b),
        Some(other) => Err(Error::programming(format!(
            "context key 'import_file' must be a boolean, got {other}"
        ))),
    }
}

/// The modules
/// are the `ir.module.module` records when the registry has that model, else the registry's
/// installed addons.
async fn check_module_prefixes(
    tx: &mut Transaction,
    records: &Records,
    batch: &[LoadRecord],
    to_create: &[usize],
) -> Result<()> {
    let candidates: Vec<&XmlId> = to_create
        .iter()
        .filter(|&&k| !batch[k].noupdate)
        .filter_map(|&k| batch[k].xml_id.as_ref())
        .collect();
    if candidates.is_empty() {
        return Ok(());
    }
    let registry = records.env().registry();
    let installed: HashSet<String> = if registry.model("ir.module.module").is_some() {
        let prefixes: Vec<&str> = candidates.iter().filter_map(|x| x.module()).collect();
        let modules = records.env().records("ir.module.module")?.sudo(true);
        let domain = Domain::condition("name", Operator::In, DValue::list(prefixes))?;
        let found = modules.search(tx, &domain).await?;
        found
            .values(tx, "name")
            .await?
            .into_iter()
            .map(|v| match v {
                Value::Str(s) => Ok(s.to_string()),
                other => Err(Error::programming(format!(
                    "ir.module.module has a module without a name: {other}"
                ))),
            })
            .collect::<Result<_>>()?
    } else {
        registry
            .addons()
            .iter()
            .map(|a| a.name.to_owned())
            .collect()
    };
    // load qualifies every external id, so each has a module
    if let Some(xid) = candidates
        .iter()
        .find(|x| x.module().is_some_and(|m| installed.contains(m)))
    {
        return Err(Error::user(format!(
            "The record {xid} has the module prefix {}. This is the part before the '.' in \
             the external id. Because the prefix refers to an existing module, the record \
             would be deleted when the module is upgraded. Use either no prefix and no dot \
             or a prefix that isn't an existing module. For example, __import__, resulting \
             in the external id __import__.{}.",
            xid.module().unwrap_or(""),
            xid.name()
        )));
    }
    Ok(())
}

/// The model spec of `ir.model.data` (for unlinking dangling rows through the ORM).
pub(crate) fn spec_of_model_data(records: &Records) -> Result<&ModelSpec> {
    table(records.env())?;
    records
        .env()
        .registry()
        .model(MODEL)
        .ok_or_else(|| Error::internal(format!("model '{MODEL}' is not in the registry")))
}

fn random_hex8(seed: i64) -> String {
    let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
    hasher.write_i64(seed);
    // a clock before 1970 only removes one entropy source; the hasher keys stay random
    if let Ok(elapsed) = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
        hasher.write_u128(elapsed.as_nanos());
    }
    format!("{:08x}", hasher.finish() & 0xffff_ffff)
}

/// When a record has several external ids, the oldest one is used.
pub(crate) async fn ensure_xml_ids(tx: &mut Transaction, records: &Records) -> Result<Vec<String>> {
    if records.is_empty() {
        return Ok(Vec::new());
    }
    let imd = table(records.env())?;
    let target = records.spec();
    if !target.has_table() {
        return Err(Error::internal(format!(
            "You can not export the column ID of model {}, because the table {} is not an \
             ordinary table.",
            target.name, target.table
        )));
    }
    let ids = records.ids();
    tx.flush().await?;
    let sql = format!(
        "SELECT res_id, module, name FROM {imd} \
         WHERE model = $1 AND res_id = ANY($2::int8[]) ORDER BY id"
    );
    let mut xids: HashMap<i64, String> = HashMap::new();
    for row in tx.query(&sql, &[&target.name, &ids]).await? {
        let res_id: i64 = get(&row, 0)?;
        let module: String = get(&row, 1)?;
        let name: String = get(&row, 2)?;
        xids.entry(res_id).or_insert_with(|| {
            if module.is_empty() {
                name
            } else {
                format!("{module}.{name}")
            }
        });
    }
    let mut missing = Vec::new();
    let mut done: HashSet<i64> = HashSet::new();
    for &id in &ids {
        if xids.contains_key(&id) || !done.insert(id) {
            continue;
        }
        let xid = XmlId::qualified(
            "__export__",
            &format!("{}_{}_{}", target.table, id, random_hex8(id)),
        )
        .map_err(|e| Error::internal(format!("generated external id: {e}")))?;
        xids.insert(id, xid.to_string());
        missing.push(XidBinding::new(xid, target.name, id, false));
    }
    update_xmlids(tx, records.env(), &missing, false).await?;
    ids.iter()
        .map(|id| {
            xids.get(id).cloned().ok_or_else(|| {
                Error::internal(format!("no external id was made for {}({id})", target.name))
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex8_is_eight_hex_digits() {
        let h = random_hex8(42);
        assert_eq!(h.len(), 8);
        assert!(h.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn qualified_xid_and_cache_lookups() {
        use crate::ast::QualifiedXmlId;

        let q = QualifiedXmlId::parse("test_mod.test_rec").unwrap();
        let binding = XidBinding::qualified(q.clone(), "test.model", 42, false);
        assert_eq!(binding.xid.module(), Some("test_mod"));
        assert_eq!(binding.xid.name(), "test_rec");
        assert_eq!(binding.qualified_xid().unwrap(), q);

        let mut cache = XidCache::new();
        let row = XidRecord {
            id: 1,
            model: "test.model".to_owned(),
            res_id: Some(42),
            noupdate: false,
        };
        cache.insert(&binding.xid, row);
        assert_eq!(cache.get_qualified(&q).unwrap().res_id, Some(42));
        assert_eq!(cache.get(&binding.xid).unwrap().res_id, Some(42));

        let unqual = XmlId::parse("unqualified").unwrap();
        assert!(cache.get(&unqual).is_none());
    }
}
