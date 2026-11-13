//! Company branch lifecycle; dynamic ORM calls exercise the native base method chain.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "test assertions"
)]
use erp_sdk::{Cell, Command, ErrorKind, RawVals, Records, Transaction, Value};
use erp_testkit::{FieldPolicy, Harness};

mod users_support;

async fn harness() -> Harness {
    Harness::builder([&erp_addon_base::ADDON])
        .build()
        .await
        .unwrap()
}
async fn currency(tx: &mut Transaction, name: &str) -> i64 {
    tx.env()
        .records("res.currency")
        .unwrap()
        .create(
            tx,
            vec![
                RawVals::new()
                    .set_cell("name", Cell::Str(name.into()))
                    .set_cell("symbol", Cell::Str(name.into())),
            ],
        )
        .await
        .unwrap()
        .ids()[0]
}
async fn company(
    tx: &mut Transaction,
    name: &str,
    currency: Option<i64>,
    parent: Option<i64>,
) -> Records {
    let mut values = RawVals::new().set_cell("name", Cell::Str(name.into()));
    if let Some(id) = currency {
        values = values.set_cell("currency_id", Cell::Id(id));
    }
    if let Some(id) = parent {
        values = values.set_cell("parent_id", Cell::Id(id));
    }
    tx.env()
        .records("res.company")
        .unwrap()
        .create(tx, vec![values])
        .await
        .unwrap()
}

#[tokio::test]
async fn branches_are_direct_children_and_all_children_include_archived() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let usd = currency(&mut tx, "CB1").await;
    let root = company(&mut tx, "Root", Some(usd), None).await;
    let first = company(&mut tx, "First", Some(usd), Some(root.ids()[0])).await;
    let archived = company(&mut tx, "Archived", Some(usd), Some(root.ids()[0])).await;
    let grandchild = company(&mut tx, "Grandchild", Some(usd), Some(first.ids()[0])).await;
    archived
        .write(
            &mut tx,
            RawVals::new().set_cell("active", Cell::Bool(false)),
        )
        .await
        .unwrap();
    assert_eq!(
        root.related(&mut tx, "child_ids").await.unwrap().ids(),
        first.ids()
    );
    let mut all = root.related(&mut tx, "all_child_ids").await.unwrap().ids();
    all.sort_unstable();
    let mut expected = vec![first.ids()[0], archived.ids()[0]];
    expected.sort_unstable();
    assert_eq!(all, expected);
    assert!(!all.contains(&grandchild.ids()[0]));
    assert_eq!(
        root.cell(&mut tx, "active").await.unwrap(),
        Cell::Bool(true)
    );
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn branch_currency_inherits_with_explicit_or_context_parent_without_partner_parent_leak() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let usd = currency(&mut tx, "CB2").await;
    let root = company(&mut tx, "Root", Some(usd), None).await;
    let branch = company(&mut tx, "Branch", None, Some(root.ids()[0])).await;
    let defaulted = tx
        .env()
        .records("res.company")
        .unwrap()
        .with_context([("default_parent_id", Value::Int(branch.ids()[0]))])
        .create(
            &mut tx,
            vec![RawVals::new().set_cell("name", Cell::Str("Nested".into()))],
        )
        .await
        .unwrap();
    assert_eq!(
        defaulted.cell(&mut tx, "parent_id").await.unwrap(),
        Cell::Id(branch.ids()[0])
    );
    assert_eq!(
        defaulted.cell(&mut tx, "currency_id").await.unwrap(),
        Cell::Id(usd)
    );
    assert_eq!(
        branch.cell(&mut tx, "currency_id").await.unwrap(),
        Cell::Id(usd)
    );
    let partner = defaulted.related(&mut tx, "partner_id").await.unwrap();
    assert_eq!(
        partner.cell(&mut tx, "parent_id").await.unwrap(),
        Cell::Null
    );
    let independent = tx
        .env()
        .records("res.company")
        .unwrap()
        .with_context([("default_parent_id", Value::Int(root.ids()[0]))])
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("name", Cell::Str("Independent".into()))
                    .set_cell("parent_id", Cell::Null)
                    .set_cell("currency_id", Cell::Id(usd)),
            ],
        )
        .await
        .unwrap();
    assert_eq!(
        independent.cell(&mut tx, "parent_id").await.unwrap(),
        Cell::Null
    );
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn every_parent_write_is_refused_even_if_unchanged() {
    for assignment in [None, Some(false), Some(true)] {
        let h = harness().await;
        let mut tx = h.begin().await.unwrap();
        let usd = currency(&mut tx, "CB3").await;
        let root = company(&mut tx, "Root", Some(usd), None).await;
        let other = company(&mut tx, "Other", Some(usd), None).await;
        let branch = company(&mut tx, "Branch", Some(usd), Some(root.ids()[0])).await;
        let before_path = branch.cell(&mut tx, "parent_path").await.unwrap();
        let parent = match assignment {
            None => Cell::Null,
            Some(false) => Cell::Id(root.ids()[0]),
            Some(true) => Cell::Id(other.ids()[0]),
        };
        let error = branch
            .write(&mut tx, RawVals::new().set_cell("parent_id", parent))
            .await
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::UserError);
        assert_eq!(error.message(), "The company hierarchy cannot be changed.");
        assert_eq!(
            branch.cell(&mut tx, "parent_path").await.unwrap(),
            before_path
        );
        assert_eq!(
            branch.cell(&mut tx, "parent_id").await.unwrap(),
            Cell::Id(root.ids()[0])
        );
        tx.rollback().await.unwrap();
        h.cleanup().await.unwrap();
    }
}

#[tokio::test]
async fn branch_cannot_choose_a_different_currency_on_create_or_write() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let usd = currency(&mut tx, "CB4").await;
    let eur = currency(&mut tx, "CB5").await;
    let root = company(&mut tx, "Root", Some(usd), None).await;
    let error = tx
        .env()
        .records("res.company")
        .unwrap()
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("name", Cell::Str("Wrong".into()))
                    .set_cell("parent_id", Cell::Id(root.ids()[0]))
                    .set_cell("currency_id", Cell::Id(eur)),
            ],
        )
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Validation);
    assert_eq!(
        error.message(),
        "A branch must use its root company's currency."
    );
    let commit_error = tx.commit().await.unwrap_err();
    assert_eq!(commit_error.kind(), ErrorKind::Validation);
    assert_eq!(
        commit_error.message(),
        "A branch must use its root company's currency."
    );
    h.cleanup().await.unwrap();
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let usd = currency(&mut tx, "CB6").await;
    let eur = currency(&mut tx, "CB7").await;
    let root = company(&mut tx, "Root", Some(usd), None).await;
    let branch = company(&mut tx, "Branch", Some(usd), Some(root.ids()[0])).await;
    let error = branch
        .write(
            &mut tx,
            RawVals::new().set_cell("currency_id", Cell::Id(eur)),
        )
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Validation);
    assert_eq!(
        error.message(),
        "A branch must use its root company's currency."
    );
    let commit_error = tx.commit().await.unwrap_err();
    assert_eq!(commit_error.kind(), ErrorKind::Validation);
    assert_eq!(
        commit_error.message(),
        "A branch must use its root company's currency."
    );
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn root_currency_reaches_nested_and_archived_branches_only_in_its_tree() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let usd = currency(&mut tx, "CB8").await;
    let eur = currency(&mut tx, "CB9").await;
    let third = currency(&mut tx, "CBR").await;
    let root = company(&mut tx, "Root", Some(usd), None).await;
    let branch = company(&mut tx, "Branch", Some(usd), Some(root.ids()[0])).await;
    let nested = company(&mut tx, "Nested", Some(usd), Some(branch.ids()[0])).await;
    let other = company(&mut tx, "Other", Some(usd), None).await;
    branch
        .write(
            &mut tx,
            RawVals::new().set_cell("active", Cell::Bool(false)),
        )
        .await
        .unwrap();
    for changed in [eur, third, third] {
        root.write(
            &mut tx,
            RawVals::new().set_cell("currency_id", Cell::Id(changed)),
        )
        .await
        .unwrap();
        for company in [&root, &branch, &nested] {
            assert_eq!(
                company.cell(&mut tx, "currency_id").await.unwrap(),
                Cell::Id(changed)
            );
        }
    }
    assert_eq!(
        other.cell(&mut tx, "currency_id").await.unwrap(),
        Cell::Id(usd)
    );
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn archive_cascades_and_unarchive_does_not_reactivate_descendants() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let usd = currency(&mut tx, "CBA").await;
    let root = company(&mut tx, "Root", Some(usd), None).await;
    let branch = company(&mut tx, "Branch", Some(usd), Some(root.ids()[0])).await;
    let nested = company(&mut tx, "Nested", Some(usd), Some(branch.ids()[0])).await;
    let other = company(&mut tx, "Other", Some(usd), None).await;
    branch.action_archive(&mut tx).await.unwrap();
    nested.action_unarchive(&mut tx).await.unwrap();
    assert_eq!(
        nested.cell(&mut tx, "active").await.unwrap(),
        Cell::Bool(true)
    );
    root.action_archive(&mut tx).await.unwrap();
    for company in [&root, &branch, &nested] {
        assert_eq!(
            company.cell(&mut tx, "active").await.unwrap(),
            Cell::Bool(false)
        );
    }
    assert_eq!(
        other.cell(&mut tx, "active").await.unwrap(),
        Cell::Bool(true)
    );
    root.action_unarchive(&mut tx).await.unwrap();
    assert_eq!(
        root.cell(&mut tx, "active").await.unwrap(),
        Cell::Bool(true)
    );
    assert_eq!(
        nested.cell(&mut tx, "active").await.unwrap(),
        Cell::Bool(false)
    );
    assert_eq!(
        branch.cell(&mut tx, "active").await.unwrap(),
        Cell::Bool(false)
    );
    let error = tx
        .savepoint(async |tx| {
            root.write(tx, RawVals::new().set_cell("active", Cell::Null))
                .await
        })
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Validation);
    assert_eq!(error.message(), "Company active must be a boolean.");
    assert_eq!(
        root.cell(&mut tx, "active").await.unwrap(),
        Cell::Bool(true)
    );
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn nested_creation_sets_parent_but_linking_an_existing_company_is_refused() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let usd = currency(&mut tx, "CBB").await;
    let root = tx
        .env()
        .records("res.company")
        .unwrap()
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("name", Cell::Str("Root".into()))
                    .set_cell("currency_id", Cell::Id(usd))
                    .set_commands(
                        "child_ids",
                        vec![Command::Create(
                            RawVals::new().set_cell("name", Cell::Str("Child".into())),
                        )],
                    ),
            ],
        )
        .await
        .unwrap();
    let child = root.related(&mut tx, "child_ids").await.unwrap();
    assert_eq!(child.len(), 1);
    assert_eq!(
        child.cell(&mut tx, "parent_id").await.unwrap(),
        Cell::Id(root.ids()[0])
    );
    assert_eq!(
        child.cell(&mut tx, "currency_id").await.unwrap(),
        Cell::Id(usd)
    );
    let other = company(&mut tx, "Other", Some(usd), None).await;
    let error = root
        .write(
            &mut tx,
            RawVals::new().set_commands("child_ids", vec![Command::Link(other.rec_ids()[0])]),
        )
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::UserError);
    assert_eq!(error.message(), "The company hierarchy cannot be changed.");
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn active_default_user_blocks_ancestor_archive() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let usd = currency(&mut tx, "CBC").await;
    let root = company(&mut tx, "Protected", Some(usd), None).await;
    let branch = company(&mut tx, "Protected Branch", Some(usd), Some(root.ids()[0])).await;
    tx.query("INSERT INTO res_users (name, login, active, company_id, share) VALUES ('Branch User', 'branch-user', true, $1, false) RETURNING id", &[&branch.ids()[0]]).await.unwrap();
    let error = root.action_archive(&mut tx).await.unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Validation);
    assert_eq!(
        error.message(),
        "The company Protected Branch cannot be archived because it is the default company of 1 active users."
    );
    let commit_error = tx.commit().await.unwrap_err();
    assert_eq!(commit_error.kind(), ErrorKind::Validation);
    assert_eq!(
        commit_error.message(),
        "The company Protected Branch cannot be archived because it is the default company of 1 active users."
    );
    h.cleanup().await.unwrap();
}

/// names the one with the lowest id and its own user count, whatever the order of the selection)
#[tokio::test]
async fn archive_refusal_names_the_lowest_protected_company_with_its_count() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let usd = currency(&mut tx, "CBG").await;
    let low = company(&mut tx, "Low protected", Some(usd), None)
        .await
        .ids()[0];
    let high = company(&mut tx, "High protected", Some(usd), None)
        .await
        .ids()[0];
    assert!(low < high);
    for (login, company) in [("low-a", low), ("low-b", low), ("high-a", high)] {
        tx.execute(
            "INSERT INTO res_users (name, login, active, company_id, share) VALUES ($1, $1, true, $2, false)",
            &[&login, &company],
        )
        .await
        .unwrap();
    }
    // the higher id comes first in the selection, so a loop in selection order would name it
    let selection = tx
        .env()
        .records("res.company")
        .unwrap()
        .browse([high, low])
        .unwrap();
    let error = selection
        .write(
            &mut tx,
            RawVals::new().set_cell("active", Cell::Bool(false)),
        )
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Validation);
    assert_eq!(
        error.message(),
        "The company Low protected cannot be archived because it is the default company of 2 active users."
    );
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn branches_action_has_direct_domain_and_inactive_context() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let usd = currency(&mut tx, "CBD").await;
    let root = company(&mut tx, "Root", Some(usd), None).await;
    let action: Value = root
        .call(&mut tx, "action_all_company_branches", ())
        .await
        .unwrap();
    let Value::Map(action) = action else {
        panic!("expected action map")
    };
    assert_eq!(action["res_model"], Value::str("res.company"));
    assert_eq!(action["target"], Value::str("current"));
    assert_eq!(
        action["domain"],
        Value::list([Value::list([
            Value::str("parent_id"),
            Value::str("="),
            Value::Int(root.ids()[0])
        ])])
    );
    let Value::Map(context) = &action["context"] else {
        panic!("expected context map")
    };
    assert_eq!(context["active_test"], Value::Bool(false));
    assert_eq!(context["default_parent_id"], Value::Int(root.ids()[0]));
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn active_user_cannot_acquire_an_archived_default_company() {
    let h = Harness::builder([&erp_addon_base::ADDON, &erp_data::model_data::ADDON])
        .with_decls([erp_data::ir_model_data_decl()])
        .build()
        .await
        .unwrap();
    let mut tx = h.begin().await.unwrap();
    let usd = currency(&mut tx, "CBE").await;
    let archived = company(&mut tx, "Archived", Some(usd), None).await;
    archived.action_archive(&mut tx).await.unwrap();
    erp_testkit::base_install::set_base_installing(&mut tx, true).unwrap();
    let users = tx.env().records("res.users").unwrap();
    let inactive = users
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("name", Cell::Str("Dormant".into()))
                    .set_cell("login", Cell::Str("dormant".into()))
                    .set_cell("active", Cell::Bool(false))
                    .set_cell("company_id", Cell::Id(archived.ids()[0])),
            ],
        )
        .await
        .unwrap();
    let error = tx
        .savepoint(async |tx| {
            inactive
                .write(tx, RawVals::new().set_cell("active", Cell::Bool(true)))
                .await
        })
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Validation);
    assert_eq!(
        error.message(),
        "An active user must have an active default company."
    );
    let error = tx
        .savepoint(async |tx| {
            users
                .create(
                    tx,
                    vec![
                        RawVals::new()
                            .set_cell("name", Cell::Str("Rejected active".into()))
                            .set_cell("login", Cell::Str("rejected-active".into()))
                            .set_cell("company_id", Cell::Id(archived.ids()[0])),
                    ],
                )
                .await
        })
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Validation);
    assert_eq!(
        error.message(),
        "An active user must have an active default company."
    );
    let live = company(&mut tx, "Live", Some(usd), None).await;
    let active = users
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("name", Cell::Str("Active assignment".into()))
                    .set_cell("login", Cell::Str("active-assignment".into()))
                    .set_cell("company_id", Cell::Id(live.ids()[0]))
                    // allowed in the archived company too, so the assignment below reaches the
                    // archived-company refusal and not the membership one
                    .set_commands(
                        "company_ids",
                        [Command::Set(vec![
                            erp_sdk::RecId::from_real(live.ids()[0]).unwrap(),
                            erp_sdk::RecId::from_real(archived.ids()[0]).unwrap(),
                        ])],
                    ),
            ],
        )
        .await
        .unwrap();
    let error = tx
        .savepoint(async |tx| {
            active
                .write(
                    tx,
                    RawVals::new().set_cell("company_id", Cell::Id(archived.ids()[0])),
                )
                .await
        })
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Validation);
    assert_eq!(
        error.message(),
        "An active user must have an active default company."
    );
    assert_eq!(
        active.cell(&mut tx, "company_id").await.unwrap(),
        Cell::Id(live.ids()[0])
    );
    assert_eq!(
        inactive.cell(&mut tx, "active").await.unwrap(),
        Cell::Bool(false)
    );
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn stale_branch_creator_preserves_retryable_serialization_error() {
    let h = harness().await;
    let mut setup = h.begin().await.unwrap();
    let old_currency = currency(&mut setup, "CBF").await;
    let new_currency = currency(&mut setup, "CBG").await;
    let root = company(&mut setup, "Root", Some(old_currency), None)
        .await
        .ids()[0];
    setup.commit().await.unwrap();
    let mut stale = h.begin().await.unwrap();
    stale
        .query("SELECT id FROM res_company WHERE id=$1", &[&root])
        .await
        .unwrap();
    let mut writer = h.begin().await.unwrap();
    writer
        .env()
        .records("res.company")
        .unwrap()
        .browse([root])
        .unwrap()
        .write(
            &mut writer,
            RawVals::new().set_cell("currency_id", Cell::Id(new_currency)),
        )
        .await
        .unwrap();
    writer.commit().await.unwrap();
    let error = stale
        .env()
        .records("res.company")
        .unwrap()
        .create(
            &mut stale,
            vec![
                RawVals::new()
                    .set_cell("name", Cell::Str("Stale Branch".into()))
                    .set_cell("parent_id", Cell::Id(root))
                    .set_cell("currency_id", Cell::Id(old_currency)),
            ],
        )
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Concurrency);
    assert_eq!(error.sqlstate(), Some("40001"));
    assert!(
        error.message().contains("could not serialize access"),
        "{error}"
    );
    stale.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn refused_parent_write_keeps_the_path() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let usd = currency(&mut tx, "CBH").await;
    let root = company(&mut tx, "Root", Some(usd), None).await;
    let branch = company(&mut tx, "Branch", Some(usd), Some(root.ids()[0])).await;
    let before = branch.cell(&mut tx, "parent_path").await.unwrap();
    for parent in [Cell::Null, Cell::Id(root.ids()[0])] {
        let error = branch
            .write(&mut tx, RawVals::new().set_cell("parent_id", parent))
            .await
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::UserError);
        assert_eq!(error.message(), "The company hierarchy cannot be changed.");
        assert_eq!(branch.cell(&mut tx, "parent_path").await.unwrap(), before);
    }
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn branch_form_currency_default_follows_its_context_parent() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let usd = currency(&mut tx, "CBI").await;
    let root = company(&mut tx, "Parent", Some(usd), None).await;
    let values: erp_sdk::ValueMap = tx
        .env()
        .records("res.company")
        .unwrap()
        .with_context([("default_parent_id", Value::Int(root.ids()[0]))])
        .call(
            &mut tx,
            "default_get",
            (vec!["parent_id".to_owned(), "currency_id".to_owned()],),
        )
        .await
        .unwrap();
    assert_eq!(values["parent_id"], Value::Int(root.ids()[0]));
    assert_eq!(values["currency_id"], Value::Int(usd));
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

mod branch_failure {
    use erp_addon_base::ResCompany;
    use erp_sdk::prelude::*;
    erp_sdk::addon!(name = "test_company_branch_failure", depends = ["base"]);
    #[extend(ResCompany, addon = "test_company_branch_failure")]
    pub struct BranchFailure;
    #[methods(ResCompany, addon = "test_company_branch_failure")]
    impl BranchFailure {
        #[method(override)]
        async fn write(
            this: &RecordSet<ResCompany>,
            tx: &mut Transaction,
            vals: RawVals,
            sup: Super,
        ) -> Result<()> {
            if vals.contains("currency_id") {
                for company in this.as_records().iter() {
                    if company.cell(tx, "name").await? == Cell::Str("Refuse currency".into()) {
                        return Err(Error::validation("fixture descendant currency refusal"));
                    }
                }
            }
            sup.call(tx, this.as_records(), (vals,)).await
        }
    }
}
#[tokio::test]
async fn failing_descendant_override_rolls_back_the_whole_currency_change() {
    let h = Harness::builder([&erp_addon_base::ADDON, &branch_failure::ADDON])
        .build()
        .await
        .unwrap();
    let mut tx = h.begin().await.unwrap();
    let usd = currency(&mut tx, "CBJ").await;
    let eur = currency(&mut tx, "CBK").await;
    tx.env()
        .records("res.currency")
        .unwrap()
        .browse([eur])
        .unwrap()
        .write(
            &mut tx,
            RawVals::new().set_cell("active", Cell::Bool(false)),
        )
        .await
        .unwrap();
    let root = company(&mut tx, "Root", Some(usd), None).await.ids()[0];
    let branch = company(&mut tx, "Refuse currency", Some(usd), Some(root))
        .await
        .ids()[0];
    tx.commit().await.unwrap();
    let mut tx = h.begin().await.unwrap();
    let error = tx
        .env()
        .records("res.company")
        .unwrap()
        .browse([root])
        .unwrap()
        .write(
            &mut tx,
            RawVals::new().set_cell("currency_id", Cell::Id(eur)),
        )
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Validation);
    assert_eq!(error.message(), "fixture descendant currency refusal");
    tx.rollback().await.unwrap();
    let mut check = h.begin().await.unwrap();
    for id in [root, branch] {
        assert_eq!(
            check
                .env()
                .records("res.company")
                .unwrap()
                .browse([id])
                .unwrap()
                .cell(&mut check, "currency_id")
                .await
                .unwrap(),
            Cell::Id(usd)
        );
    }
    assert_eq!(
        check
            .env()
            .records("res.currency")
            .unwrap()
            .browse([eur])
            .unwrap()
            .cell(&mut check, "active")
            .await
            .unwrap(),
        Cell::Bool(false)
    );
    check.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn removing_branches_through_commands_cannot_change_parentage() {
    for kind in 0..3 {
        let h = harness().await;
        let mut tx = h.begin().await.unwrap();
        let usd = currency(&mut tx, "CBL").await;
        let root = company(&mut tx, "Root", Some(usd), None).await;
        let branch = company(&mut tx, "Branch", Some(usd), Some(root.ids()[0])).await;
        let command = match kind {
            0 => Command::Unlink(branch.rec_ids()[0]),
            1 => Command::Clear,
            2 => Command::Set(vec![]),
            _ => unreachable!(),
        };
        let error = root
            .write(&mut tx, RawVals::new().set_commands("child_ids", [command]))
            .await
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::UserError);
        assert_eq!(error.message(), "The company hierarchy cannot be changed.");
        tx.rollback().await.unwrap();
        h.cleanup().await.unwrap();
    }
}

#[tokio::test]
async fn stale_user_reactivation_cannot_race_an_archive_on_another_node() {
    let h = Harness::builder([&erp_addon_base::ADDON, &erp_data::model_data::ADDON])
        .with_decls([erp_data::ir_model_data_decl()])
        .build()
        .await
        .unwrap();
    let second = erp_orm::Orm::builder(
        std::sync::Arc::clone(h.orm().registry()),
        h.db().pool().clone(),
    )
    .trust_caller_resolutions()
    .build()
    .unwrap();
    assert_ne!(h.orm().id(), second.id());
    let mut setup = h.begin().await.unwrap();
    let usd = currency(&mut setup, "CBM").await;
    let root = company(&mut setup, "Root", Some(usd), None).await.ids()[0];
    erp_testkit::base_install::set_base_installing(&mut setup, true).unwrap();
    let user = setup
        .env()
        .records("res.users")
        .unwrap()
        .create(
            &mut setup,
            vec![
                RawVals::new()
                    .set_cell("name", Cell::Str("Inactive".into()))
                    .set_cell("login", Cell::Str("race-user".into()))
                    .set_cell("active", Cell::Bool(false))
                    .set_cell("company_id", Cell::Id(root)),
            ],
        )
        .await
        .unwrap()
        .ids()[0];
    setup.commit().await.unwrap();
    let mut stale = h.begin().await.unwrap();
    stale
        .query("SELECT id FROM res_users WHERE id=$1", &[&user])
        .await
        .unwrap();
    let mut writer = second.begin(erp_orm::SUPERUSER_ID).await.unwrap();
    writer
        .env()
        .records("res.company")
        .unwrap()
        .browse([root])
        .unwrap()
        .action_archive(&mut writer)
        .await
        .unwrap();
    writer.commit().await.unwrap();
    let error = stale
        .env()
        .records("res.users")
        .unwrap()
        .browse([user])
        .unwrap()
        .write(
            &mut stale,
            RawVals::new().set_cell("active", Cell::Bool(true)),
        )
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Concurrency);
    assert_eq!(error.sqlstate(), Some("40001"));
    // Driver source text may include server detail; SQLSTATE and the serialization diagnostic are stable.
    assert!(
        error.message().contains("could not serialize access"),
        "{error}"
    );
    stale.rollback().await.unwrap();
    let mut check = h.begin().await.unwrap();
    assert_eq!(
        check
            .env()
            .records("res.users")
            .unwrap()
            .browse([user])
            .unwrap()
            .cell(&mut check, "active")
            .await
            .unwrap(),
        Cell::Bool(false)
    );
    assert_eq!(
        check
            .env()
            .records("res.company")
            .unwrap()
            .browse([root])
            .unwrap()
            .cell(&mut check, "active")
            .await
            .unwrap(),
        Cell::Bool(false)
    );
    check.rollback().await.unwrap();
    drop(second);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn null_active_is_refused_instead_of_bypassing_archive() {
    let h = harness().await;
    let mut setup = h.begin().await.unwrap();
    let usd = currency(&mut setup, "CBN").await;
    let root = company(&mut setup, "Root", Some(usd), None).await.ids()[0];
    let branch = company(&mut setup, "Branch", Some(usd), Some(root))
        .await
        .ids()[0];
    setup.commit().await.unwrap();
    let mut tx = h.begin().await.unwrap();
    let error = tx
        .env()
        .records("res.company")
        .unwrap()
        .browse([root])
        .unwrap()
        .write(&mut tx, RawVals::new().set_cell("active", Cell::Null))
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Validation);
    assert_eq!(error.message(), "Company active must be a boolean.");
    tx.rollback().await.unwrap();
    let mut check = h.begin().await.unwrap();
    for id in [root, branch] {
        assert_eq!(
            check
                .env()
                .records("res.company")
                .unwrap()
                .browse([id])
                .unwrap()
                .cell(&mut check, "active")
                .await
                .unwrap(),
            Cell::Bool(true)
        );
    }
    check.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn company_lifecycle_limits_and_missing_fence_fail_loudly() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let usd = currency(&mut tx, "CBO").await;
    let companies = tx.env().records("res.company").unwrap();
    let values = RawVals::new()
        .set_cell("name", Cell::Str("Bounded".into()))
        .set_cell("currency_id", Cell::Id(usd));
    let error = tx
        .savepoint(async |tx| companies.create(tx, vec![values.clone(); 4097]).await)
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Validation);
    assert_eq!(
        error.message(),
        "Company lifecycle maximum visited companies of 4096 exceeded."
    );
    let error = tx
        .savepoint(async |tx| {
            tx.execute("DELETE FROM res_company_lifecycle_fence", &[])
                .await?;
            companies.create(tx, vec![values.clone()]).await
        })
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Database);
    assert_eq!(
        error.message(),
        "res_company_lifecycle_fence row missing: expected one row"
    );
    assert_eq!(
        companies.create(&mut tx, vec![values]).await.unwrap().len(),
        1
    );
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn restrictive_rules_refuse_complete_subtree_mutations() {
    use erp_sdk::{Domain, Operation, Operator};
    use erp_testkit::{DomainPolicy, Otherwise};
    for deny_write in [true, false] {
        let visible = Domain::condition("name", Operator::Ne, "Hidden branch").unwrap();
        let mut policy = DomainPolicy::builder(Otherwise::Allow).rule(
            "res.company",
            Operation::Read,
            visible.clone(),
        );
        if deny_write {
            policy = policy.rule("res.company", Operation::Write, visible);
        }
        let h = Harness::builder([&erp_addon_base::ADDON])
            .policy(policy.build().unwrap())
            .build()
            .await
            .unwrap();
        let mut setup = h.begin().await.unwrap();
        let usd = currency(&mut setup, "CBP").await;
        let eur = currency(&mut setup, "CBQ").await;
        let root = company(&mut setup, "Visible root", Some(usd), None).await;
        let hidden = company(&mut setup, "Hidden branch", Some(usd), Some(root.ids()[0])).await;
        let ids = [root.ids()[0], hidden.ids()[0]];
        let home = company(&mut setup, "Actor home", Some(usd), None).await;
        setup.execute("INSERT INTO res_users (id,name,login,active,company_id,share) VALUES (2,'Lifecycle actor','lifecycle-actor',true,$1,false)", &[&home.ids()[0]]).await.unwrap();
        setup.commit().await.unwrap();
        for values in [
            RawVals::new().set_cell("active", Cell::Bool(false)),
            RawVals::new().set_cell("currency_id", Cell::Id(eur)),
        ] {
            let companies = vec![erp_orm::CompanyId::new(ids[0]).unwrap()];
            let principal = erp_orm::PrincipalBuilder::user(erp_orm::UserId::new(2).unwrap())
                .resolution(
                    erp_orm::Resolution::in_id_order(
                        erp_orm::GroupSet::default(),
                        companies.clone(),
                        std::num::NonZeroU64::MIN,
                    )
                    .unwrap(),
                )
                .companies(companies)
                .build()
                .unwrap();
            let mut tx = h
                .orm()
                .begin_opts(erp_orm::BeginOptions::for_principal(principal))
                .await
                .unwrap();
            let root = tx
                .env()
                .records("res.company")
                .unwrap()
                .browse([ids[0]])
                .unwrap();
            let error = root.write(&mut tx, values).await.unwrap_err();
            assert_eq!(error.kind(), ErrorKind::Access, "{error:?}");
            assert_eq!(
                error.message(),
                "Company lifecycle requires read and write access to every affected branch."
            );
            tx.rollback().await.unwrap();
            let mut verify = h.begin().await.unwrap();
            let both = verify
                .env()
                .records("res.company")
                .unwrap()
                .browse(ids)
                .unwrap();
            for company in both.iter() {
                assert_eq!(
                    company.cell(&mut verify, "active").await.unwrap(),
                    Cell::Bool(true)
                );
                assert_eq!(
                    company.cell(&mut verify, "currency_id").await.unwrap(),
                    Cell::Id(usd)
                );
            }
            verify.rollback().await.unwrap();
        }
        h.cleanup().await.unwrap();
    }
}

#[tokio::test]
async fn overlapping_selection_cannot_truncate_hidden_descendant_preflight() {
    use erp_sdk::{Domain, Operation, Operator};
    use erp_testkit::{DomainPolicy, Otherwise};
    let policy = DomainPolicy::builder(Otherwise::Allow)
        .rule(
            "res.company",
            Operation::Read,
            Domain::condition("name", Operator::Ne, "Hidden boundary branch").unwrap(),
        )
        .build()
        .unwrap();
    let h = Harness::builder([&erp_addon_base::ADDON])
        .policy(policy)
        .build()
        .await
        .unwrap();
    let mut setup = h.begin().await.unwrap();
    let usd = currency(&mut setup, "CBS").await;
    let root = company(&mut setup, "Boundary root", Some(usd), None).await;
    let root_id = root.ids()[0];
    let home = company(&mut setup, "Boundary actor home", Some(usd), None).await;
    setup.flush().await.unwrap();
    // Seed valid stored rows without making this functional admission test perform thousands
    // of unrelated company/contact creation calls. Root plus children is exactly 4096 rows.
    setup.execute(
        "INSERT INTO res_company (id,name,parent_id,currency_id,active,parent_path) \
         SELECT n,CASE WHEN n=104094 THEN 'Hidden boundary branch' ELSE 'Visible boundary branch' END, \
         $1::int8,$2::int8,true,($1::int8)::text || '/' || n::text || '/' FROM generate_series(100000,104094) n",
        &[&root_id, &usd],
    ).await.unwrap();
    setup.execute("INSERT INTO res_users (id,name,login,active,company_id,share) VALUES (2,'Boundary actor','boundary-actor',true,$1,false)", &[&home.ids()[0]]).await.unwrap();
    setup.commit().await.unwrap();
    drop(setup);
    let companies = vec![erp_orm::CompanyId::new(root_id).unwrap()];
    let principal = erp_orm::PrincipalBuilder::user(erp_orm::UserId::new(2).unwrap())
        .resolution(
            erp_orm::Resolution::in_id_order(
                erp_orm::GroupSet::default(),
                companies.clone(),
                std::num::NonZeroU64::MIN,
            )
            .unwrap(),
        )
        .companies(companies)
        .build()
        .unwrap();
    let mut tx = h
        .orm()
        .begin_opts(erp_orm::BeginOptions::for_principal(principal))
        .await
        .unwrap();
    let selection = tx
        .env()
        .records("res.company")
        .unwrap()
        .browse(std::iter::once(root_id).chain(100000..104094))
        .unwrap();
    let outcome = selection
        .write(
            &mut tx,
            RawVals::new().set_cell("active", Cell::Bool(false)),
        )
        .await;
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
    let error = outcome.unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Access);
    assert_eq!(
        error.message(),
        "Company lifecycle requires read and write access to every affected branch."
    );
}

#[tokio::test]
async fn branch_created_from_a_saved_company_takes_parent_and_root_currency() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let usd = currency(&mut tx, "CBP").await;
    let root = company(&mut tx, "Root", Some(usd), None).await;
    root.write(
        &mut tx,
        RawVals::new().set_commands(
            "child_ids",
            vec![Command::Create(
                RawVals::new().set_cell("name", Cell::Str("Page branch".into())),
            )],
        ),
    )
    .await
    .unwrap();
    let child = root.related(&mut tx, "child_ids").await.unwrap();
    assert_eq!(child.len(), 1);
    assert_eq!(
        child.cell(&mut tx, "parent_id").await.unwrap(),
        Cell::Id(root.ids()[0])
    );
    assert_eq!(
        child.cell(&mut tx, "currency_id").await.unwrap(),
        Cell::Id(usd)
    );
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn all_child_ids_commands_cannot_change_parentage() {
    for kind in 0..4 {
        let h = harness().await;
        let mut tx = h.begin().await.unwrap();
        let usd = currency(&mut tx, "CBA").await;
        let root = company(&mut tx, "Root", Some(usd), None).await;
        let branch = company(&mut tx, "Branch", Some(usd), Some(root.ids()[0])).await;
        let other = company(&mut tx, "Other", Some(usd), None).await;
        let command = match kind {
            0 => Command::Link(other.rec_ids()[0]),
            1 => Command::Unlink(branch.rec_ids()[0]),
            2 => Command::Clear,
            3 => Command::Set(vec![]),
            _ => unreachable!(),
        };
        let error = root
            .write(
                &mut tx,
                RawVals::new().set_commands("all_child_ids", [command]),
            )
            .await
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::UserError, "command {kind}");
        assert_eq!(error.message(), "The company hierarchy cannot be changed.");
        assert_eq!(
            branch.cell(&mut tx, "parent_id").await.unwrap(),
            Cell::Id(root.ids()[0])
        );
        tx.rollback().await.unwrap();
        h.cleanup().await.unwrap();
    }
}

#[tokio::test]
async fn ordinary_write_on_more_companies_than_the_lifecycle_bound_succeeds() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let usd = currency(&mut tx, "CBN").await;
    let companies = tx.env().records("res.company").unwrap();
    let values = RawVals::new()
        .set_cell("name", Cell::Str("Many".into()))
        .set_cell("currency_id", Cell::Id(usd));
    let first = companies
        .create(&mut tx, vec![values.clone(); 2048])
        .await
        .unwrap();
    let second = companies.create(&mut tx, vec![values; 2049]).await.unwrap();
    let all = first.union(&second).unwrap();
    assert_eq!(all.len(), 4097);
    all.write(
        &mut tx,
        RawVals::new().set_cell("name", Cell::Str("Renamed".into())),
    )
    .await
    .unwrap();
    tx.flush().await.unwrap();
    let ids = all.ids();
    let rows = tx
        .query(
            "SELECT count(*) FROM res_company WHERE id = ANY($1::int8[]) AND name = 'Renamed'",
            &[&ids],
        )
        .await
        .unwrap();
    assert_eq!(rows[0].get::<_, i64>(0), 4097);
    let error = tx
        .savepoint(async |tx| {
            all.write(tx, RawVals::new().set_cell("active", Cell::Bool(false)))
                .await
        })
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Validation);
    assert_eq!(
        error.message(),
        "Company lifecycle maximum visited companies of 4096 exceeded."
    );
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn company_parent_is_indexed() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let rows = tx
        .query(
            "SELECT 1 FROM pg_index i JOIN pg_attribute a ON a.attrelid = i.indrelid AND a.attnum = i.indkey[0] \
             WHERE i.indrelid = 'res_company'::regclass AND a.attname = 'parent_id'",
            &[],
        )
        .await
        .unwrap();
    assert!(!rows.is_empty(), "res_company.parent_id has no index");
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

// ----: a created company joins the creator's allowed companies ----------------

/// The companies allowed to `user` by the stored relation rows, in id order. Pending writes are
/// flushed first, so this reads what the ORM wrote, never a value the test typed.
async fn allowed_of(tx: &mut Transaction, user: i64) -> Vec<i64> {
    tx.flush().await.unwrap();
    tx.query(
        "SELECT cid::int8 FROM res_company_users_rel WHERE user_id = $1 ORDER BY cid",
        &[&user],
    )
    .await
    .unwrap()
    .iter()
    .map(|row| row.get(0))
    .collect()
}

/// The users that hold company `cid` in their allowed companies, in id order.
async fn holders_of(tx: &mut Transaction, cid: i64) -> Vec<i64> {
    tx.flush().await.unwrap();
    tx.query(
        "SELECT user_id::int8 FROM res_company_users_rel WHERE cid = $1 ORDER BY user_id",
        &[&cid],
    )
    .await
    .unwrap()
    .iter()
    .map(|row| row.get(0))
    .collect()
}

/// A database whose field policy keeps `company_ids` administrator-only, one company `Home` and an
/// ordinary internal user `me` allowed in it only. Returns the harness, `me`, a currency and `Home`.
async fn ordinary_creator() -> (Harness, i64, i64, i64) {
    let policy = FieldPolicy::new().with_overrides();
    let h = Harness::builder([&erp_addon_base::ADDON, &erp_data::model_data::ADDON])
        .with_decls([erp_data::ir_model_data_decl()])
        .policy(policy.clone())
        .build()
        .await
        .unwrap();
    policy.attach(h.registry()).unwrap();
    let mut tx = h.begin().await.unwrap();
    users_support::user_types_and_admin(&mut tx).await;
    let usd = erp_testkit::fixtures::seed_currency_in(&mut tx)
        .await
        .unwrap();
    let home = users_support::create(
        &mut tx,
        "res.company",
        &[
            ("name", users_support::s("Home")),
            ("currency_id", users_support::int(usd)),
        ],
    )
    .await;
    let me = users_support::create(
        &mut tx,
        "res.users",
        &[
            ("name", users_support::s("Creator")),
            ("login", users_support::s("creator")),
            ("company_id", users_support::int(home)),
            ("company_ids", users_support::set_cmd(&[home])),
        ],
    )
    .await;
    policy.join(me, "base.group_user");
    tx.commit().await.unwrap();
    (h, me, usd, home)
}

#[tokio::test]
async fn superuser_creating_a_company_links_it_once() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let usd = currency(&mut tx, "CL1").await;
    let created = company(&mut tx, "Linked", Some(usd), None).await;
    let id = created.ids()[0];
    let superuser = erp_orm::SUPERUSER_ID.get();
    assert_eq!(allowed_of(&mut tx, superuser).await, vec![id]);
    // the creator is the superuser: one row, not two
    assert_eq!(holders_of(&mut tx, id).await, vec![superuser]);
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn batch_create_links_every_created_company() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let usd = currency(&mut tx, "CL2").await;
    let created = tx
        .env()
        .records("res.company")
        .unwrap()
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("name", Cell::Str("First linked".into()))
                    .set_cell("currency_id", Cell::Id(usd)),
                RawVals::new()
                    .set_cell("name", Cell::Str("Second linked".into()))
                    .set_cell("currency_id", Cell::Id(usd)),
            ],
        )
        .await
        .unwrap();
    let mut expected = created.ids();
    expected.sort_unstable();
    assert_eq!(expected.len(), 2);
    assert_eq!(
        allowed_of(&mut tx, erp_orm::SUPERUSER_ID.get()).await,
        expected
    );
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

/// creator still cannot write `company_ids` directly)
#[tokio::test]
async fn ordinary_creator_and_superuser_both_get_the_new_company() {
    let (h, me, usd, home) = ordinary_creator().await;
    let superuser = erp_orm::SUPERUSER_ID.get();
    let mut tx = h.begin().await.unwrap();
    let created = tx
        .env()
        .records("res.company")
        .unwrap()
        .with_user(me)
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("name", Cell::Str("Mine".into()))
                    .set_cell("currency_id", Cell::Id(usd)),
            ],
        )
        .await
        .unwrap();
    let id = created.ids()[0];
    let mut expected = vec![home, id];
    expected.sort_unstable();
    assert_eq!(allowed_of(&mut tx, me).await, expected);
    assert_eq!(holders_of(&mut tx, id).await, vec![superuser, me]);
    // the creator reads the new company in its own environment. The harness policy does not
    // restrict records, so this does not prove the membership refresh; that needs a resolving
    // policy and is not covered here.
    assert_eq!(
        created.cell(&mut tx, "name").await.unwrap(),
        Cell::Str("Mine".into())
    );
    // the allowed set changed, the default company did not
    let own = users_support::records(&tx, "res.users", &[me]);
    assert_eq!(
        own.cell(&mut tx, "company_id").await.unwrap(),
        Cell::Id(home)
    );
    tx.commit().await.unwrap();
    drop(tx);

    // the elevation covered the link only: the same user still cannot write the field
    let mut tx = h.begin().await.unwrap();
    let own = tx
        .env()
        .records("res.users")
        .unwrap()
        .with_user(me)
        .browse([me])
        .unwrap();
    let error = own
        .write_values(
            &mut tx,
            &users_support::map(&[("company_ids", users_support::set_cmd(&[home, id]))]),
        )
        .await
        .unwrap_err();
    users_support::assert_error(
        &error,
        ErrorKind::Access,
        "You do not have enough rights to access the field \"company_ids\" on User \
         (res.users). Please contact your system administrator.\n\nOperation: write",
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

/// company is linked like any other created company)
#[tokio::test]
async fn branch_created_through_a_command_is_linked_to_the_creator() {
    let (h, me, usd, home) = ordinary_creator().await;
    let superuser = erp_orm::SUPERUSER_ID.get();
    let mut tx = h.begin().await.unwrap();
    let root = tx
        .env()
        .records("res.company")
        .unwrap()
        .with_user(me)
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("name", Cell::Str("Linked root".into()))
                    .set_cell("currency_id", Cell::Id(usd)),
            ],
        )
        .await
        .unwrap();
    root.write(
        &mut tx,
        RawVals::new().set_commands(
            "child_ids",
            vec![Command::Create(
                RawVals::new().set_cell("name", Cell::Str("Linked branch".into())),
            )],
        ),
    )
    .await
    .unwrap();
    let branch = root.related(&mut tx, "child_ids").await.unwrap();
    assert_eq!(branch.len(), 1);
    let branch = branch.ids()[0];
    let mut expected = vec![home, root.ids()[0], branch];
    expected.sort_unstable();
    assert_eq!(allowed_of(&mut tx, me).await, expected);
    assert_eq!(holders_of(&mut tx, branch).await, vec![superuser, me]);
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

/// allowed companies change)
#[tokio::test]
async fn refused_creator_changes_nobody_s_allowed_companies() {
    let h = Harness::builder([&erp_addon_base::ADDON])
        .policy(erp_orm::StaticPolicy::SuperuserOnly)
        .build()
        .await
        .unwrap();
    let mut setup = h.begin().await.unwrap();
    let usd = currency(&mut setup, "CL3").await;
    let home = company(&mut setup, "Home", Some(usd), None).await.ids()[0];
    setup.execute(
        "INSERT INTO res_users (id,name,login,active,company_id,share) VALUES (2,'Refused creator','refused-creator',true,$1,false)",
        &[&home],
    )
    .await
    .unwrap();
    setup
        .execute(
            "INSERT INTO res_company_users_rel (user_id, cid) VALUES (2, $1)",
            &[&home],
        )
        .await
        .unwrap();
    setup.commit().await.unwrap();
    drop(setup);
    let superuser = erp_orm::SUPERUSER_ID.get();
    let mut before = h.begin().await.unwrap();
    let super_before = allowed_of(&mut before, superuser).await;
    let user_before = allowed_of(&mut before, 2).await;
    let rows_before: i64 = before
        .query("SELECT count(*) FROM res_company", &[])
        .await
        .unwrap()[0]
        .get(0);
    before.rollback().await.unwrap();
    drop(before);

    let mut tx = h.begin_as(2).await.unwrap();
    let error = tx
        .env()
        .records("res.company")
        .unwrap()
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("name", Cell::Str("Not allowed".into()))
                    .set_cell("currency_id", Cell::Id(usd)),
            ],
        )
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Access, "{error:?}");
    assert_eq!(
        error.message(),
        "You are not allowed to create 'Company' (res.company) records."
    );
    tx.rollback().await.unwrap();
    drop(tx);

    let mut after = h.begin().await.unwrap();
    assert_eq!(allowed_of(&mut after, superuser).await, super_before);
    assert_eq!(allowed_of(&mut after, 2).await, user_before);
    let rows_after: i64 = after
        .query("SELECT count(*) FROM res_company", &[])
        .await
        .unwrap()[0]
        .get(0);
    assert_eq!(rows_after, rows_before);
    after.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

/// Every `(user, company)` pair of the allowed-company relation, in order.
async fn all_relations(tx: &mut Transaction) -> Vec<(i64, i64)> {
    tx.flush().await.unwrap();
    tx.query(
        "SELECT user_id::int8, cid::int8 FROM res_company_users_rel ORDER BY 1, 2",
        &[],
    )
    .await
    .unwrap()
    .iter()
    .map(|row| (row.get(0), row.get(1)))
    .collect()
}

#[tokio::test]
async fn failing_link_fails_the_whole_create() {
    let (h, _me, usd, _home) = ordinary_creator().await;
    // an active user without a default company, as a database that predates the rule could hold
    let mut setup = h.begin().await.unwrap();
    let user: i64 = setup
        .query(
            "INSERT INTO res_users (name, login, active, company_id, share) VALUES ('No Default', 'no-default', true, NULL, false) RETURNING id::int8",
            &[],
        )
        .await
        .unwrap()[0]
        .get(0);
    setup.commit().await.unwrap();
    drop(setup);

    let mut tx = h.begin().await.unwrap();
    let user_name: String = tx
        .query("SELECT name FROM res_users WHERE id = $1", &[&user])
        .await
        .unwrap()[0]
        .get(0);
    let relations_before = all_relations(&mut tx).await;
    let companies = tx.env().records("res.company").unwrap().with_user(user);
    let error = tx
        .savepoint(async |tx| {
            companies
                .create(
                    tx,
                    vec![
                        RawVals::new()
                            .set_cell("name", Cell::Str("Never created".into()))
                            .set_cell("currency_id", Cell::Id(usd)),
                    ],
                )
                .await
        })
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Validation, "{error:?}");
    assert_eq!(
        error.message(),
        format!(
            "The user {user_name} has no default company. Choose one of the user's allowed companies."
        )
    );
    tx.flush().await.unwrap();
    let created: i64 = tx
        .query(
            "SELECT count(*)::int8 FROM res_company WHERE name = 'Never created'",
            &[],
        )
        .await
        .unwrap()[0]
        .get(0);
    assert_eq!(created, 0, "the refused create must leave no company");
    assert_eq!(all_relations(&mut tx).await, relations_before);
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}
