// SPDX-License-Identifier: MIT OR Apache-2.0
//! Role bindings of addon-owned SQL objects (amendment A2, slice B4; plan
//! ). Schema sync runs as `schema_owner`; the
//! operator names the database roles of `definer`, `maintenance` and `request`
//! ([`SqlRoleBindings`]). S0 validates every configured role before any DDL (the shared
//! [`Transaction::check_grantee`] checks, then the binding rules of A2); after the triggers a
//! lifecycle sync gives each `SECURITY DEFINER` function to the definer role, reconciles the
//! declared EXECUTE and table grants, and checks the privileges each bound role effectively holds
//! (through every membership, `PUBLIC` included) against the declaration.

use std::collections::{BTreeMap, BTreeSet};

use erp_fields::{Error, ErrorKind, Result};
use erp_registry::{
    SqlBinding, SqlFunction, SqlObjectGraph, SqlObjectId, SqlObjectKind, SqlSecurity, SqlTableOp,
};
use erp_sql::sqlobj;

use crate::Transaction;
use crate::grantee::{Grant, reach_cte};

/// The database roles the operator binds to the SQL object role bindings A2.
/// `schema_owner` is the role schema sync runs as (`current_user`), so it is not configured
/// here. A binding left unset may not be named by any declared object: such an object is refused
/// by name, and nothing is granted to a role by default.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SqlRoleBindings {
    definer: Option<&'static str>,
    maintenance: Option<&'static str>,
    request: Option<&'static str>,
}

impl SqlRoleBindings {
    /// No binding configured.
    pub const fn new() -> Self {
        Self {
            definer: None,
            maintenance: None,
            request: None,
        }
    }

    /// The NOLOGIN role that owns the `SECURITY DEFINER` functions.
    #[must_use]
    pub const fn definer(mut self, role: &'static str) -> Self {
        self.definer = Some(role);
        self
    }

    /// The maintenance pool's login role.
    #[must_use]
    pub const fn maintenance(mut self, role: &'static str) -> Self {
        self.maintenance = Some(role);
        self
    }

    /// The request (runtime) role.
    #[must_use]
    pub const fn request(mut self, role: &'static str) -> Self {
        self.request = Some(role);
        self
    }

    /// The role configured for `binding`; always `None` for `schema_owner` (`current_user`).
    pub const fn role(&self, binding: SqlBinding) -> Option<&'static str> {
        match binding {
            SqlBinding::SchemaOwner => None,
            SqlBinding::Definer => self.definer,
            SqlBinding::Maintenance => self.maintenance,
            SqlBinding::Request => self.request,
        }
    }
}

/// The configurable bindings, in the order they are validated.
const CONFIGURED: [SqlBinding; 3] = [
    SqlBinding::Definer,
    SqlBinding::Maintenance,
    SqlBinding::Request,
];

/// One validated binding: its role and the role quoted for statements.
struct Bound {
    binding: SqlBinding,
    role: &'static str,
    quoted: String,
}

/// The configured bindings, validated by S0, and the schemas of the A1 search paths.
pub(crate) struct BoundRoles {
    roles: Vec<Bound>,
    schemas: Vec<String>,
}

impl BoundRoles {
    fn get(&self, binding: SqlBinding) -> Result<&Bound> {
        // S0 refused every object naming a binding that is not configured
        self.roles
            .iter()
            .find(|b| b.binding == binding)
            .ok_or_else(|| {
                Error::programming(format!(
                    "SQL object role binding '{binding}' was not validated"
                ))
            })
    }
}

/// For each role (`$1`): whether it can log in, has BYPASSRLS, and is a member of the schema
/// owner (`current_user`), directly or through other roles, whatever the grant options.
const ROLE_SQL: &str = "SELECT r.rolcanlogin, r.rolbypassrls, \
     pg_has_role(r.oid, (SELECT oid FROM pg_roles WHERE rolname = current_user), 'MEMBER') \
     FROM pg_roles r WHERE r.rolname = $1";

/// The first ordered pair of bound roles (`$1`, by position) where the first is a member of the
/// second, through any membership row.
const PAIR_SQL: &str = "SELECT a.n::int4, b.n::int4 \
     FROM unnest($1::text[]) WITH ORDINALITY AS a (r, n), unnest($1::text[]) WITH ORDINALITY AS b (r, n) \
     WHERE a.n <> b.n AND pg_has_role(a.r::name, b.r::name, 'MEMBER') ORDER BY a.n, b.n LIMIT 1";

/// Whether the schema owner may `SET ROLE` to the definer (`$1`) and inherits its privileges:
/// `ALTER FUNCTION … OWNER TO` needs the first, `CREATE OR REPLACE` of a definer-owned function
/// the second.
const OWNER_SQL: &str = "SELECT pg_has_role(current_user, $1::text::name, 'SET') AND \
     pg_has_role(current_user, $1::text::name, 'USAGE'), current_user::text";

/// The owner of each function signature (`$1`), in order; `NULL` for a missing one.
const PROOWNER_SQL: &str = "SELECT pg_get_userbyid(p.proowner)::text \
     FROM unnest($1::text[]) WITH ORDINALITY AS u (s, n) \
     LEFT JOIN pg_proc p ON p.oid = to_regprocedure(u.s) ORDER BY u.n";

/// The first privilege PUBLIC holds on a SQL object function (`$1`) or restricted table (`$2`,
/// table or column ACL): the kind, the position and the privilege.
const PUBLIC_SQL: &str = "\
SELECT 'function', f.n::int4, a.privilege_type::text
  FROM unnest($1::text[]) WITH ORDINALITY AS f (sig, n)
  JOIN pg_proc p ON p.oid = to_regprocedure(f.sig),
       aclexplode(coalesce(p.proacl, acldefault('f', p.proowner))) a
 WHERE a.grantee = 0
UNION ALL
SELECT 'table', t.n::int4, a.privilege_type::text
  FROM unnest($2::text[]) WITH ORDINALITY AS t (name, n)
  JOIN pg_class c ON c.oid = to_regclass(t.name),
       aclexplode(coalesce(c.relacl, acldefault('r', c.relowner))) a
 WHERE a.grantee = 0
UNION ALL
SELECT 'table', t.n::int4, a.privilege_type::text
  FROM unnest($2::text[]) WITH ORDINALITY AS t (name, n)
  JOIN pg_attribute att ON att.attrelid = to_regclass(t.name) AND att.attnum > 0
       AND att.attacl IS NOT NULL,
       aclexplode(att.attacl) a
 WHERE a.grantee = 0
 ORDER BY 1, 2, 3 LIMIT 1";

/// The bound role (`$1`) and every role it reaches through any membership row ([`reach_cte`]).
macro_rules! roles_cte {
    () => {
        concat!(
            reach_cte!(),
            ", roles AS (SELECT oid FROM pg_roles WHERE rolname = $1 UNION SELECT oid FROM reach)\n"
        )
    };
}

/// The first SQL object function (`$2`, by position) on which the bound role or a role it
/// reaches holds EXECUTE while `$3` says it may not, with the role that holds it: a role whose
/// own ACL entry grants it (the bound role first), else the bound role.
const FUNCTION_LEAK_SQL: &str = concat!(
    roles_cte!(),
    "SELECT f.n::int4, r.rolname::text
  FROM unnest($2::text[], $3::bool[]) WITH ORDINALITY AS f (sig, allowed, n)
  JOIN pg_proc p ON p.oid = to_regprocedure(f.sig)
  CROSS JOIN roles JOIN pg_roles r ON r.oid = roles.oid
 WHERE NOT f.allowed AND has_function_privilege(roles.oid, p.oid, 'EXECUTE')
 ORDER BY f.n,
       NOT EXISTS (SELECT FROM aclexplode(coalesce(p.proacl, acldefault('f', p.proowner))) a
                    WHERE a.grantee = roles.oid AND a.privilege_type = 'EXECUTE'),
       r.rolname <> $1, r.rolname LIMIT 1"
);

/// The first table privilege the bound role or a role it reaches holds beyond what it may: on
/// the restricted tables (`$2`) beyond the comma-separated operations of `$3`, and with `$4` on
/// every other relation of the current schema at all. Table privileges and column privileges
/// both count. Returns the restricted table's position (`NULL` for another relation), the
/// relation, the role that holds the privilege (one whose own table or column ACL entry grants it,
/// the bound role first, else the bound role) and the privilege.
const TABLE_LEAK_SQL: &str = concat!(
    roles_cte!(),
    ", policy AS (
    SELECT to_regclass(p.t)::oid AS oid, p.a, p.n FROM unnest($2::text[], $3::text[]) WITH ORDINALITY AS p (t, a, n)
), tables AS (
    SELECT oid, a, n FROM policy WHERE oid IS NOT NULL
  UNION ALL
    SELECT c.oid, '', NULL FROM pg_class c
     WHERE $4 AND c.relnamespace = (SELECT oid FROM pg_namespace WHERE nspname = current_schema())
       AND c.relkind IN ('r', 'p', 'v', 'm', 'f')
       AND c.oid NOT IN (SELECT oid FROM policy WHERE oid IS NOT NULL)
)
SELECT t.n::int4, c.relname::text, r.rolname::text, p.priv
  FROM tables t JOIN pg_class c ON c.oid = t.oid
  CROSS JOIN roles JOIN pg_roles r ON r.oid = roles.oid
  CROSS JOIN unnest(ARRAY['SELECT', 'INSERT', 'UPDATE', 'DELETE', 'TRUNCATE', 'REFERENCES',
                          'TRIGGER', 'MAINTAIN']) AS p (priv)
 WHERE NOT (p.priv = ANY (string_to_array(t.a, ',')))
   AND (has_table_privilege(roles.oid, t.oid, p.priv)
        OR (p.priv IN ('SELECT', 'INSERT', 'UPDATE', 'REFERENCES')
            AND has_any_column_privilege(roles.oid, t.oid, p.priv)))
 ORDER BY t.n NULLS LAST, c.relname, p.priv,
       NOT (EXISTS (SELECT FROM aclexplode(coalesce(c.relacl, acldefault('r', c.relowner))) a
                     WHERE a.grantee = roles.oid AND a.privilege_type = p.priv)
            OR EXISTS (SELECT FROM pg_attribute att, aclexplode(att.attacl) a
                        WHERE att.attrelid = c.oid AND att.attnum > 0 AND att.attacl IS NOT NULL
                          AND a.grantee = roles.oid AND a.privilege_type = p.priv)),
       r.rolname <> $1, r.rolname LIMIT 1"
);

/// The first schema of the A1 search paths (`$2`) on which the bound role or a role it reaches
/// holds CREATE (it could plant a function or relation a body resolves), with the role that
/// holds it (one whose own ACL entry grants it, the bound role first, else the bound role).
const SCHEMA_LEAK_SQL: &str = concat!(
    roles_cte!(),
    "SELECT s.name, r.rolname::text
  FROM unnest($2::text[]) WITH ORDINALITY AS s (name, n)
  JOIN pg_namespace ns ON ns.nspname = s.name
  CROSS JOIN roles JOIN pg_roles r ON r.oid = roles.oid
 WHERE has_schema_privilege(roles.oid, ns.oid, 'CREATE')
 ORDER BY s.n,
       NOT EXISTS (SELECT FROM aclexplode(coalesce(ns.nspacl, acldefault('n', ns.nspowner))) a
                    WHERE a.grantee = roles.oid AND a.privilege_type = 'CREATE'),
       r.rolname <> $1, r.rolname LIMIT 1"
);

fn decode(e: tokio_postgres::Error) -> Error {
    Error::new(
        ErrorKind::Database,
        "could not decode a SQL object role check row",
    )
    .with_source(e)
}

fn missing_row(what: &str) -> Error {
    Error::new(
        ErrorKind::Database,
        format!("the SQL object role check {what} returned no row"),
    )
}

/// `SQL object '<addon>/<key>': role '<role>' has forbidden <capability>` (contract §2), with the
/// role that holds it when the bound role reaches it through membership.
fn forbidden(who: &str, role: &str, holder: &str, capability: &str) -> Error {
    let through = if holder == role {
        String::new()
    } else {
        format!(" (held by role '{holder}', which it reaches through membership)")
    };
    Error::access(format!(
        "{who}: role '{role}' has forbidden {capability}{through}"
    ))
}

/// Every binding the graph names, with the first object (in dependency order) naming it.
fn needed(graph: &SqlObjectGraph) -> BTreeMap<SqlBinding, SqlObjectId> {
    let mut first = BTreeMap::new();
    for object in graph.objects() {
        let mut add = |binding| {
            first.entry(binding).or_insert(object.id());
        };
        match object.kind() {
            SqlObjectKind::Function(f) => {
                if f.security() == SqlSecurity::Definer {
                    add(SqlBinding::Definer);
                }
                f.execute().iter().copied().for_each(&mut add);
            }
            SqlObjectKind::TableGrant(g) => add(g.binding()),
            _ => {}
        }
    }
    first
}

fn functions(graph: &SqlObjectGraph) -> Vec<(SqlObjectId, &SqlFunction)> {
    graph
        .objects()
        .iter()
        .filter_map(|o| match o.kind() {
            SqlObjectKind::Function(f) => Some((o.id(), f)),
            _ => None,
        })
        .collect()
}

/// One grant policy: its identity, binding and operations.
type Policy = (SqlObjectId, SqlBinding, &'static [SqlTableOp]);

/// The restricted tables of the graph with their policies (at least one each), by table.
fn restricted(graph: &SqlObjectGraph) -> BTreeMap<&str, Vec<Policy>> {
    let mut tables: BTreeMap<&str, Vec<Policy>> = BTreeMap::new();
    for object in graph.objects() {
        if let SqlObjectKind::TableGrant(g) = object.kind() {
            let policies = tables.entry(g.model().table()).or_default();
            policies.push((object.id(), g.binding(), g.ops()));
        }
    }
    tables
}

/// The policy of `binding` on a restricted table, else its first one: the object a refusal
/// about that table names.
fn policy_id(policies: &[Policy], binding: SqlBinding) -> Option<SqlObjectId> {
    policies
        .iter()
        .find(|p| p.1 == binding)
        .or(policies.first())
        .map(|p| p.0)
}

impl Transaction {
    /// S0: refuse an object that names a binding `roles` leaves unset, then validate every
    /// configured role before any DDL: the shared grantee checks (no superuser, CREATEROLE or
    /// REPLICATION, none reached, no data roles, owning or reaching no owner of the existing
    /// `tables`), no BYPASSRLS, not a member of the schema owner, the definer NOLOGIN, the bound
    /// roles distinct and none a member of another, and, with a definer function, a schema owner
    /// that may `SET ROLE` to the definer and inherits it. `schemas` are the A1 search path schemas
    /// the effective check covers.
    pub(crate) async fn check_sql_roles(
        &mut self,
        graph: &SqlObjectGraph,
        roles: crate::SqlRoleBindings,
        tables: &[&str],
        schemas: Vec<String>,
    ) -> Result<BoundRoles> {
        let needed = needed(graph);
        for (binding, id) in &needed {
            if roles.role(*binding).is_none() {
                return Err(Error::programming(format!(
                    "SQL object '{id}': role binding '{binding}' is not configured; the operator \
                     names its database role in the schema sync role bindings"
                )));
            }
        }
        let mut bound: Vec<Bound> = Vec::new();
        for binding in CONFIGURED {
            let Some(role) = roles.role(binding) else {
                continue;
            };
            if let Some(other) = bound.iter().find(|b| b.role == role) {
                return Err(Error::access(format!(
                    "SQL object role bindings '{}' and '{binding}' name the same role {role:?}; \
                     each binding needs a role of its own",
                    other.binding
                )));
            }
            let quoted = self.check_sql_role(binding, role, tables).await?;
            bound.push(Bound {
                binding,
                role,
                quoted,
            });
        }
        if bound.len() > 1 {
            let names: Vec<&str> = bound.iter().map(|b| b.role).collect();
            let pair = self.query(PAIR_SQL, &[&names]).await?;
            if let Some(row) = pair.first() {
                let at = |i: usize| -> Result<&Bound> {
                    let n: i32 = row.try_get(i).map_err(decode)?;
                    position(n)
                        .and_then(|n| bound.get(n))
                        .ok_or_else(|| missing_row("of role pairs"))
                };
                let (a, b) = (at(0)?, at(1)?);
                return Err(Error::access(format!(
                    "SQL object role binding '{}': role {:?} is a member of role {:?} of binding \
                     '{}'; the bound roles must not reach each other",
                    a.binding, a.role, b.role, b.binding
                )));
            }
        }
        if needed.contains_key(&SqlBinding::Definer) {
            let definer = bound
                .iter()
                .find(|b| b.binding == SqlBinding::Definer)
                .ok_or_else(|| missing_row("of the definer"))?;
            let rows = self.query(OWNER_SQL, &[&definer.role]).await?;
            let row = rows
                .first()
                .ok_or_else(|| missing_row("of the schema owner"))?;
            let can: bool = row.try_get(0).map_err(decode)?;
            if !can {
                let owner: String = row.try_get(1).map_err(decode)?;
                let (d, o) = (erp_sql::quote(definer.role), erp_sql::quote(&owner));
                return Err(Error::access(format!(
                    "SQL object role binding 'definer': the schema owner {owner:?} cannot act as \
                     role {:?}; the operator runs GRANT {d} TO {o} WITH INHERIT TRUE, SET TRUE",
                    definer.role
                )));
            }
        }
        Ok(BoundRoles {
            roles: bound,
            schemas,
        })
    }

    /// The checks of one configured role; returns it quoted.
    async fn check_sql_role(
        &mut self,
        binding: SqlBinding,
        role: &str,
        tables: &[&str],
    ) -> Result<String> {
        let grant = Grant {
            call: "Transaction::init_models_with",
            tables,
            grantee: match binding {
                SqlBinding::Definer => "the SQL object definer role",
                SqlBinding::Maintenance => "the SQL object maintenance role",
                _ => "the SQL object request role",
            },
            missing: |table| format!("table {table} used by a SQL object is missing"),
            owner: |role, table| {
                format!("role {role:?} owns {table}: a SQL object role binding must not own it")
            },
        };
        let hint = if binding == SqlBinding::Definer {
            "; the definer is a role the operator creates NOLOGIN NOSUPERUSER NOCREATEDB \
             NOCREATEROLE NOREPLICATION NOBYPASSRLS and grants to the schema owner WITH INHERIT \
             TRUE, SET TRUE"
        } else {
            ""
        };
        let quoted = self.check_grantee(&grant, role).await.map_err(|e| {
            let kind = match e.kind() {
                ErrorKind::Database => ErrorKind::Database,
                _ => ErrorKind::Access,
            };
            let message = format!("SQL object role binding '{binding}': {}{hint}", e.message());
            Error::new(kind, message).with_source(e)
        })?;
        let refuse = |reason: &str| {
            Error::access(format!(
                "SQL object role binding '{binding}': role {role:?} {reason}"
            ))
        };
        let rows = self.query(ROLE_SQL, &[&role]).await?;
        let row = rows.first().ok_or_else(|| missing_row("of the role"))?;
        let (login, bypass, owner): (bool, bool, bool) = (
            row.try_get(0).map_err(decode)?,
            row.try_get(1).map_err(decode)?,
            row.try_get(2).map_err(decode)?,
        );
        if bypass {
            return Err(refuse(
                "has BYPASSRLS; no SQL object role binding may have it",
            ));
        }
        if binding == SqlBinding::Definer && login {
            return Err(refuse(&format!(
                "can log in; the definer must be a NOLOGIN role{hint}"
            )));
        }
        if owner {
            return Err(refuse(
                "is a member of the schema owner; it would use or take on the owner's rights",
            ));
        }
        Ok(quoted)
    }

    /// After the triggers, in a lifecycle sync: give each `SECURITY DEFINER` function to the
    /// definer role, reconcile the EXECUTE grants of every function and the grants of every
    /// restricted table (PUBLIC and every bound role revoked, the declared grants given), then
    /// check the effective privileges ([`Self::verify_sql_object_privileges`]). Idempotent: a
    /// grant the declaration no longer lists is revoked.
    pub(crate) async fn grant_sql_objects(
        &mut self,
        graph: &SqlObjectGraph,
        bound: &BoundRoles,
    ) -> Result<()> {
        let functions = functions(graph);
        let signatures: Vec<String> = functions
            .iter()
            .map(|(_, f)| sqlobj::signature(f))
            .collect();
        let owners: Vec<Option<String>> = if signatures.is_empty() {
            Vec::new()
        } else {
            self.query(PROOWNER_SQL, &[&signatures])
                .await?
                .iter()
                .map(|r| r.try_get(0).map_err(decode))
                .collect::<Result<_>>()?
        };
        if owners.len() != functions.len() {
            return Err(missing_row("of function owners"));
        }
        for ((id, f), owner) in functions.iter().zip(&owners) {
            let definer = f.security() == SqlSecurity::Definer;
            let mut sql = String::new();
            if definer {
                let d = bound.get(SqlBinding::Definer)?;
                if owner.as_deref() != Some(d.role) {
                    sql = format!("{};\n", sqlobj::transfer_to_definer(f, &d.quoted));
                }
            }
            // the definer keeps its owner's rights on its own functions
            let revoke: Vec<String> = bound
                .roles
                .iter()
                .filter(|b| !(definer && b.binding == SqlBinding::Definer))
                .map(|b| b.quoted.clone())
                .collect();
            let execute: Vec<String> = f
                .execute()
                .iter()
                .map(|b| bound.get(*b).map(|b| b.quoted.clone()))
                .collect::<Result<_>>()?;
            sql.push_str(&sqlobj::function_grants(f, &revoke, &execute));
            self.sql_object_ddl(*id, &sql).await?;
        }
        let all: Vec<String> = bound.roles.iter().map(|b| b.quoted.clone()).collect();
        for (table, policies) in restricted(graph) {
            let grants: Vec<(String, &[SqlTableOp])> = policies
                .iter()
                .map(|(_, b, ops)| bound.get(*b).map(|b| (b.quoted.clone(), *ops)))
                .collect::<Result<_>>()?;
            let id = policies
                .first()
                .map(|p| p.0)
                .ok_or_else(|| missing_row("of a policy"))?;
            self.sql_object_ddl(id, &sqlobj::table_grants(table, &all, &grants))
                .await?;
        }
        self.verify_sql_object_privileges(graph, bound, &signatures)
            .await
    }

    /// The effective-privilege check (A2, A4): PUBLIC holds nothing on any SQL object function or
    /// restricted table; each bound role, and every role it reaches through any membership row,
    /// holds EXECUTE only where declared (the definer on the functions it owns), on a restricted
    /// table only the declared operations (table and column privileges both count), no CREATE
    /// on an A1 search path schema, and the definer no privilege on any other relation of the
    /// current schema. The first excess is refused `Access`, naming object, role and privilege.
    async fn verify_sql_object_privileges(
        &mut self,
        graph: &SqlObjectGraph,
        bound: &BoundRoles,
        signatures: &[String],
    ) -> Result<()> {
        let functions = functions(graph);
        let restricted = restricted(graph);
        let tables: Vec<String> = restricted.keys().map(|t| erp_sql::quote(t)).collect();
        let policies: Vec<&Vec<Policy>> = restricted.values().collect();
        let table_id = |n: i32, binding: SqlBinding| {
            position(n)
                .and_then(|n| policies.get(n))
                .and_then(|p| policy_id(p, binding))
                .ok_or_else(|| missing_row("position"))
        };
        let table_name = |n: i32| {
            position(n)
                .and_then(|n| tables.get(n))
                .cloned()
                .ok_or_else(|| missing_row("position"))
        };
        let at = |n: i32, of: &[SqlObjectId]| {
            position(n)
                .and_then(|n| of.get(n).copied())
                .ok_or_else(|| missing_row("position"))
        };
        let function_ids: Vec<SqlObjectId> = functions.iter().map(|(id, _)| *id).collect();
        if let Some(row) = self
            .query(PUBLIC_SQL, &[&signatures, &tables])
            .await?
            .first()
        {
            let kind: String = row.try_get(0).map_err(decode)?;
            let n: i32 = row.try_get(1).map_err(decode)?;
            let privilege: String = row.try_get(2).map_err(decode)?;
            let (id, what) = if kind == "function" {
                let id = at(n, &function_ids)?;
                (
                    id,
                    format!("{privilege} on function {}", signature_of(&functions, id)),
                )
            } else {
                let id = table_id(n, SqlBinding::SchemaOwner)?;
                (id, format!("{privilege} on table {}", table_name(n)?))
            };
            return Err(forbidden(
                &format!("SQL object '{id}'"),
                "PUBLIC",
                "PUBLIC",
                &what,
            ));
        }
        for b in &bound.roles {
            let allowed: Vec<bool> = functions
                .iter()
                .map(|(_, f)| {
                    f.execute().contains(&b.binding)
                        || (b.binding == SqlBinding::Definer
                            && f.security() == SqlSecurity::Definer)
                })
                .collect();
            if !signatures.is_empty()
                && let Some(row) = self
                    .query(FUNCTION_LEAK_SQL, &[&b.role, &signatures, &allowed])
                    .await?
                    .first()
            {
                let id = at(row.try_get(0).map_err(decode)?, &function_ids)?;
                let holder: String = row.try_get(1).map_err(decode)?;
                let what = format!("EXECUTE on function {}", signature_of(&functions, id));
                return Err(forbidden(
                    &format!("SQL object '{id}'"),
                    b.role,
                    &holder,
                    &what,
                ));
            }
            let ops: Vec<String> = policies
                .iter()
                .map(|policies| {
                    let mut ops: BTreeSet<&str> = BTreeSet::new();
                    for (_, binding, list) in policies.iter() {
                        if *binding == b.binding {
                            ops.extend(list.iter().map(|op| op.sql()));
                        }
                    }
                    ops.into_iter().collect::<Vec<_>>().join(",")
                })
                .collect();
            let every = b.binding == SqlBinding::Definer;
            if let Some(row) = self
                .query(TABLE_LEAK_SQL, &[&b.role, &tables, &ops, &every])
                .await?
                .first()
            {
                let n: Option<i32> = row.try_get(0).map_err(decode)?;
                let relation: String = row.try_get(1).map_err(decode)?;
                let holder: String = row.try_get(2).map_err(decode)?;
                let privilege: String = row.try_get(3).map_err(decode)?;
                let who = match n {
                    Some(n) => format!("SQL object '{}'", table_id(n, b.binding)?),
                    None => format!("SQL object role binding '{}'", b.binding),
                };
                let what = format!("{privilege} on table {}", erp_sql::quote(&relation));
                return Err(forbidden(&who, b.role, &holder, &what));
            }
            if let Some(row) = self
                .query(SCHEMA_LEAK_SQL, &[&b.role, &bound.schemas])
                .await?
                .first()
            {
                let schema: String = row.try_get(0).map_err(decode)?;
                let holder: String = row.try_get(1).map_err(decode)?;
                let what = format!("CREATE on schema {}", erp_sql::quote(&schema));
                let who = format!("SQL object role binding '{}'", b.binding);
                return Err(forbidden(&who, b.role, &holder, &what));
            }
        }
        Ok(())
    }
}

/// The zero-based index of a one-based `WITH ORDINALITY` position.
fn position(n: i32) -> Option<usize> {
    n.checked_sub(1).and_then(|n| usize::try_from(n).ok())
}

fn signature_of(functions: &[(SqlObjectId, &SqlFunction)], id: SqlObjectId) -> String {
    functions
        .iter()
        .find(|(i, _)| *i == id)
        .map(|(_, f)| sqlobj::signature(f))
        .unwrap_or_default()
}
