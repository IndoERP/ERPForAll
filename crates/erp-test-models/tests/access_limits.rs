//! The access engine's bounds: the per-transaction record memo holds at most
//! 100 000 ids (`MEMO_MAX_IDS`; beyond that a check simply runs again), and a chain of `access`
//! conditions through distinct models stops at 32 hops (`MAX_ACCESS_DEPTH`), so a registry deep
//! enough never recurses without bound even where no cycle forms.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

use std::num::NonZeroU64;

use erp_orm::{
    BeginOptions, GroupSet, IntoPolicyProvider, Operation, PrincipalBuilder, Resolution, UserId,
};
use erp_registry::{AddonDescriptor, FieldDecl, ModelDecl};
use erp_sdk::{Domain, ErrorKind, Operator, Records, Transaction};
use erp_test_models::{base_users, test_access};
use erp_testkit::{DomainPolicy, Harness, Otherwise, assert_query_count};

const OBJ: &str = "test_access.some_obj";
const DEMO: i64 = 2;

async fn begin_demo(h: &Harness) -> Transaction {
    let principal = PrincipalBuilder::user(UserId::new(DEMO).unwrap())
        .resolution(Resolution::in_id_order(GroupSet::default(), [], NonZeroU64::MIN).unwrap())
        .build()
        .unwrap();
    h.orm()
        .begin_opts(BeginOptions::for_principal(principal))
        .await
        .unwrap()
}

fn cond(field: &str, op: Operator, value: impl Into<erp_sdk::domain::Value>) -> Domain {
    Domain::condition(field, op, value).unwrap()
}

fn objs(tx: &Transaction, ids: impl IntoIterator<Item = i64>) -> Records {
    tx.env().records(OBJ).unwrap().browse(ids).unwrap()
}

/// The memo of record checks keeps at most 100 000 ids per transaction: once full, a further
/// record is checked again every time, while the ids it holds stay free.
#[tokio::test]
async fn the_record_memo_holds_at_most_100k_ids() {
    let policy = DomainPolicy::builder(Otherwise::Allow)
        .rule(OBJ, Operation::Read, cond("val", Operator::Gt, 0))
        .build()
        .unwrap();
    let h = Harness::builder([&base_users::ADDON, &test_access::ADDON])
        .policy(policy)
        .build()
        .await
        .expect("harness");
    let mut su = h.begin().await.unwrap();
    su.batch(
        "INSERT INTO test_access_some_obj (name, val) \
         SELECT 'o' || g, 1 FROM generate_series(1, 100001) AS g",
    )
    .await
    .unwrap();
    su.commit().await.unwrap();

    let mut tx = begin_demo(&h).await;
    objs(&tx, 1..=100_000)
        .check_access(&mut tx, Operation::Read)
        .await
        .unwrap();
    // remembered: free
    assert_query_count!(tx, <= 0, {
        objs(&tx, [1, 50_000, 100_000])
            .check_access(&mut tx, Operation::Read)
            .await
            .unwrap();
    });
    // the memo is full: the 100 001st record is checked each time
    for _ in 0..2 {
        assert_query_count!(tx, >= 1, <= 1, {
            objs(&tx, [100_001])
                .check_access(&mut tx, Operation::Read)
                .await
                .unwrap();
        });
    }
    drop(tx);
    h.cleanup().await.unwrap();
}

static DEEP: AddonDescriptor = AddonDescriptor {
    name: "x_deep",
    depends: &["base"],
    exact_only: false,
};

/// `x.m00` … a chain of `n` models, each with a many2one `next_id` to the next one.
fn chain(n: usize) -> (Vec<&'static str>, Vec<ModelDecl>) {
    let names: Vec<&'static str> = (0..n)
        .map(|i| &*Box::leak(format!("x.m{i:02}").into_boxed_str()))
        .collect();
    let decls = (0..n)
        .map(|i| {
            let fields: &'static [FieldDecl] = match names.get(i + 1).copied() {
                Some(next) => Box::leak(Box::new([FieldDecl::many2one("next_id", next)])),
                None => &[],
            };
            ModelDecl::new("x_deep", names[i]).fields(fields)
        })
        .collect();
    (names, decls)
}

/// Every model of the chain but the last reads through `next_id` (`('next_id', 'access',
/// 'read')`).
fn chain_policy(names: &[&str]) -> impl IntoPolicyProvider {
    let mut builder = DomainPolicy::builder(Otherwise::Allow);
    for name in &names[..names.len() - 1] {
        builder = builder.rule(
            name,
            Operation::Read,
            cond("next_id", Operator::Access, "read"),
        );
    }
    builder.build().unwrap()
}

/// 32 hops of `access` conditions through distinct models resolve; the 33rd is refused with the
/// chain, before any cycle could form.
#[tokio::test]
async fn an_access_chain_stops_at_32_hops() {
    // 33 models: m00 reads through m01 … m32, 32 hops
    let (names, decls) = chain(33);
    let h = Harness::builder([&base_users::ADDON, &DEEP])
        .with_decls(decls)
        .policy(chain_policy(&names))
        .build()
        .await
        .expect("harness");
    let mut tx = begin_demo(&h).await;
    let found = tx
        .env()
        .records(names[0])
        .unwrap()
        .search(&mut tx, &Domain::TRUE)
        .await
        .unwrap();
    assert!(found.is_empty());
    drop(tx);
    h.cleanup().await.unwrap();

    // 34 models: the 33rd hop is refused
    let (names, decls) = chain(34);
    let h = Harness::builder([&base_users::ADDON, &DEEP])
        .with_decls(decls)
        .policy(chain_policy(&names))
        .build()
        .await
        .expect("harness");
    let mut tx = begin_demo(&h).await;
    let err = tx
        .env()
        .records(names[0])
        .unwrap()
        .search(&mut tx, &Domain::TRUE)
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Programming, "{err}");
    let hops: Vec<String> = names[1..].iter().map(|n| format!("{n} read")).collect();
    assert_eq!(
        err.message(),
        format!(
            "The 'access' operator chain is longer than 32 hops: {}",
            hops.join(" → ")
        )
    );
    drop(tx);
    h.cleanup().await.unwrap();
}
