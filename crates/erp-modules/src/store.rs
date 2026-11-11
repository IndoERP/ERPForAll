//! Reading and writing the module catalog in the database (`ir_module_*` tables).
//!
//! Raw SQL is used because these rows are the module
//! system's own bookkeeping; the record cache of the touched models is invalidated afterwards.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use erp_fields::{Error as OrmError, Result as OrmResult};
use erp_orm::Transaction;

use crate::base_models::{CATEGORY, DEPENDENCY, EXCLUSION, MODULE};
use crate::graph::ModuleState;
use crate::lifecycle::{
    Catalog, CategoryPath, DependencyRow, MAX_CATEGORY_DEPTH, ModuleInfo, ModuleRow, StoredModule,
};
use crate::manifest::ModuleName;
use crate::version::SemverVersion;

const SYSTEM_UID: i64 = 1;

/// Whether the database has the module tables.
pub(crate) async fn is_initialized(tx: &mut Transaction) -> OrmResult<bool> {
    let rows = tx
        .query("SELECT to_regclass('ir_module_module') IS NOT NULL", &[])
        .await?;
    match rows.first() {
        Some(row) => row.try_get::<_, bool>(0).map_err(db_shape),
        None => Err(OrmError::programming("to_regclass returned no row")),
    }
}

/// The name of the connected database (`current_database()`), for messages.
pub(crate) async fn database_name(tx: &mut Transaction) -> OrmResult<String> {
    let rows = tx.query("SELECT current_database()::text", &[]).await?;
    match rows.first() {
        Some(row) => row.try_get::<_, String>(0).map_err(db_shape),
        None => Err(OrmError::programming("current_database() returned no row")),
    }
}

fn db_shape(e: tokio_postgres::Error) -> OrmError {
    OrmError::programming(format!("unexpected column shape in the module tables: {e}"))
}

fn bad_row(module: &str, message: impl std::fmt::Display) -> OrmError {
    OrmError::programming(format!(
        "inconsistent ir_module_module row '{module}': {message}"
    ))
}

/// The `country_codes` column: a JSON array of strings, or NULL for none.
fn country_codes(
    module: &str,
    value: Option<serde_json::Value>,
) -> OrmResult<Vec<crate::CountryCode>> {
    let Some(value) = value else {
        return Ok(Vec::new());
    };
    let not_array = |v: &serde_json::Value| {
        bad_row(
            module,
            format!("country_codes is not a JSON array of strings: {v}"),
        )
    };
    let serde_json::Value::Array(items) = &value else {
        return Err(not_array(&value));
    };
    items
        .iter()
        .map(|item| match item {
            serde_json::Value::String(code) => crate::CountryCode::parse(code)
                .map_err(|e| bad_row(module, format!("country_codes: {e}"))),
            _ => Err(not_array(&value)),
        })
        .collect()
}

/// Parse a stored installed version (`major.minor.patch`, as written by [`save`]).
pub(crate) fn parse_semver(module: &str, raw: &str) -> OrmResult<SemverVersion> {
    let parts: Vec<&str> = raw.split('.').collect();
    let [major, minor, patch] = parts.as_slice() else {
        return Err(bad_row(
            module,
            format!("latest_version '{raw}' is not major.minor.patch"),
        ));
    };
    let num = |p: &str| {
        p.parse::<u32>()
            .map_err(|_| bad_row(module, format!("latest_version '{raw}' is not numeric")))
    };
    Ok(SemverVersion::new(num(major)?, num(minor)?, num(patch)?))
}

const CATEGORY_PATHS: &str = "WITH RECURSIVE cat(id, path, exclusive) AS ( \
       SELECT id, ARRAY[name::text], coalesce(exclusive, false) FROM ir_module_category \
       WHERE parent_id IS NULL \
       UNION ALL \
       SELECT c.id, cat.path || c.name::text, coalesce(c.exclusive, false) \
       FROM ir_module_category c JOIN cat ON c.parent_id = cat.id \
       WHERE cardinality(cat.path) <= $1 \
     ) ";

/// Load the whole catalog (four statements). Rows get no manifest: the caller attaches them.
///
/// # Errors
/// Database errors; rows that cannot be read exactly (`Programming`, naming the row); the
/// catalog's own errors as they are ([`crate::Error::DuplicateModuleRow`], …).
pub(crate) async fn load(tx: &mut Transaction) -> crate::Result<Catalog> {
    let depth = i32::try_from(MAX_CATEGORY_DEPTH)
        .map_err(|_| OrmError::programming("MAX_CATEGORY_DEPTH does not fit i32"))?;
    let modules = tx
        .query(
            &format!(
                "{CATEGORY_PATHS} SELECT m.name, m.state, m.latest_version, \
                 coalesce(m.demo, false), coalesce(m.shortdesc, ''), coalesce(m.summary, ''), \
                 coalesce(m.description, ''), coalesce(m.author, ''), coalesce(m.website, ''), \
                 coalesce(m.license, ''), coalesce(m.sequence, 100), \
                 coalesce(m.application, false), coalesce(m.auto_install, false), \
                 coalesce(m.icon, ''), \
                 m.country_codes, cat.path \
                 FROM ir_module_module m LEFT JOIN cat ON cat.id = m.category_id \
                 ORDER BY m.name"
            ),
            &[&depth],
        )
        .await?;
    let deps = tx
        .query(
            "SELECT m.name, d.name, d.auto_install_required \
             FROM ir_module_module_dependency d JOIN ir_module_module m ON m.id = d.module_id \
             ORDER BY m.name, d.name",
            &[],
        )
        .await?;
    let excls = tx
        .query(
            "SELECT m.name, e.name FROM ir_module_module_exclusion e \
             JOIN ir_module_module m ON m.id = e.module_id ORDER BY m.name, e.name",
            &[],
        )
        .await?;
    let exclusive = tx
        .query(
            &format!("{CATEGORY_PATHS} SELECT path FROM cat WHERE exclusive"),
            &[&depth],
        )
        .await?;

    let mut depends: BTreeMap<String, Vec<DependencyRow>> = BTreeMap::new();
    for row in &deps {
        let module: String = row.try_get(0).map_err(db_shape)?;
        let name: Option<String> = row.try_get(1).map_err(db_shape)?;
        let name = name.ok_or_else(|| bad_row(&module, "a dependency row has no name"))?;
        let name = ModuleName::new(&name)
            .map_err(|e| bad_row(&module, format!("dependency name: {e}")))?;
        // The column defaults to true (`ir_module.py` `auto_install_required`): a NULL is not a
        // value this module system writes, and neither reading is safe.
        let required: Option<bool> = row.try_get(2).map_err(db_shape)?;
        let required = required.ok_or_else(|| {
            bad_row(
                &module,
                format!("dependency '{name}' has no auto_install_required"),
            )
        })?;
        depends
            .entry(module)
            .or_default()
            .push(DependencyRow::new(name, required));
    }
    let mut excludes: BTreeMap<String, BTreeSet<ModuleName>> = BTreeMap::new();
    for row in &excls {
        let module: String = row.try_get(0).map_err(db_shape)?;
        let name: Option<String> = row.try_get(1).map_err(db_shape)?;
        let name = name.ok_or_else(|| bad_row(&module, "an exclusion row has no name"))?;
        let name =
            ModuleName::new(&name).map_err(|e| bad_row(&module, format!("exclusion name: {e}")))?;
        excludes.entry(module).or_default().insert(name);
    }

    let mut rows = Vec::with_capacity(modules.len());
    for row in &modules {
        let raw_name: String = row.try_get(0).map_err(db_shape)?;
        let name =
            ModuleName::new(&raw_name).map_err(|e| bad_row(&raw_name, format!("name: {e}")))?;
        let state: Option<String> = row.try_get(1).map_err(db_shape)?;
        let state = state.ok_or_else(|| bad_row(&raw_name, "state is empty"))?;
        let state = ModuleState::from_db(&state)
            .ok_or_else(|| bad_row(&raw_name, format!("unknown state '{state}'")))?;
        let latest: Option<String> = row.try_get(2).map_err(db_shape)?;
        let latest_version = match latest.as_deref() {
            None | Some("") => None,
            Some(raw) => Some(parse_semver(&raw_name, raw)?),
        };
        let mut info = ModuleInfo::empty();
        info.shortdesc = row.try_get(4).map_err(db_shape)?;
        info.summary = row.try_get(5).map_err(db_shape)?;
        info.description = row.try_get(6).map_err(db_shape)?;
        info.author = row.try_get(7).map_err(db_shape)?;
        info.website = row.try_get(8).map_err(db_shape)?;
        info.license = row.try_get(9).map_err(db_shape)?;
        info.sequence = row.try_get(10).map_err(db_shape)?;
        info.application = row.try_get(11).map_err(db_shape)?;
        info.auto_install = row.try_get(12).map_err(db_shape)?;
        info.icon = row.try_get(13).map_err(db_shape)?;
        let countries: Option<serde_json::Value> = row.try_get(14).map_err(db_shape)?;
        let countries = country_codes(&raw_name, countries)?;
        let path: Option<Vec<String>> = row.try_get(15).map_err(db_shape)?;
        let path = path.ok_or_else(|| bad_row(&raw_name, "no category (or a category cycle)"))?;
        let category = CategoryPath::from_segments(path)
            .map_err(|e| bad_row(&raw_name, format!("category: {e}")))?;
        let mut stored = StoredModule::new(name, state, info, category);
        stored.latest_version = latest_version;
        stored.demo = row.try_get(3).map_err(db_shape)?;
        // A module without dependency or exclusion rows has none (`base`).
        stored.depends = depends.remove(&raw_name).unwrap_or_default();
        stored.excludes = excludes.remove(&raw_name).unwrap_or_default();
        stored.countries = countries.into_iter().collect();
        rows.push(ModuleRow::from_stored(stored).map_err(|e| bad_row(&raw_name, e))?);
    }
    let mut exclusive_categories = Vec::with_capacity(exclusive.len());
    for row in &exclusive {
        let path: Vec<String> = row.try_get(0).map_err(db_shape)?;
        exclusive_categories.push(
            CategoryPath::from_segments(path)
                .map_err(|e| OrmError::programming(format!("ir_module_category: {e}")))?,
        );
    }
    Catalog::from_rows(rows, exclusive_categories)
}

/// A category to find or create: own name, parent external id name, depth.
type Wanted = (String, Option<String>, usize);

/// Ids of the categories of `paths` (and their ancestors), created with their `base.` external
/// ids when missing.
/// One statement to read, then one insert and one external-id insert per missing level.
pub(crate) async fn ensure_categories(
    tx: &mut Transaction,
    paths: impl IntoIterator<Item = &CategoryPath>,
) -> OrmResult<HashMap<String, i64>> {
    // xml id name -> (own name, parent xml id name, depth)
    let mut wanted: BTreeMap<String, Wanted> = BTreeMap::new();
    for path in paths {
        let mut parent: Option<String> = None;
        for (depth, prefix) in path.chain().into_iter().enumerate() {
            let xml = prefix.xml_id_name();
            wanted
                .entry(xml.clone())
                .or_insert_with(|| (prefix.name().to_owned(), parent.clone(), depth));
            parent = Some(xml);
        }
    }
    let names: Vec<String> = wanted.keys().cloned().collect();
    let mut ids: HashMap<String, i64> = HashMap::new();
    for row in tx
        .query(
            "SELECT d.name, d.res_id, c.id IS NOT NULL FROM ir_model_data d \
             LEFT JOIN ir_module_category c ON c.id = d.res_id \
             WHERE d.module = 'base' AND d.model = 'ir.module.category' AND d.name = ANY($1)",
            &[&names],
        )
        .await?
    {
        let name: String = row.try_get(0).map_err(db_shape)?;
        let res_id: Option<i64> = row.try_get(1).map_err(db_shape)?;
        let res_id = res_id.ok_or_else(|| {
            OrmError::programming(format!("external id base.{name} has no record"))
        })?;
        let exists: bool = row.try_get(2).map_err(db_shape)?;
        if !exists {
            return Err(OrmError::programming(format!(
                "external id base.{name} points at ir.module.category {res_id}, which does not exist"
            )));
        }
        ids.insert(name, res_id);
    }
    for level in 0..MAX_CATEGORY_DEPTH {
        let missing: Vec<(&String, &Wanted)> = wanted
            .iter()
            .filter(|(xml, (_, _, depth))| *depth == level && !ids.contains_key(*xml))
            .collect();
        if missing.is_empty() {
            continue;
        }
        let mut xmls: Vec<String> = Vec::with_capacity(missing.len());
        let mut labels: Vec<String> = Vec::with_capacity(missing.len());
        let mut parents: Vec<Option<i64>> = Vec::with_capacity(missing.len());
        for (xml, (label, parent, _)) in missing {
            let parent_id = match parent {
                None => None,
                Some(p) => Some(*ids.get(p).ok_or_else(|| {
                    OrmError::programming(format!("category {p} was not created before {xml}"))
                })?),
            };
            xmls.push(xml.clone());
            labels.push(label.clone());
            parents.push(parent_id);
        }
        let rows = tx
            .query(
                &format!(
                    "INSERT INTO ir_module_category (name, parent_id, visible, exclusive, \
                     create_uid, create_date, write_uid, write_date) \
                     SELECT t.name, t.parent_id, true, false, {SYSTEM_UID}, {now}, {SYSTEM_UID}, \
                     {now} \
                     FROM unnest($1::text[], $2::int8[]) AS t(name, parent_id) \
                     RETURNING id, name, parent_id",
                    now = erp_sql::NOW_UTC
                ),
                &[&labels, &parents],
            )
            .await?;
        // Distinct external ids at one level under one parent have distinct names (the id is
        // derived from the path), so (name, parent) finds each row.
        let mut by_key: HashMap<(String, Option<i64>), i64> = HashMap::new();
        for row in &rows {
            let id: i64 = row.try_get(0).map_err(db_shape)?;
            let name: String = row.try_get(1).map_err(db_shape)?;
            let parent: Option<i64> = row.try_get(2).map_err(db_shape)?;
            by_key.insert((name, parent), id);
        }
        let mut new_ids: Vec<i64> = Vec::with_capacity(xmls.len());
        for (label, parent) in labels.iter().zip(&parents) {
            let id = by_key.get(&(label.clone(), *parent)).ok_or_else(|| {
                OrmError::programming(format!("category '{label}' was not inserted"))
            })?;
            new_ids.push(*id);
        }
        tx.execute(
            "INSERT INTO ir_model_data (module, name, model, res_id, noupdate) \
             SELECT 'base', t.name, 'ir.module.category', t.res_id, true \
             FROM unnest($1::text[], $2::int8[]) AS t(name, res_id)",
            &[&xmls, &new_ids],
        )
        .await?;
        for (xml, id) in xmls.into_iter().zip(new_ids) {
            ids.insert(xml, id);
        }
    }
    Ok(ids)
}

fn category_id(ids: &HashMap<String, i64>, path: &CategoryPath) -> OrmResult<i64> {
    ids.get(&path.xml_id_name()).copied().ok_or_else(|| {
        OrmError::programming(format!("category {} was not created", path.xml_id_name()))
    })
}

/// Columns of the info part of a row, as parallel arrays.
#[derive(Default)]
struct InfoColumns {
    names: Vec<String>,
    shortdesc: Vec<String>,
    summary: Vec<String>,
    description: Vec<String>,
    author: Vec<String>,
    website: Vec<String>,
    license: Vec<String>,
    sequence: Vec<i32>,
    application: Vec<bool>,
    auto_install: Vec<bool>,
    icon: Vec<String>,
    category: Vec<i64>,
    countries: Vec<serde_json::Value>,
}

impl InfoColumns {
    fn push(&mut self, row: &ModuleRow, category: i64) {
        let info = row.info();
        self.names.push(row.name().to_string());
        self.shortdesc.push(info.shortdesc.clone());
        self.summary.push(info.summary.clone());
        self.description.push(info.description.clone());
        self.author.push(info.author.clone());
        self.website.push(info.website.clone());
        self.license.push(info.license.clone());
        self.sequence.push(info.sequence);
        self.application.push(info.application);
        self.auto_install.push(info.auto_install);
        self.icon.push(info.icon.clone());
        self.category.push(category);
        self.countries.push(serde_json::Value::Array(
            row.countries()
                .iter()
                .map(|c| serde_json::Value::String(c.as_str().to_owned()))
                .collect(),
        ));
    }
}

/// Write what changed in the catalog: new rows (with categories and `base.module_<name>`
/// external ids, `noupdate`), changed info and satellite rows, states, installed versions and
/// demo flags. Then the catalog is marked saved.
pub(crate) async fn save(tx: &mut Transaction, catalog: &mut Catalog) -> OrmResult<()> {
    tx.flush().await?;
    let added: Vec<&ModuleRow> = catalog.added().collect();
    let dirty: Vec<&ModuleRow> = catalog.dirty().collect();
    let categories =
        ensure_categories(tx, added.iter().chain(dirty.iter()).map(|r| r.category())).await?;

    if !added.is_empty() {
        let mut cols = InfoColumns::default();
        let mut states: Vec<&str> = Vec::with_capacity(added.len());
        for row in &added {
            cols.push(row, category_id(&categories, row.category())?);
            states.push(row.state().as_str());
        }
        let rows = tx
            .query(
                &format!(
                    "INSERT INTO ir_module_module (name, state, shortdesc, summary, description, \
                     author, website, license, sequence, application, auto_install, icon, \
                     category_id, country_codes, demo, to_buy, create_uid, create_date, \
                     write_uid, write_date) \
                     SELECT t.*, false, false, {SYSTEM_UID}, {now}, {SYSTEM_UID}, {now} \
                     FROM unnest($1::text[], $2::text[], $3::text[], \
                     $4::text[], $5::text[], $6::text[], $7::text[], $8::text[], $9::int4[], \
                     $10::bool[], $11::bool[], $12::text[], $13::int8[], $14::jsonb[]) AS t \
                     RETURNING id, name",
                    now = erp_sql::NOW_UTC
                ),
                &[
                    &cols.names,
                    &states,
                    &cols.shortdesc,
                    &cols.summary,
                    &cols.description,
                    &cols.author,
                    &cols.website,
                    &cols.license,
                    &cols.sequence,
                    &cols.application,
                    &cols.auto_install,
                    &cols.icon,
                    &cols.category,
                    &cols.countries,
                ],
            )
            .await?;
        expect_rows("ir_module_module", RowVerb::Insert, added.len(), rows.len())?;
        let mut xml_names: Vec<String> = Vec::with_capacity(rows.len());
        let mut res_ids: Vec<i64> = Vec::with_capacity(rows.len());
        for row in &rows {
            res_ids.push(row.try_get(0).map_err(db_shape)?);
            let name: String = row.try_get(1).map_err(db_shape)?;
            xml_names.push(format!("module_{name}"));
        }
        let inserted = tx
            .execute(
                "INSERT INTO ir_model_data (module, name, model, res_id, noupdate) \
                 SELECT 'base', t.name, 'ir.module.module', t.res_id, true \
                 FROM unnest($1::text[], $2::int8[]) AS t(name, res_id)",
                &[&xml_names, &res_ids],
            )
            .await?;
        expect_rows("ir_model_data", RowVerb::Insert, xml_names.len(), inserted)?;
    }

    if !dirty.is_empty() {
        let mut cols = InfoColumns::default();
        for row in &dirty {
            cols.push(row, category_id(&categories, row.category())?);
        }
        let updated = tx
            .execute(
                "UPDATE ir_module_module m SET shortdesc = t.shortdesc, summary = t.summary, \
             description = t.description, author = t.author, website = t.website, \
             license = t.license, sequence = t.sequence, application = t.application, \
             auto_install = t.auto_install, icon = t.icon, category_id = t.category_id, \
             country_codes = t.country_codes \
             FROM unnest($1::text[], $2::text[], $3::text[], $4::text[], $5::text[], \
             $6::text[], $7::text[], $8::int4[], $9::bool[], $10::bool[], $11::text[], \
             $12::int8[], $13::jsonb[]) AS t(name, shortdesc, summary, description, author, \
             website, license, sequence, application, auto_install, icon, category_id, \
             country_codes) WHERE m.name = t.name",
                &[
                    &cols.names,
                    &cols.shortdesc,
                    &cols.summary,
                    &cols.description,
                    &cols.author,
                    &cols.website,
                    &cols.license,
                    &cols.sequence,
                    &cols.application,
                    &cols.auto_install,
                    &cols.icon,
                    &cols.category,
                    &cols.countries,
                ],
            )
            .await?;
        expect_rows("ir_module_module", RowVerb::Update, dirty.len(), updated)?;
    }

    let synced: Vec<&ModuleRow> = added.iter().chain(dirty.iter()).copied().collect();
    if !synced.is_empty() {
        let modules: Vec<String> = synced.iter().map(|r| r.name().to_string()).collect();
        let (mut dep_mod, mut dep_name, mut dep_req) = (Vec::new(), Vec::new(), Vec::new());
        let (mut ex_mod, mut ex_name) = (Vec::new(), Vec::new());
        for row in &synced {
            for dep in row.depends() {
                dep_mod.push(row.name().to_string());
                dep_name.push(dep.name().to_string());
                dep_req.push(dep.auto_install_required());
            }
            for ex in row.excludes() {
                ex_mod.push(row.name().to_string());
                ex_name.push(ex.to_string());
            }
        }
        tx.execute(
            "DELETE FROM ir_module_module_dependency WHERE module_id IN \
             (SELECT id FROM ir_module_module WHERE name = ANY($1))",
            &[&modules],
        )
        .await?;
        tx.execute(
            "DELETE FROM ir_module_module_exclusion WHERE module_id IN \
             (SELECT id FROM ir_module_module WHERE name = ANY($1))",
            &[&modules],
        )
        .await?;
        if !dep_mod.is_empty() {
            let inserted = tx
                .execute(
                    "INSERT INTO ir_module_module_dependency (module_id, name, \
                     auto_install_required) \
                     SELECT m.id, t.dep, t.req FROM unnest($1::text[], $2::text[], $3::bool[]) \
                     AS t(module, dep, req) JOIN ir_module_module m ON m.name = t.module",
                    &[&dep_mod, &dep_name, &dep_req],
                )
                .await?;
            expect_rows(
                "ir_module_module_dependency",
                RowVerb::Insert,
                dep_mod.len(),
                inserted,
            )?;
        }
        if !ex_mod.is_empty() {
            let inserted = tx
                .execute(
                    "INSERT INTO ir_module_module_exclusion (module_id, name) \
                     SELECT m.id, t.ex FROM unnest($1::text[], $2::text[]) AS t(module, ex) \
                     JOIN ir_module_module m ON m.name = t.module",
                    &[&ex_mod, &ex_name],
                )
                .await?;
            expect_rows(
                "ir_module_module_exclusion",
                RowVerb::Insert,
                ex_mod.len(),
                inserted,
            )?;
        }
    }

    let changed: Vec<&ModuleRow> = catalog.row_changes().collect();
    if !changed.is_empty() {
        let names: Vec<String> = changed.iter().map(|r| r.name().to_string()).collect();
        let states: Vec<&str> = changed.iter().map(|r| r.state().as_str()).collect();
        let versions: Vec<Option<String>> = changed
            .iter()
            .map(|r| r.latest_version().map(|v| v.to_string()))
            .collect();
        let demo: Vec<bool> = changed.iter().map(|r| r.demo()).collect();
        let updated = tx
            .execute(
                &format!(
                    "UPDATE ir_module_module m SET state = t.state, latest_version = t.version, \
                     demo = t.demo, write_uid = {SYSTEM_UID}, write_date = {now} \
                     FROM unnest($1::text[], $2::text[], $3::text[], $4::bool[]) \
                     AS t(name, state, version, demo) WHERE m.name = t.name",
                    now = erp_sql::NOW_UTC
                ),
                &[&names, &states, &versions, &demo],
            )
            .await?;
        expect_rows("ir_module_module", RowVerb::Update, names.len(), updated)?;
    }
    for model in [
        MODULE,
        CATEGORY,
        DEPENDENCY,
        EXCLUSION,
        erp_data::model_data::MODEL,
    ] {
        tx.invalidate_model(model, None).await?;
    }
    catalog.mark_saved();
    Ok(())
}

#[derive(Clone, Copy)]
enum RowVerb {
    Insert,
    Update,
}

impl RowVerb {
    fn verb(self) -> &'static str {
        match self {
            Self::Insert => "insert",
            Self::Update => "update",
        }
    }

    fn done(self) -> &'static str {
        match self {
            Self::Insert => "inserted",
            Self::Update => "updated",
        }
    }
}

/// A set-based statement must touch exactly one row per catalog row sent: anything else means a
/// row changed or disappeared behind the module system's back.
fn expect_rows(
    table: &str,
    verb: RowVerb,
    expected: usize,
    actual: impl TryInto<u64>,
) -> OrmResult<()> {
    let expected = u64::try_from(expected)
        .map_err(|_| OrmError::programming(format!("{table}: row count does not fit u64")))?;
    let actual: u64 = actual
        .try_into()
        .map_err(|_| OrmError::programming(format!("{table}: row count does not fit u64")))?;
    if expected == actual {
        Ok(())
    } else {
        Err(OrmError::programming(format!(
            "{table}: {expected} row(s) to {}, {actual} {} (a row was changed or removed during \
             the operation)",
            verb.verb(),
            verb.done()
        )))
    }
}

/// Reset transient states (`reset_modules_state`): one statement. Returns the
/// modules reset.
pub(crate) async fn reset_transient_states(tx: &mut Transaction) -> OrmResult<Vec<String>> {
    let rows = tx
        .query(
            &format!(
                "UPDATE ir_module_module SET state = CASE state WHEN 'to install' THEN \
                 'uninstalled' ELSE 'installed' END, write_uid = {SYSTEM_UID}, \
                 write_date = {now} WHERE state IN ('to install', 'to upgrade', 'to remove') \
                 RETURNING name",
                now = erp_sql::NOW_UTC
            ),
            &[],
        )
        .await?;
    let mut names = Vec::with_capacity(rows.len());
    for row in &rows {
        names.push(row.try_get(0).map_err(db_shape)?);
    }
    names.sort();
    Ok(names)
}
