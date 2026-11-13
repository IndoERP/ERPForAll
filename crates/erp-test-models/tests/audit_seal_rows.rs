// SPDX-License-Identifier: MIT OR Apache-2.0
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]
#[path = "support/audit_sealer.rs"]
mod support;
use erp_orm::audit::seal::{
    BatchDigest, CanonicalValue as V, RowDigest, SealCursor, SealIdentity, SealInput, SealOptions,
};
use support::*;

#[tokio::test]
async fn persisted_row_matches_independently_mapped_canonical_values() {
    let h = initialized().await;
    let c = owner(&h).await;
    let cursor = insert(
        &c,
        123,
        r#"[{"z":null,"n":9007199254740993.100,"a":[true,"é"]}]"#,
    )
    .await;
    c.batch_execute("UPDATE erp_event.ir_audit_entry SET error_class='validation',correlation_id=uuidv7(),causation_id=uuidv7(),original_operation_id=uuidv7(),delegating_user_id=7,registry_generation=11,policy_generation=12,model='res.partner',method='write',effect='write',target_ids='[42,9007199254740993.1]',args_digest=decode(repeat('ab',32),'hex'),redaction_version=2").await.unwrap();
    let r=c.query_one("SELECT id,xid::text,(extract(epoch FROM create_date)*1000000)::bigint,uuid_send(entry_id),uuid_send(operation_id),uuid_send(correlation_id),uuid_send(causation_id),uuid_send(original_operation_id) FROM erp_event.ir_audit_entry", &[]).await.unwrap();
    let uuid = |i| V::uuid(r.get::<_, Vec<u8>>(i).try_into().unwrap());
    let events = V::array(&[V::object(&[
        ("z", V::null()),
        ("n", V::numeric("9007199254740993.1").unwrap()),
        (
            "a",
            V::array(&[V::boolean(true), V::text("é").unwrap()]).unwrap(),
        ),
    ])
    .unwrap()])
    .unwrap();
    let columns = vec![
        ("id", V::integer(r.get(0))),
        ("xid", V::xid(r.get::<_, String>(1).parse().unwrap())),
        ("create_date", V::timestamp_micros(r.get(2))),
        ("entry_id", uuid(3)),
        ("operation_id", uuid(4)),
        ("correlation_id", uuid(5)),
        ("causation_id", uuid(6)),
        ("original_operation_id", uuid(7)),
        ("schema_version", V::integer(1)),
        ("entry_kind", V::text("change").unwrap()),
        ("origin", V::text("internal").unwrap()),
        ("outcome", V::text("committed").unwrap()),
        ("error_class", V::text("validation").unwrap()),
        ("attempt", V::integer(1)),
        ("principal_kind", V::text("user").unwrap()),
        ("subject_id", V::integer(1)),
        ("actor_kind", V::text("user").unwrap()),
        ("actor_id", V::integer(1)),
        ("delegating_user_id", V::integer(7)),
        (
            "company_ids",
            V::array(&[V::integer(1), V::integer(2)]).unwrap(),
        ),
        ("registry_generation", V::integer(11)),
        ("policy_generation", V::integer(12)),
        ("model", V::text("res.partner").unwrap()),
        ("method", V::text("write").unwrap()),
        ("effect", V::text("write").unwrap()),
        (
            "target_ids",
            V::array(&[
                V::numeric("42").unwrap(),
                V::numeric("9007199254740993.1").unwrap(),
            ])
            .unwrap(),
        ),
        ("args_digest", V::bytes(&[0xab; 32]).unwrap()),
        ("redaction_version", V::integer(2)),
        ("events", events),
        ("encoded_bytes", V::integer(123)),
    ];
    let batch = BatchDigest::new(&[RowDigest::schema1(&columns).unwrap()]).unwrap();
    let mut tx = begin(&h).await;
    tx.batch("SET LOCAL TIME ZONE 'Asia/Jakarta'")
        .await
        .unwrap();
    let seal = staged(tx.seal_audit_batch(&SealOptions::default()).await.unwrap());
    assert_eq!(seal.upper_cursor(), cursor);
    assert_eq!(seal.batch_digest(), batch.as_bytes());
    let expected = SealInput::new(
        SealIdentity::new(*seal.identity().incarnation(), 1, 1).unwrap(),
        SealCursor::new(0, 0).unwrap(),
        SealCursor::new(cursor.0, cursor.1).unwrap(),
        batch,
        [0; 32],
    )
    .unwrap()
    .hash();
    assert_eq!(seal.seal_hash(), &expected);
    tx.commit().await.unwrap();
}

#[tokio::test]
async fn unsupported_shapes_and_forged_small_estimates_are_refused() {
    let h = initialized().await;
    let c = owner(&h).await;
    insert(&c, 1, "[]").await;
    for (sql, message) in [
        (
            "UPDATE erp_event.ir_audit_entry SET company_ids='[0:1]={1,2}'::bigint[]",
            "a later release: persisted audit value has an unsupported shape or type",
        ),
        (
            "UPDATE erp_event.ir_audit_entry SET company_ids=ARRAY[1,2]::bigint[],events=jsonb_build_array(repeat('x',8388608))",
            "a later release: projected row payload exceeds the safety limit",
        ),
    ] {
        c.batch_execute(sql).await.unwrap();
        let mut tx = begin(&h).await;
        refused(
            &tx.seal_audit_batch(&SealOptions::default())
                .await
                .unwrap_err(),
            message,
        );
        tx.rollback().await.unwrap();
        drop(tx);
        assert_eq!(seals(&c).await, 0);
    }
}

#[tokio::test]
async fn extra_persisted_column_is_not_ignored() {
    let h = initialized().await;
    let c = owner(&h).await;
    insert(&c, 10, "[]").await;
    c.batch_execute("ALTER TABLE erp_event.ir_audit_entry ADD COLUMN unrecognized text")
        .await
        .unwrap();
    let mut tx = begin(&h).await;
    let e = tx
        .seal_audit_batch(&SealOptions::default())
        .await
        .unwrap_err();
    assert_eq!(e.kind(), erp_fields::ErrorKind::Programming);
    assert_eq!(
        e.message(),
        "a later release event schema refused (forged-column): \"erp_event\".\"ir_audit_entry\" has an unexpected column \"unrecognized\"; nothing was changed for it, fix or drop the object by hand as the owner role (the install never adopts or repairs it)"
    );
    tx.rollback().await.unwrap();
}

#[tokio::test]
async fn actual_transport_budget_stops_at_a_contiguous_prefix() {
    let h = initialized().await;
    let c = owner(&h).await;
    let events = format!("[\"{}\"]", "x".repeat(3 * 1024 * 1024));
    let mut cursors = Vec::new();
    for _ in 0..6 {
        cursors.push(insert(&c, 1, &events).await);
    }
    let mut tx = begin(&h).await;
    let first = staged(tx.seal_audit_batch(&SealOptions::default()).await.unwrap());
    assert_eq!(first.row_count(), 5);
    assert_eq!(first.upper_cursor(), cursors[4]);
    tx.commit().await.unwrap();
    drop(tx);
    let mut tx = begin(&h).await;
    let second = staged(tx.seal_audit_batch(&SealOptions::default()).await.unwrap());
    assert_eq!(second.row_count(), 1);
    assert_eq!(second.lower_cursor(), cursors[4]);
    assert_eq!(second.upper_cursor(), cursors[5]);
    tx.commit().await.unwrap();
}
