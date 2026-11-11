//! An action and its children, read in one statement and checked before anything runs.

use erp_sdk::access::GroupId;
use erp_sdk::prelude::*;
use erp_sdk::{FieldType, Registry};

use super::error::excerpt;
use super::{
    MAX_ACTIONS, MAX_DEPTH, MAX_FIELD_BYTES, MAX_GROUPS, MAX_TREE_BYTES, RunLimit,
    ServerActionError,
};
use crate::ir_actions::{ActionId, ident};

/// `evaluation_type`: how `object_write` reads `value`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EvaluationType {
    /// The text as given, converted by the field's type.
    Value,
    /// The next number of a sequence.
    Sequence,
    /// A computed value.
    Equation,
}

/// `update_m2m_operation`: what `object_write` does to an x2many field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum M2mOperation {
    /// Link the record.
    Add,
    /// Unlink the record.
    Remove,
    /// Replace the links by the record.
    Set,
    /// Remove every link.
    Clear,
}

/// A stored selection value: absent, one of the selection, or text the selection does not have
/// (a row written outside the ORM), kept to be refused, with its reason, before anything runs.
fn parse_choice<T: Copy>(
    stored: Option<String>,
    what: &str,
    choices: &[(&str, T)],
) -> Result<Option<T>, String> {
    let Some(text) = stored else {
        return Ok(None);
    };
    if let Some((_, value)) = choices.iter().find(|(key, _)| *key == text) {
        return Ok(Some(*value));
    }
    let names: Vec<&str> = choices.iter().map(|(key, _)| *key).collect();
    Err(format!(
        "its {what} is '{}', not one of {}",
        excerpt(&text),
        names.join(", ")
    ))
}

/// The parsed value, or `None` with the first reason kept in `defect`.
fn kept<T>(defect: &mut Option<String>, parsed: Result<Option<T>, String>) -> Option<T> {
    match parsed {
        Ok(value) => value,
        Err(reason) => {
            if defect.is_none() {
                *defect = Some(reason);
            }
            None
        }
    }
}

/// A column of `table` that does not read as its type: the driver's error as the source.
pub(crate) fn column_error(
    table: &str,
    what: &str,
    error: impl std::error::Error + Send + Sync + 'static,
) -> Error {
    Error::db(format!("unexpected value in {table}.{what}: {error}")).with_source(error)
}

/// One server action as a run sees it (read as stored; the run never reads it again).
#[derive(Debug, Clone, PartialEq)]
pub struct ServerAction {
    id: ActionId,
    name: String,
    state: String,
    /// The model it runs on, as the registry names it.
    model: &'static str,
    pub(crate) groups: Box<[GroupId]>,
    pub(crate) parent: Option<ActionId>,
    /// `crud_model_id`'s model name.
    pub(crate) crud_model: Option<String>,
    pub(crate) update_path: Option<String>,
    /// `update_field_id`'s `(model, name)`.
    pub(crate) update_field: Option<(String, String)>,
    value: Option<String>,
    pub(crate) code: Option<String>,
    pub(crate) evaluation_type: Option<EvaluationType>,
    pub(crate) m2m_operation: Option<M2mOperation>,
    /// `update_boolean_value == 'true'`; absent is `None`.
    pub(crate) boolean_value: Option<bool>,
    pub(crate) run_as_superuser: bool,
    /// `link_field_id`'s `(model, name)`.
    pub(crate) link_field: Option<(String, String)>,
    /// Who last wrote the action, which is who set a `run_as_superuser` flag (audit evidence).
    pub(crate) write_uid: Option<i64>,
    /// When it was last written, microseconds since the Unix epoch UTC.
    pub(crate) write_date_utc_micros: Option<i64>,
}

impl ServerAction {
    /// The action's id.
    pub fn id(&self) -> ActionId {
        self.id
    }

    /// Its name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Its type (`state`).
    pub fn state(&self) -> &str {
        &self.state
    }

    /// The model it runs on.
    pub fn model(&self) -> &'static str {
        self.model
    }

    /// Its `value`, as stored.
    pub fn value(&self) -> Option<&str> {
        self.value.as_deref()
    }

    /// `name` for messages.
    pub(crate) fn misconfigured(&self, reason: impl Into<String>) -> ServerActionError {
        ServerActionError::Misconfigured {
            name: self.name.clone(),
            reason: reason.into(),
        }
    }
}

/// One action of the tree.
#[derive(Debug)]
pub(crate) struct Node {
    pub(crate) action: ServerAction,
    /// Children, in `(sequence, name, id)` order (`ir.actions.server`'s `_order`).
    pub(crate) children: Vec<usize>,
    /// The action or a child has warnings.
    pub(crate) warning: bool,
    /// A stored value the run cannot use (`Misconfigured`), refused before any action runs.
    pub(crate) defect: Option<ServerActionError>,
}

/// The action asked for (node 0) and every action below it.
#[derive(Debug)]
pub(crate) struct Tree {
    pub(crate) nodes: Vec<Node>,
}

/// The table and column names the statement uses, from the registry.
struct Names {
    server: String,
    groups_rel: String,
    groups_col1: String,
    groups_col2: String,
    model: String,
    fields: String,
}

fn names(registry: &Registry) -> Result<Names> {
    let spec = registry
        .model("ir.actions.server")
        .ok_or_else(|| Error::programming("the registry has no model 'ir.actions.server'"))?;
    let Some(FieldType::Many2many {
        relation: Some(relation),
        ..
    }) = spec.field("group_ids").map(|f| &f.ty)
    else {
        return Err(Error::programming(
            "ir.actions.server.group_ids is not a stored many2many",
        ));
    };
    let table = |model: &str| {
        registry
            .model(model)
            .map(|m| m.table.clone())
            .ok_or_else(|| Error::programming(format!("the registry has no model '{model}'")))
    };
    Ok(Names {
        server: spec.table.clone(),
        groups_rel: relation.table.clone(),
        groups_col1: relation.column1.clone(),
        groups_col2: relation.column2.clone(),
        model: table("ir.model")?,
        fields: table("ir.model.fields")?,
    })
}

/// The statement reading the tree: the actions down to level `MAX_DEPTH + 1` (one level more
/// than allowed, to see an excess), at most `MAX_ACTIONS + 1` rows (a loop written outside the
/// ORM stops on either bound), each with its model, groups and the fields it names; ordered so a
/// parent's children come in `(sequence, name, id)` order.
///
/// `bounded` has `LIMIT` without `ORDER BY` on purpose: which rows it keeps does not matter,
/// because reaching the limit refuses the run ([`RunLimit::Actions`]) and below it every row is
/// kept; the final `ORDER BY` orders what is read.
fn statement(n: &Names) -> String {
    let server = ident(&n.server);
    let model = ident(&n.model);
    let fields = ident(&n.fields);
    // Count only bounded scalar metadata before projecting any variable payload. Each text
    // column selected below appears here too; octet lengths are widened before summing.
    // The group probe stops at cap + 1. Its accepted set is then read in full in this same
    // statement/snapshot, so no truncated authorization set can ever be used.
    let text_columns = [
        "a.name",
        "a.state",
        "m.model",
        "cm.model",
        "a.update_path",
        "uf.model",
        "uf.name",
        "a.value",
        "a.evaluation_type",
        "a.update_m2m_operation",
        "a.update_boolean_value",
        "lf.model",
        "lf.name",
        "a.code",
    ];
    let sizes = text_columns
        .iter()
        .map(|column| format!("(COALESCE(octet_length({column}), 0)::int8)"))
        .collect::<Vec<_>>()
        .join(", ");
    let gated = |column: &str| format!("CASE WHEN budget.excess = 0 THEN {column} END");
    let text = text_columns.map(gated);
    let joins = format!(
        "JOIN {server} a ON a.id = b.id \
         LEFT JOIN {model} m ON m.id = a.model_id \
         LEFT JOIN {model} cm ON cm.id = a.crud_model_id \
         LEFT JOIN {fields} uf ON uf.id = a.update_field_id \
         LEFT JOIN {fields} lf ON lf.id = a.link_field_id"
    );
    format!(
        "WITH RECURSIVE tree(id, depth) AS ( \
             SELECT a.id, 1::int8 FROM {server} a WHERE a.id = $1 \
           UNION ALL \
             SELECT c.id, t.depth + 1 FROM tree t JOIN {server} c ON c.parent_id = t.id \
             WHERE t.depth <= $2 \
         ), bounded AS MATERIALIZED (SELECT id, depth FROM tree LIMIT $3), \
         measured AS MATERIALIZED ( \
             SELECT b.id, b.depth, sizes.field_bytes, sizes.text_bytes, \
                    (SELECT count(*) FROM (SELECT 1 FROM {rel} r WHERE r.{c1} = a.id \
                                           LIMIT ($5::int8 + 1)) limited_groups) AS group_count \
               FROM bounded b {joins} \
               CROSS JOIN LATERAL (SELECT max(bytes) AS field_bytes, sum(bytes)::int8 AS text_bytes \
                                     FROM (VALUES {sizes}) field_sizes(bytes)) sizes \
         ), budget AS MATERIALIZED ( \
             SELECT CASE WHEN max(field_bytes) > $4 THEN 1 \
                         WHEN max(group_count) > $5 THEN 2 \
                         WHEN sum(text_bytes + group_count * 8) > $6::int8 THEN 3 \
                         ELSE 0 END AS excess FROM measured \
         ) \
         SELECT a.id, b.depth, a.parent_id, {name}, {state}, {model_name}, \
                CASE WHEN budget.excess = 0 THEN \
                    ARRAY(SELECT r.{c2}::int8 FROM {rel} r WHERE r.{c1} = a.id ORDER BY 1) END, \
                {crud_model}, {update_path}, {uf_model}, {uf_name}, {value}, {evaluation_type}, \
                {m2m_operation}, {boolean_value}, a.run_as_superuser, \
                {lf_model}, {lf_name}, budget.excess, {code}, a.write_uid::int8, \
                (floor(extract(epoch FROM a.write_date) * 1000000))::int8 \
           FROM measured b CROSS JOIN budget {joins} \
          ORDER BY b.depth, a.parent_id, a.sequence, {name}, a.id",
        rel = ident(&n.groups_rel),
        c1 = ident(&n.groups_col1),
        c2 = ident(&n.groups_col2),
        name = text[0],
        state = text[1],
        model_name = text[2],
        crud_model = text[3],
        update_path = text[4],
        uf_model = text[5],
        uf_name = text[6],
        value = text[7],
        evaluation_type = text[8],
        m2m_operation = text[9],
        boolean_value = text[10],
        lf_model = text[11],
        lf_name = text[12],
        code = text[13],
    )
}

/// One row read.
struct Row {
    depth: i64,
    action: ServerAction,
    defect: Option<ServerActionError>,
}

impl Tree {
    /// Read the action `id` and its children: one statement.
    pub(crate) async fn fetch(tx: &mut Transaction, id: ActionId) -> Result<Self> {
        let registry = Arc::clone(tx.env().registry());
        let n = names(&registry)?;
        // pending action writes of this transaction must be in the tables first
        tx.flush().await?;
        let depth = i64::try_from(MAX_DEPTH).map_err(|_| Error::programming("MAX_DEPTH"))?;
        let limit =
            i64::try_from(MAX_ACTIONS + 1).map_err(|_| Error::programming("MAX_ACTIONS"))?;
        let field_bytes =
            i64::try_from(MAX_FIELD_BYTES).map_err(|_| Error::programming("MAX_FIELD_BYTES"))?;
        let groups = i64::try_from(MAX_GROUPS).map_err(|_| Error::programming("MAX_GROUPS"))?;
        let tree_bytes =
            i64::try_from(MAX_TREE_BYTES).map_err(|_| Error::programming("MAX_TREE_BYTES"))?;
        let rows = tx
            .query(
                &statement(&n),
                &[
                    &id.get(),
                    &depth,
                    &limit,
                    &field_bytes,
                    &groups,
                    &tree_bytes,
                ],
            )
            .await?;
        if rows.is_empty() {
            return Err(ServerActionError::NotFound { id }.into());
        }
        if rows.len() > MAX_ACTIONS {
            return Err(ServerActionError::Limit {
                kind: RunLimit::Actions,
            }
            .into());
        }
        let mut read = Vec::with_capacity(rows.len());
        for row in &rows {
            let bad = |what: &str, error| column_error(&n.server, what, error);
            // On excess every variable-sized projection is NULL. Inspect the fixed status
            // before decoding payload, and never quote a stored value in this pre-access error.
            let excess: i32 = row.try_get(18).map_err(|e| bad("payload budget", e))?;
            let kind = match excess {
                0 => None,
                1 => Some(RunLimit::FieldBytes),
                2 => Some(RunLimit::Groups),
                3 => Some(RunLimit::TreeBytes),
                other => {
                    return Err(Error::db(format!(
                        "unexpected action payload budget status {other}"
                    )));
                }
            };
            if let Some(kind) = kind {
                return Err(ServerActionError::Limit { kind }.into());
            }
            let id: i64 = row.try_get(0).map_err(|e| bad("id", e))?;
            let depth: i64 = row.try_get(1).map_err(|e| bad("depth", e))?;
            let parent: Option<i64> = row.try_get(2).map_err(|e| bad("parent_id", e))?;
            let name: String = row.try_get(3).map_err(|e| bad("name", e))?;
            let state: String = row.try_get(4).map_err(|e| bad("state", e))?;
            let model: Option<String> = row.try_get(5).map_err(|e| bad("model_id", e))?;
            let groups: Vec<i64> = row.try_get(6).map_err(|e| bad("group_ids", e))?;
            let crud_model: Option<String> = row.try_get(7).map_err(|e| bad("crud_model_id", e))?;
            let update_path: Option<String> = row.try_get(8).map_err(|e| bad("update_path", e))?;
            let uf_model: Option<String> = row.try_get(9).map_err(|e| bad("update_field_id", e))?;
            let uf_name: Option<String> = row.try_get(10).map_err(|e| bad("update_field_id", e))?;
            let value: Option<String> = row.try_get(11).map_err(|e| bad("value", e))?;
            let evaluation_type: Option<String> =
                row.try_get(12).map_err(|e| bad("evaluation_type", e))?;
            let m2m_operation: Option<String> = row
                .try_get(13)
                .map_err(|e| bad("update_m2m_operation", e))?;
            let boolean_value: Option<String> = row
                .try_get(14)
                .map_err(|e| bad("update_boolean_value", e))?;
            let run_as_superuser: Option<bool> =
                row.try_get(15).map_err(|e| bad("run_as_superuser", e))?;
            let lf_model: Option<String> = row.try_get(16).map_err(|e| bad("link_field_id", e))?;
            let lf_name: Option<String> = row.try_get(17).map_err(|e| bad("link_field_id", e))?;
            let code: Option<String> = row.try_get(19).map_err(|e| bad("code", e))?;
            let write_uid: Option<i64> = row.try_get(20).map_err(|e| bad("write_uid", e))?;
            let write_date_utc_micros: Option<i64> =
                row.try_get(21).map_err(|e| bad("write_date", e))?;
            // ids come from the table's own key: a value that is no id is a broken table
            let not_an_id = |what: &str, value: i64| {
                Error::db(format!(
                    "unexpected value in {}.{what}: {value} is not an id",
                    n.server
                ))
            };
            let id = ActionId::new(id).map_err(|_| not_an_id("id", id))?;
            let parent = parent
                .map(|p| ActionId::new(p).map_err(|_| not_an_id("parent_id", p)))
                .transpose()?;
            let misconfigured = |reason: String| ServerActionError::Misconfigured {
                name: name.clone(),
                reason,
            };
            let model = match model {
                Some(model) => registry
                    .model(&model)
                    .map(|spec| spec.name)
                    .ok_or_else(|| {
                        misconfigured(format!(
                            "its model '{}' is not a model of this registry",
                            excerpt(&model)
                        ))
                    })?,
                None => return Err(misconfigured("it has no model".to_owned()).into()),
            };
            let groups = groups
                .into_iter()
                .map(|g| GroupId::new(g).map_err(|_| not_an_id("group_ids", g)))
                .collect::<Result<Box<[GroupId]>>>()?;
            let pair = |m: Option<String>, f: Option<String>| m.zip(f);
            // stored selections parsed once; a value outside them is refused before the run
            let mut defect = None;
            let evaluation_type = kept(
                &mut defect,
                parse_choice(
                    evaluation_type,
                    "value type",
                    &[
                        ("value", EvaluationType::Value),
                        ("sequence", EvaluationType::Sequence),
                        ("equation", EvaluationType::Equation),
                    ],
                ),
            );
            let m2m_operation = kept(
                &mut defect,
                parse_choice(
                    m2m_operation,
                    "many2many operation",
                    &[
                        ("add", M2mOperation::Add),
                        ("remove", M2mOperation::Remove),
                        ("set", M2mOperation::Set),
                        ("clear", M2mOperation::Clear),
                    ],
                ),
            );
            let boolean_value = kept(
                &mut defect,
                parse_choice(
                    boolean_value,
                    "boolean value",
                    &[("true", true), ("false", false)],
                ),
            );
            let defect = defect.map(misconfigured);
            read.push(Row {
                depth,
                defect,
                action: ServerAction {
                    id,
                    name,
                    state,
                    model,
                    groups,
                    parent,
                    crud_model,
                    update_path,
                    update_field: pair(uf_model, uf_name),
                    value,
                    code,
                    evaluation_type,
                    m2m_operation,
                    boolean_value,
                    // a NULL boolean column is false, as the ORM reads it
                    run_as_superuser: run_as_superuser == Some(true),
                    link_field: pair(lf_model, lf_name),
                    write_uid,
                    write_date_utc_micros,
                },
            });
        }
        if read
            .iter()
            .any(|r| usize::try_from(r.depth).map_or(true, |d| d > MAX_DEPTH))
        {
            return Err(ServerActionError::Limit {
                kind: RunLimit::Depth,
            }
            .into());
        }
        Self::build(&registry, read)
    }

    /// Link the rows (root first, then by level) into nodes and compute the warnings.
    fn build(registry: &Registry, rows: Vec<Row>) -> Result<Self> {
        let mut nodes: Vec<Node> = Vec::with_capacity(rows.len());
        let mut ids: Vec<ActionId> = Vec::with_capacity(rows.len());
        for (index, row) in rows.into_iter().enumerate() {
            let id = row.action.id;
            if ids.contains(&id) {
                // only a loop repeats an action, and a loop exceeds the depth bound first
                return Err(ServerActionError::Limit {
                    kind: RunLimit::Depth,
                }
                .into());
            }
            if index > 0 {
                let parent = row
                    .action
                    .parent
                    .and_then(|p| ids.iter().position(|i| *i == p))
                    .ok_or_else(|| {
                        Error::programming(format!(
                            "server action {id} was read as a child, but its parent was not read \
                             before it"
                        ))
                    })?;
                nodes[parent].children.push(index);
            }
            ids.push(id);
            nodes.push(Node {
                action: row.action,
                children: Vec::new(),
                warning: false,
                defect: row.defect,
            });
        }
        // children come after their parent: compute bottom-up
        for index in (0..nodes.len()).rev() {
            let warning = own_warning(registry, &nodes, index);
            nodes[index].warning = warning;
        }
        Ok(Self { nodes })
    }
}

/// The webhook payload warning is not computed: `webhook` does
/// not run before.
fn own_warning(registry: &Registry, nodes: &[Node], index: usize) -> bool {
    let node = &nodes[index];
    let action = &node.action;
    let children = node.children.iter().map(|c| &nodes[*c]);
    let sorted = |g: &[GroupId]| {
        let mut g = g.to_vec();
        g.sort_unstable();
        g
    };
    let own_groups = sorted(&action.groups);
    for child in children {
        if child.action.model != action.model
            || (!own_groups.is_empty() && sorted(&child.action.groups) != own_groups)
            || child.warning
        {
            return true;
        }
    }
    // the last field of the path is JSON (`:765-766`); a path that does not resolve has no such
    // warning, and the runner refuses it with the reason (`object_write`)
    if let Some(path) = &action.update_path
        && let Ok((model, field)) = super::runners::resolve_path(registry, action, path)
        && let Some(spec) = registry.model(model).and_then(|m| m.field(field))
    {
        if matches!(spec.ty, FieldType::Json) {
            return true;
        }
        // a sequence fills text fields only (`:768-769`)
        if action.state == "object_write"
            && action.evaluation_type == Some(EvaluationType::Sequence)
            && !matches!(spec.ty, FieldType::Char { .. } | FieldType::Text)
        {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "tests")]

    use super::{MAX_FIELD_BYTES, MAX_GROUPS, MAX_TREE_BYTES, Names, statement};

    /// Assert the actual SQL result, before Tree::fetch can decode or reject it. A Rust-only
    /// payload check would pass the behavioral refusal tests while failing this wire guard.
    #[tokio::test]
    async fn oversized_payload_is_null_in_sql_result() {
        let db = erp_testkit::TestDb::create().await.unwrap();
        let client = db.client().await.unwrap();
        client
            .batch_execute(
                "CREATE TABLE payload_action (
                id int8 PRIMARY KEY, parent_id int8, sequence int4,
                name text, state text, model_id int8, crud_model_id int8,
                update_path text, update_field_id int8, value text,
                evaluation_type text, update_m2m_operation text, update_boolean_value text,
                run_as_superuser bool, link_field_id int8, code text,
                write_uid int8, write_date timestamp);
             CREATE TABLE payload_model (id int8 PRIMARY KEY, model text);
             CREATE TABLE payload_field (id int8 PRIMARY KEY, model text, name text);
             CREATE TABLE payload_group (action_id int8, group_id int8);
             INSERT INTO payload_model VALUES (1, 'x.test');
             INSERT INTO payload_action (id, name, state, model_id, value)
                 VALUES (1, 'root', 'multi', 1, 'yes');",
            )
            .await
            .unwrap();
        let sql = statement(&Names {
            server: "payload_action".to_owned(),
            model: "payload_model".to_owned(),
            fields: "payload_field".to_owned(),
            groups_rel: "payload_group".to_owned(),
            groups_col1: "action_id".to_owned(),
            groups_col2: "group_id".to_owned(),
        });
        let field_bytes = i64::try_from(MAX_FIELD_BYTES).unwrap();
        let groups = i64::try_from(MAX_GROUPS).unwrap();
        let tree_bytes = i64::try_from(MAX_TREE_BYTES).unwrap();
        let root = 1_i64;
        let depth = 8_i64;
        let actions = 257_i64;
        let params: &[&(dyn erp_testkit::tokio_postgres::types::ToSql + Sync)] =
            &[&root, &depth, &actions, &field_bytes, &groups, &tree_bytes];
        let accepted = client.query_one(&sql, params).await.unwrap();
        assert_eq!(accepted.get::<_, i32>(18), 0);
        assert_eq!(accepted.get::<_, &str>(11), "yes");

        let assert_suppressed = |rows: &[erp_testkit::tokio_postgres::Row], status: i32| {
            assert!(!rows.is_empty());
            for row in rows {
                assert_eq!(row.get::<_, i32>(18), status);
                for index in [3, 4, 5, 7, 8, 9, 10, 11, 12, 13, 14, 16, 17, 19] {
                    assert_eq!(row.get::<_, Option<&str>>(index), None);
                }
                assert_eq!(row.get::<_, Option<Vec<i64>>>(6), None);
            }
        };
        // root + multi + x.test + yes is 18 UTF-8 bytes; one group contributes eight.
        client
            .batch_execute("INSERT INTO payload_group VALUES (1, 7)")
            .await
            .unwrap();
        let accepted = client
            .query_one(
                &sql,
                &[&root, &depth, &actions, &field_bytes, &groups, &26_i64],
            )
            .await
            .unwrap();
        assert_eq!(accepted.get::<_, i32>(18), 0);
        assert_eq!(accepted.get::<_, Vec<i64>>(6), vec![7]);
        assert_suppressed(
            &client
                .query(
                    &sql,
                    &[&root, &depth, &actions, &field_bytes, &groups, &25_i64],
                )
                .await
                .unwrap(),
            3,
        );
        client
            .batch_execute("DELETE FROM payload_group")
            .await
            .unwrap();
        client
            .execute(
                "UPDATE payload_action SET value = repeat('x', $1::int4) WHERE id = 1",
                &[&i32::try_from(MAX_FIELD_BYTES + 1).unwrap()],
            )
            .await
            .unwrap();
        assert_suppressed(&client.query(&sql, params).await.unwrap(), 1);

        client
            .batch_execute("UPDATE payload_action SET value = ''")
            .await
            .unwrap();
        client
            .execute(
                "INSERT INTO payload_group SELECT 1, i FROM generate_series(1, $1::int8) i",
                &[&(groups + 1)],
            )
            .await
            .unwrap();
        assert_suppressed(&client.query(&sql, params).await.unwrap(), 2);

        client
            .batch_execute(
                "DELETE FROM payload_group;
             INSERT INTO payload_action (id, parent_id, name, state, model_id)
                 VALUES (2, 1, 'child', 'multi', 1);",
            )
            .await
            .unwrap();
        client
            .execute(
                "UPDATE payload_action SET value = repeat('x', $1::int4)",
                &[&i32::try_from(MAX_FIELD_BYTES).unwrap()],
            )
            .await
            .unwrap();
        // Lower only the aggregate cap: both individual fields still fit.
        let rows = client
            .query(
                &sql,
                &[&root, &depth, &actions, &field_bytes, &groups, &field_bytes],
            )
            .await
            .unwrap();
        assert_eq!(rows.len(), 2);
        assert_suppressed(&rows, 3);
        drop(client);
        db.cleanup().await.unwrap();
    }
}
