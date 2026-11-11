//! Ordinary native fields only: no server_set, outbox storage or grant changes.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "integration assertions"
)]

use erp_registry::{AddonDescriptor, FieldDecl, ModelDecl};
use erp_sdk::{Cell, ErrorKind, FieldOp, Load, RawVals, TimestampTz, Uuid, Value, Xid8};
use erp_test_models::base_users;
use erp_testkit::{FieldPolicy, Harness, tokio_postgres::types::Type};

static ADDON: AddonDescriptor = AddonDescriptor {
    name: "test_native_acceptance",
    depends: &["base"],
    exact_only: false,
};
const MODEL: &str = "test.native.acceptance";
const FIELDS: &[&str] = &["u", "x", "t"];

fn declaration() -> ModelDecl {
    ModelDecl::new("test_native_acceptance", MODEL)
        .table("test_native_acceptance")
        .fields(Box::leak(
            vec![
                FieldDecl::char("name"),
                FieldDecl::uuid("u"),
                FieldDecl::xid8("x"),
                FieldDecl::timestamptz("t"),
            ]
            .into_boxed_slice(),
        ))
}

async fn harness(policy: FieldPolicy) -> Harness {
    Harness::builder([&base_users::ADDON, &ADDON])
        .with_decls([declaration()])
        .policy(policy)
        .build()
        .await
        .unwrap()
}

fn values() -> (Uuid, Xid8, TimestampTz) {
    (
        Uuid::parse("00000000-0000-0000-0000-000000000000").unwrap(),
        Xid8::from_u64(u64::MAX),
        TimestampTz::parse("2026-11-02T07:23:45.123456+07:00").unwrap(),
    )
}

#[tokio::test]
async fn ordinary_native_fields_keep_catalog_and_values_across_cache_and_commit() {
    let h = harness(FieldPolicy::new()).await;
    let mut tx = h.begin().await.unwrap();
    tx.execute("SET LOCAL TIME ZONE 'America/New_York'", &[])
        .await
        .unwrap();
    let (u, x, t) = values();
    let records = tx
        .env()
        .records(MODEL)
        .unwrap()
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("u", Cell::Uuid(u))
                    .set_cell("x", Cell::Xid8(x))
                    .set_cell("t", Cell::TimestampTz(t)),
                RawVals::new()
                    .set_cell("u", Cell::Null)
                    .set_cell("x", Cell::Null)
                    .set_cell("t", Cell::Null),
            ],
        )
        .await
        .unwrap();
    let ids = records.ids();
    let catalog = tx.query("SELECT attname, atttypid, atthasdef FROM pg_attribute WHERE attrelid='test_native_acceptance'::regclass AND attname IN ('u','x','t') ORDER BY attname", &[]).await.unwrap();
    assert_eq!(catalog.len(), 3);
    for (row, name, oid) in [
        (0, "t", Type::TIMESTAMPTZ.oid()),
        (1, "u", Type::UUID.oid()),
        (2, "x", Type::XID8.oid()),
    ] {
        assert_eq!(catalog[row].get::<_, String>(0), name);
        assert_eq!(catalog[row].get::<_, u32>(1), oid);
        assert!(
            !catalog[row].get::<_, bool>(2),
            "ordinary native field must not gain an implicit generator"
        );
    }
    tx.flush().await.unwrap();
    tx.invalidate_all().await.unwrap();
    let rows = records.read_rows(&mut tx, FIELDS, Load::Raw).await.unwrap();
    assert_eq!(rows.get(0, "u").unwrap(), Value::Uuid(u));
    assert_eq!(rows.get(0, "x").unwrap(), Value::Xid8(x));
    assert_eq!(rows.get(0, "t").unwrap(), Value::TimestampTz(t));
    for field in FIELDS {
        assert_eq!(rows.get(1, field).unwrap(), Value::False);
    }
    tx.commit().await.unwrap();
    let mut tx = h.begin().await.unwrap();
    tx.execute("SET LOCAL TIME ZONE 'Asia/Jakarta'", &[])
        .await
        .unwrap();
    let records = tx.env().records(MODEL).unwrap().browse(ids).unwrap();
    let rows = records.read(&mut tx, FIELDS).await.unwrap();
    assert_eq!(rows[0]["u"], Value::Uuid(u));
    assert_eq!(rows[0]["x"], Value::Xid8(x));
    assert_eq!(rows[0]["t"], Value::TimestampTz(t));
    records
        .write(
            &mut tx,
            RawVals::new()
                .set_cell("u", Cell::Null)
                .set_cell("x", Cell::Null)
                .set_cell("t", Cell::Null),
        )
        .await
        .unwrap();
    tx.flush().await.unwrap();
    tx.invalidate_all().await.unwrap();
    for row in records.read(&mut tx, FIELDS).await.unwrap() {
        for field in FIELDS {
            assert_eq!(row[*field], Value::False);
        }
    }
    tx.rollback().await.unwrap();
    let mut tx = h.begin().await.unwrap();
    let restored = tx
        .env()
        .records(MODEL)
        .unwrap()
        .browse([records.ids()[0]])
        .unwrap()
        .read(&mut tx, FIELDS)
        .await
        .unwrap();
    assert_eq!(
        restored[0]["x"],
        Value::Xid8(x),
        "rollback must preserve full-width persisted value"
    );
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn native_array_parameters_round_trip_with_typed_normal_and_pipelined_queries() {
    let h = harness(FieldPolicy::new()).await;
    let client = h.db().client().await.unwrap();
    let (u, x, t) = values();
    let uuids = vec![Some(u), None];
    let xids = vec![Some(x), Some(Xid8::from_u64(1 << 63)), None];
    let times = vec![None, Some(t)];
    let sql = "SELECT $1::uuid[], $2::xid8[], $3::timestamptz[]";
    let statement = client.prepare(sql).await.unwrap();
    assert_eq!(
        statement.params(),
        &[Type::UUID_ARRAY, Type::XID8_ARRAY, Type::TIMESTAMPTZ_ARRAY]
    );
    let normal = client
        .query_one(&statement, &[&uuids, &xids, &times])
        .await
        .unwrap();
    assert_eq!(normal.columns()[0].type_(), &Type::UUID_ARRAY);
    assert_eq!(normal.columns()[1].type_(), &Type::XID8_ARRAY);
    assert_eq!(normal.columns()[2].type_(), &Type::TIMESTAMPTZ_ARRAY);
    assert_eq!(normal.get::<_, Vec<Option<Uuid>>>(0), uuids);
    assert_eq!(normal.get::<_, Vec<Option<Xid8>>>(1), xids);
    assert_eq!(normal.get::<_, Vec<Option<TimestampTz>>>(2), times);
    let empty_u: Vec<Option<Uuid>> = vec![];
    let empty_x: Vec<Option<Xid8>> = vec![];
    let empty_t: Vec<Option<TimestampTz>> = vec![];
    let null_u: Option<Vec<Option<Uuid>>> = None;
    let null_x: Option<Vec<Option<Xid8>>> = None;
    let null_t: Option<Vec<Option<TimestampTz>>> = None;
    let empty_params: &[&(dyn erp_testkit::tokio_postgres::types::ToSql + Sync)] =
        &[&empty_u, &empty_x, &empty_t];
    let null_params: &[&(dyn erp_testkit::tokio_postgres::types::ToSql + Sync)] =
        &[&null_u, &null_x, &null_t];
    let (empty, null) = tokio::try_join!(
        client.query_one(&statement, empty_params),
        client.query_one(&statement, null_params)
    )
    .unwrap();
    assert!(empty.get::<_, Vec<Option<Uuid>>>(0).is_empty());
    assert!(empty.get::<_, Vec<Option<Xid8>>>(1).is_empty());
    assert!(empty.get::<_, Vec<Option<TimestampTz>>>(2).is_empty());
    assert_eq!(null.get::<_, Option<Vec<Option<Uuid>>>>(0), None);
    assert_eq!(null.get::<_, Option<Vec<Option<Xid8>>>>(1), None);
    assert_eq!(null.get::<_, Option<Vec<Option<TimestampTz>>>>(2), None);
    drop(client);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn native_field_masks_refuse_null_reads_and_mixed_writes_without_changing_neighbors() {
    let mut policy = FieldPolicy::new();
    for field in FIELDS {
        policy = policy
            .deny(MODEL, field, FieldOp::Read)
            .deny(MODEL, field, FieldOp::Write);
    }
    let h = harness(policy).await;
    let mut tx = h.begin().await.unwrap();
    let created = tx
        .env()
        .records(MODEL)
        .unwrap()
        .create(
            &mut tx,
            vec![RawVals::new().set_cell("name", Cell::Str("before".into()))],
        )
        .await
        .unwrap();
    for field in FIELDS {
        let restricted = created.with_user(7);
        let error = restricted.read(&mut tx, &[*field]).await.unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Access);
        assert!(
            error.message().contains(field) && error.message().contains("read"),
            "{error}"
        );
        let error = restricted
            .write(
                &mut tx,
                RawVals::new()
                    .set_cell("name", Cell::Str("forged".into()))
                    .set_cell(*field, Cell::Null),
            )
            .await
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Access);
        assert!(
            error.message().contains(field) && error.message().contains("write"),
            "{error}"
        );
        assert_eq!(
            created.read(&mut tx, &["name"]).await.unwrap()[0]["name"],
            Value::str("before")
        );
    }
    tx.flush().await.unwrap();
    tx.invalidate_all().await.unwrap();
    assert_eq!(
        created.read(&mut tx, &["name"]).await.unwrap()[0]["name"],
        Value::str("before")
    );
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn persisted_policy_applies_native_field_groups_before_null_and_mixed_mutations() {
    use erp_orm::{Orm, PolicyProvider, UserId};
    use erp_security::{resolver::Resolver, source::PolicySource};
    use std::sync::Arc;

    const POLICY_MODEL: &str = "test.native.policy";
    let h = Harness::builder([&erp_addon_base::ADDON, &erp_data::model_data::ADDON, &ADDON])
        .with_decls([
            erp_data::ir_model_data_decl(),
            ModelDecl::new("test_native_acceptance", POLICY_MODEL)
                .description("Native policy acceptance")
                .table("test_native_policy")
                .fields(
                    const {
                        &[
                            FieldDecl::char("name"),
                            FieldDecl::uuid("u").groups("test_native_acceptance.group_native"),
                            FieldDecl::xid8("x").groups("test_native_acceptance.group_native"),
                            FieldDecl::timestamptz("t")
                                .groups("test_native_acceptance.group_native"),
                        ]
                    },
                ),
        ])
        .build()
        .await
        .unwrap();
    let client = h.db().client().await.unwrap();
    client.batch_execute("INSERT INTO res_groups(id,name) VALUES(100,'Internal'),(101,'Native readers'); \
        INSERT INTO ir_model_data(module,name,model,res_id) VALUES \
        ('base','group_user','res.groups',100), \
        ('test_native_acceptance','group_native','res.groups',101); \
        INSERT INTO res_users(id,name,login,active,share) VALUES \
        (20,'Allowed','native-allowed',true,false),(21,'Restricted','native-restricted',true,false); \
        INSERT INTO res_groups_users_rel(uid,gid) VALUES(20,100),(20,101),(21,100)").await.unwrap();
    let model: i64 = client.query_one("INSERT INTO ir_model(name,model,state,transient) VALUES('Native policy acceptance','test.native.policy','base',false) RETURNING id::int8", &[]).await.unwrap().get(0);
    client.execute("INSERT INTO ir_access(name,model_id,group_id,operation,active) VALUES('Native ordinary CRUD',$1,100,'cru',true)", &[&model]).await.unwrap();
    let (u, x, t) = values();
    let id: i64 = client.query_one("INSERT INTO test_native_policy(name,u,x,t) VALUES('before',$1,$2,$3) RETURNING id::int8", &[&u,&x,&t]).await.unwrap().get(0);
    let null_id: i64 = client
        .query_one(
            "INSERT INTO test_native_policy(name) VALUES('null row') RETURNING id::int8",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    let pool = erp_orm::pool::create_pool(h.db().config(), 2).unwrap();
    let provider: Arc<dyn PolicyProvider> = Arc::new(
        PolicySource::build(Arc::clone(h.registry()), pool.clone())
            .await
            .unwrap(),
    );
    let orm = Orm::builder(Arc::clone(h.registry()), pool)
        .policy(provider)
        .resolver(Resolver::id_order())
        .build()
        .unwrap();
    let mut allowed = orm.begin(UserId::new(20).unwrap()).await.unwrap();
    let record = allowed
        .env()
        .records(POLICY_MODEL)
        .unwrap()
        .browse([id])
        .unwrap();
    let row = record.read(&mut allowed, FIELDS).await.unwrap();
    assert_eq!(row[0]["u"], Value::Uuid(u));
    assert_eq!(row[0]["x"], Value::Xid8(x));
    assert_eq!(row[0]["t"], Value::TimestampTz(t));
    allowed.rollback().await.unwrap();

    let mut restricted = orm.begin(UserId::new(21).unwrap()).await.unwrap();
    let records = restricted.env().records(POLICY_MODEL).unwrap();
    let expected = |field: &str, op: &str| {
        format!(
            "You do not have enough rights to access the field \"{field}\" on Native policy acceptance (test.native.policy). Please contact your system administrator.\n\nOperation: {op}"
        )
    };
    for field in FIELDS {
        for target in [id, null_id] {
            let error = records
                .browse([target])
                .unwrap()
                .read(&mut restricted, &[*field])
                .await
                .unwrap_err();
            assert_eq!(error.kind(), ErrorKind::Access);
            assert_eq!(error.message(), expected(field, "read"));
        }
        let error = records
            .browse([id])
            .unwrap()
            .write(
                &mut restricted,
                RawVals::new()
                    .set_cell("name", Cell::Str("forged".into()))
                    .set_cell(*field, Cell::Null),
            )
            .await
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Access);
        assert_eq!(error.message(), expected(field, "write"));
        let error = records
            .create(
                &mut restricted,
                vec![
                    RawVals::new()
                        .set_cell("name", Cell::Str("forged".into()))
                        .set_cell(*field, Cell::Null),
                ],
            )
            .await
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Access);
        assert_eq!(error.message(), expected(field, "write"));
    }
    restricted.flush().await.unwrap();
    assert_eq!(
        records
            .browse([id])
            .unwrap()
            .read(&mut restricted, &["name"])
            .await
            .unwrap()[0]["name"],
        Value::str("before")
    );
    restricted.rollback().await.unwrap();
    assert_eq!(
        client
            .query_one("SELECT count(*) FROM test_native_policy", &[])
            .await
            .unwrap()
            .get::<_, i64>(0),
        2
    );
    assert_eq!(
        client
            .query_one("SELECT x FROM test_native_policy WHERE id=$1", &[&id])
            .await
            .unwrap()
            .get::<_, Xid8>(0),
        x
    );
    drop(orm);
    drop(client);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn ordinary_read_only_login_reads_native_values_and_denies_direct_sql_mutation() {
    let h = harness(FieldPolicy::new()).await;
    let owner = h.db().client().await.unwrap();
    let (u, x, t) = values();
    let id: i64 = owner
        .query_one(
            "INSERT INTO test_native_acceptance(u,x,t) VALUES($1,$2,$3) RETURNING id::int8",
            &[&u, &x, &t],
        )
        .await
        .unwrap()
        .get(0);
    // Existing testkit helper publishes SELECT only in this disposable ordinary-model DB.
    // This is not an outbox/request-role publisher or generated-column grant contract.
    let config = h.read_only_role().await.unwrap();
    let (reader, connection) = config
        .connect(erp_testkit::tokio_postgres::NoTls)
        .await
        .unwrap();
    let connection = tokio::spawn(connection);
    let identity: String = reader
        .query_one("SELECT session_user::text", &[])
        .await
        .unwrap()
        .get(0);
    assert_eq!(identity, config.get_user().unwrap());
    assert_ne!(identity, h.db().config().get_user().unwrap());
    let row = reader
        .query_one(
            "SELECT u,x,t FROM test_native_acceptance WHERE id=$1",
            &[&id],
        )
        .await
        .unwrap();
    assert_eq!(row.columns()[0].type_(), &Type::UUID);
    assert_eq!(row.columns()[1].type_(), &Type::XID8);
    assert_eq!(row.columns()[2].type_(), &Type::TIMESTAMPTZ);
    assert_eq!(row.get::<_, Uuid>(0), u);
    assert_eq!(row.get::<_, Xid8>(1), x);
    assert_eq!(row.get::<_, TimestampTz>(2), t);
    for sql in [
        "UPDATE test_native_acceptance SET x='1'::xid8",
        "INSERT INTO test_native_acceptance(x) VALUES('1'::xid8)",
    ] {
        let error = reader.execute(sql, &[]).await.unwrap_err();
        let db_error = error.as_db_error().unwrap();
        assert_eq!(
            db_error.code(),
            &erp_testkit::tokio_postgres::error::SqlState::INSUFFICIENT_PRIVILEGE
        );
        assert_eq!(
            db_error.message(),
            "permission denied for table test_native_acceptance"
        );
    }
    assert_eq!(
        owner
            .query_one("SELECT x FROM test_native_acceptance WHERE id=$1", &[&id])
            .await
            .unwrap()
            .get::<_, Xid8>(0),
        x
    );
    drop(reader);
    connection.await.unwrap().unwrap();
    drop(owner);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn native_mutations_and_queries_preserve_types_across_copy_upsert_and_savepoints() {
    use erp_fields::ValueMap;
    use erp_registry::{KeyDecl, TableObjectDecl, TableObjectKind};
    use erp_sdk::domain::{Operator, Value as DomainValue};
    use erp_sdk::{Domain, Error, SearchOptions, UpsertCall, UpsertOutcome};

    const MUTATIONS: &str = "test.native.mutations";
    static OBJECTS: &[TableObjectDecl] = &[TableObjectDecl {
        name: "_native_key",
        kind: TableObjectKind::Key(KeyDecl::new(&["u"], "Native UUID key must be unique.")),
    }];
    let h = Harness::builder([&base_users::ADDON, &ADDON])
        .with_decls([ModelDecl::new("test_native_acceptance", MUTATIONS)
            .fields(
                const {
                    &[
                        FieldDecl::char("name"),
                        FieldDecl::uuid("u"),
                        FieldDecl::xid8("x"),
                        FieldDecl::timestamptz("t"),
                    ]
                },
            )
            .table_objects(OBJECTS)])
        .build()
        .await
        .unwrap();
    let mut tx = h.begin().await.unwrap();
    let model = tx.env().records(MUTATIONS).unwrap();
    let (u, x, t) = values();
    let row = |u, x, t| {
        RawVals::new()
            .set_cell("u", Cell::Uuid(u))
            .set_cell("x", Cell::Xid8(x))
            .set_cell("t", Cell::TimestampTz(t))
    };
    let first = model
        .upsert(
            &mut tx,
            UpsertCall::new("_native_key")
                .with_rows([row(u, x, t)])
                .with_update(["x", "t"]),
        )
        .await
        .unwrap();
    assert_eq!(first.rows()[0].outcome(), UpsertOutcome::Inserted);
    let original = model.browse_ids(first.distinct_ids());
    let original_id = original.ids()[0];
    let changed_x = Xid8::from_u64(1 << 63);
    let changed_t = TimestampTz::parse("2026-11-02T00:23:45.654321Z").unwrap();
    let changed = model
        .upsert(
            &mut tx,
            UpsertCall::new("_native_key")
                .with_rows([row(u, changed_x, changed_t)])
                .with_update(["x", "t"]),
        )
        .await
        .unwrap();
    assert_eq!(changed.ids(), first.ids());
    assert_eq!(changed.rows()[0].outcome(), UpsertOutcome::Updated);
    let same = model
        .upsert(
            &mut tx,
            UpsertCall::new("_native_key")
                .with_rows([row(u, changed_x, changed_t)])
                .with_update(["x", "t"]),
        )
        .await
        .unwrap();
    assert_eq!(same.rows()[0].outcome(), UpsertOutcome::Unchanged);
    let second_u = Uuid::parse("00000000-0000-0000-0000-000000000001").unwrap();
    let inserted = model
        .upsert(
            &mut tx,
            UpsertCall::new("_native_key")
                .with_rows([row(second_u, x, t)])
                .with_update(["x", "t"]),
        )
        .await
        .unwrap();
    let second_id = model.browse_ids(inserted.distinct_ids()).ids()[0];
    let copy_u = Uuid::parse("00000000-0000-0000-0000-000000000002").unwrap();
    let mut overrides = ValueMap::new();
    overrides.insert("u".into(), Value::Uuid(copy_u));
    let copied = original.copy(&mut tx, Some(overrides)).await.unwrap();
    let copy_id = copied.ids()[0];
    let copied_values = copied.read(&mut tx, FIELDS).await.unwrap();
    assert_eq!(copied_values[0]["u"], Value::Uuid(copy_u));
    assert_eq!(copied_values[0]["x"], Value::Xid8(changed_x));
    assert_eq!(copied_values[0]["t"], Value::TimestampTz(changed_t));
    let rolled_back: erp_sdk::Result<()> = tx
        .savepoint(async |tx| {
            original
                .write(
                    tx,
                    RawVals::new()
                        .set_cell("x", Cell::Xid8(Xid8::from_u64(0)))
                        .set_cell("t", Cell::Null),
                )
                .await?;
            tx.flush().await?;
            Err(Error::validation("native savepoint fixture"))
        })
        .await;
    let error = rolled_back.unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Validation);
    assert_eq!(error.message(), "native savepoint fixture");
    tx.invalidate_all().await.unwrap();
    let restored = original.read(&mut tx, FIELDS).await.unwrap();
    assert_eq!(restored[0]["x"], Value::Xid8(changed_x));
    assert_eq!(restored[0]["t"], Value::TimestampTz(changed_t));
    let domain =
        Domain::condition("x", Operator::Eq, DomainValue::Str(changed_x.to_string())).unwrap();
    assert_eq!(
        model.search(&mut tx, &domain).await.unwrap().ids(),
        vec![original_id, copy_id]
    );
    let uuid_domain =
        Domain::condition("u", Operator::Eq, DomainValue::Str(copy_u.to_string())).unwrap();
    assert_eq!(
        model.search(&mut tx, &uuid_domain).await.unwrap().ids(),
        vec![copy_id]
    );
    let time_domain =
        Domain::condition("t", Operator::Eq, DomainValue::Str(changed_t.to_string())).unwrap();
    assert_eq!(
        model.search(&mut tx, &time_domain).await.unwrap().ids(),
        vec![original_id, copy_id]
    );
    assert_eq!(
        model
            .search_with(
                &mut tx,
                &Domain::TRUE,
                &SearchOptions::default().order("x desc, id asc")
            )
            .await
            .unwrap()
            .ids(),
        vec![second_id, original_id, copy_id]
    );
    tx.commit().await.unwrap();
    let mut tx = h.begin().await.unwrap();
    let persisted = tx
        .env()
        .records(MUTATIONS)
        .unwrap()
        .browse([original_id, copy_id])
        .unwrap()
        .read(&mut tx, FIELDS)
        .await
        .unwrap();
    for values in persisted {
        assert_eq!(values["x"], Value::Xid8(changed_x));
        assert_eq!(values["t"], Value::TimestampTz(changed_t));
    }
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}
