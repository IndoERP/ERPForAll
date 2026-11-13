// SPDX-License-Identifier: MIT OR Apache-2.0
#![allow(dead_code, reason = "shared integration-test helpers")]
use erp_fields::ErrorKind;
use erp_orm::Transaction;
use erp_test_models::base_users;
use erp_testkit::{
    Harness,
    tokio_postgres::{Client, NoTls},
};

pub async fn harness() -> Harness {
    Harness::builder([&base_users::ADDON])
        .build()
        .await
        .unwrap()
}
pub async fn installed() -> Harness {
    let h = harness().await;
    let mut tx = begin(&h).await;
    install(&mut tx).await;
    tx.commit().await.unwrap();
    h
}
pub async fn owner(h: &Harness) -> Client {
    let (c, connection) = h.db().config().connect(NoTls).await.unwrap();
    tokio::spawn(connection);
    c
}
pub fn refused(e: &erp_fields::Error, message: &str) {
    assert_eq!(e.kind(), ErrorKind::Programming);
    assert_eq!(e.message(), message);
}
pub async fn count(tx: &mut Transaction) -> i64 {
    tx.query("SELECT count(*) FROM erp_event.ir_audit_chain", &[])
        .await
        .unwrap()[0]
        .get(0)
}
// Metadata fixture only: this does not seal or verify database audit rows.
pub async fn seed_seal(tx: &mut Transaction) -> Vec<u8> {
    use erp_orm::audit::seal::{
        BatchDigest, CanonicalValue, RowDigest, SealCursor, SealIdentity, SealInput,
    };
    let names = tx.query("SELECT attname::text FROM pg_attribute WHERE attrelid='erp_event.ir_audit_entry'::regclass AND attnum>0 AND NOT attisdropped", &[]).await.unwrap();
    let names: Vec<String> = names.iter().map(|r| r.get(0)).collect();
    let values: Vec<(&str, CanonicalValue)> = names
        .iter()
        .map(|n| (n.as_str(), CanonicalValue::null()))
        .collect();
    let digest = BatchDigest::new(&[RowDigest::schema1(&values).unwrap()]).unwrap();
    let incarnation: Vec<u8> = tx
        .query(
            "SELECT uuid_send(incarnation) FROM erp_event.ir_audit_chain WHERE epoch=1",
            &[],
        )
        .await
        .unwrap()[0]
        .get(0);
    let hash = SealInput::new(
        SealIdentity::new(incarnation.try_into().unwrap(), 1, 1).unwrap(),
        SealCursor::new(0, 0).unwrap(),
        SealCursor::new(42, 7).unwrap(),
        digest,
        [0; 32],
    )
    .unwrap()
    .hash()
    .to_vec();
    tx.query("INSERT INTO erp_event.ir_audit_seal (chain_epoch,batch_no,lower_xid,lower_id,upper_xid,upper_id,row_count,batch_digest,prev_seal_hash,seal_hash,format_version) VALUES (1,1,'0',0,'42',7,1,$1,$2,$3,1)", &[&&digest.as_bytes()[..], &vec![0u8;32], &hash]).await.unwrap();
    hash
}

pub async fn install(tx: &mut Transaction) -> erp_orm::audit::seal::ChainSchemaReport {
    match tx.install_audit_chain_schema().await {
        Ok(report) => report,
        Err(error) => {
            let catalog = tx.query("SELECT conname::text,pg_get_constraintdef(oid),connoinherit FROM pg_constraint WHERE conrelid=to_regclass('erp_event.ir_audit_chain') AND contype<>'n' ORDER BY conname", &[]).await;
            if let Ok(rows) = catalog {
                for row in rows {
                    eprintln!(
                        "constraint {}: {} noinherit={}",
                        row.get::<_, String>(0),
                        row.get::<_, String>(1),
                        row.get::<_, bool>(2)
                    );
                }
            }
            panic!("chain installation failed: {error}");
        }
    }
}

pub async fn begin(h: &Harness) -> Transaction {
    h.orm()
        .begin_opts(
            erp_orm::BeginOptions::new(erp_orm::SUPERUSER_ID)
                .isolation(erp_orm::Isolation::ReadCommitted),
        )
        .await
        .unwrap()
}
