// SPDX-License-Identifier: MIT OR Apache-2.0
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]
#[path = "support/audit_sealer.rs"]
mod support;
use erp_fields::ErrorKind;
use erp_orm::audit::seal::{
    Anchor, AnchorInventory, Assurance, EpochChange, EpochReason, FailurePoint, SealIdentity,
    SealOptions, SealReport, VerifyOutcome, VerifyProblem, VerifyReport, VerifyScope, WitnessError,
    WitnessEvidence,
};
use erp_testkit::{Harness, tokio_postgres::Client};
use support::*;

const PREFIX: &str = "a later release: ";

struct Fixture {
    h: Harness,
    c: Client,
    rows: Vec<(u64, i64)>,
    seals: Vec<SealReport>,
}
async fn seal_one(h: &Harness) -> SealReport {
    seal_with(h, 1).await
}
async fn seal_with(h: &Harness, rows: u32) -> SealReport {
    let mut tx = begin(h).await;
    let report = staged(
        tx.seal_audit_batch(&SealOptions::new(rows, 1000).unwrap())
            .await
            .unwrap(),
    );
    tx.commit().await.unwrap();
    report
}
// `n` rows, each sealed in its own batch of epoch 1.
async fn fixture(n: usize) -> Fixture {
    let h = initialized().await;
    let c = owner(&h).await;
    let mut rows = Vec::new();
    for _ in 0..n {
        rows.push(insert(&c, 10, "[]").await);
    }
    let mut seals = Vec::new();
    for _ in 0..n {
        seals.push(seal_one(&h).await);
    }
    Fixture { h, c, rows, seals }
}
async fn verify_tx(h: &Harness, scope: &VerifyScope, w: &WitnessEvidence) -> VerifyOutcome {
    let mut tx = h
        .orm()
        .begin_opts(
            erp_orm::BeginOptions::new(erp_orm::SUPERUSER_ID)
                .isolation(erp_orm::Isolation::RepeatableRead),
        )
        .await
        .unwrap();
    let out = tx.verify_audit_chain(scope, w).await.unwrap();
    tx.rollback().await.unwrap();
    out
}
fn anchor(r: &SealReport) -> Anchor {
    Anchor::new(r.identity(), *r.seal_hash(), 1).unwrap()
}
fn evidence(reports: &[&SealReport]) -> WitnessEvidence {
    WitnessEvidence::Inventory(
        AnchorInventory::new(reports.iter().map(|r| anchor(r)).collect()).unwrap(),
    )
}
fn seal_at(epoch: u64, batch_no: u64) -> FailurePoint {
    FailurePoint::Seal { epoch, batch_no }
}
fn verified(out: VerifyOutcome) -> VerifyReport {
    match out {
        VerifyOutcome::Verified(r) => r,
        VerifyOutcome::Failed(f) => panic!("expected verified, got {f}"),
    }
}
fn failed(out: VerifyOutcome, at: FailurePoint, problem: VerifyProblem, message: &str) {
    match out {
        VerifyOutcome::Failed(f) => {
            assert_eq!(f.at(), at);
            assert_eq!(f.problem(), problem);
            assert_eq!(f.to_string(), message);
            assert!(f.problem().to_string().starts_with(PREFIX));
        }
        VerifyOutcome::Verified(r) => panic!("expected failure, got {r:?}"),
    }
}
async fn micros(c: &Client, sql: &str) -> Vec<Option<i64>> {
    let r = c.query_one(sql, &[]).await.unwrap();
    (0..r.len()).map(|i| r.get(i)).collect()
}
const SEAL_MICROS: &str = "SELECT min((extract(epoch FROM create_date)*1000000)::bigint),max((extract(epoch FROM create_date)*1000000)::bigint) FROM erp_event.ir_audit_seal";

#[tokio::test]
async fn empty_store_reports_zero_epochs_and_anchors_need_a_chain() {
    let h = installed().await;
    let none = verified(verify_tx(&h, &VerifyScope::full(), &WitnessEvidence::NotConfigured).await);
    assert!(!none.partial());
    assert_eq!((none.epochs(), none.seals(), none.rows()), (0, 0, 0));
    assert_eq!(none.assurance(), Assurance::NoWitness);
    assert_eq!(none.unsealed_tail().unwrap().count(), 0);
    insert(&owner(&h).await, 10, "[]").await;
    let none = verified(verify_tx(&h, &VerifyScope::full(), &WitnessEvidence::NotConfigured).await);
    assert_eq!(none.unsealed_tail().unwrap().count(), 1);
    let empty = WitnessEvidence::Inventory(AnchorInventory::new(vec![]).unwrap());
    let r = verified(verify_tx(&h, &VerifyScope::full(), &empty).await);
    assert_eq!(r.assurance(), Assurance::NoAnchors);
    let stray = Anchor::new(SealIdentity::new([7; 16], 1, 1).unwrap(), [9; 32], 1).unwrap();
    let w = WitnessEvidence::Inventory(AnchorInventory::new(vec![stray]).unwrap());
    failed(
        verify_tx(&h, &VerifyScope::full(), &w).await,
        FailurePoint::Chain,
        VerifyProblem::ChainMissing,
        "a later release: anchors exist but no audit chain is initialized (at chain)",
    );
    failed(
        verify_tx(
            &h,
            &VerifyScope::range(1, 1, 1).unwrap(),
            &WitnessEvidence::NotConfigured,
        )
        .await,
        FailurePoint::Epoch(1),
        VerifyProblem::RangeNotFound,
        "a later release: requested epoch or batch range has no stored seal (at epoch 1)",
    );
}

#[tokio::test]
async fn initialized_chain_without_seals_has_an_empty_head() {
    let h = initialized().await;
    let c = owner(&h).await;
    insert(&c, 10, "[]").await;
    let r = verified(verify_tx(&h, &VerifyScope::full(), &WitnessEvidence::NotConfigured).await);
    assert_eq!((r.epochs(), r.seals(), r.rows()), (1, 0, 0));
    assert!(r.head_epoch_empty());
    assert_eq!(r.unsealed_tail().unwrap().count(), 1);
}

#[tokio::test]
async fn intact_history_reports_assurance_and_tails() {
    let f = fixture(3).await;
    let [a, b, c] = [&f.seals[0], &f.seals[1], &f.seals[2]];
    let created = micros(&f.c, SEAL_MICROS).await;
    let none =
        verified(verify_tx(&f.h, &VerifyScope::full(), &WitnessEvidence::NotConfigured).await);
    assert_eq!((none.epochs(), none.seals(), none.rows()), (1, 3, 3));
    assert_eq!(none.assurance(), Assurance::NoWitness);
    assert_eq!(none.unanchored_tail().count(), 3);
    assert_eq!(
        [
            none.unanchored_tail().first_micros(),
            none.unanchored_tail().last_micros()
        ],
        [created[0], created[1]]
    );
    assert_eq!(none.unsealed_tail().unwrap().count(), 0);
    assert!(!none.head_epoch_empty());
    let empty = WitnessEvidence::Inventory(AnchorInventory::default());
    let r = verified(verify_tx(&f.h, &VerifyScope::full(), &empty).await);
    assert_eq!(r.assurance(), Assurance::NoAnchors);
    assert_eq!(r.unanchored_tail().count(), 3);
    let r = verified(verify_tx(&f.h, &VerifyScope::full(), &evidence(&[a, b, c])).await);
    assert_eq!(
        r.assurance(),
        Assurance::AnchoredThrough {
            epoch: 1,
            batch_no: 3
        }
    );
    assert_eq!(r.unanchored_tail().count(), 0);
    let r = verified(verify_tx(&f.h, &VerifyScope::full(), &evidence(&[a, b])).await);
    assert_eq!(
        r.assurance(),
        Assurance::AnchoredThrough {
            epoch: 1,
            batch_no: 2
        }
    );
    assert_eq!(r.unanchored_tail().count(), 1);
    // An unanchored seal before an anchored one is covered by the chain links.
    let r = verified(verify_tx(&f.h, &VerifyScope::full(), &evidence(&[c])).await);
    assert_eq!(
        r.assurance(),
        Assurance::AnchoredThrough {
            epoch: 1,
            batch_no: 3
        }
    );
    insert(&f.c, 10, "[]").await;
    insert(&f.c, 10, "[]").await;
    let tail = micros(&f.c, "SELECT min((extract(epoch FROM create_date)*1000000)::bigint),max((extract(epoch FROM create_date)*1000000)::bigint) FROM erp_event.ir_audit_entry WHERE id>(SELECT max(upper_id) FROM erp_event.ir_audit_seal)").await;
    let r = verified(verify_tx(&f.h, &VerifyScope::full(), &WitnessEvidence::NotConfigured).await);
    let unsealed = r.unsealed_tail().unwrap();
    assert_eq!(unsealed.count(), 2);
    assert_eq!(
        [unsealed.first_micros(), unsealed.last_micros()],
        [tail[0], tail[1]]
    );
}

#[tokio::test]
async fn changed_or_deleted_rows_fail_their_seal() {
    let cases: [(&str, VerifyProblem, FailurePoint, &str); 6] = [
        (
            "UPDATE erp_event.ir_audit_entry SET subject_id=99 WHERE id=$ID1",
            VerifyProblem::BatchDigestMismatch,
            seal_at(1, 2),
            "a later release: recomputed batch digest differs from the stored digest (at seal epoch 1 batch 2)",
        ),
        (
            "DELETE FROM erp_event.ir_audit_entry WHERE id=$ID1",
            VerifyProblem::RowCountMismatch,
            seal_at(1, 2),
            "a later release: stored row count differs from the rows in the sealed range (at seal epoch 1 batch 2)",
        ),
        (
            "UPDATE erp_event.ir_audit_entry SET id=id+1000000 WHERE id=$ID1",
            VerifyProblem::RowCountMismatch,
            seal_at(1, 2),
            "a later release: stored row count differs from the rows in the sealed range (at seal epoch 1 batch 2)",
        ),
        (
            "UPDATE erp_event.ir_audit_entry SET xid='1'::xid8 WHERE id=$ID1",
            VerifyProblem::RowCountMismatch,
            seal_at(1, 1),
            "a later release: stored row count differs from the rows in the sealed range (at seal epoch 1 batch 1)",
        ),
        (
            "UPDATE erp_event.ir_audit_entry SET schema_version=2 WHERE id=$ID1",
            VerifyProblem::RowSchema,
            seal_at(1, 2),
            "a later release: a sealed row has an unsupported schema version (at seal epoch 1 batch 2)",
        ),
        (
            "UPDATE erp_event.ir_audit_entry SET subject_id=77",
            VerifyProblem::BatchDigestMismatch,
            seal_at(1, 1),
            "a later release: recomputed batch digest differs from the stored digest (at seal epoch 1 batch 1)",
        ),
    ];
    for (sql, problem, at, message) in cases {
        let f = fixture(3).await;
        f.c.batch_execute(&sql.replace("$ID1", &f.rows[1].1.to_string()))
            .await
            .unwrap();
        failed(
            verify_tx(&f.h, &VerifyScope::full(), &WitnessEvidence::NotConfigured).await,
            at,
            problem,
            message,
        );
    }
}

#[tokio::test]
async fn edited_seal_columns_fail_at_their_seal() {
    let zero = "decode(repeat('00',32),'hex')";
    let cases: [(String, VerifyProblem, u64, &str); 6] = [
        (
            "row_count=2".into(),
            VerifyProblem::RowCountMismatch,
            2,
            "stored row count differs from the rows in the sealed range",
        ),
        (
            format!("batch_digest={zero}"),
            VerifyProblem::BatchDigestMismatch,
            2,
            "recomputed batch digest differs from the stored digest",
        ),
        (
            format!("seal_hash={zero}"),
            VerifyProblem::SealHashMismatch,
            2,
            "recomputed seal hash differs from the stored hash",
        ),
        (
            format!("prev_seal_hash={zero}"),
            VerifyProblem::PredecessorHash,
            2,
            "seal predecessor hash does not match the previous seal",
        ),
        (
            "upper_id=upper_id+1".into(),
            VerifyProblem::UpperCursor,
            2,
            "last sealed row does not match the stored upper cursor",
        ),
        (
            "lower_xid='0',lower_id=0".into(),
            VerifyProblem::CursorChain,
            3,
            "seal lower cursor does not continue the previous boundary",
        ),
    ];
    for (set, problem, batch, text) in cases {
        let f = fixture(3).await;
        f.c.batch_execute(&format!(
            "UPDATE erp_event.ir_audit_seal SET {set} WHERE batch_no={batch}"
        ))
        .await
        .unwrap();
        failed(
            verify_tx(&f.h, &VerifyScope::full(), &WitnessEvidence::NotConfigured).await,
            seal_at(1, batch),
            problem,
            &format!("{PREFIX}{text} (at seal epoch 1 batch {batch})"),
        );
    }
}

#[tokio::test]
async fn deleted_seals_need_anchors_unless_a_later_seal_remains() {
    let f = fixture(3).await;
    let all = evidence(&[&f.seals[0], &f.seals[1], &f.seals[2]]);
    f.c.batch_execute("DELETE FROM erp_event.ir_audit_seal WHERE batch_no=2")
        .await
        .unwrap();
    failed(
        verify_tx(&f.h, &VerifyScope::full(), &WitnessEvidence::NotConfigured).await,
        seal_at(1, 2),
        VerifyProblem::SealNumbering,
        "a later release: seal batch numbers are not contiguous (at seal epoch 1 batch 2)",
    );
    failed(
        verify_tx(&f.h, &VerifyScope::full(), &all).await,
        seal_at(1, 2),
        VerifyProblem::AnchorWithoutSeal,
        "a later release: anchored seal is missing from the database (at seal epoch 1 batch 2)",
    );
    let f = fixture(3).await;
    let all = evidence(&[&f.seals[0], &f.seals[1], &f.seals[2]]);
    f.c.batch_execute("DELETE FROM erp_event.ir_audit_seal WHERE batch_no=3")
        .await
        .unwrap();
    let r = verified(verify_tx(&f.h, &VerifyScope::full(), &WitnessEvidence::NotConfigured).await);
    assert_eq!(r.seals(), 2);
    assert_eq!(r.unsealed_tail().unwrap().count(), 1);
    failed(
        verify_tx(&f.h, &VerifyScope::full(), &all).await,
        seal_at(1, 3),
        VerifyProblem::AnchorWithoutSeal,
        "a later release: anchored seal is missing from the database (at seal epoch 1 batch 3)",
    );
}

#[tokio::test]
async fn anchor_hash_mismatch_and_first_defect_order() {
    let f = fixture(3).await;
    let wrong = Anchor::new(f.seals[1].identity(), [0xab; 32], 1).unwrap();
    let w = WitnessEvidence::Inventory(AnchorInventory::new(vec![wrong]).unwrap());
    failed(
        verify_tx(&f.h, &VerifyScope::full(), &w).await,
        seal_at(1, 2),
        VerifyProblem::AnchorHashMismatch,
        "a later release: stored seal hash differs from its anchor (at seal epoch 1 batch 2)",
    );
    // Seal 3 is damaged and anchored seal 2 is missing: the earlier position is reported.
    f.c.batch_execute("UPDATE erp_event.ir_audit_seal SET seal_hash=decode(repeat('00',32),'hex') WHERE batch_no=3; DELETE FROM erp_event.ir_audit_seal WHERE batch_no=2")
        .await
        .unwrap();
    failed(
        verify_tx(&f.h, &VerifyScope::full(), &evidence(&[&f.seals[1]])).await,
        seal_at(1, 2),
        VerifyProblem::AnchorWithoutSeal,
        "a later release: anchored seal is missing from the database (at seal epoch 1 batch 2)",
    );
    // A damaged earlier row is reported before a missing later anchored seal.
    let f = fixture(3).await;
    f.c.batch_execute(&format!(
        "UPDATE erp_event.ir_audit_entry SET subject_id=99 WHERE id={}; DELETE FROM erp_event.ir_audit_seal WHERE batch_no=3",
        f.rows[0].1
    ))
    .await
    .unwrap();
    failed(
        verify_tx(&f.h, &VerifyScope::full(), &evidence(&[&f.seals[2]])).await,
        seal_at(1, 1),
        VerifyProblem::BatchDigestMismatch,
        "a later release: recomputed batch digest differs from the stored digest (at seal epoch 1 batch 1)",
    );
}

#[tokio::test]
async fn epochs_link_and_a_deleted_anchored_epoch_fails() {
    let f = fixture(2).await;
    let mut tx = begin(&f.h).await;
    assert!(matches!(
        tx.new_audit_chain_epoch(&EpochReason::new("rotate.test").unwrap())
            .await
            .unwrap(),
        EpochChange::Created(_)
    ));
    tx.commit().await.unwrap();
    drop(tx);
    insert(&f.c, 10, "[]").await;
    let mut tx = begin(&f.h).await;
    let two = staged(tx.seal_audit_batch(&SealOptions::default()).await.unwrap());
    tx.commit().await.unwrap();
    drop(tx);
    assert_eq!(two.identity().epoch(), 2);
    let all = evidence(&[&f.seals[0], &f.seals[1], &two]);
    let r = verified(verify_tx(&f.h, &VerifyScope::full(), &all).await);
    assert_eq!((r.epochs(), r.seals(), r.rows()), (2, 3, 3));
    assert_eq!(
        r.assurance(),
        Assurance::AnchoredThrough {
            epoch: 2,
            batch_no: 1
        }
    );
    // A range at the start of epoch 2 starts from the inherited boundary.
    let r = verified(verify_tx(&f.h, &VerifyScope::range(2, 1, 1).unwrap(), &all).await);
    assert!(r.partial());
    assert_eq!((r.epochs(), r.seals(), r.rows()), (1, 1, 1));
    assert_eq!(r.unsealed_tail(), None);
    f.c.batch_execute(
        "UPDATE erp_event.ir_audit_chain SET link_hash=decode(repeat('00',32),'hex') WHERE epoch=2",
    )
    .await
    .unwrap();
    failed(
        verify_tx(&f.h, &VerifyScope::full(), &WitnessEvidence::NotConfigured).await,
        FailurePoint::Epoch(2),
        VerifyProblem::EpochLink,
        "a later release: epoch link does not match the predecessor epoch's final seal (at epoch 2)",
    );
    failed(
        verify_tx(
            &f.h,
            &VerifyScope::range(2, 1, 1).unwrap(),
            &WitnessEvidence::NotConfigured,
        )
        .await,
        FailurePoint::Epoch(2),
        VerifyProblem::EpochLink,
        "a later release: epoch link does not match the predecessor epoch's final seal (at epoch 2)",
    );
    f.c.batch_execute("UPDATE erp_event.ir_audit_chain SET link_hash=(SELECT seal_hash FROM erp_event.ir_audit_seal WHERE chain_epoch=1 AND batch_no=2) WHERE epoch=2; UPDATE erp_event.ir_audit_chain SET incarnation=uuidv7() WHERE epoch=2")
        .await
        .unwrap();
    failed(
        verify_tx(&f.h, &VerifyScope::full(), &WitnessEvidence::NotConfigured).await,
        FailurePoint::Epoch(2),
        VerifyProblem::EpochIncarnation,
        "a later release: epoch incarnation differs from the chain's (at epoch 2)",
    );
    // Remove the whole anchored epoch (seals first because of the foreign key).
    f.c.batch_execute("DELETE FROM erp_event.ir_audit_seal WHERE chain_epoch=2; DELETE FROM erp_event.ir_audit_chain WHERE epoch=2")
        .await
        .unwrap();
    failed(
        verify_tx(&f.h, &VerifyScope::full(), &all).await,
        seal_at(2, 1),
        VerifyProblem::AnchorWithoutSeal,
        "a later release: anchored seal is missing from the database (at seal epoch 2 batch 1)",
    );
    let r = verified(verify_tx(&f.h, &VerifyScope::full(), &WitnessEvidence::NotConfigured).await);
    assert_eq!(r.epochs(), 1);
}

#[tokio::test]
async fn empty_non_head_epoch_fails() {
    let f = fixture(1).await;
    f.c.batch_execute(
        "INSERT INTO erp_event.ir_audit_chain(epoch,incarnation,format_version,previous_epoch,link_hash,reason)
         SELECT 2,c.incarnation,1,1,s.seal_hash,'manual.test' FROM erp_event.ir_audit_chain c,erp_event.ir_audit_seal s WHERE c.epoch=1 AND s.batch_no=1;
         INSERT INTO erp_event.ir_audit_chain(epoch,incarnation,format_version,previous_epoch,link_hash,reason)
         SELECT 3,incarnation,1,2,decode(repeat('11',32),'hex'),'manual.test' FROM erp_event.ir_audit_chain WHERE epoch=1",
    )
    .await
    .unwrap();
    failed(
        verify_tx(&f.h, &VerifyScope::full(), &WitnessEvidence::NotConfigured).await,
        FailurePoint::Epoch(2),
        VerifyProblem::EmptyEpoch,
        "a later release: epoch has no seal but is not the head epoch (at epoch 2)",
    );
}

#[tokio::test]
async fn reinitialised_chain_rejects_old_anchors() {
    let f = fixture(2).await;
    let old = evidence(&[&f.seals[0], &f.seals[1]]);
    f.c.batch_execute("DELETE FROM erp_event.ir_audit_seal; DELETE FROM erp_event.ir_audit_chain")
        .await
        .unwrap();
    let mut tx = begin(&f.h).await;
    tx.initialize_audit_chain(&EpochReason::new("again.test").unwrap())
        .await
        .unwrap();
    tx.commit().await.unwrap();
    drop(tx);
    failed(
        verify_tx(&f.h, &VerifyScope::full(), &old).await,
        seal_at(1, 1),
        VerifyProblem::ForeignIncarnation,
        "a later release: anchor belongs to a different chain incarnation (at seal epoch 1 batch 1)",
    );
    failed(
        verify_tx(&f.h, &VerifyScope::range(1, 2, 2).unwrap(), &old).await,
        seal_at(1, 1),
        VerifyProblem::ForeignIncarnation,
        "a later release: anchor belongs to a different chain incarnation (at seal epoch 1 batch 1)",
    );
}

#[tokio::test]
async fn ranges_check_the_predecessor_boundary() {
    let f = fixture(3).await;
    let r = verified(
        verify_tx(
            &f.h,
            &VerifyScope::range(1, 2, 3).unwrap(),
            &WitnessEvidence::NotConfigured,
        )
        .await,
    );
    assert!(r.partial());
    assert_eq!((r.epochs(), r.seals(), r.rows()), (1, 2, 2));
    // A range beyond the last stored seal verifies what is stored.
    let r = verified(
        verify_tx(
            &f.h,
            &VerifyScope::range(1, 1, 100).unwrap(),
            &WitnessEvidence::NotConfigured,
        )
        .await,
    );
    assert_eq!(r.seals(), 3);
    // Anchors outside the range are not compared.
    let outside = evidence(&[&f.seals[0]]);
    let r = verified(verify_tx(&f.h, &VerifyScope::range(1, 2, 3).unwrap(), &outside).await);
    assert_eq!(r.assurance(), Assurance::NoAnchors);
    let inside = evidence(&[&f.seals[1]]);
    let r = verified(verify_tx(&f.h, &VerifyScope::range(1, 2, 3).unwrap(), &inside).await);
    assert_eq!(
        r.assurance(),
        Assurance::AnchoredThrough {
            epoch: 1,
            batch_no: 2
        }
    );
    failed(
        verify_tx(
            &f.h,
            &VerifyScope::range(1, 5, 6).unwrap(),
            &WitnessEvidence::NotConfigured,
        )
        .await,
        FailurePoint::Epoch(1),
        VerifyProblem::RangeNotFound,
        "a later release: requested epoch or batch range has no stored seal (at epoch 1)",
    );
    failed(
        verify_tx(
            &f.h,
            &VerifyScope::range(1, 4, 6).unwrap(),
            &WitnessEvidence::NotConfigured,
        )
        .await,
        FailurePoint::Epoch(1),
        VerifyProblem::RangeNotFound,
        "a later release: requested epoch or batch range has no stored seal (at epoch 1)",
    );
    let absent = Anchor::new(
        SealIdentity::new(*f.seals[0].identity().incarnation(), 1, 4).unwrap(),
        [5; 32],
        1,
    )
    .unwrap();
    let w = WitnessEvidence::Inventory(AnchorInventory::new(vec![absent]).unwrap());
    failed(
        verify_tx(&f.h, &VerifyScope::range(1, 4, 6).unwrap(), &w).await,
        seal_at(1, 4),
        VerifyProblem::AnchorWithoutSeal,
        "a later release: anchored seal is missing from the database (at seal epoch 1 batch 4)",
    );
    failed(
        verify_tx(
            &f.h,
            &VerifyScope::range(9, 1, 1).unwrap(),
            &WitnessEvidence::NotConfigured,
        )
        .await,
        FailurePoint::Epoch(9),
        VerifyProblem::RangeNotFound,
        "a later release: requested epoch or batch range has no stored seal (at epoch 9)",
    );
    // The stored hash of seal 1 is the boundary of a range starting at 2.
    f.c.batch_execute("UPDATE erp_event.ir_audit_seal SET seal_hash=decode(repeat('00',32),'hex') WHERE batch_no=1")
        .await
        .unwrap();
    failed(
        verify_tx(
            &f.h,
            &VerifyScope::range(1, 2, 3).unwrap(),
            &WitnessEvidence::NotConfigured,
        )
        .await,
        seal_at(1, 2),
        VerifyProblem::PredecessorHash,
        "a later release: seal predecessor hash does not match the previous seal (at seal epoch 1 batch 2)",
    );
    f.c.batch_execute("DELETE FROM erp_event.ir_audit_seal WHERE batch_no=1")
        .await
        .unwrap();
    failed(
        verify_tx(
            &f.h,
            &VerifyScope::range(1, 2, 3).unwrap(),
            &WitnessEvidence::NotConfigured,
        )
        .await,
        seal_at(1, 2),
        VerifyProblem::RangePredecessor,
        "a later release: predecessor seal of the requested range is missing (at seal epoch 1 batch 2)",
    );
}

#[test]
fn inventory_validates_its_input() {
    let id = SealIdentity::new([3; 16], 1, 1).unwrap();
    let one = Anchor::new(id, [1; 32], 1).unwrap();
    let inventory = AnchorInventory::new(vec![one, one]).unwrap();
    assert_eq!(inventory.len(), 1);
    let other = Anchor::new(id, [2; 32], 1).unwrap();
    let e = AnchorInventory::new(vec![one, other]).unwrap_err();
    assert_eq!(e, WitnessError::Conflict);
    assert_eq!(e.to_string(), "audit witness: conflicting anchor identity");
    let many: Vec<Anchor> = (1..=65_537u64)
        .map(|b| Anchor::new(SealIdentity::new([3; 16], 1, b).unwrap(), [1; 32], 1).unwrap())
        .collect();
    let e = AnchorInventory::new(many).unwrap_err();
    assert_eq!(
        e.to_string(),
        "audit witness: anchor inventory limit exceeded"
    );
    let sorted = AnchorInventory::new(vec![
        Anchor::new(SealIdentity::new([3; 16], 2, 1).unwrap(), [1; 32], 1).unwrap(),
        one,
    ])
    .unwrap();
    assert_eq!(sorted.anchors()[0], one);
}

#[test]
fn invalid_scopes_are_refused() {
    for (epoch, first, last) in [
        (0, 1, 1),
        (1, 0, 1),
        (1, 2, 1),
        (1, 1, 0),
        (u64::MAX, 1, 1),
        (1, 1, u64::MAX),
    ] {
        let e = VerifyScope::range(epoch, first, last).unwrap_err();
        refused(
            &e,
            "a later release: range scope requires positive SQL bigint epoch and batch bounds with first <= last",
        );
    }
    let scope = VerifyScope::range(2, 3, 4).unwrap();
    assert_eq!(
        (scope.epoch(), scope.batches(), scope.is_full()),
        (Some(2), Some((3, 4)), false)
    );
    assert!(VerifyScope::full().is_full());
}

#[tokio::test]
async fn admission_and_isolation_are_explicit() {
    let f = fixture(1).await;
    let mut tx = begin(&f.h).await;
    let e = tx
        .verify_audit_chain(&VerifyScope::full(), &WitnessEvidence::NotConfigured)
        .await
        .unwrap_err();
    refused(
        &e,
        "a later release: verification requires REPEATABLE READ or SERIALIZABLE isolation",
    );
    tx.rollback().await.unwrap();
    drop(tx);
    let mut tx = f.h.begin_as(2).await.unwrap();
    let before = tx.query_count();
    let e = tx
        .verify_audit_chain(&VerifyScope::full(), &WitnessEvidence::NotConfigured)
        .await
        .unwrap_err();
    assert_eq!(e.kind(), ErrorKind::Access);
    assert_eq!(
        e.message(),
        "Transaction::verify_audit_chain: only a transaction of the System principal with full elevation may do this (this one acts as user 2)"
    );
    assert_eq!(tx.query_count(), before);
    tx.rollback().await.unwrap();
}

// One statement, so every row shares one xid: the sealer's xid ordering cannot cross a decimal
// digit boundary inside the fixture.
async fn insert_many(c: &Client, n: i32) -> Vec<(u64, i64)> {
    let rows = c.query("INSERT INTO erp_event.ir_audit_entry(schema_version,entry_kind,origin,outcome,attempt,operation_id,principal_kind,subject_id,actor_kind,actor_id,company_ids,target_ids,events,encoded_bytes) SELECT 1,'change','internal','committed',1,uuidv7(),'user',1,'user',1,ARRAY[1,2]::bigint[],'[]','[]'::jsonb,10 FROM generate_series(1,$1) RETURNING xid::text,id", &[&n]).await.unwrap();
    let mut out: Vec<(u64, i64)> = rows
        .iter()
        .map(|r| (r.get::<_, String>(0).parse().unwrap(), r.get(1)))
        .collect();
    out.sort_by_key(|c| c.1);
    assert!(out.iter().all(|c| c.0 == out[0].0));
    out
}
// Nine rows in three sealed batches of three.
async fn multi_row_fixture() -> Fixture {
    let h = initialized().await;
    let c = owner(&h).await;
    let rows = insert_many(&c, 9).await;
    let mut seals = Vec::new();
    for _ in 0..3 {
        seals.push(seal_with(&h, 3).await);
    }
    assert!(seals.iter().all(|s| s.row_count() == 3));
    Fixture { h, c, rows, seals }
}

#[tokio::test]
async fn multi_row_batches_verify_and_detect_edits() {
    let f = multi_row_fixture().await;
    let all = evidence(&[&f.seals[0], &f.seals[1], &f.seals[2]]);
    let r = verified(verify_tx(&f.h, &VerifyScope::full(), &all).await);
    assert_eq!((r.epochs(), r.seals(), r.rows()), (1, 3, 9));
    assert_eq!(
        r.assurance(),
        Assurance::AnchoredThrough {
            epoch: 1,
            batch_no: 3
        }
    );
    f.c.batch_execute(&format!(
        "UPDATE erp_event.ir_audit_entry SET subject_id=99 WHERE id={}",
        f.rows[4].1
    ))
    .await
    .unwrap();
    failed(
        verify_tx(&f.h, &VerifyScope::full(), &all).await,
        seal_at(1, 2),
        VerifyProblem::BatchDigestMismatch,
        "a later release: recomputed batch digest differs from the stored digest (at seal epoch 1 batch 2)",
    );
    f.c.batch_execute(&format!(
        "DELETE FROM erp_event.ir_audit_entry WHERE id={}",
        f.rows[1].1
    ))
    .await
    .unwrap();
    failed(
        verify_tx(&f.h, &VerifyScope::full(), &all).await,
        seal_at(1, 1),
        VerifyProblem::RowCountMismatch,
        "a later release: stored row count differs from the rows in the sealed range (at seal epoch 1 batch 1)",
    );
}

#[tokio::test]
async fn repeated_cursor_inside_a_batch_fails_with_row_order() {
    // Rows are read ordered by (xid,id), so disorder can only surface as a repeated cursor: give
    // the third row the second row's id (a different create_date keeps the primary key unique).
    let f = multi_row_fixture().await;
    f.c.batch_execute(&format!(
        "UPDATE erp_event.ir_audit_entry SET id={},create_date=create_date+interval '1 microsecond' WHERE id={}",
        f.rows[1].1, f.rows[2].1
    ))
    .await
    .unwrap();
    failed(
        verify_tx(&f.h, &VerifyScope::full(), &WitnessEvidence::NotConfigured).await,
        seal_at(1, 1),
        VerifyProblem::RowOrder,
        "a later release: sealed rows are not strictly increasing by cursor (at seal epoch 1 batch 1)",
    );
}

#[tokio::test]
async fn limits_and_row_shape_fail_the_seal() {
    let f = fixture(3).await;
    f.c.batch_execute("UPDATE erp_event.ir_audit_seal SET row_count=65537 WHERE batch_no=2")
        .await
        .unwrap();
    failed(
        verify_tx(&f.h, &VerifyScope::full(), &WitnessEvidence::NotConfigured).await,
        seal_at(1, 2),
        VerifyProblem::RowCountLimit,
        "a later release: stored row count exceeds the verifier limit (at seal epoch 1 batch 2)",
    );
    // A company_ids array whose lower bound is not 1 has no schema-1 spelling.
    let f = fixture(3).await;
    f.c.batch_execute(&format!(
        "UPDATE erp_event.ir_audit_entry SET company_ids='[2:3]={{1,2}}'::bigint[] WHERE id={}",
        f.rows[1].1
    ))
    .await
    .unwrap();
    failed(
        verify_tx(&f.h, &VerifyScope::full(), &WitnessEvidence::NotConfigured).await,
        seal_at(1, 2),
        VerifyProblem::RowValue,
        "a later release: a sealed row has an unsupported shape or type (at seal epoch 1 batch 2)",
    );
    let f = fixture(3).await;
    f.c.batch_execute(&format!(
        "UPDATE erp_event.ir_audit_entry SET events=jsonb_build_array(repeat('a',9000000)) WHERE id={}",
        f.rows[1].1
    ))
    .await
    .unwrap();
    failed(
        verify_tx(&f.h, &VerifyScope::full(), &WitnessEvidence::NotConfigured).await,
        seal_at(1, 2),
        VerifyProblem::RowPayloadLimit,
        "a later release: a sealed row exceeds the payload safety limit (at seal epoch 1 batch 2)",
    );
    // Three rows below the per-row cap whose sum passes the 16 MiB transfer cap.
    let f = multi_row_fixture().await;
    f.c.batch_execute(&format!(
        "UPDATE erp_event.ir_audit_entry SET events=jsonb_build_array(repeat('a',6000000)) WHERE id IN ({},{},{})",
        f.rows[0].1, f.rows[1].1, f.rows[2].1
    ))
    .await
    .unwrap();
    failed(
        verify_tx(&f.h, &VerifyScope::full(), &WitnessEvidence::NotConfigured).await,
        seal_at(1, 1),
        VerifyProblem::WireLimit,
        "a later release: sealed rows exceed the cumulative transfer limit (at seal epoch 1 batch 1)",
    );
}

#[tokio::test]
async fn seals_beyond_one_page_are_verified() {
    let h = initialized().await;
    let c = owner(&h).await;
    insert_many(&c, 515).await;
    let mut tx = begin(&h).await;
    let mut last = None;
    for _ in 0..515 {
        last = Some(staged(
            tx.seal_audit_batch(&SealOptions::new(1, 100).unwrap())
                .await
                .unwrap(),
        ));
    }
    tx.commit().await.unwrap();
    drop(tx);
    let last = last.unwrap();
    assert_eq!(last.identity().batch_no(), 515);
    let w = evidence(&[&last]);
    let r = verified(verify_tx(&h, &VerifyScope::full(), &w).await);
    assert_eq!((r.seals(), r.rows()), (515, 515));
    assert_eq!(
        r.assurance(),
        Assurance::AnchoredThrough {
            epoch: 1,
            batch_no: 515
        }
    );
    c.batch_execute("UPDATE erp_event.ir_audit_seal SET row_count=2 WHERE batch_no=514")
        .await
        .unwrap();
    failed(
        verify_tx(&h, &VerifyScope::full(), &WitnessEvidence::NotConfigured).await,
        seal_at(1, 514),
        VerifyProblem::RowCountMismatch,
        "a later release: stored row count differs from the rows in the sealed range (at seal epoch 1 batch 514)",
    );
}

#[tokio::test]
async fn serializable_isolation_is_accepted() {
    let f = fixture(1).await;
    let mut tx = begin(&f.h).await;
    tx.batch("SET TRANSACTION ISOLATION LEVEL SERIALIZABLE")
        .await
        .unwrap();
    let out = tx
        .verify_audit_chain(&VerifyScope::full(), &WitnessEvidence::NotConfigured)
        .await
        .unwrap();
    assert_eq!(verified(out).seals(), 1);
    tx.rollback().await.unwrap();
}
