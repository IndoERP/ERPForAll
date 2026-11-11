//! `_parent_store`: `parent_path` maintained on create and move, loops refused, hierarchy
//! operators through the paths (CRUD §4.11, DOM §4.8).
#![allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]

mod support;

use std::sync::Arc;

use erp_registry::{AddonDescriptor, FieldDecl, ModelDecl, RegistryBuilder};
use erp_sdk::{Domain, ErrorKind, IndexKind, Orm, Records, StaticPolicy, Transaction, Value};
use erp_testkit::{Harness, assert_query_count};
use support::{dom, map};

const MODEL: &str = "test_dep.category";

async fn create(tx: &mut Transaction, name: &str, parent: Option<&Records>) -> Records {
    let model = tx.env().records(MODEL).unwrap();
    let mut row = vec![("name", Value::str(name))];
    if let Some(p) = parent {
        row.push(("parent", Value::Int(p.ids()[0])));
    }
    model.create_values(tx, &[map(&row)]).await.unwrap()
}

async fn tree(tx: &mut Transaction) -> Vec<Records> {
    let c0 = create(tx, "0", None).await;
    let c1 = create(tx, "1", Some(&c0)).await;
    let c2 = create(tx, "2", Some(&c0)).await;
    let c3 = create(tx, "3", Some(&c0)).await;
    let c4 = create(tx, "4", Some(&c3)).await;
    let c5 = create(tx, "5", Some(&c3)).await;
    let c6 = create(tx, "6", Some(&c3)).await;
    let c7 = create(tx, "7", Some(&c6)).await;
    let c8 = create(tx, "8", Some(&c6)).await;
    let c9 = create(tx, "9", Some(&c6)).await;
    vec![c0, c1, c2, c3, c4, c5, c6, c7, c8, c9]
}

fn ids(cats: &[Records], indexes: &[usize]) -> Vec<i64> {
    let mut out: Vec<i64> = indexes.iter().map(|i| cats[*i].ids()[0]).collect();
    out.sort_unstable();
    out
}

async fn search(tx: &mut Transaction, domain: Domain) -> Vec<i64> {
    let model = tx.env().records(MODEL).unwrap();
    let mut found = model.search(tx, &domain).await.unwrap().ids();
    found.sort_unstable();
    found
}

async fn child_of(tx: &mut Transaction, cats: &[Records], i: usize) -> Vec<i64> {
    search(
        tx,
        dom(erp_sdk::serde_json::json!([[
            "id",
            "child_of",
            [cats[i].ids()[0]]
        ]])),
    )
    .await
}

async fn parent_of(tx: &mut Transaction, cats: &[Records], i: usize) -> Vec<i64> {
    search(
        tx,
        dom(erp_sdk::serde_json::json!([[
            "id",
            "parent_of",
            [cats[i].ids()[0]]
        ]])),
    )
    .await
}

async fn move_to(tx: &mut Transaction, cats: &[Records], moved: &[usize], parent: Option<usize>) {
    let ids: Vec<i64> = moved.iter().map(|i| cats[*i].ids()[0]).collect();
    let records = tx.env().records(MODEL).unwrap().browse(ids).unwrap();
    let value = parent.map_or(Value::False, |p| Value::Int(cats[p].ids()[0]));
    records
        .write_values(tx, &map(&[("parent", value)]))
        .await
        .unwrap();
}

#[tokio::test]
async fn spec_crud_055_parent_path_on_create() {
    let h = support::dep_orm().await;
    let mut tx = h.begin().await.unwrap();
    let cats = tree(&mut tx).await;
    create(&mut tx, "warm", Some(&cats[6])).await;
    tx.flush().await.unwrap();
    // the INSERT and one statement computing the path (… UPDATE … RETURNING)
    tx.capture_statements(true);
    let c10 = assert_query_count!(tx, >= 2, <= 2, { create(&mut tx, "10", Some(&cats[6])).await });
    assert!(
        tx.captured()[1].starts_with("WITH RECURSIVE \"__n\""),
        "{:?}",
        tx.captured()
    );
    tx.capture_statements(false);
    let expected = format!(
        "{}/{}/{}/{}/",
        cats[0].ids()[0],
        cats[3].ids()[0],
        cats[6].ids()[0],
        c10.ids()[0]
    );
    // cached from RETURNING: no query
    assert_query_count!(tx, <= 0, {
        assert_eq!(c10.value(&mut tx, "parent_path").await.unwrap(), Value::str(&expected));
    });
    assert_eq!(c10.value(&mut tx, "depth").await.unwrap(), Value::Int(3));
    tx.invalidate_all().await.unwrap();
    assert_eq!(
        c10.value(&mut tx, "parent_path").await.unwrap(),
        Value::str(&expected)
    );
    // the path is never taken from values
    let model = tx.env().records(MODEL).unwrap();
    let c11 = model
        .create_values(
            &mut tx,
            &[map(&[
                ("name", Value::str("11")),
                ("parent_path", Value::str("nope")),
            ])],
        )
        .await
        .unwrap();
    assert_eq!(
        c11.value(&mut tx, "parent_path").await.unwrap(),
        Value::str(format!("{}/", c11.ids()[0]))
    );
    // the initial tree
    for (i, children) in [
        (0, vec![0, 1, 2, 3, 4, 5, 6, 7, 8, 9]),
        (1, vec![1]),
        (3, vec![3, 4, 5, 6, 7, 8, 9]),
        (6, vec![6, 7, 8, 9]),
        (9, vec![9]),
    ] {
        let mut expected = ids(&cats, &children);
        if i == 0 || i == 3 || i == 6 {
            expected.extend(c10.ids());
            expected.extend(
                tx.env()
                    .records(MODEL)
                    .unwrap()
                    .search(
                        &mut tx,
                        &dom(erp_sdk::serde_json::json!([["name", "=", "warm"]])),
                    )
                    .await
                    .unwrap()
                    .ids(),
            );
            expected.sort_unstable();
        }
        assert_eq!(child_of(&mut tx, &cats, i).await, expected, "child_of {i}");
    }
    for (i, parents) in [
        (0, vec![0]),
        (1, vec![0, 1]),
        (4, vec![0, 3, 4]),
        (9, vec![0, 3, 6, 9]),
    ] {
        assert_eq!(
            parent_of(&mut tx, &cats, i).await,
            ids(&cats, &parents),
            "parent_of {i}"
        );
    }
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_crud_056_moving_subtrees() {
    let h = support::dep_orm().await;
    type Case = (
        &'static [usize],
        Option<usize>,
        &'static [(usize, &'static [usize])],
        &'static [(usize, &'static [usize])],
    );
    let cases: [Case; 6] = [
        // 6 to the root
        (
            &[6],
            None,
            &[
                (0, &[0, 1, 2, 3, 4, 5]),
                (3, &[3, 4, 5]),
                (6, &[6, 7, 8, 9]),
            ],
            &[(9, &[6, 9])],
        ),
        // 6 under 1
        (
            &[6],
            Some(1),
            &[
                (0, &[0, 1, 2, 3, 4, 5, 6, 7, 8, 9]),
                (1, &[1, 6, 7, 8, 9]),
                (3, &[3, 4, 5]),
                (6, &[6, 7, 8, 9]),
            ],
            &[(9, &[0, 1, 6, 9])],
        ),
        // 6 under 0
        (
            &[6],
            Some(0),
            &[
                (0, &[0, 1, 2, 3, 4, 5, 6, 7, 8, 9]),
                (3, &[3, 4, 5]),
                (6, &[6, 7, 8, 9]),
            ],
            &[(9, &[0, 6, 9])],
        ),
        // 5 and 6 to the root
        (
            &[5, 6],
            None,
            &[
                (0, &[0, 1, 2, 3, 4]),
                (3, &[3, 4]),
                (5, &[5]),
                (6, &[6, 7, 8, 9]),
            ],
            &[(5, &[5]), (9, &[6, 9])],
        ),
        // 5 and 6 under 1
        (
            &[5, 6],
            Some(1),
            &[
                (0, &[0, 1, 2, 3, 4, 5, 6, 7, 8, 9]),
                (1, &[1, 5, 6, 7, 8, 9]),
                (3, &[3, 4]),
                (5, &[5]),
                (6, &[6, 7, 8, 9]),
            ],
            &[(5, &[0, 1, 5]), (9, &[0, 1, 6, 9])],
        ),
        // 5 and 6 under 0
        (
            &[5, 6],
            Some(0),
            &[
                (0, &[0, 1, 2, 3, 4, 5, 6, 7, 8, 9]),
                (3, &[3, 4]),
                (5, &[5]),
                (6, &[6, 7, 8, 9]),
            ],
            &[(5, &[0, 5]), (9, &[0, 6, 9])],
        ),
    ];
    for (moved, parent, children, parents) in cases {
        let mut tx = h.begin().await.unwrap();
        let cats = tree(&mut tx).await;
        tx.flush().await.unwrap();
        // the descendants' paths: one UPDATE per new parent
        tx.capture_statements(true);
        move_to(&mut tx, &cats, moved, parent).await;
        let path_updates = tx
            .captured()
            .iter()
            .filter(|s| s.starts_with("UPDATE \"test_dep_category\" AS \"child\""))
            .count();
        assert_eq!(path_updates, 1, "{moved:?} -> {parent:?}");
        tx.capture_statements(false);
        for (i, expected) in children {
            assert_eq!(
                child_of(&mut tx, &cats, *i).await,
                ids(&cats, expected),
                "child_of {i} after {moved:?} -> {parent:?}"
            );
        }
        for (i, expected) in parents {
            assert_eq!(
                parent_of(&mut tx, &cats, *i).await,
                ids(&cats, expected),
                "parent_of {i} after {moved:?} -> {parent:?}"
            );
        }
        // the cache and the database agree
        let cached = cats[9].value(&mut tx, "parent_path").await.unwrap();
        tx.invalidate_all().await.unwrap();
        assert_eq!(cats[9].value(&mut tx, "parent_path").await.unwrap(), cached);
        tx.rollback().await.unwrap();
        drop(tx);
    }
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_crud_057_hierarchy_loops_are_refused() {
    let h = support::dep_orm().await;
    let mut tx = h.begin().await.unwrap();
    let cats = tree(&mut tx).await;
    tx.flush().await.unwrap();
    let before: Vec<Value> = {
        let all = cats[0].concat_all(cats[1..].iter()).unwrap();
        all.values(&mut tx, "parent_path").await.unwrap()
    };
    for (moved, parent) in [(vec![3], 9), (vec![1, 3], 9), (vec![6], 6)] {
        let ids: Vec<i64> = moved.iter().map(|i| cats[*i].ids()[0]).collect();
        let records = tx.env().records(MODEL).unwrap().browse(ids).unwrap();
        let err = tx
            .savepoint(async |tx| {
                records
                    .write_values(tx, &map(&[("parent", Value::Int(cats[parent].ids()[0]))]))
                    .await
            })
            .await
            .unwrap_err();
        assert_eq!(err.kind(), ErrorKind::UserError);
        assert_eq!(
            err.message(),
            "You are creating a loop in your 'Dep Category' records. A record cannot be a child \
             of itself or one of its own sub-items. Please select a different parent."
        );
    }
    assert!(!tx.is_poisoned(), "refused before any write");
    tx.invalidate_all().await.unwrap();
    let all = cats[0].concat_all(cats[1..].iter()).unwrap();
    assert_eq!(
        all.values(&mut tx, "parent_path").await.unwrap(),
        before,
        "no path changed"
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

/// Review item 6 (next to): a move cancelled while its path
/// statement runs poisons the transaction; only rollback recovers it, and nothing is written.
#[tokio::test]
async fn review_06_cancelled_move_poisons_the_transaction() {
    let h = support::dep_orm().await;
    let mut tx = h.begin().await.unwrap();
    let cats = tree(&mut tx).await;
    tx.commit().await.unwrap();
    let all = cats[0].concat_all(cats[1..].iter()).unwrap();
    let before = all.values(&mut tx, "parent_path").await.unwrap();
    // another transaction holds a descendant of the moved record: the path UPDATE waits for it
    let mut other = h.begin().await.unwrap();
    other
        .query(
            &format!(
                "SELECT id FROM test_dep_category WHERE id = {} FOR UPDATE",
                cats[7].ids()[0]
            ),
            &[],
        )
        .await
        .unwrap();
    let records = tx
        .env()
        .records(MODEL)
        .unwrap()
        .browse(cats[6].ids())
        .unwrap();
    let parent = map(&[("parent", Value::Int(cats[1].ids()[0]))]);
    let moved = tokio::time::timeout(
        std::time::Duration::from_millis(500),
        records.write_values(&mut tx, &parent),
    )
    .await;
    assert!(moved.is_err(), "the move waits for the other transaction");
    assert!(tx.is_poisoned(), "cancelled inside the path statements");
    assert_eq!(tx.flush().await.unwrap_err().kind(), ErrorKind::Programming);
    other.rollback().await.unwrap();
    tx.rollback().await.unwrap();
    assert!(!tx.is_poisoned());
    tx.invalidate_all().await.unwrap();
    assert_eq!(
        all.values(&mut tx, "parent_path").await.unwrap(),
        before,
        "nothing written"
    );
    drop(tx);
    drop(other);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_dom_033_hierarchy_with_parent_store() {
    let h = support::dep_orm().await;
    let mut tx = h.begin().await.unwrap();
    let cats = tree(&mut tx).await;
    let items = tx.env().records("test_dep.categorized").unwrap();
    let rows: Vec<_> = [1, 4, 7]
        .iter()
        .map(|i| {
            map(&[
                ("name", Value::str(i.to_string())),
                ("category_id", Value::Int(cats[*i].ids()[0])),
            ])
        })
        .collect();
    let created = items.create_values(&mut tx, &rows).await.unwrap();
    tx.flush().await.unwrap();
    // child_of on the parent field: a path prefix, no recursive query
    tx.capture_statements(true);
    assert_eq!(
        child_of(&mut tx, &cats, 3).await,
        ids(&cats, &[3, 4, 5, 6, 7, 8, 9])
    );
    let sql = tx.captured().last().cloned().unwrap();
    assert!(sql.contains("\"parent_path\""), "{sql}");
    assert!(!sql.contains("RECURSIVE"), "{sql}");
    tx.capture_statements(false);
    // parent_of: the ids of the paths
    assert_eq!(
        parent_of(&mut tx, &cats, 7).await,
        ids(&cats, &[0, 3, 6, 7])
    );
    // through a many2one of another model
    let mut found = items
        .search(
            &mut tx,
            &dom(erp_sdk::serde_json::json!([[
                "category_id",
                "child_of",
                cats[3].ids()[0]
            ]])),
        )
        .await
        .unwrap()
        .ids();
    found.sort_unstable();
    assert_eq!(found, created.ids()[1..].to_vec());
    let found = items
        .search(
            &mut tx,
            &dom(erp_sdk::serde_json::json!([[
                "category_id",
                "parent_of",
                cats[1].ids()[0]
            ]])),
        )
        .await
        .unwrap()
        .ids();
    assert_eq!(found, created.ids()[..1].to_vec());
    // in memory (filtered_domain) too
    let kept = created
        .filtered_domain(
            &mut tx,
            &dom(erp_sdk::serde_json::json!([[
                "category_id",
                "child_of",
                cats[6].ids()[0]
            ]])),
        )
        .await
        .unwrap();
    assert_eq!(kept.ids(), created.ids()[2..].to_vec());
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_crud_058_parent_store_initialization_and_checks() {
    static ADDON: AddonDescriptor = AddonDescriptor {
        name: "test_pstore",
        depends: &["base"],
        exact_only: false,
    };
    fn decl(parent_store: bool, path: Option<FieldDecl>) -> ModelDecl {
        let mut fields = vec![
            FieldDecl::char("name"),
            FieldDecl::many2one("parent_id", "x.pstore"),
        ];
        fields.extend(path);
        let fields: &'static [FieldDecl] = Box::leak(fields.into_boxed_slice());
        let decl = ModelDecl::new("test_pstore", "x.pstore").fields(fields);
        if parent_store {
            decl.parent_store(true)
        } else {
            decl
        }
    }
    // (b) and (c)
    let err = RegistryBuilder::new([&ADDON])
        .with_decls([&*Box::leak(Box::new(decl(true, None)))])
        .build()
        .unwrap_err();
    assert!(err.message().contains("add a field parent_path"), "{err}");
    let err = RegistryBuilder::new([&ADDON])
        .with_decls([&*Box::leak(Box::new(decl(
            true,
            Some(FieldDecl::char("parent_path")),
        )))])
        .build()
        .unwrap_err();
    assert!(err.message().contains("should be indexed"), "{err}");
    // a computed parent field: its value would change without a write the paths could follow
    let err = RegistryBuilder::new([&ADDON])
        .with_decls([&*Box::leak(Box::new({
            let fields: &'static [FieldDecl] = Box::leak(
                vec![
                    FieldDecl::char("name"),
                    FieldDecl::many2one("parent_id", "x.pstore")
                        .compute("_compute_parent")
                        .store(true),
                    FieldDecl::char("parent_path").index(Some(IndexKind::Btree)),
                ]
                .into_boxed_slice(),
            );
            ModelDecl::new("test_pstore", "x.pstore")
                .fields(fields)
                .parent_store(true)
        }))])
        .build()
        .unwrap_err();
    assert!(
        err.message().contains(
            "_parent_store on model 'x.pstore' needs a parent field that is not computed"
        ),
        "{err}"
    );
    // (a): rows first, then the parent store
    let h = Harness::builder([&ADDON])
        .with_decls([decl(false, None)])
        .build()
        .await
        .unwrap();
    let mut tx = h.begin().await.unwrap();
    let model = tx.env().records("x.pstore").unwrap();
    let root = model
        .create_values(&mut tx, &[map(&[("name", Value::str("r"))])])
        .await
        .unwrap();
    let child = model
        .create_values(
            &mut tx,
            &[map(&[
                ("name", Value::str("c")),
                ("parent_id", Value::Int(root.ids()[0])),
            ])],
        )
        .await
        .unwrap();
    let grandchild = model
        .create_values(
            &mut tx,
            &[map(&[
                ("name", Value::str("g")),
                ("parent_id", Value::Int(child.ids()[0])),
            ])],
        )
        .await
        .unwrap();
    tx.commit().await.unwrap();
    drop(tx);
    let registry = RegistryBuilder::new([&ADDON])
        .with_decls([&*Box::leak(Box::new(decl(
            true,
            Some(FieldDecl::char("parent_path").index(Some(IndexKind::Btree))),
        )))])
        .build()
        .unwrap();
    let orm = Orm::builder(Arc::new(registry), h.db().pool().clone())
        .policy(StaticPolicy::AllowAll)
        .build()
        .unwrap();
    let mut tx = orm.begin(erp_sdk::SUPERUSER_ID).await.unwrap();
    tx.init_models(&[]).await.unwrap();
    let model = tx.env().records("x.pstore").unwrap();
    let all = model
        .browse([root.ids()[0], child.ids()[0], grandchild.ids()[0]])
        .unwrap();
    let (r, c, g) = (root.ids()[0], child.ids()[0], grandchild.ids()[0]);
    assert_eq!(
        all.values(&mut tx, "parent_path").await.unwrap(),
        [
            Value::str(format!("{r}/")),
            Value::str(format!("{r}/{c}/")),
            Value::str(format!("{r}/{c}/{g}/")),
        ]
    );
    drop(tx);
    drop(orm);
    h.cleanup().await.unwrap();
}

// ----- review round of S4b ---------------------------------------------------------------------------

async fn paths(tx: &mut Transaction, records: &Records) -> Vec<Value> {
    records.values(tx, "parent_path").await.unwrap()
}

/// Review item 7: the paths are indexed in byte order and `child_of` ranges compare in byte
/// order, so a database whose collation ignores `/` (en_US) gives the same subtrees and uses the
/// index. Skipped when the server's `template1` uses the C collation.
#[tokio::test]
async fn review_07_path_ranges_in_byte_order_on_a_non_c_database() {
    let h = Harness::builder([
        &erp_test_models::base_users::ADDON,
        &erp_test_models::test_depends::ADDON,
    ])
    .template("template1")
    .build()
    .await
    .unwrap();
    let mut tx = h.begin().await.unwrap();
    let collation = support::rows(
        &mut tx,
        "SELECT datcollate::text FROM pg_database WHERE datname = current_database()",
    )
    .await;
    let collation = collation[0][0].clone().unwrap_or_default();
    if collation == "C" || collation == "POSIX" {
        eprintln!("review_07 skipped: template1 uses the {collation} collation");
        drop(tx);
        h.cleanup().await.unwrap();
        return;
    }
    let defs = support::rows(
        &mut tx,
        "SELECT indexdef FROM pg_indexes WHERE tablename = 'test_dep_category' \
         AND indexdef LIKE '%parent_path%'",
    )
    .await;
    assert!(
        defs.iter()
            .any(|r| r[0].as_deref().is_some_and(|d| d.contains("COLLATE \"C\""))),
        "{defs:?}"
    );
    // a root with 39 children (ids into the 30s), grandchildren under two of them whose ids
    // share a prefix (3 and 30 on a fresh database)
    let root = create(&mut tx, "r", None).await;
    let mut nodes: Vec<(i64, Option<i64>)> = vec![(root.ids()[0], None)];
    let mut children = Vec::new();
    for i in 0..39 {
        let c = create(&mut tx, &format!("c{i}"), Some(&root)).await;
        nodes.push((c.ids()[0], Some(root.ids()[0])));
        children.push(c);
    }
    for parent in [&children[1], &children[28]] {
        for g in 0..3 {
            let c = create(&mut tx, &format!("g{g}"), Some(parent)).await;
            nodes.push((c.ids()[0], Some(parent.ids()[0])));
        }
    }
    tx.flush().await.unwrap();
    let below = |start: i64| -> Vec<i64> {
        let mut out = vec![start];
        let mut i = 0;
        while i < out.len() {
            let current = out[i];
            out.extend(
                nodes
                    .iter()
                    .filter(|(_, p)| *p == Some(current))
                    .map(|(id, _)| *id),
            );
            i += 1;
        }
        out.sort_unstable();
        out
    };
    for (id, _) in &nodes {
        let found = search(
            &mut tx,
            dom(erp_sdk::serde_json::json!([["id", "child_of", [id]]])),
        )
        .await;
        assert_eq!(found, below(*id), "child_of {id}");
    }
    // the range uses the byte-order index
    tx.batch("SET LOCAL enable_seqscan = off").await.unwrap();
    let path = match paths(&mut tx, &children[1]).await.remove(0) {
        Value::Str(p) => p.to_string(),
        other => unreachable!("{other:?}"),
    };
    let sql = format!(
        "EXPLAIN {}",
        erp_sql::relational::subtree_by_paths("test_dep_category", "$1::text[]")
    );
    let plan: Vec<String> = tx
        .query(&sql, &[&vec![path]])
        .await
        .unwrap()
        .iter()
        .map(|r| r.try_get::<_, String>(0).unwrap())
        .collect();
    assert!(
        plan.iter().any(|l| l.contains("parent_path_c")),
        "{}",
        plan.join("\n")
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

/// Review item 8: two transactions moving X under Y and Y under X at the same time: the second
/// one waits for the store's lock, then fails cleanly; no cycle is stored.
#[tokio::test]
async fn review_08_concurrent_opposite_moves_cannot_make_a_cycle() {
    let h = support::dep_orm().await;
    let mut tx = h.begin().await.unwrap();
    let x = create(&mut tx, "x", None).await;
    let y = create(&mut tx, "y", None).await;
    tx.commit().await.unwrap();
    drop(tx);
    let mut a = h.begin().await.unwrap();
    let mut b = h.begin().await.unwrap();
    // both snapshots are taken before either move
    a.query("SELECT 1", &[]).await.unwrap();
    b.query("SELECT 1", &[]).await.unwrap();
    let xa = a.env().records(MODEL).unwrap().browse(x.ids()).unwrap();
    xa.write_values(&mut a, &map(&[("parent", Value::Int(y.ids()[0]))]))
        .await
        .unwrap();
    let yb = b.env().records(MODEL).unwrap().browse(y.ids()).unwrap();
    let under_x = map(&[("parent", Value::Int(x.ids()[0]))]);
    let (committed, moved) = tokio::join!(
        async {
            tokio::time::sleep(std::time::Duration::from_millis(300)).await;
            a.commit().await
        },
        yb.write_values(&mut b, &under_x),
    );
    committed.unwrap();
    let err = moved.unwrap_err();
    assert!(
        matches!(err.kind(), ErrorKind::Concurrency | ErrorKind::UserError),
        "{err}"
    );
    b.rollback().await.unwrap();
    drop(a);
    drop(b);
    let mut c = h.begin().await.unwrap();
    let both = c
        .env()
        .records(MODEL)
        .unwrap()
        .browse([x.ids()[0], y.ids()[0]])
        .unwrap();
    let (xi, yi) = (x.ids()[0], y.ids()[0]);
    assert_eq!(
        paths(&mut c, &both).await,
        [
            Value::str(format!("{yi}/{xi}/")),
            Value::str(format!("{yi}/"))
        ]
    );
    drop(c);
    h.cleanup().await.unwrap();
}

async fn insert_raw(tx: &mut Transaction, name: &str, parent: Option<i64>) -> i64 {
    let sql = format!(
        "INSERT INTO test_dep_category (name, parent) VALUES ('{name}', {}) RETURNING id",
        parent.map_or_else(|| "NULL".to_owned(), |p| p.to_string())
    );
    tx.query(&sql, &[]).await.unwrap()[0].try_get(0).unwrap()
}

/// Review item 9: the paths of a created batch are computed parents first, whatever the order
/// of the rows, when a record's parent is created in the same batch; rows forming a cycle among
/// themselves get no path (the ORM reports them).
#[tokio::test]
async fn review_09_paths_of_a_batch_with_parents_in_the_batch() {
    let h = support::dep_orm().await;
    let mut tx = h.begin().await.unwrap();
    let root = create(&mut tx, "r", None).await;
    tx.flush().await.unwrap();
    let r = root.ids()[0];
    let a = insert_raw(&mut tx, "a", Some(r)).await;
    let b = insert_raw(&mut tx, "b", Some(a)).await;
    let c = insert_raw(&mut tx, "c", Some(b)).await;
    let d = insert_raw(&mut tx, "d", None).await;
    let e = insert_raw(&mut tx, "e", Some(d)).await;
    tx.query(
        &format!("UPDATE test_dep_category SET parent = {e} WHERE id = {d}"),
        &[],
    )
    .await
    .unwrap();
    let sql = erp_sql::relational::parent_paths_of_new("test_dep_category", "parent");
    // children before their parents
    let batch: Vec<i64> = vec![c, e, b, d, a];
    let mut got: Vec<(i64, Option<String>)> = tx
        .query(&sql, &[&batch])
        .await
        .unwrap()
        .iter()
        .map(|row| (row.try_get(0).unwrap(), row.try_get(1).unwrap()))
        .collect();
    got.sort_unstable();
    assert_eq!(
        got,
        [
            (a, Some(format!("{r}/{a}/"))),
            (b, Some(format!("{r}/{a}/{b}/"))),
            (c, Some(format!("{r}/{a}/{b}/{c}/"))),
        ]
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

/// Review item 10: a record without a path (a cycle, rows written outside the ORM) is an error
/// for creates and moves under it and for moving it, never a silent root; building a parent
/// store over data with a cycle fails instead of leaving rows without a path.
#[tokio::test]
async fn review_10_a_missing_parent_path_is_an_error() {
    let h = support::dep_orm().await;
    let mut tx = h.begin().await.unwrap();
    let cats = tree(&mut tx).await;
    tx.flush().await.unwrap();
    tx.query(
        &format!(
            "UPDATE test_dep_category SET parent_path = NULL WHERE id = {}",
            cats[3].ids()[0]
        ),
        &[],
    )
    .await
    .unwrap();
    tx.invalidate_all().await.unwrap();
    let model = tx.env().records(MODEL).unwrap();
    let under = map(&[
        ("name", Value::str("z")),
        ("parent", Value::Int(cats[3].ids()[0])),
    ]);
    let err = tx
        .savepoint(async |tx| {
            model
                .create_values(tx, std::slice::from_ref(&under))
                .await
                .map(|_| ())
        })
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Programming, "{err}");
    assert!(err.message().contains("parent_path is empty"), "{err}");
    for (moved, parent) in [(1, 3), (3, 1)] {
        let records = model.browse(cats[moved].ids()).unwrap();
        let value = map(&[("parent", Value::Int(cats[parent].ids()[0]))]);
        let err = tx
            .savepoint(async |tx| records.write_values(tx, &value).await)
            .await
            .unwrap_err();
        assert_eq!(err.kind(), ErrorKind::Programming, "{err}");
        assert!(err.message().contains("parent_path is empty"), "{err}");
    }
    drop(tx);
    h.cleanup().await.unwrap();

    // a cycle in existing data when the parent store is built
    static ADDON: AddonDescriptor = AddonDescriptor {
        name: "test_pcycle",
        depends: &["base"],
        exact_only: false,
    };
    fn decl(path: Option<FieldDecl>) -> ModelDecl {
        let mut fields = vec![
            FieldDecl::char("name"),
            FieldDecl::many2one("parent_id", "x.pcycle"),
        ];
        let store = path.is_some();
        fields.extend(path);
        let fields: &'static [FieldDecl] = Box::leak(fields.into_boxed_slice());
        ModelDecl::new("test_pcycle", "x.pcycle")
            .fields(fields)
            .parent_store(store)
    }
    let h = Harness::builder([&ADDON])
        .with_decls([decl(None)])
        .build()
        .await
        .unwrap();
    let mut tx = h.begin().await.unwrap();
    tx.batch(
        "INSERT INTO x_pcycle (id, name) VALUES (1, 'a'), (2, 'b'), (3, 'root'); \
         UPDATE x_pcycle SET parent_id = 2 WHERE id = 1; \
         UPDATE x_pcycle SET parent_id = 1 WHERE id = 2",
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    drop(tx);
    let registry = RegistryBuilder::new([&ADDON])
        .with_decls([&*Box::leak(Box::new(decl(Some(
            FieldDecl::char("parent_path").index(Some(IndexKind::Btree)),
        ))))])
        .build()
        .unwrap();
    let orm = Orm::builder(Arc::new(registry), h.db().pool().clone())
        .policy(StaticPolicy::AllowAll)
        .build()
        .unwrap();
    let mut tx = orm.begin(erp_sdk::SUPERUSER_ID).await.unwrap();
    let err = tx.init_models(&[]).await.unwrap_err();
    assert!(
        err.message().contains("no root reaches the rows 1, 2"),
        "{err}"
    );
    drop(tx);
    drop(orm);
    h.cleanup().await.unwrap();
}

/// Review item 11: the constraints of `create` see the final paths (`test_dep.category` checks
/// that its path ends with its id), for a batch of records with parents.
#[tokio::test]
async fn review_11_create_constraints_see_the_final_path() {
    let h = support::dep_orm().await;
    let mut tx = h.begin().await.unwrap();
    let root = create(&mut tx, "r", None).await;
    let model = tx.env().records(MODEL).unwrap();
    let rows: Vec<_> = (0..3)
        .map(|i| {
            map(&[
                ("name", Value::str(format!("c{i}"))),
                ("parent", Value::Int(root.ids()[0])),
            ])
        })
        .collect();
    let created = model.create_values(&mut tx, &rows).await.unwrap();
    let r = root.ids()[0];
    assert_eq!(
        paths(&mut tx, &created).await,
        created
            .ids()
            .iter()
            .map(|c| Value::str(format!("{r}/{c}/")))
            .collect::<Vec<_>>()
    );
    drop(tx);
    h.cleanup().await.unwrap();
}
