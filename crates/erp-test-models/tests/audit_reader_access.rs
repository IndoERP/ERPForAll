// SPDX-License-Identifier: MIT OR Apache-2.0
//! Real audit storage, restricted reader login and compiled current access policy.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]
use erp_fields::ErrorKind;
use erp_orm::audit::reader::{AuditQuery, AuditReader, EntryKey, ReadRequest};
use erp_orm::{Orm, PolicyProvider, UserId};
use erp_registry::{FieldDecl, ModelDecl, PartitionDecl, PartitionInterval, PartitionRetain};
use erp_security::{resolver::Resolver, source::PolicySource};
use erp_testkit::{
    Harness,
    tokio_postgres::{Client, NoTls},
};
use serde_json::{Value, json};
use std::sync::Arc;

struct Fixture {
    h: Harness,
    owner: Client,
    reader: AuditReader,
    orm: Orm,
    user: i64,
    keys: Vec<Value>,
}
fn fields() -> Value {
    json!([
        "entry_id",
        "create_date",
        "entry_kind",
        "outcome",
        "model",
        "method",
        "effect",
        "target_ids"
    ])
}
fn query(limit: usize) -> AuditQuery {
    AuditQuery::parse(&json!({"model":"test.reader.target", "from":"2026-11-01T00:00:00Z", "until":"2026-11-02T00:00:00Z", "fields":fields(), "limit":limit})).unwrap()
}
async fn fixture() -> Fixture {
    let audit = ModelDecl::new("base", "ir.audit.entry")
        .log_access(false)
        .fields(
            const {
                &[
                    FieldDecl::timestamptz("create_date")
                        .required(true)
                        .server_set(true),
                    FieldDecl::char("entry_id"),
                    FieldDecl::char("entry_kind"),
                    FieldDecl::char("outcome"),
                    FieldDecl::char("model"),
                    FieldDecl::char("method"),
                    FieldDecl::char("effect").groups("reader_test.projection"),
                    FieldDecl::char("target_ids"),
                ]
            },
        )
        .partition(
            PartitionDecl::new(PartitionInterval::Day, 31, PartitionRetain::All).by("create_date"),
        );
    let target = ModelDecl::new("base", "test.reader.target").fields(
        const {
            &[
                FieldDecl::char("name"),
                FieldDecl::integer("owner"),
                FieldDecl::integer("company"),
            ]
        },
    );
    let h = Harness::builder([&erp_addon_base::ADDON, &erp_data::model_data::ADDON])
        .with_decls([erp_data::ir_model_data_decl(), target])
        .with_core_event_decls([audit])
        .build()
        .await
        .unwrap();
    let (owner, connection) = h.db().config().connect(NoTls).await.unwrap();
    tokio::spawn(async move {
        connection.await.expect("owner connection");
    });
    // Plain SQL fixtures intentionally avoid relying on base installation/default ACLs.
    owner.batch_execute("INSERT INTO res_groups (id,name) VALUES (101,'Reader'),(102,'Capability only'),(103,'ACL only'),(104,'Projection'),(105,'Internal User');
        INSERT INTO ir_model_data(module,name,model,res_id) VALUES ('reader_test','reader','res.groups',101),('reader_test','cap','res.groups',102),('reader_test','acl','res.groups',103),('reader_test','projection','res.groups',104),('base','group_user','res.groups',105);
        INSERT INTO res_currency(id,name,symbol,rounding,active) VALUES (101,'TST','T',0.01,true);
        INSERT INTO res_company(id,name,currency_id,active) VALUES (1,'C1',101,true),(2,'C2',101,true);
        INSERT INTO res_users(id,name,login,active,share,company_id) VALUES (101,'Reader','reader',true,false,1),(102,'Cap','cap',true,false,1),(103,'ACL','acl',true,false,1);
        INSERT INTO res_groups_users_rel(uid,gid) VALUES (101,101),(101,104),(101,105),(102,102),(102,105),(103,103),(103,105);
        INSERT INTO res_company_users_rel(user_id,cid) VALUES (101,1),(101,2),(102,1),(103,1);
        INSERT INTO ir_model(id,name,model,state,transient) VALUES (101,'Audit','ir.audit.entry','base',false),(102,'Target','test.reader.target','base',false);
        INSERT INTO ir_capability(name,groups,description) VALUES ('audit.read','reader_test.reader,reader_test.cap','Audit reader');
        INSERT INTO ir_access(name,model_id,group_id,operation,domain,active) VALUES
          ('audit',101,101,'r',NULL,true),('audit acl only',101,103,'r',NULL,true),
          ('targets',102,101,'r','[(\"owner\",\"=\",user.id),(\"company\",\"in\",company_ids)]',true),
          ('targets cap only',102,102,'r',NULL,true),('targets acl only',102,103,'r',NULL,true);
        INSERT INTO test_reader_target(id,name,owner,company) VALUES (1,'visible',101,1),(2,'hidden',999,2),(3,'visible too',101,1),(4,'other company',101,2);") .await.unwrap();
    // Partition dates come from the fixture, rather than the machine's current date.
    owner.batch_execute("CREATE TABLE erp_event.reader_test_20261101 PARTITION OF erp_event.ir_audit_entry FOR VALUES FROM ('2026-11-01') TO ('2026-11-02')").await.unwrap();
    let mut keys = Vec::new();
    for (i, targets, method, version) in [
        (1, "[2]", "read", 1),
        (2, "[1]", "read", 1),
        (3, "[1,2]", "read", 1),
        (4, "[3]", "read", 1),
        (5, "[999]", "read", 1),
        (6, "[]", "read", 1),
        (7, "[1]", "removed_method", 1),
        (8, "[1]", "read", 9),
        (9, "[4]", "read", 1),
    ] {
        let uuid = format!("018f3a1c-7b2d-7c4e-9a10-{i:012x}");
        let r=owner.query_one("INSERT INTO erp_event.ir_audit_entry(entry_id,create_date,schema_version,entry_kind,origin,outcome,attempt,operation_id,subject_id,company_ids,model,method,effect,target_ids,events,args_digest,redaction_version,encoded_bytes) VALUES ($1::text::uuid,'2026-11-01T12:00:00Z',$2,'change','rpc','committed',1,uuidv7(),101,'{987654321}','test.reader.target',$3,'read',$4::text::jsonb,'[{\"canary\":\"HIDDEN-EVENT-CANARY\"}]',decode(repeat('ab',32),'hex'),2,1) RETURNING entry_id::text", &[&uuid,&(version as i16),&method,&targets]).await.unwrap();
        keys.push(json!({"entry_id":r.get::<_,String>(0),"create_date":"2026-11-01T12:00:00Z"}));
    }
    // Each harness has a separate database, while role names are cluster-wide.
    let config = h.read_only_role().await.unwrap();
    let role = config.get_user().unwrap();
    owner.batch_execute(&format!("REVOKE SELECT ON ALL TABLES IN SCHEMA public FROM \"{role}\"; GRANT USAGE ON SCHEMA erp_event TO \"{role}\"; GRANT SELECT ON erp_event.ir_audit_entry TO \"{role}\"")).await.unwrap();
    let reader = AuditReader::for_tests(erp_orm::pool::create_pool(config, 1).unwrap());
    let pool = erp_orm::pool::create_pool(h.db().config(), 2).unwrap();
    let source = PolicySource::build(Arc::clone(h.registry()), pool.clone())
        .await
        .unwrap();
    let provider: Arc<dyn PolicyProvider> = Arc::new(source);
    let orm = Orm::builder(Arc::clone(h.registry()), pool)
        .policy(provider)
        .resolver(Resolver::id_order())
        .build()
        .unwrap();
    Fixture {
        h,
        owner,
        reader,
        orm,
        user: 101,
        keys,
    }
}
async fn begin(f: &Fixture, uid: i64) -> erp_orm::Transaction {
    f.orm
        .begin_opts(
            erp_orm::BeginOptions::new(UserId::new(uid).unwrap()).context(if uid == f.user {
                erp_orm::Context::from_pairs([(
                    "allowed_company_ids",
                    erp_fields::Value::list([erp_fields::Value::Int(1)]),
                )])
            } else {
                erp_orm::Context::new()
            }),
        )
        .await
        .unwrap()
}
fn denied(e: erp_fields::Error) {
    assert_eq!(e.kind(), ErrorKind::Access);
    assert_eq!(e.message(), "a later release: audit access denied");
}

#[tokio::test]
async fn audit_reader_projection_uses_real_current_access_for_every_surface() {
    let f = fixture().await;
    let mut tx = begin(&f, f.user).await;
    let q = query(100);
    let page = f
        .reader
        .search(&mut tx, &q, 16 * 1024 * 1024)
        .await
        .unwrap();
    let rows = page.to_json()["rows"].as_array().unwrap().clone();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0]["target_ids"], json!([1]));
    assert_eq!(rows[1]["target_ids"], json!([3]));
    assert_eq!(
        f.reader
            .export(&mut tx, &q, 16 * 1024 * 1024)
            .await
            .unwrap()
            .to_json(),
        page.to_json()
    );
    let req = ReadRequest::parse(
        &json!({"keys":[f.keys[3],f.keys[0],f.keys[1],f.keys[3]],"fields":fields()}),
    )
    .unwrap();
    let read = f
        .reader
        .read(&mut tx, &req, 16 * 1024 * 1024)
        .await
        .unwrap();
    assert_eq!(read.to_json()["rows"], json!([rows[1], rows[0]]));
    let encoded = page.to_json().to_string();
    for secret in [
        "HIDDEN-EVENT-CANARY",
        "987654321",
        "args_digest",
        "events",
        "subject_id",
        "policy_generation",
        "correlation_id",
    ] {
        assert!(!encoded.contains(secret));
    }
    let hidden = ReadRequest::parse(&json!({"keys":[f.keys[0]],"fields":fields()})).unwrap();
    let absent=ReadRequest::parse(&json!({"keys":[{"entry_id":"018f3a1c-7b2d-7c4e-9a10-ffffffffffff","create_date":"2026-11-01T12:00:00Z"}],"fields":fields()})).unwrap();
    assert_eq!(
        f.reader
            .read(&mut tx, &hidden, 10000)
            .await
            .unwrap()
            .to_json(),
        f.reader
            .read(&mut tx, &absent, 10000)
            .await
            .unwrap()
            .to_json()
    );
    tx.rollback().await.unwrap();
    drop(tx);
    for uid in [102, 103, 1] {
        let mut tx = begin(&f, uid).await;
        denied(f.reader.search(&mut tx, &q, 10000).await.unwrap_err());
        tx.rollback().await.unwrap();
        drop(tx);
    }
    // Full elevation on a non-System caller cannot replace the caller's missing audit ACL.
    let principal = f
        .orm
        .env(UserId::new(102).unwrap(), erp_orm::Context::new())
        .sudo(true)
        .principal()
        .clone();
    let mut elevated = f
        .orm
        .begin_opts(erp_orm::BeginOptions::for_principal(principal))
        .await
        .unwrap();
    denied(f.reader.search(&mut elevated, &q, 10000).await.unwrap_err());
    elevated.rollback().await.unwrap();
    drop(elevated);
    // An unrestricted current target predicate still must prove existence.
    f.owner.batch_execute("UPDATE ir_access SET domain=NULL WHERE model_id=102; DELETE FROM test_reader_target WHERE id=1").await.unwrap();
    let source = PolicySource::build(
        Arc::clone(f.h.registry()),
        erp_orm::pool::create_pool(f.h.db().config(), 2).unwrap(),
    )
    .await
    .unwrap();
    let orm = Orm::builder(
        Arc::clone(f.h.registry()),
        erp_orm::pool::create_pool(f.h.db().config(), 2).unwrap(),
    )
    .policy(Arc::new(source) as Arc<dyn PolicyProvider>)
    .resolver(Resolver::id_order())
    .build()
    .unwrap();
    let mut tx = orm.begin(UserId::new(f.user).unwrap()).await.unwrap();
    let rows = f.reader.search(&mut tx, &q, 10000).await.unwrap().to_json();
    assert!(
        !rows["rows"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["target_ids"] == json!([1]) || r["target_ids"] == json!([999]))
    );
    tx.rollback().await.unwrap();
    drop(tx);
}
#[tokio::test]
async fn audit_reader_cursor_limits_and_integrity_fail_without_partial_output() {
    let f = fixture().await;
    let mut tx = begin(&f, f.user).await;
    let page = f
        .reader
        .search(&mut tx, &query(1), 10000)
        .await
        .unwrap()
        .to_json();
    assert_eq!(page["rows"][0]["entry_id"], f.keys[1]["entry_id"]);
    let token = page["continuation"].as_str().unwrap();
    let next=AuditQuery::parse(&json!({"model":"test.reader.target","from":"2026-11-01T00:00:00Z","until":"2026-11-02T00:00:00Z","fields":fields(),"limit":1,"continuation":token})).unwrap();
    let p = f
        .reader
        .search(&mut tx, &next, 10000)
        .await
        .unwrap()
        .to_json();
    assert_eq!(p["rows"][0]["entry_id"], f.keys[3]["entry_id"]);
    let e = f.reader.export(&mut tx, &query(100), 10).await.unwrap_err();
    assert_eq!(e.kind(), ErrorKind::ResourceExhausted);
    assert_eq!(e.message(), "a later release: reader safety limit exceeded");
    tx.rollback().await.unwrap();
    drop(tx);
    f.owner.execute("UPDATE erp_event.ir_audit_entry SET target_ids='[[1,9223372036854775807]]' WHERE entry_id=$1::text::uuid",&[&f.keys[3]["entry_id"].as_str().unwrap()]).await.unwrap();
    let mut tx = begin(&f, f.user).await;
    let e = f
        .reader
        .search(&mut tx, &query(100), 10000)
        .await
        .unwrap_err();
    assert_eq!(e.kind(), ErrorKind::ResourceExhausted);
    assert_eq!(e.message(), "a later release: reader safety limit exceeded");
    tx.rollback().await.unwrap();
    drop(tx);
    // Supported malformed history is an error rather than a concealed/empty success.
    f.owner.execute("UPDATE erp_event.ir_audit_entry SET target_ids='[[3,1]]' WHERE entry_id=$1::text::uuid",&[&f.keys[3]["entry_id"].as_str().unwrap()]).await.unwrap();
    let mut tx = begin(&f, f.user).await;
    let e = f
        .reader
        .search(&mut tx, &query(100), 10000)
        .await
        .unwrap_err();
    assert_eq!(e.kind(), ErrorKind::Internal);
    assert_eq!(e.message(), "a later release: invalid stored audit data");
    tx.rollback().await.unwrap();
    drop(tx);
    assert!(EntryKey::parse(&f.keys[1]).is_ok());
}

async fn reload(f: &Fixture) -> Orm {
    let pool = erp_orm::pool::create_pool(f.h.db().config(), 2).unwrap();
    let source = PolicySource::build(Arc::clone(f.h.registry()), pool.clone())
        .await
        .unwrap();
    Orm::builder(Arc::clone(f.h.registry()), pool)
        .policy(Arc::new(source) as Arc<dyn PolicyProvider>)
        .resolver(Resolver::id_order())
        .build()
        .unwrap()
}
#[tokio::test]
async fn audit_reader_revocation_company_scope_and_unavailable_reader_fail_closed() {
    let mut f = fixture().await;
    let mut tx = begin(&f, f.user).await;
    let q = query(1);
    let page = f.reader.search(&mut tx, &q, 10000).await.unwrap().to_json();
    let token = page["continuation"].as_str().unwrap();
    let next=AuditQuery::parse(&json!({"model":"test.reader.target","from":"2026-11-01T00:00:00Z","until":"2026-11-02T00:00:00Z","fields":fields(),"limit":1,"continuation":token})).unwrap();
    let e = AuditReader::unavailable()
        .search(&mut tx, &q, 10000)
        .await
        .unwrap_err();
    assert_eq!(e.kind(), ErrorKind::Internal);
    assert_eq!(e.message(), "a later release: audit reader unavailable");
    let e = tx
        .env()
        .records("ir.audit.entry")
        .unwrap()
        .search(&mut tx, &erp_domain::Domain::TRUE)
        .await
        .unwrap_err();
    assert_eq!(e.kind(), ErrorKind::Programming);
    assert_eq!(
        e.message(),
        "model 'ir.audit.entry' is in the closed event namespace: the generic ORM refuses every record operation; the a later release capture path is the only writer and nothing reads it through the ORM"
    );
    tx.rollback().await.unwrap();
    drop(tx);
    let context = erp_orm::Context::from_pairs([(
        "allowed_company_ids",
        erp_fields::Value::list([erp_fields::Value::Int(2)]),
    )]);
    let mut tx = f
        .orm
        .begin_opts(erp_orm::BeginOptions::new(UserId::new(f.user).unwrap()).context(context))
        .await
        .unwrap();
    let rows = f
        .reader
        .search(&mut tx, &query(100), 10000)
        .await
        .unwrap()
        .to_json();
    assert_eq!(rows["rows"].as_array().unwrap().len(), 1);
    assert_eq!(rows["rows"][0]["target_ids"], json!([4]));
    let e = f.reader.search(&mut tx, &next, 10000).await.unwrap_err();
    assert_eq!(e.kind(), ErrorKind::Validation);
    assert_eq!(
        e.message(),
        "a later release: stale or mismatched continuation"
    );
    tx.rollback().await.unwrap();
    drop(tx);
    // Keep the capability and audit ACL, revoke only the projection field's group.
    f.owner
        .batch_execute("DELETE FROM res_groups_users_rel WHERE uid=101 AND gid=104")
        .await
        .unwrap();
    f.orm = reload(&f).await;
    let mut tx = begin(&f, f.user).await;
    denied(
        f.reader
            .search(&mut tx, &query(100), 10000)
            .await
            .unwrap_err(),
    );
    tx.rollback().await.unwrap();
    drop(tx);
    f.owner
        .batch_execute("INSERT INTO res_groups_users_rel(uid,gid) VALUES (101,104)")
        .await
        .unwrap();
    f.orm = reload(&f).await;
    let mut tx = begin(&f, f.user).await;
    assert_eq!(
        f.reader
            .search(&mut tx, &query(100), 10000)
            .await
            .unwrap()
            .to_json()["rows"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    tx.rollback().await.unwrap();
    drop(tx);
    let mut tx = begin(&f, f.user).await;
    let e = f.reader.search(&mut tx, &next, 10000).await.unwrap_err();
    assert_eq!(e.kind(), ErrorKind::Validation);
    assert_eq!(
        e.message(),
        "a later release: stale or mismatched continuation"
    );
    tx.rollback().await.unwrap();
    drop(tx);
    // The private role actually has no mutation privilege.
    let (client, connection) = f.h.read_only_config().connect(NoTls).await.unwrap();
    tokio::spawn(async move {
        connection.await.unwrap();
    });
    let e = client
        .execute("DELETE FROM erp_event.ir_audit_entry", &[])
        .await
        .unwrap_err();
    assert_eq!(
        e.code(),
        Some(&erp_testkit::tokio_postgres::error::SqlState::INSUFFICIENT_PRIVILEGE)
    );
    assert_eq!(
        e.as_db_error().unwrap().message(),
        "permission denied for table ir_audit_entry"
    );
    let mut tx = begin(&f, f.user).await;
    tx.taint_cancelled_for_tests();
    let e = f
        .reader
        .search(&mut tx, &query(100), 10000)
        .await
        .unwrap_err();
    assert_eq!(e.kind(), ErrorKind::Internal);
    assert_eq!(e.message(), "a later release: audit reader unavailable");
    tx.rollback().await.unwrap();
    drop(tx);
}
#[tokio::test]
async fn audit_reader_hidden_scan_budget_is_an_error_and_large_payload_never_becomes_a_page() {
    let f = fixture().await;
    f.owner.batch_execute("INSERT INTO erp_event.ir_audit_entry(create_date,schema_version,entry_kind,origin,outcome,attempt,operation_id,subject_id,company_ids,model,method,effect,target_ids,events,encoded_bytes) SELECT '2026-11-01T01:00:00Z',1,'change','rpc','committed',1,uuidv7(),101,'{}','test.reader.target','read','read','[2]','[]',1 FROM generate_series(1,4097)").await.unwrap();
    let mut tx = begin(&f, f.user).await;
    let e = f
        .reader
        .search(&mut tx, &query(100), 10000)
        .await
        .unwrap_err();
    assert_eq!(e.kind(), ErrorKind::ResourceExhausted);
    assert_eq!(e.message(), "a later release: reader safety limit exceeded");
    tx.rollback().await.unwrap();
    drop(tx);
    f.owner.batch_execute("DELETE FROM erp_event.ir_audit_entry WHERE create_date='2026-11-01T01:00:00Z'; UPDATE erp_event.ir_audit_entry SET target_ids=(SELECT jsonb_agg(1) FROM generate_series(1,600000)),encoded_bytes=1 WHERE entry_id='018f3a1c-7b2d-7c4e-9a10-000000000004'").await.unwrap();
    let mut tx = begin(&f, f.user).await;
    let e = f
        .reader
        .export(&mut tx, &query(100), 10000)
        .await
        .unwrap_err();
    assert_eq!(e.kind(), ErrorKind::ResourceExhausted);
    assert_eq!(e.message(), "a later release: reader safety limit exceeded");
    tx.rollback().await.unwrap();
    drop(tx);
}

#[tokio::test]
async fn audit_reader_preserves_large_integer_targets_and_requested_field_subset() {
    let f = fixture().await;
    let id = 9_007_199_254_740_993_i64;
    f.owner.execute("INSERT INTO test_reader_target(id,name,owner,company) VALUES ($1,'large identity',101,1)",&[&id]).await.unwrap();
    let targets = erp_orm::audit::TargetIds::from_ids(&[id]).unwrap();
    assert_eq!(targets.runs(), &[(id, id)]);
    let encoded = json!([id]).to_string();
    f.owner.execute("INSERT INTO erp_event.ir_audit_entry(entry_id,create_date,schema_version,entry_kind,origin,outcome,attempt,operation_id,subject_id,company_ids,model,method,effect,target_ids,events,encoded_bytes) VALUES ('018f3a1c-7b2d-7c4e-9a10-000000000010','2026-11-01T13:00:00Z',1,'change','rpc','committed',1,uuidv7(),101,'{}','test.reader.target','read','read',$1::text::jsonb,'[]',1)",&[&encoded]).await.unwrap();
    let request=ReadRequest::parse(&json!({"keys":[{"entry_id":"018f3a1c-7b2d-7c4e-9a10-000000000010","create_date":"2026-11-01T13:00:00Z"}],"fields":["target_ids"]})).unwrap();
    let mut tx = begin(&f, f.user).await;
    let result = f
        .reader
        .read(&mut tx, &request, 10000)
        .await
        .unwrap()
        .to_json();
    assert_eq!(result["rows"], json!([{"target_ids":[id]}]));
    assert!(result.to_string().contains(&id.to_string()));
    tx.rollback().await.unwrap();
    drop(tx);
}

/// reader's own denial is exercised for a System-rooted impersonated principal only, since a
/// user-rooted one is refused when its transaction opens, A4)
#[tokio::test]
async fn audit_reader_impersonation_never_replaces_original_caller() {
    let f = fixture().await;
    let principal = f
        .orm
        .env(UserId::new(102).unwrap(), erp_orm::Context::new())
        .with_user(f.user)
        .principal()
        .clone();
    // a principal another user switched to opens no transaction at all (A4)
    let error = f
        .orm
        .begin_opts(erp_orm::BeginOptions::for_principal(principal))
        .await
        .err()
        .unwrap();
    assert_eq!(error.kind(), erp_fields::ErrorKind::Access, "{error}");
    assert_eq!(
        error.message(),
        format!(
            "a transaction cannot be opened for user {} as switched to by user 102: an \
             environment made by with_user is admitted by the engine only; open the transaction \
             for the caller (Orm::begin_caller)",
            f.user
        )
    );
    let principal = f
        .orm
        .env(erp_orm::SUPERUSER_ID, erp_orm::Context::new())
        .with_user(f.user)
        .principal()
        .clone();
    let mut tx = f
        .orm
        .begin_opts(erp_orm::BeginOptions::for_principal(principal))
        .await
        .unwrap();
    denied(
        f.reader
            .search(&mut tx, &query(100), 10000)
            .await
            .unwrap_err(),
    );
    tx.rollback().await.unwrap();
    drop(tx);
}
