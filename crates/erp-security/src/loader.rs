//! The policy loader (and 4, loader
//! column contract): every policy row of a database, with the
//! `policy` generation, in **five** statements read in one `REPEATABLE READ` snapshot.
//!
//! | # | Statement | Rows |
//! |---|---|---|
//! | 1 | the size of the policy: row counts and bytes of text, checked against the limits of [`crate::rows`] before anything is loaded (database rows are untrusted input) | — |
//! | 2 | groups with their XML ids and implied ids (`array_agg`) | [`GroupRow`], [`Implication`] |
//! | 3 | active `ir.access` rows and archived generated company restrictions, with their model name and XML id, and the generation as a scalar on a one-row driver (so it is read also when there is no rule) | [`RuleRow`], [`PolicyRows::generation`] |
//! | 4 | privileges | [`PrivilegeRow`] |
//! | 5 | `ir_capability` and the field-groups overlay of custom fields (`UNION ALL`) | [`CapabilityRow`], [`FieldGroupsRow`] |
//!
//! The snapshot is the caller's: [`load_rows`] runs the statements on whatever [`RowSource`]
//! it gets. A shared build opens its own `REPEATABLE READ READ ONLY` transaction on a connection
//! of its own ([`load_rows_from`]); a private rebuild (SEC-018) runs inside the writing
//! transaction, which is `REPEATABLE READ` too and sees its own uncommitted rows. Because the
//! generation is read in the same snapshot as the rows, a snapshot is stamped with exactly the
//! changes it contains: a change committed after it adds weight it does not have, so the next
//! transaction reads a larger sum and rebuilds (R-M2).
//!
//! XML ids without a module (`ir_model_data.module = ''`) are not read: a policy names groups and
//! rules by `module.name` only (`has_group`, `erp access disable-rule`), and such ids cannot be
//! written that way.

use erp_fields::{Error, ErrorKind, Result};
use erp_orm::{GroupId, QualifiedXmlId, Transaction};
use erp_registry::Registry;
use tokio_postgres::Row;

use crate::company_access::GeneratedRow;
use crate::rows::{
    CapabilityRow, FieldGroupsRow, GroupRow, Implication, MAX_CAPABILITIES, MAX_FIELD_GROUPS,
    MAX_GROUPS, MAX_IMPLICATIONS, MAX_PRIVILEGES, MAX_RULES, MAX_TOTAL_TEXT_BYTES, PolicyRows,
    PrivilegeId, PrivilegeRow, RowError, RowKind, RuleId, RuleRow, group_id,
};

/// The models (and their tables) the loader reads, with the addon that brings each one.
const SOURCES: [(&str, &str, &str); 7] = [
    ("ir.access", "ir_access", "the base addon (erp_addon_base)"),
    ("ir.model", "ir_model", "the base addon (erp_addon_base)"),
    (
        "ir.model.fields",
        "ir_model_fields",
        "the base addon (erp_addon_base)",
    ),
    (
        "res.groups",
        "res_groups",
        "the base addon (erp_addon_base)",
    ),
    (
        "res.groups.privilege",
        "res_groups_privilege",
        "the base addon (erp_addon_base)",
    ),
    (
        "ir.capability",
        "ir_capability",
        "the base addon (erp_addon_base)",
    ),
    (
        "ir.model.data",
        "ir_model_data",
        "erp_data::model_data::ADDON with the declaration erp_data::IR_MODEL_DATA",
    ),
];

/// Statement 1: how many rows of each kind the next statements read and how many bytes of text
/// they carry, so an oversized policy is refused before it is loaded into memory.
pub(crate) const SIZE_SQL: &str = "\
SELECT (SELECT count(*) FROM ir_access a WHERE a.active OR a.generated IS TRUE)::int8,
       (SELECT count(*) FROM res_groups)::int8,
       (SELECT count(*) FROM res_groups_implied_rel)::int8,
       (SELECT count(*) FROM res_groups_privilege)::int8,
       (SELECT count(*) FROM ir_capability)::int8,
       (SELECT count(*) FROM ir_model_fields f
         WHERE f.state = 'manual'
           AND EXISTS (SELECT FROM ir_model_fields_group_rel r WHERE r.field_id = f.id))::int8,
       (  COALESCE((SELECT sum(octet_length(a.name) + octet_length(m.model)
                               + COALESCE(octet_length(a.domain), 0))
                      FROM ir_access a JOIN ir_model m ON m.id = a.model_id
                     WHERE a.active OR a.generated IS TRUE), 0)
        + COALESCE((SELECT sum(octet_length(g.name)) FROM res_groups g), 0)
        + COALESCE((SELECT sum(octet_length(p.name)) FROM res_groups_privilege p), 0)
        + COALESCE((SELECT sum(octet_length(c.name) + octet_length(c.groups))
                      FROM ir_capability c), 0)
        + COALESCE((SELECT sum(octet_length(d.module) + octet_length(d.name)) FROM ir_model_data d
                     WHERE d.model IN ('res.groups', 'ir.access')), 0)
       )::int8";

/// Statement 2: groups, their XML ids (all of them, oldest first) and the groups they imply.
pub(crate) const GROUPS_SQL: &str = "\
SELECT g.id::int8, g.name, g.privilege_id::int8, g.sequence, g.share, g.api_key_duration,
       x.modules, x.names,
       ARRAY(SELECT r.hid::int8 FROM res_groups_implied_rel r WHERE r.gid = g.id ORDER BY r.hid)
  FROM res_groups g
  LEFT JOIN LATERAL (
       SELECT COALESCE(array_agg(d.module::text ORDER BY d.id), '{}') AS modules,
              COALESCE(array_agg(d.name::text ORDER BY d.id), '{}') AS names
         FROM ir_model_data d
        WHERE d.model = 'res.groups' AND d.res_id = g.id AND d.module <> '') x ON true
 ORDER BY g.id";

/// Statement 3: the `policy` generation (one row even without rules) and the active rules with
/// their model name and oldest XML id.
pub(crate) const RULES_SQL: &str = "\
SELECT gen.generation, a.id::int8, a.name, m.model::text, a.group_id::int8, a.operation::text,
       a.domain::text, x.module, x.name, COALESCE(a.generated, false), COALESCE(a.active, false)
  FROM (SELECT COALESCE(sum(s.weight), 0)::int8 AS generation
          FROM erp_signal_generation s WHERE s.key = 'policy') gen
  LEFT JOIN (ir_access a JOIN ir_model m ON m.id = a.model_id) ON (a.active OR a.generated IS TRUE)
  LEFT JOIN LATERAL (
       SELECT d.module::text AS module, d.name::text AS name
         FROM ir_model_data d
        WHERE d.model = 'ir.access' AND d.res_id = a.id AND d.module <> ''
        ORDER BY d.id LIMIT 1) x ON true
 ORDER BY a.id";

/// Statement 4: privileges.
pub(crate) const PRIVILEGES_SQL: &str = "\
SELECT p.id::int8, p.name, p.sequence FROM res_groups_privilege p ORDER BY p.id";

/// Statement 5: capability mappings and the groups of custom fields, in two typed branches.
pub(crate) const CAPABILITIES_SQL: &str = "\
SELECT 'capability'::text, c.name::text, c.groups::text, NULL::text, NULL::int8[]
  FROM ir_capability c
UNION ALL
SELECT 'field'::text, f.model::text, NULL::text, f.name::text,
       ARRAY(SELECT r.group_id::int8 FROM ir_model_fields_group_rel r
              WHERE r.field_id = f.id ORDER BY 1)
  FROM ir_model_fields f
 WHERE f.state = 'manual'
   AND EXISTS (SELECT FROM ir_model_fields_group_rel r WHERE r.field_id = f.id)";

/// The statements of one load, in the order they run.
pub const STATEMENTS: [&str; 5] = [
    SIZE_SQL,
    GROUPS_SQL,
    RULES_SQL,
    PRIVILEGES_SQL,
    CAPABILITIES_SQL,
];

/// Where the loader reads: a connection or transaction whose snapshot covers every statement.
pub trait RowSource {
    /// Run `sql` (no parameters) and return its rows.
    fn rows(&mut self, sql: &'static str) -> impl Future<Output = Result<Vec<Row>>> + Send;
}

impl RowSource for Transaction {
    async fn rows(&mut self, sql: &'static str) -> Result<Vec<Row>> {
        self.query(sql, &[]).await
    }
}

impl RowSource for tokio_postgres::Transaction<'_> {
    async fn rows(&mut self, sql: &'static str) -> Result<Vec<Row>> {
        self.query(sql, &[]).await.map_err(|e| {
            Error::new(
                ErrorKind::Database,
                format!("the policy loader's statement failed: {e}"),
            )
            .with_sqlstate(e.code().map(|c| c.code().to_owned()))
            .with_source(e)
        })
    }
}

/// Check that `registry` has every model the loader reads, on its standard table.
///
/// # Errors
///
/// `Programming` naming the first missing model and the addon that brings it, or a model whose
/// table is not the one the loader reads.
pub fn check_registry(registry: &Registry) -> Result<()> {
    for (model, table, addon) in SOURCES {
        let Some(spec) = registry.model(model) else {
            return Err(Error::programming(format!(
                "the access policy reads model '{model}', which is not in the registry: add {addon}"
            )));
        };
        if spec.table != table {
            return Err(Error::programming(format!(
                "the access policy reads model '{model}' from table '{table}', but the registry \
                 puts it in '{}'",
                spec.table
            )));
        }
    }
    Ok(())
}

/// Load every policy row through `source`, whose snapshot must cover all five statements
/// ([`STATEMENTS`], run in that order).
///
/// # Errors
///
/// `ResourceExhausted` ([`RowError::TooMany`], [`RowError::TooMuchText`]) when the policy is
/// larger than the limits of [`crate::rows`], before its rows are read; the statement's error;
/// `Database` for a row that does not decode; the row validation errors of [`crate::rows`]
/// (`Programming`) for rows that are inconsistent; `Programming` for a stored XML id that is not
/// `module.name`.
pub async fn load_rows<S: RowSource>(source: &mut S) -> Result<PolicyRows> {
    check_size(&source.rows(SIZE_SQL).await?)?;
    let groups = source.rows(GROUPS_SQL).await?;
    let rules = source.rows(RULES_SQL).await?;
    let privileges = source.rows(PRIVILEGES_SQL).await?;
    let mappings = source.rows(CAPABILITIES_SQL).await?;

    let generation = generation(&rules)?;
    let mut builder = PolicyRows::builder(generation);
    for row in &groups {
        let (group, implied) = group_row(row)?;
        builder = builder.group(group).implications(implied);
    }
    for row in &rules {
        builder = match rule_row(row)? {
            None => builder,
            Some((rule, true)) => builder.rule(rule),
            // an archived row is invisible to the compiler; only a generated one is read
            Some((rule, false)) => {
                builder.archived_generated(GeneratedRow::from_active(&rule).archived())
            }
        };
    }
    for row in &privileges {
        builder = builder.privilege(privilege_row(row)?);
    }
    for row in &mappings {
        builder = match get::<String>(row, 0, "ir_capability")?.as_str() {
            "capability" => builder.capability(capability_row(row)?),
            "field" => builder.field_group(field_groups_row(row)?),
            // `CAPABILITIES_SQL` produces exactly the two kinds above
            other => {
                return Err(Error::programming(format!(
                    "the policy loader read a mapping row of unknown kind {other:?}"
                )));
            }
        };
    }
    Ok(builder.build()?)
}

/// Load every policy row in a `REPEATABLE READ READ ONLY` transaction of its own on `client`
/// (the shared build: never the connection of a waiting request).
///
/// # Errors
///
/// As [`load_rows`]; `Database` when the transaction cannot begin or end.
pub async fn load_rows_from(client: &mut tokio_postgres::Client) -> Result<PolicyRows> {
    let mut tx = client
        .build_transaction()
        .isolation_level(tokio_postgres::IsolationLevel::RepeatableRead)
        .read_only(true)
        .start()
        .await
        .map_err(|e| database(e, "could not begin the policy loader's transaction"))?;
    let rows = load_rows(&mut tx).await;
    // read only: nothing to keep, and a failed load must not leave the transaction open
    let ended = tx.rollback().await;
    match (rows, ended) {
        // the load's own error comes first; a connection that could not even roll back is
        // closed, and the pool drops it
        (Err(error), _) => Err(error),
        (Ok(_), Err(e)) => Err(database(e, "could not end the policy loader's transaction")),
        (Ok(rows), Ok(())) => Ok(rows),
    }
}

/// Refuse a policy larger than the limits of [`crate::rows`] from the counts of [`SIZE_SQL`].
fn check_size(rows: &[Row]) -> Result<()> {
    const T: &str = "policy size";
    let Some(row) = rows.first() else {
        return Err(Error::programming(
            "the policy loader's size statement returned no row",
        ));
    };
    let count = |idx: usize| -> Result<usize> {
        let n: i64 = get(row, idx, T)?;
        usize::try_from(n)
            .map_err(|_| Error::programming(format!("the policy size statement counted {n} rows")))
    };
    for (idx, what, max) in [
        (0, RowKind::Rule, MAX_RULES),
        (1, RowKind::Group, MAX_GROUPS),
        (2, RowKind::Implication, MAX_IMPLICATIONS),
        (3, RowKind::Privilege, MAX_PRIVILEGES),
        (4, RowKind::Capability, MAX_CAPABILITIES),
        (5, RowKind::FieldGroups, MAX_FIELD_GROUPS),
    ] {
        let n = count(idx)?;
        if n > max {
            return Err(RowError::TooMany {
                what,
                count: n,
                max,
            }
            .into());
        }
    }
    let bytes = count(6)?;
    if bytes > MAX_TOTAL_TEXT_BYTES {
        return Err(RowError::TooMuchText {
            bytes,
            max: MAX_TOTAL_TEXT_BYTES,
        }
        .into());
    }
    Ok(())
}

fn database(e: tokio_postgres::Error, what: &str) -> Error {
    Error::new(ErrorKind::Database, format!("{what}: {e}"))
        .with_sqlstate(e.code().map(|c| c.code().to_owned()))
        .with_source(e)
}

/// Column `idx` of a row of `table`, decoded.
fn get<'r, T: tokio_postgres::types::FromSql<'r>>(
    row: &'r Row,
    idx: usize,
    table: &str,
) -> Result<T> {
    row.try_get(idx).map_err(|e| {
        Error::new(
            ErrorKind::Database,
            format!("could not decode column {idx} of a {table} row of the access policy: {e}"),
        )
        .with_source(e)
    })
}

fn xmlid(module: &str, name: &str) -> Result<QualifiedXmlId> {
    QualifiedXmlId::parse(&format!("{module}.{name}")).map_err(|e| {
        Error::programming(format!(
            "the access policy cannot use the external id '{module}.{name}': {e}"
        ))
    })
}

fn generation(rules: &[Row]) -> Result<u64> {
    let Some(first) = rules.first() else {
        return Err(Error::programming(
            "the policy loader's rule statement returned no row (it always returns the \
             generation)",
        ));
    };
    let sum: i64 = get(first, 0, "erp_signal_generation")?;
    u64::try_from(sum).map_err(|_| {
        Error::programming(format!(
            "the policy generation read by the loader is negative ({sum})"
        ))
    })
}

fn group_row(row: &Row) -> Result<(GroupRow, Vec<Implication>)> {
    const T: &str = "res_groups";
    let id = group_id(get(row, 0, T)?)?;
    let name: String = get(row, 1, T)?;
    let mut group = GroupRow::new(id, &name)?;
    if let Some(privilege) = get::<Option<i64>>(row, 2, T)? {
        group = group.with_privilege(PrivilegeId::new(privilege)?);
    }
    if let Some(sequence) = get::<Option<i32>>(row, 3, T)? {
        group = group.with_sequence(sequence);
    }
    // a boolean column holds NULL until written; the ORM reads NULL as False (FIELD spec)
    if get::<Option<bool>>(row, 4, T)? == Some(true) {
        group = group.with_share(true);
    }
    if let Some(days) = get::<Option<f64>>(row, 5, T)? {
        group = group.with_api_key_duration(days)?;
    }
    let modules: Vec<String> = get(row, 6, T)?;
    let names: Vec<String> = get(row, 7, T)?;
    if modules.len() != names.len() {
        return Err(Error::programming(format!(
            "the policy loader read {} modules but {} names for the external ids of group {id}",
            modules.len(),
            names.len()
        )));
    }
    for (module, name) in modules.iter().zip(&names) {
        group = group.with_xmlid(xmlid(module, name)?);
    }
    let implied: Vec<i64> = get(row, 8, T)?;
    let implied = implied
        .into_iter()
        .map(|hid| group_id(hid).map(|hid| Implication::new(id, hid)))
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok((group, implied))
}

/// `None` for the driver row of a database without rules; else the row and whether it is active.
/// An archived row is read only when it is a generated company restriction.
fn rule_row(row: &Row) -> Result<Option<(RuleRow, bool)>> {
    const T: &str = "ir_access";
    let Some(id) = get::<Option<i64>>(row, 1, T)? else {
        return Ok(None);
    };
    let id = RuleId::new(id)?;
    let name: String = get(row, 2, T)?;
    let model: String = get(row, 3, T)?;
    let operation: String = get(row, 5, T)?;
    let mut rule = RuleRow::new(id, &name, &model, &operation)?;
    let generated: bool = get(row, 9, T)?;
    let active: bool = get(row, 10, T)?;
    if generated {
        rule = rule.with_generated();
    } else if !active {
        return Err(Error::programming(format!(
            "the policy loader read the archived rule {id}, which is not a generated restriction"
        )));
    }
    if let Some(group) = get::<Option<i64>>(row, 4, T)? {
        rule = rule.with_group(group_id(group)?);
    }
    if let Some(domain) = get::<Option<String>>(row, 6, T)? {
        rule = rule.with_domain(&domain);
    }
    let module: Option<String> = get(row, 7, T)?;
    let xml_name: Option<String> = get(row, 8, T)?;
    match (module, xml_name) {
        (Some(module), Some(xml_name)) => rule = rule.with_xmlid(xmlid(&module, &xml_name)?),
        (None, None) => {}
        _ => {
            return Err(Error::programming(format!(
                "the policy loader read half an external id for rule {id}"
            )));
        }
    }
    Ok(Some((rule, active)))
}

fn privilege_row(row: &Row) -> Result<PrivilegeRow> {
    const T: &str = "res_groups_privilege";
    let id = PrivilegeId::new(get(row, 0, T)?)?;
    let name: String = get(row, 1, T)?;
    let mut privilege = PrivilegeRow::new(id, &name)?;
    if let Some(sequence) = get::<Option<i32>>(row, 2, T)? {
        privilege = privilege.with_sequence(sequence);
    }
    Ok(privilege)
}

fn capability_row(row: &Row) -> Result<CapabilityRow> {
    const T: &str = "ir_capability";
    let name: String = get(row, 1, T)?;
    let groups: String = get(row, 2, T)?;
    Ok(CapabilityRow::new(&name, &groups)?)
}

fn field_groups_row(row: &Row) -> Result<FieldGroupsRow> {
    const T: &str = "ir_model_fields";
    let model: String = get(row, 1, T)?;
    let field: String = get(row, 3, T)?;
    let groups: Vec<i64> = get(row, 4, T)?;
    let groups = groups
        .into_iter()
        .map(group_id)
        .collect::<std::result::Result<Vec<GroupId>, _>>()?;
    Ok(FieldGroupsRow::new(&model, &field, groups)?)
}
