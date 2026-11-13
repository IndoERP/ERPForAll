// SPDX-License-Identifier: MIT OR Apache-2.0
#![allow(dead_code, reason = "shared integration-test helpers")]
#[path = "audit_chain.rs"]
mod chain;
pub async fn begin_unsettled(h: &Harness) -> erp_orm::Transaction {
    chain::begin(h).await
}
pub use chain::{installed, owner, refused};

// Snapshot xmin is cluster-wide: unrelated test transactions can legitimately delay admission.
// Wait for the actual database frontier, never an assumed elapsed commit delay.
pub async fn begin(h: &Harness) -> erp_orm::Transaction {
    let c = owner(h).await;
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        let safe:bool=c.query_one("SELECT COALESCE(max(xid),'0'::xid8)<pg_snapshot_xmin(pg_current_snapshot()) FROM erp_event.ir_audit_entry",&[]).await.unwrap().get(0);
        if safe {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "committed test rows did not reach the snapshot xmin frontier"
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    chain::begin(h).await
}
use erp_orm::audit::seal::{EpochReason, SealOutcome, SealReport};
use erp_testkit::{Harness, tokio_postgres::Client};

pub async fn initialized() -> Harness {
    let h = installed().await;
    let mut tx = begin(&h).await;
    tx.initialize_audit_chain(&EpochReason::new("sealer.test").unwrap())
        .await
        .unwrap();
    tx.commit().await.unwrap();
    h
}
pub async fn insert(c: &Client, bytes: i32, events: &str) -> (u64, i64) {
    let r = c.query_one("INSERT INTO erp_event.ir_audit_entry(schema_version,entry_kind,origin,outcome,attempt,operation_id,principal_kind,subject_id,actor_kind,actor_id,company_ids,target_ids,events,encoded_bytes) VALUES(1,'change','internal','committed',1,uuidv7(),'user',1,'user',1,ARRAY[1,2]::bigint[],'[]',$1::text::jsonb,$2) RETURNING xid::text,id", &[&events,&bytes]).await.unwrap();
    (r.get::<_, String>(0).parse().unwrap(), r.get(1))
}
pub fn staged(value: SealOutcome) -> SealReport {
    match value {
        SealOutcome::Staged(r) => r,
        other => panic!("expected staged seal, got {other:?}"),
    }
}
pub async fn seals(c: &Client) -> i64 {
    c.query_one("SELECT count(*) FROM erp_event.ir_audit_seal", &[])
        .await
        .unwrap()
        .get(0)
}
