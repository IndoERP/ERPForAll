// SPDX-License-Identifier: MIT OR Apache-2.0
//! Test-only C4 probe. No product model or financial default data is installed.
#![allow(clippy::unwrap_used, clippy::expect_used, reason = "test fixture")]

use std::sync::Arc;

use erp_fields::{Error, ErrorKind};
use erp_orm::{Orm, SUPERUSER_ID, SchemaSyncOptions, Signal, SqlRoleBindings, StaticPolicy};
use erp_registry::*;
use erp_sdk::Transaction;
use erp_test_models::base_users;
use erp_testkit::Harness;
use tokio_postgres::{Client, Config, NoTls};

const ADDON: &str = "test_anl_probe";
const OWNER: &str = "test_anl_probe.owner";
const ACCOUNT: &str = "test_anl_probe.account";
const MEMBER: &str = "test_anl_probe.member";
const MARKER: &str = "test_anl_probe.marker";
pub const MESSAGE: &str = "Analytic probe membership is invalid.";
pub const CONDITION: &str = "anl_probe_membership";
pub const STATE: &str = "ERP90";

static ADDON_DECL: AddonDescriptor = AddonDescriptor {
    name: ADDON,
    depends: &["base"],
    exact_only: false,
};

fn leak<T>(value: T) -> &'static T {
    Box::leak(Box::new(value))
}

fn slice<T>(value: Vec<T>) -> &'static [T] {
    Box::leak(value.into_boxed_slice())
}

fn function(key: &'static str, body: &'static str) -> SqlObjectDecl {
    SqlObjectDecl::function(
        ADDON,
        key,
        SqlFunctionDecl::new(key, SqlLanguage::PlPgSql, SqlResult::Trigger, body)
            .security(SqlSecurity::Definer)
            .deps(&[
                SqlDep::Model(OWNER),
                SqlDep::Model(ACCOUNT),
                SqlDep::Model(MEMBER),
                SqlDep::Model(MARKER),
            ]),
    )
}

// The queued NEW record is authoritative for which owner needs checking. Re-reading the marker
// table would permit a deletion to erase the pending check. Final membership, however, is read
// from the current relations so a temporary gap repaired by a later statement is legitimate.
const CHECK: &str = r"
BEGIN
    IF EXISTS (SELECT FROM public.test_anl_probe_owner o WHERE o.id::text = NEW.owner_key)
       AND (NOT EXISTS (SELECT FROM public.test_anl_probe_member m
                        WHERE m.owner_id::text = NEW.owner_key)
            OR EXISTS (SELECT FROM public.test_anl_probe_member m
                       LEFT JOIN public.test_anl_probe_account a ON a.id = m.account_id
                       WHERE m.owner_id::text = NEW.owner_key
                         AND (a.id IS NULL OR a.root IS DISTINCT FROM m.root))) THEN
        RAISE EXCEPTION USING ERRCODE = 'ERP90', CONSTRAINT = 'anl_probe_membership',
            MESSAGE = 'private probe diagnostic', DETAIL = 'private member detail',
            HINT = 'private member hint';
    END IF;
    DELETE FROM public.test_anl_probe_marker WHERE id = NEW.id;
    RETURN NULL;
END";

fn objects(schedule: bool) -> Vec<SqlObjectDecl> {
    let mut objects = vec![
        function("anl_probe_check", CHECK),
        function(
            "anl_probe_stamp",
            "BEGIN NEW.xact := pg_current_xact_id()::text; RETURN NEW; END",
        ),
        SqlObjectDecl::condition(
            ADDON,
            "membership_condition",
            SqlConditionDecl::new(STATE, CONDITION, MESSAGE).kind(SqlConditionKind::Validation),
        ),
        SqlObjectDecl::trigger(
            ADDON,
            "marker_check",
            SqlTriggerDecl::new(
                "anl_probe_deferred",
                MARKER,
                (ADDON, "anl_probe_check"),
                SqlTiming::After,
                &[SqlEvent::Insert],
                SqlLevel::Row,
            )
            .constraint(SqlDeferral::InitiallyDeferred),
        ),
        SqlObjectDecl::trigger(
            ADDON,
            "marker_stamp",
            SqlTriggerDecl::new(
                "anl_probe_stamp",
                MARKER,
                (ADDON, "anl_probe_stamp"),
                SqlTiming::Before,
                &[SqlEvent::Insert],
                SqlLevel::Row,
            ),
        ),
    ];
    // Scalar text preserves the exact bigint owner identity without an FK that would cascade
    // pending markers away when an owner is deleted. No financial amount lives in this fixture.
    // Fresh markers per statement deliberately avoid transaction-wide deduplication: SET
    // CONSTRAINTS may drain earlier events before a later mutation of the same owner.
    if schedule {
        for (model, prefix, column) in [
            (OWNER, "owner", "id"),
            (MEMBER, "member", "owner_id"),
            (ACCOUNT, "account", "id"),
        ] {
            for (event, suffix) in [
                (SqlEvent::Insert, "insert"),
                (SqlEvent::Update, "update"),
                (SqlEvent::Delete, "delete"),
            ] {
                let key: &'static str =
                    Box::leak(format!("anl_probe_{prefix}_{suffix}").into_boxed_str());
                let ids = match event {
                    SqlEvent::Insert => format!("SELECT {column} FROM n"),
                    SqlEvent::Update => {
                        format!("SELECT {column} FROM o UNION SELECT {column} FROM n")
                    }
                    SqlEvent::Delete => format!("SELECT {column} FROM o"),
                    SqlEvent::Truncate => unreachable!("fixture events above are closed"),
                };
                let affected = if model == ACCOUNT {
                    format!(
                        "SELECT DISTINCT m.owner_id::text FROM public.test_anl_probe_member m WHERE m.account_id IN ({ids})"
                    )
                } else {
                    format!(
                        "SELECT DISTINCT affected_id::text FROM ({ids}) AS affected(affected_id)"
                    )
                };
                let body = Box::leak(format!("BEGIN INSERT INTO public.test_anl_probe_marker (owner_key) {affected}; RETURN NULL; END").into_boxed_str());
                objects.push(function(key, body));
                let trigger = SqlTriggerDecl::new(
                    key,
                    model,
                    (ADDON, key),
                    SqlTiming::After,
                    slice(vec![event]),
                    SqlLevel::Statement,
                );
                let trigger = match event {
                    SqlEvent::Insert => trigger.new_table("n"),
                    SqlEvent::Update => trigger.old_table("o").new_table("n"),
                    SqlEvent::Delete => trigger.old_table("o"),
                    SqlEvent::Truncate => unreachable!("fixture events above are closed"),
                };
                objects.push(SqlObjectDecl::trigger(
                    ADDON,
                    Box::leak(format!("{prefix}_{suffix}_trigger").into_boxed_str()),
                    trigger,
                ));
            }
        }
    }
    for (model, prefix) in [
        (OWNER, "owner"),
        (ACCOUNT, "account"),
        (MEMBER, "member"),
        (MARKER, "marker"),
    ] {
        let all = &[
            SqlTableOp::Select,
            SqlTableOp::Insert,
            SqlTableOp::Update,
            SqlTableOp::Delete,
        ];
        let definer_ops: &[SqlTableOp] = if model == MARKER {
            &[SqlTableOp::Select, SqlTableOp::Insert, SqlTableOp::Delete]
        } else {
            &[SqlTableOp::Select]
        };
        for (binding, suffix, ops) in [
            (SqlBinding::Definer, "definer", definer_ops),
            (
                SqlBinding::Request,
                "request",
                if model == MARKER {
                    &[SqlTableOp::Insert]
                } else {
                    all.as_slice()
                },
            ),
        ] {
            objects.push(SqlObjectDecl::table_grant(
                ADDON,
                Box::leak(format!("{prefix}_{suffix}_grant").into_boxed_str()),
                SqlTableGrantDecl::new(model, binding, slice(ops.to_vec())),
            ));
        }
    }
    objects
}

fn registry(schedule: bool) -> Arc<Registry> {
    let models = vec![
        ModelDecl::new(ADDON, OWNER)
            .log_access(false)
            .fields(const { &[FieldDecl::char("name")] }),
        ModelDecl::new(ADDON, ACCOUNT)
            .log_access(false)
            .fields(const { &[FieldDecl::integer("root").required(true)] }),
        ModelDecl::new(ADDON, MEMBER).log_access(false).fields(
            const {
                &[
                    FieldDecl::many2one("owner_id", OWNER)
                        .required(true)
                        .ondelete(erp_fields::OnDelete::Cascade),
                    FieldDecl::many2one("account_id", ACCOUNT).required(true),
                    FieldDecl::integer("root").required(true),
                ]
            },
        ),
        ModelDecl::new(ADDON, MARKER).log_access(false).fields(
            const {
                &[
                    FieldDecl::char("owner_key").required(true),
                    FieldDecl::char("xact").required(true),
                ]
            },
        ),
    ];
    let objects = objects(schedule);
    let keys = slice(objects.iter().map(SqlObjectDecl::key).collect());
    Arc::new(
        RegistryBuilder::new([&base_users::ADDON, &ADDON_DECL])
            .linked(false)
            .with_decls(models.into_iter().map(leak))
            .require_sql_objects([leak(SqlObjectManifest::new(ADDON, keys))])
            .with_sql_objects(objects.into_iter().map(leak))
            .build()
            .unwrap(),
    )
}

// Like sql_objects_security.rs, keep role cleanup alive until before the database drops, including
// panic paths. These are generated disposable fixture credentials, never an installation secret.
struct Roles {
    config: Config,
    names: Vec<String>,
}
impl Drop for Roles {
    fn drop(&mut self) {
        let config = self.config.clone();
        let names = std::mem::take(&mut self.names);
        let worker = std::thread::spawn(move || {
            tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(async {
                let (client, connection) = config.connect(NoTls).await.unwrap();
                tokio::spawn(connection);
                client.batch_execute("SET lock_timeout = '60s'").await.unwrap();
                for name in names {
                    let q = erp_sql::quote(&name);
                    client.batch_execute(&format!("REASSIGN OWNED BY {q} TO CURRENT_USER; DROP OWNED BY {q}; DROP ROLE {q}")).await.unwrap();
                }
            });
        });
        worker.join().expect("probe role cleanup");
    }
}

pub struct Probe {
    // Drop ORM's pool before role cleanup, and roles before the harness's database.
    pub orm: Orm,
    _roles: Roles,
    pub h: Harness,
    pub request: String,
    pub definer: String,
    maintenance: String,
    password: String,
    registry: Arc<Registry>,
}

impl Probe {
    pub async fn new(schedule: bool) -> Self {
        let h = Harness::builder([&base_users::ADDON])
            .build()
            .await
            .unwrap();
        let mut tx = h.begin().await.unwrap();
        tx.install_signal().await.unwrap();
        tx.commit().await.unwrap();
        drop(tx);
        let client = h.db().client().await.unwrap();
        let version: i32 = client
            .query_one("SELECT current_setting('server_version_num')::int", &[])
            .await
            .unwrap()
            .get(0);
        assert_eq!(version / 10000, 18, "probe requires PostgreSQL 18");
        println!("probe PostgreSQL server_version_num={version}");
        let tag: i32 = client
            .query_one(
                "SELECT oid::int FROM pg_database WHERE datname = current_database()",
                &[],
            )
            .await
            .unwrap()
            .get(0);
        let request = format!("anlp_{tag}_request");
        let definer = format!("anlp_{tag}_definer");
        let maintenance = format!("anlp_{tag}_maint");
        let owner = format!("anlp_{tag}_schema");
        let password = format!("fixture-anlp-{tag}");
        let r = erp_sql::quote(&request);
        let d = erp_sql::quote(&definer);
        let m = erp_sql::quote(&maintenance);
        let pw = erp_sql::literal(&password);
        let o = erp_sql::quote(&owner);
        let database = erp_sql::quote(h.db().name());
        client.batch_execute(&format!("CREATE ROLE {r} LOGIN PASSWORD {pw}; CREATE ROLE {d} NOLOGIN; CREATE ROLE {m} LOGIN PASSWORD {pw}; GRANT USAGE ON SCHEMA public TO {r}, {d}, {m}")).await.unwrap();
        let roles = Roles {
            config: h.db().config(),
            names: vec![
                definer.clone(),
                request.clone(),
                maintenance.clone(),
                owner.clone(),
            ],
        };
        client.batch_execute(&format!("CREATE ROLE {o} NOLOGIN; ALTER DATABASE {database} OWNER TO {o}; ALTER SCHEMA public OWNER TO pg_database_owner; GRANT {d} TO {o} WITH INHERIT TRUE, SET TRUE; {}", reown_public(&owner))).await.unwrap();
        let signal = Signal::read(&client).await.unwrap();
        drop(client);
        let registry = registry(schedule);
        let mut config = h.db().config();
        config.options(format!("-c role={owner}"));
        let pool = erp_orm::pool::create_pool(config, 2).unwrap();
        let orm = Orm::builder(Arc::clone(&registry), pool)
            .policy(StaticPolicy::AllowAll)
            .signal(Signal::new(signal))
            .build()
            .unwrap();
        let f = Self {
            orm,
            _roles: roles,
            h,
            request,
            definer,
            maintenance,
            password,
            registry,
        };
        f.sync().await;
        f
    }

    pub async fn sync(&self) {
        let mut tx = self.begin().await;
        let options = SchemaSyncOptions::inline().lifecycle(true).sql_roles(
            SqlRoleBindings::new()
                .request_owned(self.request.clone())
                .definer_owned(self.definer.clone())
                .maintenance_owned(self.maintenance.clone()),
        );
        assert!(
            tx.init_models_with(&[], &options)
                .await
                .unwrap()
                .is_complete()
        );
        tx.commit().await.unwrap();
    }

    pub async fn begin(&self) -> Transaction {
        self.orm.begin(SUPERUSER_ID).await.unwrap()
    }

    pub async fn login(&self) -> Client {
        let mut config = self.h.db().config();
        config.user(&self.request).password(&self.password);
        let (client, connection) = config.connect(NoTls).await.unwrap();
        tokio::spawn(connection);
        client
    }

    pub async fn scalar(&self, sql: &str) -> i64 {
        self.h
            .db()
            .client()
            .await
            .unwrap()
            .query_one(sql, &[])
            .await
            .unwrap()
            .get(0)
    }

    pub async fn empty_markers(&self) {
        assert_eq!(
            self.scalar("SELECT count(*) FROM test_anl_probe_marker")
                .await,
            0
        );
    }

    pub async fn assert_definer_privileges(&self) {
        let client = self.h.db().client().await.unwrap();
        let rows = client.query(
            "SELECT count(*) FROM (VALUES ('test_anl_probe_owner', 'INSERT'), \
             ('test_anl_probe_owner', 'UPDATE'), ('test_anl_probe_owner', 'DELETE'), \
             ('test_anl_probe_account', 'INSERT'), ('test_anl_probe_account', 'UPDATE'), \
             ('test_anl_probe_account', 'DELETE'), ('test_anl_probe_member', 'INSERT'), \
             ('test_anl_probe_member', 'UPDATE'), ('test_anl_probe_member', 'DELETE'), \
             ('test_anl_probe_marker', 'UPDATE'), ('test_anl_probe_marker', 'TRUNCATE')) \
             AS forbidden(relation, operation) WHERE has_table_privilege($1::text, relation, operation)",
            &[&self.definer],
        ).await.unwrap();
        assert_eq!(rows[0].get::<_, i64>(0), 0);
        let row = client.query_one(
            "SELECT NOT rolsuper AND NOT rolcanlogin AND NOT rolbypassrls FROM pg_roles WHERE rolname = $1", &[&self.definer],
        ).await.unwrap();
        assert!(row.get::<_, bool>(0));
    }

    pub async fn seed(&self) {
        let mut tx = self.begin().await;
        tx.batch("INSERT INTO test_anl_probe_account (id, root) VALUES (1, 1), (2, 2); INSERT INTO test_anl_probe_owner (id, name) VALUES (1, 'one'), (2, 'two'); INSERT INTO test_anl_probe_member (id, owner_id, account_id, root) VALUES (1, 1, 1, 1), (2, 2, 2, 2)").await.unwrap();
        tx.commit().await.unwrap();
        self.empty_markers().await;
    }

    pub fn assert_invalid(&self, error: Error) {
        assert_eq!(error.kind(), ErrorKind::Validation, "{error:?}");
        assert_eq!(error.message(), MESSAGE);
        assert_eq!(error.sqlstate(), Some(STATE));
        assert_eq!(error.constraint(), Some(CONDITION));
        let exposed = format!("{error} {:?}", error.context());
        for private in [
            "private probe diagnostic",
            "private member detail",
            "private member hint",
        ] {
            assert!(
                !exposed.contains(private),
                "private diagnostic leaked: {exposed}"
            );
        }
    }

    pub fn object_count(&self) -> usize {
        // schema_sqlobj.rs persists installation provenance for objects, while TableGrant
        // policies are reconciled separately. Match that documented catalog boundary, not the
        // larger declaration graph. Their effective privileges are tested as the request role.
        self.registry
            .sql_objects()
            .objects()
            .iter()
            .filter(|object| !matches!(object.kind(), SqlObjectKind::TableGrant(_)))
            .count()
    }
}

// Same isolated schema-owner setup as sql_objects_security.rs.
fn reown_public(owner: &str) -> String {
    let owner = erp_sql::literal(owner);
    format!(
        "DO $$ DECLARE r record; me oid := (SELECT oid FROM pg_roles WHERE rolname = current_user);
         BEGIN
           FOR r IN SELECT c.oid::regclass AS t, c.relkind FROM pg_class c
                     WHERE c.relnamespace = 'public'::regnamespace AND c.relowner = me
                       AND c.relkind IN ('r', 'p', 'v', 'm', 'f') LOOP
             EXECUTE format('ALTER %s %s OWNER TO %I', CASE r.relkind WHEN 'v' THEN 'VIEW'
               WHEN 'm' THEN 'MATERIALIZED VIEW' WHEN 'f' THEN 'FOREIGN TABLE' ELSE 'TABLE' END,
               r.t, {owner});
           END LOOP;
           FOR r IN SELECT c.oid::regclass AS t FROM pg_class c
                     WHERE c.relnamespace = 'public'::regnamespace AND c.relowner = me
                       AND c.relkind = 'S' LOOP
             EXECUTE format('ALTER SEQUENCE %s OWNER TO %I', r.t, {owner});
           END LOOP;
           FOR r IN SELECT p.oid::regprocedure AS f FROM pg_proc p
                     WHERE p.pronamespace = 'public'::regnamespace AND p.proowner = me LOOP
             EXECUTE format('ALTER ROUTINE %s OWNER TO %I', r.f, {owner});
           END LOOP;
         END $$"
    )
}
