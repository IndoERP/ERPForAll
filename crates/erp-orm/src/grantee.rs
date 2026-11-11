//! The grantee checks shared by the role grants on core tables
//! ([`Transaction::grant_node_role`](crate::Transaction::grant_node_role),
//! [`Transaction::grant_signal_request_role`](crate::Transaction::grant_signal_request_role)):
//! a grant must restrict exactly the named role, so roles it cannot restrict, or that it would
//! reach far beyond, are refused before any privilege changes.
//!
//! The checks read the catalogs once, when the grant runs: a membership, an attribute or an
//! `ADMIN OPTION` given to the role later is not seen. Run the grant again after changing the role
//! (it checks again), and keep the roles that may change it (superusers, `CREATEROLE` roles, the
//! holders of `ADMIN OPTION` on it) under the same control as the table owner.
//!
//! Accepted on purpose, because none of them reads or writes the rows of a covered table:
//! `BYPASSRLS` (no covered table uses row-level security); `pg_maintain` (PostgreSQL 17+: it can
//! `VACUUM`, `ANALYZE`, `REINDEX`, `CLUSTER`, refresh materialized views and `LOCK TABLE` every
//! table, so it can make the registry wait, as any role with a lock on a covered table can, which
//! the registry's lock and statement timeouts bound, but it reads no rows); `pg_signal_backend` (it
//! can cancel or end other sessions, not a superuser's; a cancelled renewal is a failed
//! renewal, which the lease absorbs); `pg_create_subscription` (it can create a subscription only
//! in a database it has `CREATE` on and with a connection the server allows, and a subscription
//! writes as its owner, so it gives no table privilege of its own). Any other predefined role is
//! accepted on the same ground; the refused ones are [`DATA_ROLES`].
//!
//! The catalog reads run in the caller's transaction with its settings: these checks set no
//! `statement_timeout` of their own. Their cost is bounded by the role graph: the membership
//! closure ([`reach_cte`]) has one row per reachable role, and the path to a role is derived only
//! for the roles a check refuses, at most one row per (reached role, refused role) pair
//! ([`REACH_SQL`]).

use erp_fields::{Error, ErrorKind, Result};

use crate::Transaction;

/// Longest role name PostgreSQL keeps (`NAMEDATALEN - 1`).
const MAX_ROLE_LEN: usize = 63;

/// The owner of each table (`NULL` for a missing one), in the order given.
const OWNERS_SQL: &str = "\
SELECT t.name, (SELECT pg_get_userbyid(relowner) FROM pg_class WHERE oid = to_regclass(t.name))::text
  FROM unnest($1::text[]) WITH ORDINALITY AS t (name, n)
 ORDER BY t.n";

/// Whether the role exists, is a superuser, has `CREATEROLE`, and has `REPLICATION` (no row: it
/// does not exist).
const ROLE_SQL: &str =
    "SELECT rolsuper, rolcreaterole, rolreplication FROM pg_roles WHERE rolname = $1";

/// The roles the role (`$1`) reaches: every role it is a member of, directly or through other
/// roles, by any membership row whatever its `INHERIT`, `SET` and `ADMIN` options (a role it holds
/// the `ADMIN OPTION` on is one of them: on every supported PostgreSQL version that grant is a
/// membership row, and the role can widen it to `SET`). The recursion carries the role only, so
/// the closure has at most one row per role (`UNION` drops the repeats; PostgreSQL refuses circular
/// grants); the path to a role is derived afterwards ([`REACH_SQL`]).
macro_rules! reach_cte {
    () => {
        "WITH RECURSIVE reach (oid) AS (
    SELECT m.roleid FROM pg_auth_members m
     WHERE m.member = (SELECT oid FROM pg_roles WHERE rolname = $1)
  UNION
    SELECT m.roleid FROM reach JOIN pg_auth_members m ON m.member = reach.oid
)
"
    };
}

/// For each owner (`$2`): whether the role or a role it reaches holds the `ADMIN OPTION` on the
/// owner or on a member of it (it can make itself a member of the owner), and whether it is a
/// member of the owner, directly or through other roles (`MEMBER`: whatever the grant's `INHERIT`
/// and `SET` options, so a member that inherits nothing but may `SET ROLE` to the owner,
/// PostgreSQL 16+, counts too). The `ADMIN OPTION` is checked first for its own message.
const MEMBER_SQL: &str = concat!(
    reach_cte!(),
    "SELECT o.owner,
       EXISTS (SELECT 1 FROM pg_auth_members m
                WHERE m.admin_option
                  AND (m.member = (SELECT oid FROM pg_roles WHERE rolname = $1)
                       OR m.member IN (SELECT oid FROM reach))
                  AND pg_has_role(m.roleid, (SELECT oid FROM pg_roles WHERE rolname = o.owner),
                                  'MEMBER')),
       pg_has_role($1::text::name, o.owner::name, 'MEMBER')
  FROM unnest($2::text[]) WITH ORDINALITY AS o (owner, n)
 ORDER BY o.n"
);

/// The roles the role reaches ([`reach_cte`]) that a check refuses: those with `SUPERUSER`,
/// `CREATEROLE` or `REPLICATION`, and the predefined roles named in `$2` ([`DATA_ROLES`]), by name:
/// its name, the first role on the path to it (a role granted to the role itself from which a
/// chain of membership rows leads to the reached role: the reached role first, then by name), and
/// its attributes. The path is derived by walking the membership rows down from each refused role
/// through the reached roles only (`down`), so it costs at most one row per (reached role,
/// refused role) pair; it does not use `pg_has_role`, which counts a superuser as a member of every role and
/// would name it as the way to any of them.
const REACH_SQL: &str = concat!(
    reach_cte!(),
    ", refused AS (
    SELECT r.oid, r.rolname, r.rolsuper, r.rolcreaterole, r.rolreplication
      FROM reach JOIN pg_roles r ON r.oid = reach.oid
     WHERE r.rolsuper OR r.rolcreaterole OR r.rolreplication OR r.rolname = ANY ($2::text[])
), down (oid, target) AS (
    SELECT oid, oid FROM refused
  UNION
    SELECT m.member, down.target FROM down JOIN pg_auth_members m ON m.roleid = down.oid
     WHERE m.member IN (SELECT oid FROM reach)
)
SELECT f.rolname::text,
       (SELECT d.rolname::text
          FROM pg_auth_members dm JOIN pg_roles d ON d.oid = dm.roleid
         WHERE dm.member = (SELECT oid FROM pg_roles WHERE rolname = $1)
           AND EXISTS (SELECT 1 FROM down WHERE down.target = f.oid AND down.oid = dm.roleid)
         ORDER BY dm.roleid = f.oid DESC, d.rolname
         LIMIT 1),
       f.rolsuper, f.rolcreaterole, f.rolreplication
  FROM refused f
 ORDER BY f.rolname"
);

/// Predefined roles whose members reach every table whatever is revoked from them, and how.
/// (`pg_database_owner` is not among them: it owns only what the database owner owns, and a table
/// owner is refused above.)
const DATA_ROLES: [(&str, &str); 5] = [
    ("pg_write_all_data", "it can write every table"),
    ("pg_read_all_data", "it can read every table"),
    (
        "pg_execute_server_program",
        "it can run programs as the database server's operating system user, which reaches every \
         table",
    ),
    (
        "pg_write_server_files",
        "it can write files as the database server's operating system user, which reaches every \
         table",
    ),
    (
        "pg_read_server_files",
        "it can read files as the database server's operating system user, which reaches every \
         table",
    ),
];

/// A role the grantee reaches that a check refuses ([`REACH_SQL`]).
struct Reached {
    name: String,
    /// The first role on the path (the reached role itself for a direct membership).
    via: String,
    superuser: bool,
    create_role: bool,
    replication: bool,
}

impl Reached {
    /// How the grantee gets to this role, for a message: `is a member of "r"`, or `is a member
    /// of "z", which leads to "r"`.
    fn path(&self) -> String {
        let name = &self.name;
        let via = &self.via;
        if name == via {
            format!("is a member of {name:?}")
        } else {
            format!("is a member of {via:?}, which leads to {name:?}")
        }
    }

    /// The attribute that lets a member act beyond any grant, described for a message.
    fn attribute(&self) -> Option<&'static str> {
        if self.superuser {
            Some("a superuser role: it can act as a superuser")
        } else if self.create_role {
            Some("a role with CREATEROLE: it can give itself the membership of other roles")
        } else if self.replication {
            Some("a role with REPLICATION: it can copy every table through a base backup")
        } else {
            None
        }
    }
}

/// What a grant is about, for its messages.
pub(crate) struct Grant {
    /// The entry point, for the principal refusal (`Transaction::grant_node_role`).
    pub(crate) call: &'static str,
    /// The tables the grant changes.
    pub(crate) tables: &'static [&'static str],
    /// The role being set up ("the heartbeat role").
    pub(crate) grantee: &'static str,
    /// The message for a missing table.
    pub(crate) missing: fn(&str) -> String,
    /// The message for the table owner (`role`, `table`).
    pub(crate) owner: fn(&str, &str) -> String,
}

/// A catalog read must answer one row per item it was asked about; anything else is a defect,
/// reported before the rows are paired with the items.
fn expect_rows(len: usize, expected: usize, what: &str) -> Result<()> {
    if len == expected {
        Ok(())
    } else {
        Err(Error::new(
            ErrorKind::Database,
            format!("{what} returned {len} rows for {expected} items"),
        ))
    }
}

fn decode(e: tokio_postgres::Error) -> Error {
    Error::new(
        ErrorKind::Database,
        format!("could not decode a role check row: {e}"),
    )
}

impl Transaction {
    /// Run every check of `grant` on `role`, in a fixed order: the principal (`System` with full
    /// elevation), the name's syntax, the reserved names (`PUBLIC`, `NONE`, `pg_*`), the tables
    /// (missing, then owned by `role`), the role's existence, superuser, `CREATEROLE`,
    /// `REPLICATION`, an `ADMIN OPTION` that leads to a table owner (on the owner, on a member of
    /// it, through any role the role belongs to or can join), membership of a table owner, then
    /// every role it reaches ([`REACH_SQL`]): one with `SUPERUSER`, `CREATEROLE` or `REPLICATION`,
    /// or a predefined role that reaches every table ([`DATA_ROLES`]), refused through the role
    /// that leads there. Returns the role quoted for the
    /// statement. The checks hold when the grant runs (see the module documentation).
    pub(crate) async fn check_grantee(&mut self, grant: &Grant, role: &str) -> Result<String> {
        self.require_system_full(grant.call)?;
        if role.is_empty() || role.len() > MAX_ROLE_LEN || role.contains('\0') {
            return Err(Error::new(
                ErrorKind::Programming,
                format!("invalid role name {role:?}: 1 to {MAX_ROLE_LEN} bytes without NUL"),
            ));
        }
        // PostgreSQL reads `public` as PUBLIC even quoted, refuses `none`, and reserves `pg_*`
        // for its predefined roles; compared without case so no spelling slips through
        let lower = role.to_ascii_lowercase();
        if lower == "public" || lower == "none" || lower.starts_with("pg_") {
            return Err(Error::programming(format!(
                "role {role:?} is reserved: PUBLIC, NONE and the predefined pg_* roles are \
                 refused (a grant to them would reach every role or a whole class of roles)"
            )));
        }
        let tables: Vec<&str> = grant.tables.to_vec();
        let rows = self.query(OWNERS_SQL, &[&tables]).await?;
        expect_rows(rows.len(), tables.len(), "the table owner read")?;
        let mut owners: Vec<(String, String)> = Vec::with_capacity(rows.len());
        for row in &rows {
            let table: String = row.try_get(0).map_err(decode)?;
            let owner: Option<String> = row.try_get(1).map_err(decode)?;
            let Some(owner) = owner else {
                return Err(Error::programming((grant.missing)(&table)));
            };
            if owner == role {
                return Err(Error::programming((grant.owner)(role, &table)));
            }
            owners.push((table, owner));
        }
        let found = self.query(ROLE_SQL, &[&role]).await?;
        let Some(found) = found.first() else {
            return Err(Error::programming(format!("role {role:?} does not exist")));
        };
        let superuser: bool = found.try_get(0).map_err(decode)?;
        if superuser {
            return Err(Error::programming(format!(
                "role {role:?} is a superuser: privileges do not restrict it, so {} must not be \
                 one",
                grant.grantee
            )));
        }
        let create_role: bool = found.try_get(1).map_err(decode)?;
        if create_role {
            return Err(Error::programming(format!(
                "role {role:?} has CREATEROLE: it can give itself the membership of other roles \
                 whatever this grants, so {} must not have it",
                grant.grantee
            )));
        }
        let replication: bool = found.try_get(2).map_err(decode)?;
        if replication {
            return Err(Error::programming(format!(
                "role {role:?} has REPLICATION: it can copy every table through a base backup \
                 whatever this grants, so {} must not have it",
                grant.grantee
            )));
        }
        let names: Vec<&str> = owners.iter().map(|(_, o)| o.as_str()).collect();
        let members = self.query(MEMBER_SQL, &[&role, &names]).await?;
        expect_rows(members.len(), owners.len(), "the owner membership read")?;
        for (row, (table, owner)) in members.iter().zip(&owners) {
            let admin: bool = row.try_get(1).map_err(decode)?;
            if admin {
                return Err(Error::programming(format!(
                    "role {role:?} holds the ADMIN OPTION on {owner:?}, which owns {table}, or on a \
                     role that leads to it: it can make itself a member of the owner whatever \
                     this grants, so {} must not hold it",
                    grant.grantee
                )));
            }
            let member: bool = row.try_get(2).map_err(decode)?;
            if member {
                return Err(Error::programming(format!(
                    "role {role:?} is a member of {owner:?}, which owns {table}: it can use or \
                     take on the owner's rights whatever this grants, so {} must not be a member \
                     of the owner",
                    grant.grantee
                )));
            }
        }
        // every role it reaches: one with an attribute, or a predefined role that reaches every
        // table, is refused through the role that leads there
        let data_roles: Vec<&str> = DATA_ROLES.iter().map(|(name, _)| *name).collect();
        let mut reached = Vec::new();
        for row in &self.query(REACH_SQL, &[&role, &data_roles]).await? {
            let name: String = row.try_get(0).map_err(decode)?;
            // a reached role always has a first role on its path; none is a catalog defect
            let via: Option<String> = row.try_get(1).map_err(decode)?;
            let Some(via) = via else {
                return Err(Error::new(
                    ErrorKind::Database,
                    format!("the reached role read found no path from {role:?} to {name:?}"),
                ));
            };
            reached.push(Reached {
                name,
                via,
                superuser: row.try_get(2).map_err(decode)?,
                create_role: row.try_get(3).map_err(decode)?,
                replication: row.try_get(4).map_err(decode)?,
            });
        }
        for r in &reached {
            if let Some(attribute) = r.attribute() {
                return Err(Error::programming(format!(
                    "role {role:?} {}, {attribute} whatever this grants, so {} must not reach it",
                    r.path(),
                    grant.grantee
                )));
            }
        }
        for (name, reach) in &DATA_ROLES {
            let Some(r) = reached.iter().find(|r| r.name == *name) else {
                continue;
            };
            let message = if r.via == r.name {
                format!(
                    "role {role:?} is a member of {name:?}: {reach} whatever this grants, so {} \
                     must not be a member of it",
                    grant.grantee
                )
            } else {
                format!(
                    "role {role:?} {}: {reach} whatever this grants, so {} must not reach it",
                    r.path(),
                    grant.grantee
                )
            };
            return Err(Error::programming(message));
        }
        Ok(erp_sql::quote(role))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Not a spec case: a catalog read with a row count other than the items asked about is
    /// refused before its rows are paired with them (round-2 review, finding 8).
    #[test]
    fn a_catalog_read_with_another_row_count_is_refused() {
        assert!(expect_rows(3, 3, "the owner membership read").is_ok());
        assert!(expect_rows(0, 0, "the owner membership read").is_ok());
        for (len, expected) in [(2, 3), (4, 3), (0, 1)] {
            let err = expect_rows(len, expected, "the owner membership read").unwrap_err();
            assert_eq!(err.kind(), ErrorKind::Database);
            assert_eq!(
                err.message(),
                format!("the owner membership read returned {len} rows for {expected} items")
            );
        }
    }
}
