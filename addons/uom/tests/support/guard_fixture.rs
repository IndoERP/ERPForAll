//! Owner-only setup in fresh throwaway schema fixtures, not module lifecycle evidence.
use erp_testkit::Harness;
pub async fn install(h: &Harness) {
    let mut tx = h.begin().await.unwrap();
    tx.batch(include_str!("../../src/guard_sql.sql"))
        .await
        .unwrap();
    tx.execute("INSERT INTO uom_reference_fence(singleton) VALUES (1)", &[])
        .await
        .unwrap();
    tx.commit().await.unwrap();
}
