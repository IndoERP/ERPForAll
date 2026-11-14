//! Integration tests for user settings and durable preferences.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

mod company_rows_support;
mod users_support;

use std::sync::Arc;

use company_rows_support::UserKind::{External, Internal};
use company_rows_support::fixture_user;
use erp_orm::{Orm, PolicyProvider, UserId};
use erp_sdk::{Domain, ErrorKind, SearchOptions, Transaction, Value, ValueMap};
use erp_security::resolver::Resolver;
use erp_security::source::PolicySource;
use erp_testkit::Harness;
use erp_testkit::tokio_postgres::types::ToSql;
use erp_testkit::tokio_postgres::{Client, NoTls};
use users_support::{harness, map, s};

async fn connect(h: &Harness) -> Client {
    let (client, connection) = h.db().config().connect(NoTls).await.unwrap();
    tokio::spawn(async move {
        if let Err(error) = connection.await {
            eprintln!("user_settings: connection closed with an error: {error}");
        }
    });
    client
}

async fn one(client: &Client, sql: &str, params: &[&(dyn ToSql + Sync)]) -> i64 {
    client.query_one(sql, params).await.unwrap().get(0)
}

async fn exec(client: &Client, sql: &str, params: &[&(dyn ToSql + Sync)]) {
    client.execute(sql, params).await.unwrap();
}

fn pool(h: &Harness, size: usize) -> erp_testkit::deadpool_postgres::Pool {
    erp_orm::pool::create_pool(h.db().config(), size).unwrap()
}

async fn production_orm(h: &Harness) -> Orm {
    let source = PolicySource::build(Arc::clone(h.registry()), pool(h, 2))
        .await
        .unwrap();
    let provider: Arc<dyn PolicyProvider> = Arc::new(source);
    Orm::builder(Arc::clone(h.registry()), pool(h, 2))
        .policy(provider)
        .resolver(Resolver::id_order())
        .build()
        .unwrap()
}

struct Fixture {
    h: Harness,
    client: Client,
    orm: Orm,
    u1: i64,
    u2: i64,
    /// Has no settings row.
    u3: i64,
    /// Member of `base.group_public`.
    public_user: i64,
}

/// Two internal users and the two `ir.access` rows of `base` for the model that matter to them:
/// `base.group_user` gets create/read/write/delete on the rows whose `user_id` is its own user.
async fn fixture() -> Fixture {
    let h = harness().await;
    let client = connect(&h).await;
    let internal = one(
        &client,
        "INSERT INTO res_groups (name) VALUES ('Internal User') RETURNING id::int8",
        &[],
    )
    .await;
    exec(
        &client,
        "INSERT INTO ir_model_data (module, name, model, res_id) \
         VALUES ('base', 'group_user', 'res.groups', $1)",
        &[&internal],
    )
    .await;
    let system = one(
        &client,
        "INSERT INTO res_groups (name) VALUES ('Administrator') RETURNING id::int8",
        &[],
    )
    .await;
    exec(
        &client,
        "INSERT INTO ir_model_data (module, name, model, res_id) \
         VALUES ('base', 'group_system', 'res.groups', $1)",
        &[&system],
    )
    .await;
    let public = one(
        &client,
        "INSERT INTO res_groups (name) VALUES ('Public') RETURNING id::int8",
        &[],
    )
    .await;
    exec(
        &client,
        "INSERT INTO ir_model_data (module, name, model, res_id) \
         VALUES ('base', 'group_public', 'res.groups', $1)",
        &[&public],
    )
    .await;
    let model = one(
        &client,
        "INSERT INTO ir_model (name, model, state, transient) \
         VALUES ('User Settings', 'res.users.settings', 'base', false) RETURNING id::int8",
        &[],
    )
    .await;
    // the same row as addons/base/security/ir.access.csv (access_users_settings_self_internal)
    exec(
        &client,
        "INSERT INTO ir_access (name, model_id, group_id, operation, domain, active) \
         VALUES ('own settings', $1, $2, 'crud', $3, true)",
        &[&model, &internal, &"[('user_id', '=', user.id)]"],
    )
    .await;
    // installation emits the generated company restrictions; this database has no installation
    company_rows_support::insert_generated(
        &client,
        h.registry(),
        &company_rows_support::PARTNER_AND_USERS,
    )
    .await;
    let company = company_rows_support::fixture_company(&client).await;
    let u1 = fixture_user(&client, "user_order_1", &[internal], Internal, company).await;
    let u2 = fixture_user(&client, "user_order_2", &[internal], Internal, company).await;
    let u3 = fixture_user(&client, "user_order_3", &[internal], Internal, company).await;
    let public_user = fixture_user(&client, "user_public", &[public], External, company).await;
    let orm = production_orm(&h).await;
    Fixture {
        h,
        client,
        orm,
        u1,
        u2,
        u3,
        public_user,
    }
}

impl Fixture {
    /// Every transaction lives in its own call: a transaction keeps its pooled connection until it
    /// is dropped, and a shadowed `tx` binding is not dropped (the pool holds two connections).
    async fn call(
        &self,
        uid: i64,
        method: &'static str,
        args: ValueMap,
    ) -> erp_sdk::Result<ValueMap> {
        let mut tx: Transaction = self.orm.begin(UserId::new(uid).unwrap()).await.unwrap();
        let model = tx.env().records("res.users.settings").unwrap();
        let out = if args.is_empty() {
            erp_orm::call_method::<_, ValueMap>(&mut tx, &model, method, ()).await
        } else {
            erp_orm::call_method::<_, ValueMap>(&mut tx, &model, method, (args,)).await
        };
        if out.is_ok() {
            tx.commit().await.unwrap();
        }
        out
    }

    async fn get(&self, uid: i64) -> Value {
        self.call(uid, "get_res_users_settings", ValueMap::new())
            .await
            .unwrap()["homemenu_config"]
            .clone()
    }

    async fn set(&self, uid: i64, config: Value) -> erp_sdk::Result<ValueMap> {
        let mut args = ValueMap::new();
        args.insert("homemenu_config".to_string(), config);
        self.call(uid, "set_res_users_settings", args).await
    }

    async fn finish(self) {
        let Fixture { h, client, orm, .. } = self;
        drop(orm);
        drop(client);
        h.cleanup().await.unwrap();
    }
}

#[tokio::test]
async fn test_user_settings_durable_application_order_cross_transaction() {
    let f = fixture().await;
    assert_eq!(f.get(f.u1).await, Value::False);
    assert_eq!(
        f.set(f.u1, Value::from("[12, 10, 11]")).await.unwrap()["homemenu_config"],
        Value::str("[12, 10, 11]")
    );
    // a later transaction reads the persisted order; a second set updates the same row
    assert_eq!(f.get(f.u1).await, Value::str("[12, 10, 11]"));
    f.set(f.u1, Value::from("[10, 11, 12]")).await.unwrap();
    assert_eq!(f.get(f.u1).await, Value::str("[10, 11, 12]"));
    // the other user has nothing, and setting theirs leaves the first user's order alone
    assert_eq!(f.get(f.u2).await, Value::False);
    f.set(f.u2, Value::from("[1]")).await.unwrap();
    assert_eq!(f.get(f.u1).await, Value::str("[10, 11, 12]"));
    assert_eq!(f.get(f.u2).await, Value::str("[1]"));
    // one row per user, owned (create_uid) by that user, written through the ORM
    let rows = f
        .client
        .query(
            "SELECT user_id::int8, create_uid::int8, write_uid::int8, \
                    (write_date IS NOT NULL) FROM res_users_settings ORDER BY user_id",
            &[],
        )
        .await
        .unwrap();
    assert_eq!(rows.len(), 2);
    for (row, uid) in rows.iter().zip([f.u1, f.u2]) {
        assert_eq!(row.get::<_, i64>(0), uid);
        assert_eq!(row.get::<_, i64>(1), uid);
        assert_eq!(row.get::<_, i64>(2), uid);
        assert!(row.get::<_, bool>(3));
    }
    // refused input: oversized, not JSON, not an array of ids, wrong type; the stored order stays
    for bad in [
        Value::from("x".repeat(4097)),
        Value::from("[1,"),
        Value::from("{\"a\": 1}"),
        Value::from("[0]"),
        Value::Int(5),
    ] {
        let err = f.set(f.u1, bad).await.unwrap_err();
        assert_eq!(err.kind(), ErrorKind::Validation, "{err}");
    }
    assert_eq!(f.get(f.u1).await, Value::str("[10, 11, 12]"));
    // False clears it
    f.set(f.u1, Value::False).await.unwrap();
    assert_eq!(f.get(f.u1).await, Value::False);
    f.finish().await;
}

/// the `ir.access` row, not only through the two methods)
#[tokio::test]
async fn test_user_settings_generic_rpc_cannot_reach_another_users_row() {
    let f = fixture().await;
    f.set(f.u1, Value::from("[3, 2, 1]")).await.unwrap();
    let u1_row = one(
        &f.client,
        "SELECT id::int8 FROM res_users_settings WHERE user_id = $1",
        &[&f.u1],
    )
    .await;
    f.set(f.u2, Value::from("[9]")).await.unwrap();

    let mut tx = f.orm.begin(UserId::new(f.u2).unwrap()).await.unwrap();
    let model = tx.env().records("res.users.settings").unwrap();
    // search sees only the caller's own row
    let seen = model
        .search_read(
            &mut tx,
            &Domain::TRUE,
            &["homemenu_config"],
            &SearchOptions::new(),
        )
        .await
        .unwrap();
    assert_eq!(seen.len(), 1, "{seen:?}");
    assert_eq!(seen[0]["homemenu_config"], s("[9]"));
    // reading, changing, re-owning and deleting the first user's row is refused
    let foreign = model.browse([u1_row]).unwrap();
    assert!(foreign.read(&mut tx, &["homemenu_config"]).await.is_err());
    let err = foreign
        .write_values(&mut tx, &map(&[("homemenu_config", s("[666]"))]))
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Access, "{err}");
    assert!(foreign.unlink(&mut tx).await.is_err());
    // a row for a user who has none cannot be created by someone else, nor the own row handed over
    let err = model
        .create_values(&mut tx, &[map(&[("user_id", Value::Int(f.u3))])])
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Access, "{err}");
    drop(tx);

    // the owner field is administrator-only, and the shape rule holds on the generic write path
    let mut tx = f.orm.begin(UserId::new(f.u2).unwrap()).await.unwrap();
    let model = tx.env().records("res.users.settings").unwrap();
    let own = model.search(&mut tx, &Domain::TRUE).await.unwrap();
    let err = own
        .write_values(&mut tx, &map(&[("user_id", Value::Int(f.u3))]))
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Access, "{err}");
    drop(tx);
    let mut tx = f.orm.begin(UserId::new(f.u2).unwrap()).await.unwrap();
    let model = tx.env().records("res.users.settings").unwrap();
    let own = model.search(&mut tx, &Domain::TRUE).await.unwrap();
    let err = own
        .write_values(&mut tx, &map(&[("homemenu_config", s("not json"))]))
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Validation, "{err}");
    drop(tx);

    // nothing of the first user's row changed
    assert_eq!(f.get(f.u1).await, Value::str("[3, 2, 1]"));
    assert_eq!(f.get(f.u2).await, Value::str("[9]"));
    f.finish().await;
}

/// serialization failure, not as a `_user_uniq` violation, and its retry (as `Orm::run` does)
/// writes the saved order)
#[tokio::test]
async fn test_user_settings_concurrent_first_saves_are_retryable() {
    let f = fixture().await;
    let (a, b) = tokio::join!(
        f.set(f.u3, Value::from("[1, 2]")),
        f.set(f.u3, Value::from("[2, 1]"))
    );
    let mut lost = 0;
    for (result, config) in [(a, "[1, 2]"), (b, "[2, 1]")] {
        match result {
            Ok(_) => {}
            Err(error) => {
                lost += 1;
                assert_eq!(error.kind(), ErrorKind::Concurrency, "{error}");
                assert_eq!(error.sqlstate(), Some("40001"), "{error}");
                f.set(f.u3, Value::from(config)).await.unwrap();
            }
        }
    }
    assert!(lost <= 1);
    let rows = one(
        &f.client,
        "SELECT count(*)::int8 FROM res_users_settings WHERE user_id = $1",
        &[&f.u3],
    )
    .await;
    assert_eq!(rows, 1);
    f.finish().await;
}

/// shared by every anonymous visitor, has no settings)
#[tokio::test]
async fn test_user_settings_refuse_unknown_keys_and_the_public_user() {
    let f = fixture().await;
    let mut args = ValueMap::new();
    args.insert("homemenu_confg".to_string(), Value::from("[1]"));
    let err = f
        .call(f.u1, "set_res_users_settings", args)
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Validation, "{err}");
    assert!(err.message().contains("homemenu_confg"), "{err}");
    assert_eq!(f.get(f.u1).await, Value::False);
    let err = f.set(f.public_user, Value::from("[1]")).await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Access, "{err}");
    let err = f
        .call(f.public_user, "get_res_users_settings", ValueMap::new())
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Access, "{err}");
    assert_eq!(
        one(
            &f.client,
            "SELECT count(*)::int8 FROM res_users_settings",
            &[]
        )
        .await,
        0
    );
    f.finish().await;
}

/// The lock classes of the addons are the ones registered in `erp_sql::locks`, and distinct.
#[test]
fn the_addon_lock_classes_are_registered_and_distinct() {
    assert_eq!(
        erp_addon_mail::message_attachments::PENDING_LOCK_CLASS,
        erp_sql::locks::MAIL_PENDING_UPLOAD_LOCK_CLASS
    );
    assert_eq!(
        erp_addon_account::fiscal::PERIOD_LOCK_CLASS,
        erp_sql::locks::ACCOUNT_FISCAL_PERIOD_LOCK_CLASS
    );
    assert_eq!(
        erp_addon_base::attachments::SUBJECT_BUDGET_LOCK_CLASS,
        erp_sql::locks::ATTACHMENT_SUBJECT_BUDGET_LOCK_CLASS
    );
    // every class of the `erp_sql::locks` module doc, by constant
    let all = [
        erp_sql::partition::PARTITION_LOCK_SPACE,
        erp_sql::partition::STEP_LOCK_SPACE,
        erp_sql::locks::JOBS_LOCK_CLASS,
        erp_sql::locks::MAIL_SCHEDULED_THREAD_LOCK_CLASS,
        erp_sql::locks::MAIL_PENDING_UPLOAD_LOCK_CLASS,
        erp_sql::locks::ACCOUNT_FISCAL_PERIOD_LOCK_CLASS,
        erp_orm::node::LOCK_CLASS,
        erp_sql::locks::ATTACHMENT_SUBJECT_BUDGET_LOCK_CLASS,
    ];
    let unique: std::collections::BTreeSet<_> = all.iter().collect();
    assert_eq!(unique.len(), all.len());
}
