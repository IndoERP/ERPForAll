// SPDX-License-Identifier: MIT OR Apache-2.0
//! Native company-tree queries; real ORM companies and validated principals.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

use erp_orm::{BeginOptions, CompanyId, GroupSet, Principal, PrincipalBuilder, Resolution, UserId};
use erp_sdk::{Cell, Domain, ErrorKind, Operation, Operator, RawVals, Records, Transaction, Value};
use erp_testkit::{DomainPolicy, Harness, Otherwise};
use std::num::NonZeroU64;

async fn harness() -> Harness {
    Harness::builder([&erp_addon_base::ADDON])
        .build()
        .await
        .unwrap()
}
async fn company(tx: &mut Transaction, name: &str, parent: Option<i64>) -> i64 {
    let currency = match parent {
        Some(id) => tx
            .env()
            .records("res.company")
            .unwrap()
            .browse([id])
            .unwrap()
            .cell(tx, "currency_id")
            .await
            .unwrap(),
        None => {
            let rows = tx
                .query("SELECT id::int8 FROM res_currency ORDER BY id LIMIT 1", &[])
                .await
                .unwrap();
            Cell::Id(match rows.first() {
                Some(row) => row.get(0),
                None => erp_testkit::fixtures::seed_currency_in(tx).await.unwrap(),
            })
        }
    };
    let mut vals = RawVals::new()
        .set_cell("name", Cell::Str(name.into()))
        .set_cell("currency_id", currency);
    if let Some(id) = parent {
        vals = vals.set_cell("parent_id", Cell::Id(id));
    }
    tx.env()
        .records("res.company")
        .unwrap()
        .create(tx, vec![vals])
        .await
        .unwrap()
        .ids()[0]
}
fn principal(all: &[i64], selected: &[i64]) -> Principal {
    PrincipalBuilder::user(UserId::new(2).unwrap())
        .resolution(
            Resolution::in_id_order(
                GroupSet::default(),
                all.iter().map(|id| CompanyId::new(*id).unwrap()),
                NonZeroU64::MIN,
            )
            .unwrap(),
        )
        .companies(
            selected
                .iter()
                .map(|id| CompanyId::new(*id).unwrap())
                .collect(),
        )
        .build()
        .unwrap()
}
async fn branches(tx: &mut Transaction, company: &Records) -> Vec<i64> {
    let records: Records = company.call(tx, "_accessible_branches", ()).await.unwrap();
    records.ids()
}
async fn complete(tx: &mut Transaction, company: &Records) -> bool {
    company
        .call(tx, "_all_branches_selected", ())
        .await
        .unwrap()
}

#[tokio::test]
async fn roots_resolve_for_saved_and_nested_companies() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let root = company(&mut tx, "Tree root", None).await;
    let branch = company(&mut tx, "Tree branch", Some(root)).await;
    let leaf = company(&mut tx, "Tree leaf", Some(branch)).await;
    let other = company(&mut tx, "Other root", None).await;
    let companies = tx.env().records("res.company").unwrap();
    for (id, expected) in [(root, root), (branch, root), (leaf, root), (other, other)] {
        assert_eq!(
            companies
                .browse([id])
                .unwrap()
                .cell(&mut tx, "root_id")
                .await
                .unwrap(),
            Cell::Id(expected)
        );
    }
    assert_eq!(
        companies
            .browse([leaf])
            .unwrap()
            .cell(&mut tx, "parent_path")
            .await
            .unwrap(),
        Cell::Str(format!("{root}/{branch}/{leaf}/").into())
    );
    assert_eq!(
        companies
            .browse([root])
            .unwrap()
            .related(&mut tx, "child_ids")
            .await
            .unwrap()
            .ids(),
        [branch]
    );
    let indexes = tx.query(
        "SELECT EXISTS (SELECT 1 FROM pg_index i JOIN pg_class t ON t.oid=i.indrelid JOIN pg_class x ON x.oid=i.indexrelid JOIN pg_am m ON m.oid=x.relam JOIN pg_attribute a ON a.attrelid=t.oid AND a.attnum=ANY(i.indkey) WHERE t.relname='res_company' AND a.attname='parent_path' AND m.amname='btree')",
        &[],
    ).await.unwrap();
    assert!(indexes[0].get::<_, bool>(0));
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn selected_descendants_are_returned_across_unselected_parents() {
    let h = harness().await;
    let mut setup = h.begin().await.unwrap();
    let root = company(&mut setup, "Selection root", None).await;
    let branch = company(&mut setup, "Unselected branch", Some(root)).await;
    let leaf = company(&mut setup, "Selected leaf", Some(branch)).await;
    let other = company(&mut setup, "Selected other", None).await;
    setup.commit().await.unwrap();
    drop(setup);
    let mut tx = h
        .orm()
        .begin_opts(BeginOptions::for_principal(principal(
            &[root, branch, leaf, other],
            &[root, leaf, other],
        )))
        .await
        .unwrap();
    let source = tx
        .env()
        .records("res.company")
        .unwrap()
        .browse([root])
        .unwrap();
    assert_eq!(branches(&mut tx, &source).await, [root, leaf]);
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn only_core_system_gets_the_empty_intersection_fallback() {
    let h = harness().await;
    let mut setup = h.begin().await.unwrap();
    let root = company(&mut setup, "Fallback root", None).await;
    let other = company(&mut setup, "Fallback other", None).await;
    let source = setup
        .env()
        .records("res.company")
        .unwrap()
        .browse([root])
        .unwrap();
    assert_eq!(branches(&mut setup, &source).await, [root]);
    setup.commit().await.unwrap();
    drop(setup);
    let mut tx = h
        .orm()
        .begin_opts(BeginOptions::for_principal(principal(
            &[root, other],
            &[other],
        )))
        .await
        .unwrap();
    let source = tx
        .env()
        .records("res.company")
        .unwrap()
        .browse([root])
        .unwrap();
    assert!(branches(&mut tx, &source).await.is_empty());
    assert!(branches(&mut tx, &source.sudo(true)).await.is_empty());
    let switched = source.with_env(source.env().with_user(erp_sdk::SUPERUSER_ID.get()));
    let error = switched
        .call::<_, Records>(&mut tx, "_accessible_branches", ())
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Access);
    assert_eq!(
        error.message(),
        "only the system principal may act as the superuser (user 2 called with_user(1))"
    );
    // A System-origin impersonation cannot regain System authority through the user.
    let system = h.orm().env(erp_sdk::SUPERUSER_ID, erp_sdk::Context::new());
    let impersonating = system.with_user(2);
    assert!(matches!(impersonating.principal().actor(),
        Some(erp_sdk::Actor::Impersonator(original)) if original.kind() == erp_sdk::PrincipalKind::System));
    let resumed = source.with_env(impersonating.with_user(erp_sdk::SUPERUSER_ID.get()));
    let error = resumed
        .call::<_, Records>(&mut tx, "_accessible_branches", ())
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Access);
    assert_eq!(
        error.message(),
        "only the system principal may act as the superuser (user 2 called with_user(1))"
    );
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn draft_roots_use_the_live_parent_chain_without_saving_it() {
    use erp_addon_base::ResCompany;
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let saved = company(&mut tx, "Saved draft ancestor", None).await;
    let model = tx.env().model::<ResCompany>().unwrap();
    let draft_root = model
        .new_record(&mut tx, model.vals().set(ResCompany::NAME, "Draft root"))
        .await
        .unwrap();
    let draft = model
        .new_record(
            &mut tx,
            model
                .vals()
                .set(ResCompany::NAME, "Draft branch")
                .set(ResCompany::PARENT_ID, Some(draft_root.rec_ids()[0])),
        )
        .await
        .unwrap();
    assert_eq!(
        draft_root.get(&mut tx, ResCompany::ROOT_ID).await.unwrap(),
        Some(draft_root.rec_ids()[0])
    );
    assert_eq!(
        draft.get(&mut tx, ResCompany::ROOT_ID).await.unwrap(),
        Some(draft_root.rec_ids()[0])
    );
    let branch = model
        .new_record(
            &mut tx,
            model
                .vals()
                .set(ResCompany::NAME, "Draft saved-parent branch")
                .set(ResCompany::PARENT_ID, erp_sdk::RecId::from_real(saved)),
        )
        .await
        .unwrap();
    assert_eq!(
        branch.get(&mut tx, ResCompany::ROOT_ID).await.unwrap(),
        erp_sdk::RecId::from_real(saved)
    );
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn archived_intermediate_branch_stops_traversal_and_rollback_restores_it() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let root = company(&mut tx, "Archive selection root", None).await;
    let branch = company(&mut tx, "Archive selection branch", Some(root)).await;
    let leaf = company(&mut tx, "Archive selection leaf", Some(branch)).await;
    let source = tx
        .env()
        .records("res.company")
        .unwrap()
        .browse([root])
        .unwrap()
        .with_context([(
            "allowed_company_ids",
            Value::list([Value::Int(root), Value::Int(branch), Value::Int(leaf)]),
        )]);
    assert_eq!(branches(&mut tx, &source).await, [root, branch, leaf]);
    let result: erp_sdk::Result<()> = tx
        .savepoint(async |tx| {
            let companies = source.env().records("res.company")?;
            companies
                .browse([branch])?
                .write(tx, RawVals::new().set_cell("active", Cell::Bool(false)))
                .await?;
            // Reactivating a leaf does not reactivate its parent. Traversal must still prune it.
            companies
                .browse([leaf])?
                .write(tx, RawVals::new().set_cell("active", Cell::Bool(true)))
                .await?;
            assert_eq!(branches(tx, &source).await, [root]);
            Err(erp_sdk::Error::validation(
                "Rollback the branch-selection probe.",
            ))
        })
        .await;
    let error = result.unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Validation);
    assert_eq!(error.message(), "Rollback the branch-selection probe.");
    assert_eq!(branches(&mut tx, &source).await, [root, branch, leaf]);
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn nonstored_root_search_group_and_order_are_named_refusals() {
    use erp_sdk::{ReadGroup, SearchOptions};
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let root = company(&mut tx, "Query root", None).await;
    let companies = tx.env().records("res.company").unwrap();
    let domain = Domain::condition("root_id", Operator::Eq, root).unwrap();
    let error = companies.search(&mut tx, &domain).await.unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Programming);
    assert_eq!(
        error.message(),
        format!(
            "Field has no SQL representation because it is not stored in condition ('root_id', 'in', OrderedSet([{root}]))"
        )
    );
    let error = companies
        .search_with(
            &mut tx,
            &Domain::TRUE,
            &SearchOptions::new().order("root_id"),
        )
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Programming);
    assert_eq!(
        error.message(),
        "Ordering needs a stored relation: res.company.root_id."
    );
    let error = companies
        .read_group(
            &mut tx,
            ReadGroup::new(Domain::TRUE)
                .groupby(["root_id"])
                .aggregates(["__count"]),
        )
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Programming);
    assert_eq!(
        error.message(),
        "Cannot convert res.company.root_id to SQL because it is not stored"
    );
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn whole_tree_selection_requires_every_active_member_and_no_archived_input() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let root = company(&mut tx, "Complete root", None).await;
    let first = company(&mut tx, "First member", Some(root)).await;
    let second = company(&mut tx, "Second member", Some(root)).await;
    let all = tx.env().records("res.company").unwrap();
    assert!(complete(&mut tx, &all.browse([]).unwrap()).await);
    assert!(!complete(&mut tx, &all.browse([root, first]).unwrap()).await);
    assert!(complete(&mut tx, &all.browse([second, root, first, first]).unwrap()).await);
    all.browse([second])
        .unwrap()
        .write(
            &mut tx,
            RawVals::new().set_cell("active", Cell::Bool(false)),
        )
        .await
        .unwrap();
    assert!(complete(&mut tx, &all.browse([root, first]).unwrap()).await);
    assert!(!complete(&mut tx, &all.browse([root, first, second]).unwrap()).await);
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn a_read_hidden_sibling_cannot_make_selection_complete() {
    let policy = DomainPolicy::builder(Otherwise::Allow)
        .rule(
            "res.company",
            Operation::Read,
            Domain::condition("name", Operator::Ne, "Hidden sibling").unwrap(),
        )
        .build()
        .unwrap();
    let h = Harness::builder([&erp_addon_base::ADDON])
        .policy(policy)
        .build()
        .await
        .unwrap();
    let mut setup = h.begin().await.unwrap();
    let root = company(&mut setup, "Visible root", None).await;
    let visible = company(&mut setup, "Visible branch", Some(root)).await;
    let hidden = company(&mut setup, "Hidden sibling", Some(root)).await;
    setup.commit().await.unwrap();
    drop(setup);
    let mut tx = h
        .orm()
        .begin_opts(BeginOptions::for_principal(principal(
            &[root, visible, hidden],
            &[root, visible],
        )))
        .await
        .unwrap();
    let source = tx
        .env()
        .records("res.company")
        .unwrap()
        .browse([root, visible])
        .unwrap();
    assert!(!complete(&mut tx, &source).await);
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn inconsistent_hierarchy_is_reported_instead_of_a_fabricated_root() {
    let h = harness().await;
    let mut setup = h.begin().await.unwrap();
    let root = company(&mut setup, "Valid parent", None).await;
    let child = company(&mut setup, "Broken path", Some(root)).await;
    setup
        .execute(
            "UPDATE res_company SET parent_path=$2 WHERE id=$1",
            &[&child, &format!("{child}/")],
        )
        .await
        .unwrap();
    setup.commit().await.unwrap();
    drop(setup);
    let mut tx = h.begin().await.unwrap();
    let error = tx
        .env()
        .records("res.company")
        .unwrap()
        .browse([child])
        .unwrap()
        .cell(&mut tx, "root_id")
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Validation);
    assert_eq!(
        error.message(),
        "The company tree is inconsistent; repair its parent links and paths before continuing."
    );
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn selection_context_cannot_name_an_unassigned_company_even_under_elevation() {
    let h = harness().await;
    let mut setup = h.begin().await.unwrap();
    let root = company(&mut setup, "Allowed company", None).await;
    let other = company(&mut setup, "Unassigned company", None).await;
    setup.commit().await.unwrap();
    drop(setup);
    let mut tx = h
        .orm()
        .begin_opts(BeginOptions::for_principal(principal(&[root], &[root])))
        .await
        .unwrap();
    let source = tx
        .env()
        .records("res.company")
        .unwrap()
        .browse([root])
        .unwrap()
        .sudo(true)
        .with_context([("allowed_company_ids", Value::list([Value::Int(other)]))]);
    let error = source
        .call::<_, Records>(&mut tx, "_accessible_branches", ())
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Access);
    assert_eq!(
        error.message(),
        "One of the selected companies does not exist or is not one of yours."
    );
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn helper_rereads_membership_after_a_native_change_in_the_same_transaction() {
    use erp_sdk::{Command, RecId};
    let addons = [&erp_addon_base::ADDON, &erp_data::model_data::ADDON];
    let registry = Harness::builder(addons)
        .with_decls([erp_data::ir_model_data_decl()])
        .registry_only()
        .unwrap();
    let resolver = erp_security::resolver::Resolver::for_registry(&registry).unwrap();
    let h = Harness::builder(addons)
        .with_decls([erp_data::ir_model_data_decl()])
        .resolver(resolver)
        .build()
        .await
        .unwrap();
    let mut tx = h.begin().await.unwrap();
    let root = company(&mut tx, "Membership root", None).await;
    let other = company(&mut tx, "Membership home", None).await;
    tx.execute("INSERT INTO res_users (id,name,login,active,share,company_id) VALUES (2,'Tree reader','tree-reader',true,false,$1)", &[&other]).await.unwrap();
    tx.execute(
        "INSERT INTO res_company_users_rel (user_id,cid) VALUES (2,$1),(2,$2)",
        &[&root, &other],
    )
    .await
    .unwrap();
    let reader = tx
        .env()
        .with_user(2)
        .with_context([("allowed_company_ids", Value::list([Value::Int(root)]))]);
    let source = reader
        .records("res.company")
        .unwrap()
        .browse([root])
        .unwrap();
    assert_eq!(branches(&mut tx, &source).await, [root]);
    tx.env()
        .records("res.users")
        .unwrap()
        .browse([2])
        .unwrap()
        .write(
            &mut tx,
            RawVals::new().set_commands(
                "company_ids",
                vec![Command::set(vec![RecId::from_real(other).unwrap()])],
            ),
        )
        .await
        .unwrap();
    let error = source
        .call::<_, Records>(&mut tx, "_accessible_branches", ())
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Access);
    assert_eq!(
        error.message(),
        "One of the selected companies does not exist or is not one of yours."
    );
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}
