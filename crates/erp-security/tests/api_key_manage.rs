//! Bounded native API-key collection and transaction failure boundaries.
#![allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]
mod support;
use erp_fields::ErrorKind;
use erp_orm::{BeginOptions, OperationIdentity, Transaction, UserId};
use erp_security::auth::{
    apikey::ApiKeyHasher,
    apikey_manage::collect_expired,
    apikey_store::{IssueRequest, KeyKind, KeyScope, issue},
    secret::DatabaseSecret,
};

/// A transaction as the harness principal with its own operation identity, as every audited
/// write needs one; the collector attaches one per batch attempt.
async fn audited(h: &erp_testkit::Harness) -> Transaction {
    h.orm()
        .begin_opts(
            BeginOptions::new(h.principal())
                .operation_identity(OperationIdentity::generate().unwrap()),
        )
        .await
        .unwrap()
}

#[tokio::test]
async fn collectors_skip_locked_rows_and_rollback_is_reclaimed() {
    let h = support::harness().await;
    let c = support::connect(&h).await;
    c.batch_execute("UPDATE res_users SET active=true WHERE id=1; INSERT INTO res_groups(id,name) VALUES(10,'Settings'); INSERT INTO ir_model_data(module,name,model,res_id) VALUES('base','group_system','res.groups',10); INSERT INTO res_groups_users_rel(uid,gid) VALUES(1,10)").await.unwrap();
    let hash = ApiKeyHasher::derive(
        &DatabaseSecret::from_stored("0123456789abcdef0123456789abcdef").unwrap(),
    )
    .unwrap();
    let mut tx = h.begin().await.unwrap();
    let mut ids = Vec::new();
    for _ in 0..5 {
        let request = IssueRequest::new(
            UserId::new(1).unwrap(),
            "GC fixture",
            KeyKind::User,
            KeyScope::Rpc,
            None,
        )
        .unwrap();
        ids.push(issue(&mut tx, &hash, &request).await.unwrap().id().get());
    }
    tx.execute("UPDATE res_users_apikeys SET expiration_date=now() AT TIME ZONE 'utc' - interval '1 second' WHERE id=ANY($1)", &[&ids[..3].to_vec()]).await.unwrap();
    tx.execute("UPDATE res_users_apikeys SET expiration_date=now() AT TIME ZONE 'utc' + interval '1 hour' WHERE id=$1", &[&ids[3]]).await.unwrap();
    tx.commit().await.unwrap();
    drop(tx);
    // Independent transactions represent two collectors with no shared process state.
    let mut a = audited(&h).await;
    assert_eq!(collect_expired(&mut a, 2).await.unwrap(), 2);
    let mut b = audited(&h).await;
    assert_eq!(collect_expired(&mut b, 2).await.unwrap(), 1);
    b.commit().await.unwrap();
    drop(b);
    a.rollback().await.unwrap();
    drop(a);
    let mut recovered = audited(&h).await;
    assert_eq!(collect_expired(&mut recovered, 2).await.unwrap(), 2);
    recovered.commit().await.unwrap();
    drop(recovered);
    let mut last = audited(&h).await;
    assert_eq!(collect_expired(&mut last, 1000).await.unwrap(), 0);
    let remaining: Vec<i64> = last
        .query("SELECT id::int8 FROM res_users_apikeys ORDER BY id", &[])
        .await
        .unwrap()
        .iter()
        .map(|r| r.get(0))
        .collect();
    assert_eq!(remaining, ids[3..]);
    last.rollback().await.unwrap();
}

#[tokio::test]
async fn collection_rejects_unbounded_and_non_system_requests() {
    let h = support::harness().await;
    for limit in [0, 1001, u32::MAX] {
        let mut tx = h.begin().await.unwrap();
        let e = collect_expired(&mut tx, limit).await.unwrap_err();
        assert_eq!(e.kind(), ErrorKind::Validation);
        assert_eq!(
            e.to_string(),
            "API key collection limit must be between 1 and 1000"
        );
        tx.rollback().await.unwrap();
    }
    let mut tx = h.begin_as(UserId::new(20).unwrap()).await.unwrap();
    let e = collect_expired(&mut tx, 1).await.unwrap_err();
    assert_eq!(e.kind(), ErrorKind::Access);
    assert_eq!(
        e.to_string(),
        "API key collection requires a core System principal"
    );
    tx.rollback().await.unwrap();
}

/// Every audit event of the real store, oldest first, as `(event, json text)`.
async fn audit_events(c: &erp_testkit::tokio_postgres::Client) -> Vec<(String, String)> {
    c.query(
        "SELECT e->>'event', e::text FROM erp_event.ir_audit_entry a, \
         jsonb_array_elements(a.events) WITH ORDINALITY AS t(e, n) ORDER BY a.id, t.n",
        &[],
    )
    .await
    .unwrap()
    .iter()
    .map(|r| (r.get(0), r.get(1)))
    .collect()
}

/// one event per committed bounded batch, a rolled back batch leaves no record, and no
/// credential material reaches the trail)
#[tokio::test]
async fn collection_writes_its_audit_event_atomically() {
    let h = support::harness().await;
    let c = support::connect(&h).await;
    c.batch_execute("UPDATE res_users SET active=true WHERE id=1; INSERT INTO res_groups(id,name) VALUES(10,'Settings'); INSERT INTO ir_model_data(module,name,model,res_id) VALUES('base','group_system','res.groups',10); INSERT INTO res_groups_users_rel(uid,gid) VALUES(1,10)").await.unwrap();
    let hash = ApiKeyHasher::derive(
        &DatabaseSecret::from_stored("0123456789abcdef0123456789abcdef").unwrap(),
    )
    .unwrap();
    let mut tx = h.begin().await.unwrap();
    let mut credentials = Vec::new();
    let mut ids = Vec::new();
    for _ in 0..3 {
        let request = IssueRequest::new(
            UserId::new(1).unwrap(),
            "Audit fixture name",
            KeyKind::User,
            KeyScope::Rpc,
            None,
        )
        .unwrap();
        let issued = issue(&mut tx, &hash, &request).await.unwrap();
        ids.push(issued.id().get());
        credentials.push(issued.prefix().to_owned());
        credentials.push(issued.into_secret().expose().to_owned());
    }
    tx.execute(
        "UPDATE res_users_apikeys SET expiration_date=now() AT TIME ZONE 'utc' - interval '1 second' WHERE id=ANY($1)",
        &[&ids],
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    drop(tx);
    // issuance wrote its three events; collection has written none yet
    assert_eq!(audit_events(&c).await.len(), 3);

    let mut rolled_back = audited(&h).await;
    assert_eq!(collect_expired(&mut rolled_back, 2).await.unwrap(), 2);
    rolled_back.rollback().await.unwrap();
    drop(rolled_back);
    assert_eq!(
        audit_events(&c).await.len(),
        3,
        "a rolled back batch leaves no record"
    );
    for (limit, expected) in [(2, 2), (2, 1), (2, 0)] {
        let mut tx = audited(&h).await;
        assert_eq!(collect_expired(&mut tx, limit).await.unwrap(), expected);
        tx.commit().await.unwrap();
    }
    let rows = c
        .query(
            "SELECT a.origin, jsonb_array_length(e->'key_ids')::int4, \
             ARRAY(SELECT k::bigint FROM jsonb_array_elements_text(e->'key_ids') k) \
             FROM erp_event.ir_audit_entry a, jsonb_array_elements(a.events) e \
             WHERE e->>'event' = 'api_keys.collected' ORDER BY a.id",
            &[],
        )
        .await
        .unwrap();
    assert_eq!(
        rows.len(),
        2,
        "one event per committed batch, none for an empty one"
    );
    let mut named = Vec::new();
    for row in &rows {
        assert_eq!(row.get::<_, String>(0), "auth");
        let batch: Vec<i64> = row.get(2);
        assert_eq!(usize::try_from(row.get::<_, i32>(1)).unwrap(), batch.len());
        assert!(batch.len() <= 2, "a batch never exceeds its limit");
        named.extend(batch);
    }
    named.sort_unstable();
    assert_eq!(named, ids, "the events name exactly the deleted keys");
    let all: String = c
        .query_one(
            "SELECT coalesce(string_agg(a::text, ' '), '') FROM erp_event.ir_audit_entry a",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    for credential in &credentials {
        assert!(
            !all.contains(credential.as_str()),
            "credential text in the audit trail"
        );
    }
    assert!(
        !all.contains("Audit fixture name"),
        "key name in the audit trail"
    );
}

/// fields come from the stored key)
#[tokio::test]
async fn issue_writes_exactly_one_event_matching_the_stored_key() {
    let h = support::harness().await;
    let c = support::connect(&h).await;
    c.batch_execute("UPDATE res_users SET active=true WHERE id=1; INSERT INTO res_groups(id,name) VALUES(10,'Settings'); INSERT INTO ir_model_data(module,name,model,res_id) VALUES('base','group_system','res.groups',10); INSERT INTO res_groups_users_rel(uid,gid) VALUES(1,10)").await.unwrap();
    let hash = ApiKeyHasher::derive(
        &DatabaseSecret::from_stored("0123456789abcdef0123456789abcdef").unwrap(),
    )
    .unwrap();
    let request = IssueRequest::new(
        UserId::new(1).unwrap(),
        "Direct issue name",
        KeyKind::User,
        KeyScope::Rpc,
        None,
    )
    .unwrap();
    let mut tx = h.begin().await.unwrap();
    let issued = issue(&mut tx, &hash, &request).await.unwrap();
    let id = issued.id().get();
    let credential = issued.into_secret().expose().to_owned();
    tx.commit().await.unwrap();
    drop(tx);
    let events = audit_events(&c).await;
    assert_eq!(events.len(), 1, "exactly one event");
    assert_eq!(events[0].0, "api_key.issued");
    let row = c
        .query_one(
            "SELECT e->>'key_id' = k.id::text, e->>'owner' = k.user_id::text, \
             e->>'created_by' = k.created_by::text, e->>'key_kind' = k.kind, e->>'route' = k.scope, \
             (e->>'read_only')::bool = k.read_only, \
             (e->>'capabilities_subject')::bool = (k.capabilities = 'subject'), \
             e->'expires_utc_micros' = 'null'::jsonb AND k.expiration_date IS NULL, \
             e->'model_allowlist_len' = 'null'::jsonb AND k.models IS NULL \
             FROM erp_event.ir_audit_entry a, jsonb_array_elements(a.events) e, res_users_apikeys k \
             WHERE k.id = $1",
            &[&id],
        )
        .await
        .unwrap();
    for column in 0..9 {
        assert!(
            row.get::<_, bool>(column),
            "field {column} matches the stored key"
        );
    }
    let all = events[0].1.clone();
    assert!(!all.contains(&credential) && !all.contains("Direct issue name"));
}

/// row exists)
/// nothing)
#[tokio::test]
async fn audited_writes_without_operation_identity_roll_back() {
    let h = support::harness_untagged().await;
    let c = support::connect(&h).await;
    c.batch_execute("UPDATE res_users SET active=true WHERE id=1; INSERT INTO res_groups(id,name) VALUES(10,'Settings'); INSERT INTO ir_model_data(module,name,model,res_id) VALUES('base','group_system','res.groups',10); INSERT INTO res_groups_users_rel(uid,gid) VALUES(1,10)").await.unwrap();
    let hash = ApiKeyHasher::derive(
        &DatabaseSecret::from_stored("0123456789abcdef0123456789abcdef").unwrap(),
    )
    .unwrap();
    let request = || {
        IssueRequest::new(
            UserId::new(1).unwrap(),
            "No identity",
            KeyKind::User,
            KeyScope::Rpc,
            None,
        )
        .unwrap()
    };
    let count = async || -> i64 {
        c.query_one("SELECT count(*) FROM res_users_apikeys", &[])
            .await
            .unwrap()
            .get(0)
    };
    // issuance: staged, then refused at commit; no key row survives
    let mut tx = h.begin().await.unwrap();
    issue(&mut tx, &hash, &request()).await.unwrap();
    let e = tx.commit().await.unwrap_err();
    assert_eq!(e.kind(), ErrorKind::Programming);
    assert!(e.to_string().starts_with("audit:"), "{e}");
    tx.rollback().await.unwrap();
    drop(tx);
    assert_eq!(count().await, 0);
    // collection: an expired key stays when the commit is refused
    let mut tx = audited(&h).await;
    let id = issue(&mut tx, &hash, &request()).await.unwrap().id().get();
    tx.execute("UPDATE res_users_apikeys SET expiration_date=now() AT TIME ZONE 'utc' - interval '1 second' WHERE id=$1", &[&id]).await.unwrap();
    tx.commit().await.unwrap();
    drop(tx);
    let mut tx = h.begin().await.unwrap();
    assert_eq!(collect_expired(&mut tx, 10).await.unwrap(), 1);
    let e = tx.commit().await.unwrap_err();
    assert_eq!(e.kind(), ErrorKind::Programming);
    assert!(e.to_string().starts_with("audit:"), "{e}");
    tx.rollback().await.unwrap();
    drop(tx);
    assert_eq!(count().await, 1);
}
