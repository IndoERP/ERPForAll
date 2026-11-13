// SPDX-License-Identifier: MIT OR Apache-2.0
//! Role bindings of addon-owned SQL objects, slice B4 (amendment A2; plan
//! ): a NOLOGIN definer, a maintenance and a
//! request login role, created for each test in the test cluster and dropped afterwards (also on
//! a failed test), checked as the roles themselves. Real PostgreSQL, one database per test.
//!
//! Fixture addon `test_sqlobj` and its model are test fixtures: they ship no default data.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

use std::hash::{DefaultHasher, Hash, Hasher};
use std::sync::Arc;

use erp_fields::ErrorKind;
use erp_orm::{Orm, SUPERUSER_ID, SchemaSyncOptions, Signal, SqlRoleBindings, StaticPolicy};
use erp_registry::{
    AddonDescriptor, FieldDecl, ModelDecl, Registry, RegistryBuilder, SqlBinding, SqlDep, SqlEvent,
    SqlFunctionDecl, SqlLanguage, SqlLevel, SqlObjectDecl, SqlObjectKind, SqlObjectManifest,
    SqlResult, SqlScalar, SqlSecurity, SqlTableGrantDecl, SqlTableOp, SqlTiming, SqlTriggerDecl,
    SqlType,
};
use erp_sql::{quote, sqlobj};
use erp_test_models::base_users;
use erp_testkit::Harness;
use erp_testkit::tokio_postgres::{self, Client, NoTls, error::SqlState};

static SQLOBJ: AddonDescriptor = AddonDescriptor {
    name: "test_sqlobj",
    depends: &["base"],
    exact_only: false,
};

const ADDON: &str = "test_sqlobj";
const TOKEN: &str = "test_sqlobj.token";

fn leak<T>(value: T) -> &'static T {
    Box::leak(Box::new(value))
}

fn leak_str(value: String) -> &'static str {
    Box::leak(value.into_boxed_str())
}

fn leak_slice<T>(value: Vec<T>) -> &'static [T] {
    Box::leak(value.into_boxed_slice())
}

fn token_model() -> ModelDecl {
    ModelDecl::new(ADDON, TOKEN)
        .fields(const { &[FieldDecl::char("name"), FieldDecl::char("note")] })
}

fn function(key: &'static str, decl: SqlFunctionDecl) -> SqlObjectDecl {
    SqlObjectDecl::function(ADDON, key, decl)
}

fn int_fn(name: &'static str, body: &'static str) -> SqlFunctionDecl {
    SqlFunctionDecl::new(
        name,
        SqlLanguage::Sql,
        SqlResult::Scalar(SqlScalar::Integer),
        body,
    )
}

/// The definer entry point `sqlobj_post(text)`: it inserts into the INSERT-only token table
/// through unqualified names (`test_sqlobj_token`, `lower`) and says who it ran as.
const POST_BODY: &str = "BEGIN INSERT INTO test_sqlobj_token (name) VALUES (lower($1)); \
                         RETURN current_user::text || '/' || lower($1); END";

/// The objects/052: a definer entry point for request and maintenance, an
/// invoker function executable by `listed`, one by nobody, one by maintenance, a BEFORE INSERT
/// stamp trigger, and the token table INSERT-only for request, SELECT/INSERT for the definer.
fn objects(listed: &'static [SqlBinding]) -> Vec<SqlObjectDecl> {
    vec![
        function(
            "fn_post",
            SqlFunctionDecl::new(
                "sqlobj_post",
                SqlLanguage::PlPgSql,
                SqlResult::Scalar(SqlScalar::Text),
                POST_BODY,
            )
            .args(&[SqlType::Scalar(SqlScalar::Text)])
            .security(SqlSecurity::Definer)
            .deps(&[SqlDep::Model(TOKEN)])
            .grant_execute(&[SqlBinding::Request, SqlBinding::Maintenance]),
        ),
        function(
            "fn_listed",
            int_fn("sqlobj_listed", "SELECT 1").grant_execute(listed),
        ),
        function("fn_unlisted", int_fn("sqlobj_unlisted", "SELECT 2")),
        function(
            "fn_maint",
            int_fn("sqlobj_maint", "SELECT 3").grant_execute(&[SqlBinding::Maintenance]),
        ),
        function(
            "fn_stamp",
            SqlFunctionDecl::new(
                "sqlobj_stamp",
                SqlLanguage::PlPgSql,
                SqlResult::Trigger,
                "BEGIN NEW.note := 'stamped'; RETURN NEW; END",
            )
            .deps(&[SqlDep::Model(TOKEN)]),
        ),
        SqlObjectDecl::trigger(
            ADDON,
            "trg_stamp",
            SqlTriggerDecl::new(
                "sqlobj_stamp",
                TOKEN,
                (ADDON, "fn_stamp"),
                SqlTiming::Before,
                &[SqlEvent::Insert],
                SqlLevel::Row,
            ),
        ),
        SqlObjectDecl::table_grant(
            ADDON,
            "grant_token_request",
            SqlTableGrantDecl::new(TOKEN, SqlBinding::Request, &[SqlTableOp::Insert]),
        ),
        SqlObjectDecl::table_grant(
            ADDON,
            "grant_token_definer",
            SqlTableGrantDecl::new(
                TOKEN,
                SqlBinding::Definer,
                &[SqlTableOp::Select, SqlTableOp::Insert],
            ),
        ),
    ]
}

fn build(decls: Vec<SqlObjectDecl>) -> Arc<Registry> {
    let decls: Vec<&'static SqlObjectDecl> = decls.into_iter().map(leak).collect();
    let keys: Vec<&'static str> = decls.iter().map(|d| d.key()).collect();
    let manifest = leak(SqlObjectManifest::new(ADDON, leak_slice(keys)));
    let model: &'static ModelDecl = leak(token_model());
    Arc::new(
        RegistryBuilder::new([&base_users::ADDON, &SQLOBJ])
            .with_decls([model])
            .require_sql_objects([manifest])
            .with_sql_objects(decls)
            .linked(false)
            .build()
            .unwrap(),
    )
}

/// The roles a test creates, dropped (with everything they own or were granted in the test
/// database) when this value is dropped, so also when the test fails. Declared before the
/// harness in [`Fixture`], so it drops while the database still exists.
struct Roles {
    db: tokio_postgres::Config,
    names: Vec<String>,
}

impl Roles {
    fn add(&mut self, name: String) -> String {
        self.names.push(name.clone());
        name
    }
}

impl Drop for Roles {
    fn drop(&mut self) {
        let (db, names) = (self.db.clone(), std::mem::take(&mut self.names));
        // Drop cannot await, so a thread with its own runtime does the work. The tests run on a
        // multi-thread runtime: while this blocks one worker, the other closes the connection of
        // a dropped transaction, whose locks DROP OWNED would otherwise wait for.
        let worker = std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|e| e.to_string())?;
            runtime.block_on(async move {
                let (client, connection) = db.connect(NoTls).await.map_err(|e| e.to_string())?;
                tokio::spawn(connection);
                // a cleanup that cannot get its locks reports it instead of hanging the test
                client
                    .batch_execute("SET lock_timeout = '60s'")
                    .await
                    .map_err(|e| e.to_string())?;
                for name in names {
                    let q = quote(&name);
                    let sql = format!(
                        "DO $$ BEGIN IF EXISTS (SELECT FROM pg_roles WHERE rolname = {}) THEN \
                         REASSIGN OWNED BY {q} TO CURRENT_USER; DROP OWNED BY {q}; DROP ROLE {q}; \
                         END IF; END $$",
                        erp_sql::literal(&name)
                    );
                    client
                        .batch_execute(&sql)
                        .await
                        .map_err(|e| format!("{name}: {e}"))?;
                }
                Ok::<(), String>(())
            })
        });
        match worker.join() {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                eprintln!("sql_objects_security: dropping test roles failed: {error}")
            }
            Err(_) => eprintln!("sql_objects_security: the role cleanup thread panicked"),
        }
    }
}

struct Fixture {
    roles: Roles,
    h: Harness,
    request: &'static str,
    maintenance: &'static str,
    definer: &'static str,
    password: String,
}

impl Fixture {
    /// A fresh database with `base_users` and the signal table, and a request and maintenance
    /// login role and a NOLOGIN definer, the request role named `request` after the test tag.
    async fn new(request: &str) -> Self {
        let h = Harness::builder([&base_users::ADDON])
            .build()
            .await
            .unwrap();
        let mut tx = h.begin().await.unwrap();
        tx.install_signal().await.unwrap();
        tx.commit().await.unwrap();
        let mut hasher = DefaultHasher::new();
        h.db().name().hash(&mut hasher);
        let tag = format!("sqlr{:012x}", hasher.finish() & 0xffff_ffff_ffff);
        let mut roles = Roles {
            db: h.db().config(),
            names: Vec::new(),
        };
        let request = leak_str(roles.add(format!("{tag}_{request}")));
        let maintenance = leak_str(roles.add(format!("{tag}_maint")));
        let definer = leak_str(roles.add(format!("{tag}_definer")));
        let password = format!("{tag}-login");
        let pw = erp_sql::literal(&password);
        let (r, m, d) = (quote(request), quote(maintenance), quote(definer));
        let f = Self {
            roles,
            h,
            request,
            maintenance,
            definer,
            password,
        };
        f.admin(&format!(
            "CREATE ROLE {r} LOGIN PASSWORD {pw}; CREATE ROLE {m} LOGIN PASSWORD {pw}; \
             CREATE ROLE {d} NOLOGIN NOSUPERUSER NOCREATEDB NOCREATEROLE NOREPLICATION \
             NOBYPASSRLS; GRANT USAGE ON SCHEMA public TO {r}, {m}"
        ))
        .await;
        f
    }

    fn bindings(&self) -> SqlRoleBindings {
        SqlRoleBindings::new()
            .definer(self.definer)
            .maintenance(self.maintenance)
            .request(self.request)
    }

    /// Another role of this test, dropped with the others.
    fn role(&mut self, suffix: &str) -> String {
        let name = format!("{}_{suffix}", self.definer.trim_end_matches("_definer"));
        self.roles.add(name)
    }

    /// A batch on the test database as the cluster administrator (the harness's connection).
    async fn admin(&self, sql: &str) {
        let client = self.h.db().client().await.unwrap();
        client.batch_execute(sql).await.expect(sql);
    }

    async fn admin_rows(&self, sql: &str) -> Vec<Vec<Option<String>>> {
        let client = self.h.db().client().await.unwrap();
        let rows = client.query(sql, &[]).await.expect(sql);
        rows.iter()
            .map(|r| (0..r.len()).map(|i| r.get(i)).collect())
            .collect()
    }

    /// A connection logged in as `role`.
    async fn login(&self, role: &str) -> Client {
        let mut config = self.h.db().config();
        config.user(role).password(&self.password);
        let (client, connection) = config.connect(NoTls).await.unwrap();
        tokio::spawn(connection);
        client
    }

    /// One lifecycle schema sync of `registry` with `roles`, committed; rolled back on error.
    async fn sync_with(
        &self,
        registry: Arc<Registry>,
        roles: SqlRoleBindings,
    ) -> erp_fields::Result<()> {
        let client = self.h.db().client().await.unwrap();
        let applied = Signal::read(&client).await.unwrap();
        drop(client);
        let orm = Orm::builder(registry, self.h.db().pool().clone())
            .policy(StaticPolicy::AllowAll)
            .signal(Signal::new(applied))
            .build()
            .unwrap();
        let mut tx = orm.begin(SUPERUSER_ID).await?;
        let options = SchemaSyncOptions::inline().lifecycle(true).sql_roles(roles);
        let sync = tx.init_models_with(&[], &options).await?;
        assert!(sync.is_complete());
        tx.commit().await
    }

    async fn sync(&self, registry: Arc<Registry>) -> erp_fields::Result<()> {
        self.sync_with(registry, self.bindings()).await
    }

    /// Owner and ACL of every SQL object function and of the token table.
    async fn acls(&self) -> Vec<Vec<Option<String>>> {
        self.admin_rows(
            "SELECT p.proname::text, pg_get_userbyid(p.proowner)::text, p.proacl::text FROM \
             pg_proc p WHERE p.proname LIKE 'sqlobj\\_%' UNION ALL SELECT c.relname::text, \
             pg_get_userbyid(c.relowner)::text, c.relacl::text FROM pg_class c WHERE c.oid = \
             to_regclass('test_sqlobj_token') ORDER BY 1",
        )
        .await
    }

    async fn finish(self) {
        let Self { roles, h, .. } = self;
        drop(roles);
        h.cleanup().await.unwrap();
    }
}

async fn denied(client: &Client, sql: &str) {
    let error = client.batch_execute(sql).await.expect_err(sql);
    assert_eq!(
        error.code(),
        Some(&SqlState::INSUFFICIENT_PRIVILEGE),
        "{sql}: {error:?}"
    );
}

async fn text(client: &Client, sql: &str) -> String {
    client.query_one(sql, &[]).await.expect(sql).get(0)
}

#[track_caller]
fn refused(result: erp_fields::Result<()>, kind: ErrorKind, message: &str) {
    let error = result.expect_err("the sync must refuse");
    assert_eq!(error.kind(), kind, "{error:?}");
    assert_eq!(error.message(), message);
}

/// PUBLIC executes nothing; request executes only its listed functions and the definer entry
/// point, inserts into the INSERT-only table (its identity column needs no sequence privilege,
/// the stamp trigger runs without EXECUTE on its function) and cannot read, update, delete or
/// truncate it, disable or replace a guard, read SQL object provenance or become the definer or
/// maintenance role; the entry point runs as the NOLOGIN definer, which owns it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn spec_ormx_051_restricted_grants_are_effective() {
    let f = Fixture::new("request").await;
    f.sync(build(objects(&[SqlBinding::Request])))
        .await
        .unwrap();
    let owner = f.admin_rows("SELECT current_user::text").await[0][0].clone();
    let catalog = f
        .admin_rows(
            "SELECT p.proname::text, pg_get_userbyid(p.proowner)::text, EXISTS (SELECT FROM \
             aclexplode(coalesce(p.proacl, acldefault('f', p.proowner))) a WHERE a.grantee = 0)::text \
             FROM pg_proc p WHERE p.proname LIKE 'sqlobj\\_%' ORDER BY 1",
        )
        .await;
    let expected: Vec<Vec<Option<String>>> = [
        ("sqlobj_listed", owner.clone()),
        ("sqlobj_maint", owner.clone()),
        ("sqlobj_post", Some(f.definer.to_owned())),
        ("sqlobj_stamp", owner.clone()),
        ("sqlobj_unlisted", owner.clone()),
    ]
    .into_iter()
    .map(|(name, owner)| vec![Some(name.to_owned()), owner, Some("false".to_owned())])
    .collect();
    assert_eq!(catalog, expected);

    let request = f.login(f.request).await;
    assert_eq!(
        text(&request, "SELECT public.sqlobj_listed()::text").await,
        "1"
    );
    denied(&request, "SELECT public.sqlobj_unlisted()").await;
    denied(&request, "SELECT public.sqlobj_maint()").await;
    request
        .batch_execute("INSERT INTO test_sqlobj_token (name) VALUES ('direct')")
        .await
        .unwrap();
    for sql in [
        "SELECT name FROM test_sqlobj_token",
        "UPDATE test_sqlobj_token SET name = name",
        "DELETE FROM test_sqlobj_token",
        "TRUNCATE test_sqlobj_token",
        "ALTER TABLE test_sqlobj_token DISABLE TRIGGER sqlobj_stamp",
        "CREATE OR REPLACE FUNCTION public.sqlobj_listed() RETURNS integer LANGUAGE sql AS \
         'SELECT 9'",
        "DROP FUNCTION public.sqlobj_unlisted()",
        "SELECT addon FROM erp_schema_sql_object",
        "SELECT addon FROM erp_schema_sql_object_dependency",
    ] {
        denied(&request, sql).await;
    }
    denied(
        &request,
        &format!(
            "ALTER FUNCTION public.sqlobj_post(text) OWNER TO {}",
            quote(f.request)
        ),
    )
    .await;
    denied(&request, &format!("SET ROLE {}", quote(f.definer))).await;
    denied(&request, &format!("SET ROLE {}", quote(f.maintenance))).await;
    assert_eq!(
        text(&request, "SELECT public.sqlobj_post('ABC')").await,
        format!("{}/abc", f.definer)
    );

    let maintenance = f.login(f.maintenance).await;
    assert_eq!(
        text(&maintenance, "SELECT public.sqlobj_maint()::text").await,
        "3"
    );
    assert_eq!(
        text(&maintenance, "SELECT public.sqlobj_post('M')").await,
        format!("{}/m", f.definer)
    );
    denied(&maintenance, "SELECT public.sqlobj_listed()").await;
    denied(
        &maintenance,
        "INSERT INTO test_sqlobj_token (name) VALUES ('m2')",
    )
    .await;
    assert_eq!(
        f.admin_rows("SELECT name, note FROM test_sqlobj_token ORDER BY name")
            .await,
        [["abc", "stamped"], ["direct", "stamped"], ["m", "stamped"]]
            .map(|r| r.map(|c| Some(c.to_owned())).to_vec())
            .to_vec()
    );
    drop((request, maintenance));
    f.finish().await;
}

/// A privilege the definer holds beyond its policy is found after the objects, tables and grants
/// were made in the transaction: the sync fails Access and nothing of it remains.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn spec_ormx_051_a_late_grant_failure_rolls_everything_back() {
    let f = Fixture::new("request").await;
    f.admin(&format!(
        "CREATE TABLE sqlr_other (x int); GRANT SELECT ON sqlr_other TO {}",
        quote(f.definer)
    ))
    .await;
    refused(
        f.sync(build(objects(&[SqlBinding::Request]))).await,
        ErrorKind::Access,
        &format!(
            "SQL object role binding 'definer': role '{}' has forbidden SELECT on table \
             \"sqlr_other\"",
            f.definer
        ),
    );
    assert_eq!(
        f.admin_rows(
            "SELECT (SELECT count(*) FROM pg_proc WHERE proname LIKE 'sqlobj\\_%')::text, \
             (to_regclass('test_sqlobj_token') IS NULL)::text"
        )
        .await,
        vec![vec![Some("0".to_owned()), Some("true".to_owned())]]
    );
    f.finish().await;
}

/// A request session that plants `lower(text)` in a schema ahead of `pg_catalog` on its path and
/// a temporary table named like the token table still has the definer entry point use
/// `pg_catalog.lower` and the persistent table: the function's search path is core-rendered
/// with `pg_temp` last (A1).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn spec_ormx_052_definer_functions_resist_shadowing() {
    let f = Fixture::new("request").await;
    f.sync(build(objects(&[SqlBinding::Request])))
        .await
        .unwrap();
    let r = quote(f.request);
    f.admin(&format!(
        "CREATE SCHEMA sqlr_plant; GRANT USAGE, CREATE ON SCHEMA sqlr_plant TO {r}"
    ))
    .await;
    let request = f.login(f.request).await;
    request
        .batch_execute(
            "CREATE FUNCTION sqlr_plant.lower(text) RETURNS text LANGUAGE sql AS \
             $$SELECT 'planted'$$; \
             CREATE TEMP TABLE test_sqlobj_token (name text, note text); \
             SET search_path = sqlr_plant, pg_catalog, public; \
             INSERT INTO test_sqlobj_token (name) VALUES (lower('SELF'))",
        )
        .await
        .unwrap();
    // the plants are live on the caller's own path
    assert_eq!(
        text(
            &request,
            "SELECT string_agg(name, ',') FROM test_sqlobj_token"
        )
        .await,
        "planted"
    );
    assert_eq!(
        text(&request, "SELECT public.sqlobj_post('ABC')").await,
        format!("{}/abc", f.definer)
    );
    assert_eq!(
        text(
            &request,
            "SELECT count(*)::text FROM pg_temp.test_sqlobj_token"
        )
        .await,
        "1"
    );
    assert_eq!(
        f.admin_rows("SELECT name FROM public.test_sqlobj_token")
            .await,
        vec![vec![Some("abc".to_owned())]]
    );
    drop(request);
    f.finish().await;
}

/// install is the ROLES-PUBLISH follow-up)
/// A privilege granted by hand to a role the request role reaches (INHERIT, or SET only) on the
/// restricted table or an unlisted function fails Access naming object, role, privilege and the
/// holder; a broad table grant to the request role itself and PUBLIC grants (also column grants)
/// are revoked by the reconciliation, which leaves exactly the declared policy.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn spec_ormx_052_effective_privileges_through_membership() {
    let mut f = Fixture::new("request").await;
    let registry = build(objects(&[SqlBinding::Request]));
    f.sync(Arc::clone(&registry)).await.unwrap();
    let clean = f.acls().await;
    let helper = f.role("helper");
    let (r, x) = (quote(f.request), quote(&helper));
    let leak = |object: &str, what: &str| {
        format!(
            "SQL object 'test_sqlobj/{object}': role '{}' has forbidden {what} (held by role \
             '{helper}', which it reaches through membership)",
            f.request
        )
    };
    let table = leak(
        "grant_token_request",
        "SELECT on table \"test_sqlobj_token\"",
    );
    f.admin(&format!(
        "CREATE ROLE {x} NOLOGIN; GRANT SELECT ON test_sqlobj_token TO {x}; GRANT {x} TO {r}"
    ))
    .await;
    refused(
        f.sync(Arc::clone(&registry)).await,
        ErrorKind::Access,
        &table,
    );
    f.admin(&format!(
        "REVOKE {x} FROM {r}; GRANT {x} TO {r} WITH INHERIT FALSE, SET TRUE"
    ))
    .await;
    refused(
        f.sync(Arc::clone(&registry)).await,
        ErrorKind::Access,
        &table,
    );
    f.admin(&format!(
        "REVOKE SELECT ON test_sqlobj_token FROM {x}; \
         GRANT EXECUTE ON FUNCTION public.sqlobj_unlisted() TO {x}"
    ))
    .await;
    let before = f.acls().await;
    refused(
        f.sync(Arc::clone(&registry)).await,
        ErrorKind::Access,
        &leak(
            "fn_unlisted",
            "EXECUTE on function \"public\".\"sqlobj_unlisted\"()",
        ),
    );
    assert_eq!(f.acls().await, before, "a refused sync changes no grant");

    // what the reconciliation itself takes away
    f.admin(&format!(
        "REVOKE EXECUTE ON FUNCTION public.sqlobj_unlisted() FROM {x}; \
         GRANT SELECT, INSERT, UPDATE, DELETE ON ALL TABLES IN SCHEMA public TO {r}; \
         GRANT EXECUTE ON FUNCTION public.sqlobj_unlisted() TO PUBLIC; \
         GRANT SELECT (name) ON test_sqlobj_token TO PUBLIC"
    ))
    .await;
    f.sync(Arc::clone(&registry)).await.unwrap();
    assert_eq!(f.acls().await, clean);
    let request = f.login(f.request).await;
    denied(&request, "SELECT public.sqlobj_unlisted()").await;
    denied(&request, "SELECT name FROM test_sqlobj_token").await;
    denied(&request, "UPDATE test_sqlobj_token SET note = note").await;
    request
        .batch_execute("INSERT INTO test_sqlobj_token (name) VALUES ('ok')")
        .await
        .unwrap();
    drop(request);
    f.finish().await;
}

/// An object naming an unset binding fails Programming; a missing, LOGIN, BYPASSRLS, CREATEROLE
/// or superuser-reaching role, two bindings on one role, and bound roles that reach each other
/// fail Access, all before any DDL; reserved and invalid role names are refused by the shared
/// grantee checks.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn spec_ormx_052_unsafe_bindings_fail_before_any_ddl() {
    let mut f = Fixture::new("request").await;
    let registry = build(objects(&[SqlBinding::Request]));
    let (r, m, d) = (quote(f.request), quote(f.maintenance), quote(f.definer));
    let su = f.role("su");
    let s = quote(&su);
    let nothing_installed = || async {
        assert_eq!(
            f.admin_rows(
                "SELECT (SELECT count(*) FROM pg_proc WHERE proname LIKE 'sqlobj\\_%')::text"
            )
            .await,
            vec![vec![Some("0".to_owned())]]
        );
    };
    refused(
        f.sync_with(
            Arc::clone(&registry),
            SqlRoleBindings::new()
                .maintenance(f.maintenance)
                .request(f.request),
        )
        .await,
        ErrorKind::Programming,
        "SQL object 'test_sqlobj/fn_post': role binding 'definer' is not configured; the \
         operator names its database role in the schema sync role bindings",
    );
    let hint = "; the definer is a role the operator creates NOLOGIN NOSUPERUSER NOCREATEDB \
                NOCREATEROLE NOREPLICATION NOBYPASSRLS and grants to the schema owner WITH \
                INHERIT TRUE, SET TRUE";
    let absent = leak_str(format!("{}_absent", f.definer));
    refused(
        f.sync_with(Arc::clone(&registry), f.bindings().definer(absent))
            .await,
        ErrorKind::Access,
        &format!("SQL object role binding 'definer': role {absent:?} does not exist{hint}"),
    );
    refused(
        f.sync_with(Arc::clone(&registry), f.bindings().maintenance(f.request))
            .await,
        ErrorKind::Access,
        &format!(
            "SQL object role bindings 'maintenance' and 'request' name the same role {:?}; each \
             binding needs a role of its own",
            f.request
        ),
    );
    let attribute_cases = [
        (
            format!("ALTER ROLE {d} LOGIN"),
            format!("ALTER ROLE {d} NOLOGIN"),
            format!(
                "SQL object role binding 'definer': role {:?} can log in; the definer must be a \
                 NOLOGIN role{hint}",
                f.definer
            ),
        ),
        (
            format!("ALTER ROLE {m} BYPASSRLS"),
            format!("ALTER ROLE {m} NOBYPASSRLS"),
            format!(
                "SQL object role binding 'maintenance': role {:?} has BYPASSRLS; no SQL object \
                 role binding may have it",
                f.maintenance
            ),
        ),
        (
            format!("ALTER ROLE {r} CREATEROLE"),
            format!("ALTER ROLE {r} NOCREATEROLE"),
            format!(
                "SQL object role binding 'request': role {:?} has CREATEROLE: it can give itself \
                 the membership of other roles whatever this grants, so the SQL object request \
                 role must not have it",
                f.request
            ),
        ),
        (
            format!("CREATE ROLE {s} SUPERUSER NOLOGIN; GRANT {s} TO {r}"),
            format!("REVOKE {s} FROM {r}"),
            format!(
                "SQL object role binding 'request': role {:?} is a member of {su:?}, a superuser \
                 role: it can act as a superuser whatever this grants, so the SQL object request \
                 role must not reach it",
                f.request
            ),
        ),
        (
            format!("GRANT {d} TO {r}"),
            format!("REVOKE {d} FROM {r}"),
            format!(
                "SQL object role binding 'request': role {:?} is a member of role {:?} of \
                 binding 'definer'; the bound roles must not reach each other",
                f.request, f.definer
            ),
        ),
        (
            format!("GRANT {m} TO {r} WITH INHERIT FALSE, SET FALSE"),
            format!("REVOKE {m} FROM {r}"),
            format!(
                "SQL object role binding 'request': role {:?} is a member of role {:?} of \
                 binding 'maintenance'; the bound roles must not reach each other",
                f.request, f.maintenance
            ),
        ),
    ];
    for (setup, undo, message) in attribute_cases {
        f.admin(&setup).await;
        refused(
            f.sync(Arc::clone(&registry)).await,
            ErrorKind::Access,
            &message,
        );
        f.admin(&undo).await;
        nothing_installed().await;
    }
    // hostile names never reach a statement: reserved, NUL, overlong
    let long = leak_str("r".repeat(64));
    for name in ["public", "PUBLIC", "none", "pg_monitor", "a\0b", long] {
        let error = f
            .sync_with(Arc::clone(&registry), f.bindings().request(name))
            .await
            .expect_err(name);
        assert_eq!(error.kind(), ErrorKind::Access, "{name:?}: {error:?}");
        assert!(
            error
                .message()
                .starts_with("SQL object role binding 'request': "),
            "{name:?}: {error:?}"
        );
    }
    nothing_installed().await;
    // all bindings safe: the same sync installs
    f.sync(registry).await.unwrap();
    f.finish().await;
}

/// A request role whose name holds a quote, a semicolon and a comment marker is bound, granted
/// and logged in as like any other.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn spec_ormx_051_hostile_role_names_are_quoted() {
    let f = Fixture::new("q\"x; DROP TABLE res_users; --").await;
    f.sync(build(objects(&[SqlBinding::Request])))
        .await
        .unwrap();
    let request = f.login(f.request).await;
    assert_eq!(
        text(&request, "SELECT public.sqlobj_listed()::text").await,
        "1"
    );
    denied(&request, "SELECT public.sqlobj_unlisted()").await;
    assert_eq!(
        f.admin_rows("SELECT (to_regclass('res_users') IS NOT NULL)::text")
            .await,
        vec![vec![Some("true".to_owned())]]
    );
    drop(request);
    f.finish().await;
}

/// A second sync leaves owners and ACLs as they were; a binding the declaration no longer lists
/// loses its EXECUTE.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn spec_ormx_051_reconciliation_is_idempotent_and_revokes_removed_grants() {
    let f = Fixture::new("request").await;
    f.sync(build(objects(&[SqlBinding::Request])))
        .await
        .unwrap();
    let first = f.acls().await;
    f.sync(build(objects(&[SqlBinding::Request])))
        .await
        .unwrap();
    assert_eq!(f.acls().await, first);
    let request = f.login(f.request).await;
    assert_eq!(
        text(&request, "SELECT public.sqlobj_listed()::text").await,
        "1"
    );
    f.sync(build(objects(&[]))).await.unwrap();
    denied(&request, "SELECT public.sqlobj_listed()").await;
    drop(request);
    f.finish().await;
}

/// non-superuser schema owner)
/// `ALTER FUNCTION … OWNER TO` the definer needs the schema owner's SET membership in the
/// definer and the definer's CREATE on the schema, which [`sqlobj::transfer_to_definer`] gives for
/// the one statement and takes back; `CREATE OR REPLACE` of the definer-owned function needs the
/// INHERIT membership.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn spec_ormx_052_definer_transfer_needs_membership_and_schema_create() {
    let mut f = Fixture::new("request").await;
    let registry = build(vec![function(
        "fn_probe",
        int_fn("sqlr_probe", "SELECT 1").security(SqlSecurity::Definer),
    )]);
    let SqlObjectKind::Function(probe) = registry
        .sql_objects()
        .get(ADDON, "fn_probe")
        .unwrap()
        .kind()
    else {
        panic!("fn_probe is a function");
    };
    let owner = f.role("owner");
    let (o, d) = (quote(&owner), quote(f.definer));
    let database = quote(f.h.db().name());
    let client = f.h.db().client().await.unwrap();
    let create = "CREATE FUNCTION public.sqlr_probe() RETURNS integer LANGUAGE sql SECURITY \
                  DEFINER AS 'SELECT 1'";
    client
        .batch_execute(&format!(
            "CREATE ROLE {o} NOLOGIN; ALTER DATABASE {database} OWNER TO {o}; \
             ALTER SCHEMA public OWNER TO pg_database_owner; SET ROLE {o}; {create}; RESET ROLE"
        ))
        .await
        .unwrap();
    let as_owner = |sql: String| {
        let client = &client;
        let o = o.clone();
        async move {
            let result = client.batch_execute(&format!("SET ROLE {o}; {sql}")).await;
            client.batch_execute("RESET ROLE").await.unwrap();
            result
        }
    };
    let transfer = sqlobj::transfer_to_definer(probe, &d);
    let plain = format!("ALTER FUNCTION public.sqlr_probe() OWNER TO {d}");
    let replace = "CREATE OR REPLACE FUNCTION public.sqlr_probe() RETURNS integer LANGUAGE sql \
                   SECURITY DEFINER AS 'SELECT 2'"
        .to_owned();
    let code = |r: Result<(), tokio_postgres::Error>| r.expect_err("must fail").code().cloned();
    // no membership: cannot hand the function over
    assert_eq!(
        code(as_owner(plain.clone()).await),
        Some(SqlState::INSUFFICIENT_PRIVILEGE)
    );
    // SET membership without INHERIT: the definer still needs CREATE on the schema
    client
        .batch_execute(&format!("GRANT {d} TO {o} WITH INHERIT FALSE, SET TRUE"))
        .await
        .unwrap();
    assert_eq!(
        code(as_owner(plain).await),
        Some(SqlState::INSUFFICIENT_PRIVILEGE)
    );
    as_owner(transfer).await.unwrap();
    let state = "SELECT pg_get_userbyid(proowner)::text FROM pg_proc WHERE proname = \
                 'sqlr_probe'";
    assert_eq!(text(&client, state).await, f.definer);
    assert_eq!(
        text(
            &client,
            &format!(
                "SELECT has_schema_privilege({}, 'public', 'CREATE')::text",
                erp_sql::literal(f.definer)
            )
        )
        .await,
        "false"
    );
    // replacing the definer-owned body needs the INHERIT membership
    assert_eq!(
        code(as_owner(replace.clone()).await),
        Some(SqlState::INSUFFICIENT_PRIVILEGE)
    );
    client
        .batch_execute(&format!(
            "REVOKE {d} FROM {o}; GRANT {d} TO {o} WITH INHERIT TRUE, SET TRUE"
        ))
        .await
        .unwrap();
    as_owner(replace).await.unwrap();
    assert_eq!(text(&client, state).await, f.definer);
    assert_eq!(text(&client, "SELECT public.sqlr_probe()::text").await, "2");
    let admin = text(&client, "SELECT session_user::text").await;
    client
        .batch_execute(&format!(
            "ALTER DATABASE {database} OWNER TO {}",
            quote(&admin)
        ))
        .await
        .unwrap();
    drop(client);
    f.finish().await;
}
