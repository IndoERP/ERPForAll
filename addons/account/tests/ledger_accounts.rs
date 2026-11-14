// SPDX-License-Identifier: MIT OR Apache-2.0
//! Ledger configuration models: accounts, tags, roots and per-company codes.
//! Default records are checked by `crates/erp-cli/tests/account_install.rs`.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

use erp_sdk::prelude::*;
use erp_sdk::{Command, ErrorKind};
use erp_testkit::Harness;

async fn harness() -> Harness {
    Harness::builder([
        &erp_addon_base::ADDON,
        &erp_addon_uom::ADDON,
        &erp_addon_product::ADDON,
        &erp_addon_analytic::ADDON,
        &erp_addon_account::ADDON,
    ])
    .build()
    .await
    .unwrap()
}

fn text(value: &str) -> Cell {
    Cell::Str(value.into())
}

/// A company row, a root unless `parent` is given; the harness loads no data, so the first call
/// also inserts the currency fixture.
async fn company(tx: &mut Transaction, name: &str, parent: Option<i64>) -> i64 {
    let existing = tx
        .query("SELECT id FROM res_currency ORDER BY id LIMIT 1", &[])
        .await
        .unwrap();
    let currency: i64 = match existing.first() {
        Some(row) => row.get(0),
        None => erp_testkit::fixtures::seed_currency_in(tx).await.unwrap(),
    };
    tx.query(
        "WITH n AS (SELECT nextval(pg_get_serial_sequence('res_company', 'id')) AS id) \
         INSERT INTO res_company (id, name, active, currency_id, parent_id, parent_path, account_price_include) \
         SELECT id, $1, true, $2, $3::int8, \
                COALESCE((SELECT parent_path FROM res_company WHERE id = $3::int8), '') || id::text || '/', 'tax_excluded' \
         FROM n RETURNING id::int8",
        &[&name, &currency, &parent],
    )
    .await
    .unwrap()[0]
        .get(0)
}

/// `account.account` in the environment of `company`.
fn accounts(tx: &Transaction, company: i64) -> Records {
    tx.env()
        .with_company(company)
        .unwrap()
        .records("account.account")
        .unwrap()
}

async fn create(model: &Records, tx: &mut Transaction, vals: RawVals) -> Result<Records> {
    let model = model.clone();
    tx.savepoint(async move |tx| model.create(tx, vec![vals]).await)
        .await
}

async fn write(record: &Records, tx: &mut Transaction, vals: RawVals) -> Result<()> {
    let record = record.clone();
    tx.savepoint(async move |tx| record.write(tx, vals).await)
        .await
}

fn account_vals(name: &str, account_type: &str, code: &str) -> RawVals {
    RawVals::new()
        .set_cell("name", text(name))
        .set_cell("account_type", text(account_type))
        .set_cell("code", text(code))
}

async fn str_cell(record: &Records, tx: &mut Transaction, field: &str) -> Option<String> {
    match record.cell(tx, field).await.unwrap() {
        Cell::Str(value) => Some(value.to_string()),
        _ => None,
    }
}

async fn ids_cell(record: &Records, tx: &mut Transaction, field: &str) -> Vec<i64> {
    match record.cell(tx, field).await.unwrap() {
        Cell::Ids(ids) => ids.iter().map(|id| id.get()).collect(),
        other => panic!("{field} is {other:?}"),
    }
}

async fn bool_cell(record: &Records, tx: &mut Transaction, field: &str) -> bool {
    matches!(record.cell(tx, field).await.unwrap(), Cell::Bool(true))
}

fn assert_refused(error: &Error, message: &str) {
    assert_eq!(error.kind(), ErrorKind::Validation, "{error:?}");
    assert!(
        error.message().contains(message),
        "expected {message:?} in {:?}",
        error.message()
    );
}

#[tokio::test]
async fn nineteen_account_types_and_a_value_outside_is_refused() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let company = company(&mut tx, "Root", None).await;
    let model = accounts(&tx, company);
    let types = [
        "asset_receivable",
        "asset_cash",
        "asset_current",
        "asset_non_current",
        "asset_prepayments",
        "asset_fixed",
        "liability_payable",
        "liability_credit_card",
        "liability_current",
        "liability_non_current",
        "equity",
        "equity_unaffected",
        "income",
        "income_other",
        "expense",
        "expense_other",
        "expense_depreciation",
        "expense_direct_cost",
        "off_balance",
    ];
    for (n, account_type) in types.iter().enumerate() {
        create(
            &model,
            &mut tx,
            account_vals("A", account_type, &format!("{}", 100 + n)),
        )
        .await
        .unwrap();
    }
    let bad = create(&model, &mut tx, account_vals("A", "asset_unknown", "999")).await;
    assert!(bad.is_err(), "a type outside the selection is refused");
}

#[tokio::test]
async fn internal_group_and_initial_balance_follow_the_type() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let company = company(&mut tx, "Root", None).await;
    let model = accounts(&tx, company);
    let cases = [
        ("asset_receivable", "asset", true),
        ("liability_current", "liability", true),
        ("equity", "equity", true),
        ("equity_unaffected", "equity", false),
        ("income_other", "income", false),
        ("expense_direct_cost", "expense", false),
        ("off_balance", "off", true),
    ];
    for (n, (account_type, group, initial)) in cases.into_iter().enumerate() {
        let account = create(
            &model,
            &mut tx,
            account_vals("A", account_type, &format!("{}", 10 + n)),
        )
        .await
        .unwrap();
        assert_eq!(
            str_cell(&account, &mut tx, "internal_group")
                .await
                .as_deref(),
            Some(group),
            "{account_type}"
        );
        assert_eq!(
            bool_cell(&account, &mut tx, "include_initial_balance").await,
            initial,
            "{account_type}"
        );
    }
}

#[tokio::test]
async fn reconcile_follows_the_type_and_is_guarded() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let company = company(&mut tx, "Root", None).await;
    let model = accounts(&tx, company);
    let receivable = create(
        &model,
        &mut tx,
        account_vals("AR", "asset_receivable", "1100"),
    )
    .await
    .unwrap();
    assert!(bool_cell(&receivable, &mut tx, "reconcile").await);
    for (n, account_type) in [
        "income",
        "expense",
        "equity",
        "asset_cash",
        "liability_credit_card",
        "off_balance",
    ]
    .into_iter()
    .enumerate()
    {
        let account = create(
            &model,
            &mut tx,
            account_vals("X", account_type, &format!("{}", 200 + n)),
        )
        .await
        .unwrap();
        assert!(
            !bool_cell(&account, &mut tx, "reconcile").await,
            "{account_type}"
        );
    }
    // other asset types keep the user's choice
    let current = create(
        &model,
        &mut tx,
        account_vals("C", "asset_current", "1200").set_cell("reconcile", Cell::Bool(true)),
    )
    .await
    .unwrap();
    assert!(bool_cell(&current, &mut tx, "reconcile").await);

    let error = write(
        &receivable,
        &mut tx,
        RawVals::new().set_cell("reconcile", Cell::Bool(false)),
    )
    .await
    .unwrap_err();
    assert_refused(
        &error,
        "A receivable or payable account must allow reconciliation (account code: 1100).",
    );
    let off = create(&model, &mut tx, account_vals("Off", "off_balance", "9000"))
        .await
        .unwrap();
    let error = write(
        &off,
        &mut tx,
        RawVals::new().set_cell("reconcile", Cell::Bool(true)),
    )
    .await
    .unwrap_err();
    assert_refused(
        &error,
        "Off-balance accounts take no part in reconciliation: turn reconciliation off for this account.",
    );
}

#[tokio::test]
async fn accounts_have_a_company_and_bank_accounts_only_one() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let one = company(&mut tx, "One", None).await;
    let two = company(&mut tx, "Two", None).await;
    let model = accounts(&tx, one);
    // the active company is the default
    let account = create(&model, &mut tx, account_vals("Cash", "asset_cash", "1010"))
        .await
        .unwrap();
    assert_eq!(ids_cell(&account, &mut tx, "company_ids").await, vec![one]);
    let error = write(
        &account,
        &mut tx,
        RawVals::new().set_commands(
            "company_ids",
            vec![Command::link(RecId::from_real(two).unwrap())],
        ),
    )
    .await
    .unwrap_err();
    assert_refused(
        &error,
        "A bank or cash account belongs to one company only.",
    );
    let shared = create(
        &model,
        &mut tx,
        account_vals("Shared", "asset_current", "1020").set_commands(
            "company_ids",
            vec![
                Command::link(RecId::from_real(one).unwrap()),
                Command::link(RecId::from_real(two).unwrap()),
            ],
        ),
    )
    .await
    .unwrap();
    let error = write(
        &shared,
        &mut tx,
        RawVals::new().set_commands("company_ids", vec![Command::clear()]),
    )
    .await
    .unwrap_err();
    assert_refused(
        &error,
        "Each account needs at least one company; these have none",
    );
}

#[tokio::test]
async fn code_is_stored_per_root_company_and_branches_share_it() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let root = company(&mut tx, "Root", None).await;
    let branch = company(&mut tx, "Branch", Some(root)).await;
    let other = company(&mut tx, "Other", None).await;
    // an account shared by two company trees is created by a user working in both
    let both = tx
        .env()
        .with_company(other)
        .unwrap()
        .with_company(root)
        .unwrap()
        .records("account.account")
        .unwrap();
    let account = create(
        &both,
        &mut tx,
        account_vals("Shared", "asset_current", "1111").set_commands(
            "company_ids",
            vec![Command::set([
                RecId::from_real(root).unwrap(),
                RecId::from_real(other).unwrap(),
            ])],
        ),
    )
    .await
    .unwrap();
    // a second company tree gives the same account its own code
    write(
        &accounts(&tx, other).browse([account.ids()[0]]).unwrap(),
        &mut tx,
        RawVals::new().set_cell("code", text("2222")),
    )
    .await
    .unwrap();
    let in_root = accounts(&tx, root).browse([account.ids()[0]]).unwrap();
    let in_other = accounts(&tx, other).browse([account.ids()[0]]).unwrap();
    assert_eq!(
        str_cell(&in_root, &mut tx, "code").await.as_deref(),
        Some("1111")
    );
    assert_eq!(
        str_cell(&in_other, &mut tx, "code").await.as_deref(),
        Some("2222")
    );
    // the branch reads the root's code
    let in_branch = accounts(&tx, branch).browse([account.ids()[0]]).unwrap();
    assert_eq!(
        str_cell(&in_branch, &mut tx, "code").await.as_deref(),
        Some("1111")
    );
    // and a code written from the branch changes it for the whole tree
    write(
        &in_branch,
        &mut tx,
        RawVals::new().set_cell("code", text("180002")),
    )
    .await
    .unwrap();
    assert_eq!(
        str_cell(&in_root, &mut tx, "code").await.as_deref(),
        Some("180002")
    );
    assert_eq!(
        str_cell(&in_other, &mut tx, "code").await.as_deref(),
        Some("2222")
    );
    let rows = tx
        .query(
            "SELECT count(*)::int8 FROM account_account_code WHERE account_id = $1",
            &[&account.ids()[0]],
        )
        .await
        .unwrap();
    assert_eq!(
        rows[0].get::<_, i64>(0),
        2,
        "one code row per root company, none for the branch"
    );
}

#[tokio::test]
async fn a_code_is_unique_within_a_company_tree_not_across_trees() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let root = company(&mut tx, "Root", None).await;
    let branch = company(&mut tx, "Branch", Some(root)).await;
    let sibling = company(&mut tx, "Sibling", None).await;
    create(
        &accounts(&tx, root),
        &mut tx,
        account_vals("A", "asset_current", "100000"),
    )
    .await
    .unwrap();
    let error = create(
        &accounts(&tx, branch),
        &mut tx,
        account_vals("B", "asset_current", "100000"),
    )
    .await
    .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Validation, "{error:?}");
    assert!(
        error
            .message()
            .contains("Another account of this company tree already uses this code."),
        "{:?}",
        error.message()
    );
    create(
        &accounts(&tx, sibling),
        &mut tx,
        account_vals("C", "asset_current", "100000"),
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn code_characters_are_letters_digits_dots_and_dashes() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let root = company(&mut tx, "Root", None).await;
    let model = accounts(&tx, root);
    for good in ["1100", "AB-12.3", "x"] {
        create(&model, &mut tx, account_vals("A", "asset_current", good))
            .await
            .unwrap();
    }
    for bad in ["10 20", "10/20", "caf\u{e9}"] {
        let error = create(&model, &mut tx, account_vals("A", "asset_current", bad))
            .await
            .unwrap_err();
        assert_refused(
            &error,
            &format!("Account code {bad} may use only letters, digits, dots and dashes."),
        );
    }
}

#[tokio::test]
async fn root_is_the_first_two_characters_of_the_code() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let root = company(&mut tx, "Root", None).await;
    let model = accounts(&tx, root);
    let account = create(
        &model,
        &mut tx,
        account_vals("A", "asset_current", "101200"),
    )
    .await
    .unwrap();
    let node = match account.cell(&mut tx, "root_id").await.unwrap() {
        Cell::Id(id) => tx
            .env()
            .records("account.root")
            .unwrap()
            .browse([id])
            .unwrap(),
        other => panic!("root_id is {other:?}"),
    };
    assert_eq!(
        str_cell(&node, &mut tx, "name").await.as_deref(),
        Some("10")
    );
    let parent = match node.cell(&mut tx, "parent_id").await.unwrap() {
        Cell::Id(id) => tx
            .env()
            .records("account.root")
            .unwrap()
            .browse([id])
            .unwrap(),
        other => panic!("parent_id is {other:?}"),
    };
    assert_eq!(
        str_cell(&parent, &mut tx, "name").await.as_deref(),
        Some("1")
    );
}

#[tokio::test]
async fn tag_name_is_unique_per_country_and_applicability() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let tags = tx.env().records("account.account.tag").unwrap();
    let vals = || {
        RawVals::new()
            .set_cell("name", text("Operating"))
            .set_cell("applicability", text("accounts"))
    };
    create(&tags, &mut tx, vals()).await.unwrap();
    let error = create(&tags, &mut tx, vals()).await.unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Validation, "{error:?}");
    assert_eq!(error.constraint(), Some("account_account_tag_name_uniq"));
    // same name, other applicability: allowed
    create(
        &tags,
        &mut tx,
        vals().set_cell("applicability", text("taxes")),
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn tag_applicability_is_not_enforced_on_accounts() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let root = company(&mut tx, "Root", None).await;
    let tag = create(
        &tx.env().records("account.account.tag").unwrap(),
        &mut tx,
        RawVals::new()
            .set_cell("name", text("Tax grid"))
            .set_cell("applicability", text("taxes")),
    )
    .await
    .unwrap();
    let account = create(
        &accounts(&tx, root),
        &mut tx,
        account_vals("A", "asset_current", "1500").set_commands(
            "tag_ids",
            vec![Command::link(RecId::from_real(tag.ids()[0]).unwrap())],
        ),
    )
    .await
    .unwrap();
    assert_eq!(ids_cell(&account, &mut tx, "tag_ids").await, tag.ids());
}

/// Wait until some session of the test database waits for a lock.
async fn until_a_session_waits(h: &Harness) {
    let client = h.db().client().await.unwrap();
    let started = std::time::Instant::now();
    loop {
        let waiting: i64 = client
            .query_one(
                "SELECT count(*) FROM pg_stat_activity \
                 WHERE datname = current_database() AND wait_event_type = 'Lock'",
                &[],
            )
            .await
            .unwrap()
            .get(0);
        if waiting > 0 {
            return;
        }
        assert!(
            started.elapsed() < std::time::Duration::from_secs(20),
            "no session waited for a lock"
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
}

/// Two transactions that first use the same code prefix both succeed.
#[tokio::test]
async fn two_transactions_first_using_one_prefix_both_store_their_code() {
    let h = harness().await;
    let mut setup = h.begin().await.unwrap();
    let root = company(&mut setup, "Root", None).await;
    setup.commit().await.unwrap();

    let mut a = h.begin().await.unwrap();
    create(
        &accounts(&a, root),
        &mut a,
        account_vals("A", "asset_current", "7001"),
    )
    .await
    .unwrap();
    // the second transaction meets the first one's uncommitted root rows and waits for them
    let second = h.run(async |tx| {
        create(
            &accounts(tx, root),
            tx,
            account_vals("B", "asset_current", "7002"),
        )
        .await
        .map(|created| created.ids())
    });
    let release = async {
        until_a_session_waits(&h).await;
        a.commit().await.unwrap();
    };
    let (second, ()) = tokio::join!(second, release);
    let second = second.unwrap_or_else(|e| panic!("the second transaction: {e:?}"));
    assert_eq!(second.len(), 1);
    let mut tx = h.begin().await.unwrap();
    let roots: Vec<String> = tx
        .query("SELECT name FROM account_root ORDER BY name", &[])
        .await
        .unwrap()
        .iter()
        .map(|r| r.get(0))
        .collect();
    assert_eq!(roots, ["7", "70"], "each root exists once");
    let codes: i64 = tx
        .query("SELECT count(*)::int8 FROM account_account_code", &[])
        .await
        .unwrap()[0]
        .get(0);
    assert_eq!(codes, 2);
    tx.rollback().await.unwrap();
}

/// The code of an account that is not in the active company's tree.
#[tokio::test]
async fn a_code_given_at_create_goes_to_the_tree_of_the_accounts_company() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let active = company(&mut tx, "Active", None).await;
    let owner = company(&mut tx, "Owner", None).await;
    // the user works in `active` and creates an account of `owner` only
    let account = create(
        &accounts(&tx, active),
        &mut tx,
        account_vals("Foreign", "asset_current", "4444").set_commands(
            "company_ids",
            vec![Command::set([RecId::from_real(owner).unwrap()])],
        ),
    )
    .await
    .unwrap();
    let id = account.ids()[0];
    let in_owner = accounts(&tx, owner).browse([id]).unwrap();
    assert_eq!(
        str_cell(&in_owner, &mut tx, "code").await.as_deref(),
        Some("4444"),
        "readable in the account's own company"
    );
    let in_active: i64 = tx
        .query(
            "SELECT count(*)::int8 FROM account_account_code WHERE company_id = $1",
            &[&active],
        )
        .await
        .unwrap()[0]
        .get(0);
    assert_eq!(in_active, 0, "the active company's code space is not used");
    // so the same code is free for an account of the active company
    create(
        &accounts(&tx, active),
        &mut tx,
        account_vals("Mine", "asset_current", "4444"),
    )
    .await
    .unwrap();
    // an active company that is one of the account's companies still comes first
    let shared = create(
        &accounts(&tx, active),
        &mut tx,
        account_vals("Shared", "asset_current", "5555").set_commands(
            "company_ids",
            vec![Command::set([
                RecId::from_real(owner).unwrap(),
                RecId::from_real(active).unwrap(),
            ])],
        ),
    )
    .await
    .unwrap();
    let in_active = accounts(&tx, active).browse([shared.ids()[0]]).unwrap();
    assert_eq!(
        str_cell(&in_active, &mut tx, "code").await.as_deref(),
        Some("5555")
    );
    tx.rollback().await.unwrap();
}

/// A code value that is not text is refused, not dropped.
#[tokio::test]
async fn a_code_that_is_not_text_is_refused_at_create() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let root = company(&mut tx, "Root", None).await;
    let model = accounts(&tx, root);
    for cell in [Cell::Int(5), Cell::Bool(true)] {
        let vals = RawVals::new()
            .set_cell("name", text("Odd"))
            .set_cell("account_type", text("asset_current"))
            .set_cell("code", cell);
        let error = create(&model, &mut tx, vals).await.unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Validation, "{error:?}");
        assert!(error.message().contains("code"), "{:?}", error.message());
    }
    for vals in [
        RawVals::new()
            .set_cell("name", text("Null"))
            .set_cell("account_type", text("asset_current"))
            .set_cell("code", Cell::Null),
        RawVals::new()
            .set_cell("name", text("Absent"))
            .set_cell("account_type", text("asset_current")),
    ] {
        let account = create(&model, &mut tx, vals).await.unwrap();
        assert_eq!(str_cell(&account, &mut tx, "code").await, None);
    }
    tx.rollback().await.unwrap();
}

/// A mapping row keeps its account and company.
#[tokio::test]
async fn a_mapping_rows_account_and_company_cannot_be_rewritten() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let root = company(&mut tx, "Root", None).await;
    let other = company(&mut tx, "Other", None).await;
    let first = create(
        &accounts(&tx, root),
        &mut tx,
        account_vals("A", "asset_current", "6001"),
    )
    .await
    .unwrap();
    let second = create(
        &accounts(&tx, root),
        &mut tx,
        // no code: the database's own key could not refuse the re-pointing
        RawVals::new()
            .set_cell("name", text("B"))
            .set_cell("account_type", text("asset_current")),
    )
    .await
    .unwrap();
    let mappings = tx.env().records("account.code.mapping").unwrap();
    let row = mappings
        .search(
            &mut tx,
            &Domain::condition("account_id", Operator::Eq, first.ids()[0]).unwrap(),
        )
        .await
        .unwrap();
    for vals in [
        RawVals::new().set_cell("account_id", Cell::Id(second.ids()[0])),
        RawVals::new().set_cell("company_id", Cell::Id(other)),
    ] {
        let error = write(&row, &mut tx, vals).await.unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Validation, "{error:?}");
        assert!(
            error.message().contains("cannot be changed"),
            "{:?}",
            error.message()
        );
    }
    // the code itself is still written
    write(&row, &mut tx, RawVals::new().set_cell("code", text("6009")))
        .await
        .unwrap();
    tx.rollback().await.unwrap();
}

/// No active company.
#[tokio::test]
async fn reading_the_code_without_an_active_company_is_refused() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let root = company(&mut tx, "Root", None).await;
    let account = create(
        &accounts(&tx, root),
        &mut tx,
        account_vals("A", "asset_current", "8001"),
    )
    .await
    .unwrap();
    let bare = tx
        .env()
        .records("account.account")
        .unwrap()
        .browse([account.ids()[0]])
        .unwrap();
    let error = bare.cell(&mut tx, "code").await.unwrap_err();
    assert_refused(&error, "select an active company first");
    tx.rollback().await.unwrap();
}

/// The "Codes by company" list adds a row with a company and a code: the company of a mapping row
/// is set at create, so the web client must be able to send it (it leaves readonly fields out).
#[tokio::test]
async fn a_code_row_added_in_the_account_form_is_stored_and_its_company_is_not_readonly() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let root = company(&mut tx, "Root", None).await;
    let other = company(&mut tx, "Other", None).await;
    let rid = |id: i64| RecId::from_real(id).unwrap();
    let account = create(
        &accounts(&tx, root),
        &mut tx,
        account_vals("Form", "asset_current", "3030")
            .set_commands("company_ids", vec![Command::set([rid(root), rid(other)])]),
    )
    .await
    .unwrap();
    // the new row of the "Codes by company" list, as the form sends it
    write(
        &account,
        &mut tx,
        RawVals::new().set_commands(
            "code_mapping_ids",
            vec![Command::create(
                RawVals::new()
                    .set_cell("company_id", Cell::Id(other))
                    .set_cell("code", text("3031")),
            )],
        ),
    )
    .await
    .unwrap();
    let in_other = accounts(&tx, other).browse([account.ids()[0]]).unwrap();
    assert_eq!(
        str_cell(&in_other, &mut tx, "code").await.as_deref(),
        Some("3031")
    );
    let described = tx
        .env()
        .records("account.code.mapping")
        .unwrap()
        .fields_get(
            &mut tx,
            Some(&["account_id", "company_id"]),
            Some(&["readonly"]),
        )
        .await
        .unwrap();
    let readonly = |name: &str| described[name]["readonly"].clone();
    assert_eq!(readonly("account_id"), Value::Bool(true));
    assert_eq!(readonly("company_id"), Value::Bool(false));
    tx.rollback().await.unwrap();
}

/// A write that names the stored account or company changes nothing and passes (the data loader
/// hands a record's full values on).
#[tokio::test]
async fn a_mapping_write_naming_the_stored_account_and_company_passes() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let root = company(&mut tx, "Root", None).await;
    let account = create(
        &accounts(&tx, root),
        &mut tx,
        account_vals("A", "asset_current", "6101"),
    )
    .await
    .unwrap();
    let row = tx
        .env()
        .records("account.code.mapping")
        .unwrap()
        .search(
            &mut tx,
            &Domain::condition("account_id", Operator::Eq, account.ids()[0]).unwrap(),
        )
        .await
        .unwrap();
    write(
        &row,
        &mut tx,
        RawVals::new()
            .set_cell("account_id", Cell::Id(account.ids()[0]))
            .set_cell("company_id", Cell::Id(root))
            .set_cell("code", text("6102")),
    )
    .await
    .unwrap();
    tx.rollback().await.unwrap();
}

/// A user outside superuser mode, working in `active`: a transaction of user 2.
async fn user_tx(h: &Harness) -> Transaction {
    let mut setup = h.begin().await.unwrap();
    setup
        .batch("INSERT INTO res_users (id, name, login, active, share) VALUES (2, 'Clerk', 'clerk', true, false)")
        .await
        .unwrap();
    setup.commit().await.unwrap();
    h.begin_as(2).await.unwrap()
}

/// The fallback root of a code given at create is a tree of the caller's own companies.
#[tokio::test]
async fn a_caller_stores_a_code_only_in_a_tree_of_its_own_companies() {
    let h = harness().await;
    let mut setup = h.begin().await.unwrap();
    let a = company(&mut setup, "A", None).await;
    let b = company(&mut setup, "B", None).await;
    let c = company(&mut setup, "C", None).await;
    setup.commit().await.unwrap();
    let mut tx = user_tx(&h).await;
    let rid = |id: i64| RecId::from_real(id).unwrap();
    let ids = |list: &[i64]| Value::list(list.iter().map(|i| Value::Int(*i)));
    let env = |tx: &Transaction, list: &[i64]| {
        tx.env().with_context([("allowed_company_ids", ids(list))])
    };
    // the user works in A and C; the account is of B and C: the code goes to C, not to the
    // lower-id B
    let model = env(&tx, &[a, c]).records("account.account").unwrap();
    let account = create(
        &model,
        &mut tx,
        account_vals("Both", "asset_current", "2020")
            .set_commands("company_ids", vec![Command::set([rid(b), rid(c)])]),
    )
    .await
    .unwrap();
    let rows = tx
        .query(
            "SELECT company_id FROM account_account_code WHERE account_id = $1",
            &[&account.ids()[0]],
        )
        .await
        .unwrap();
    let stored: Vec<i64> = rows.iter().map(|r| r.get(0)).collect();
    assert_eq!(stored, [c], "B's code space is untouched");
    // the user works in A only and the account is of B only: refused by name
    let model = env(&tx, &[a]).records("account.account").unwrap();
    let error = create(
        &model,
        &mut tx,
        account_vals("Foreign", "asset_current", "2021")
            .set_commands("company_ids", vec![Command::set([rid(b)])]),
    )
    .await
    .unwrap_err();
    assert_refused(&error, "belongs to no company you are working in");
    tx.rollback().await.unwrap();
}

fn code_row(company: i64, code: &str) -> Command {
    Command::create(
        RawVals::new()
            .set_cell("company_id", Cell::Id(company))
            .set_cell("code", text(code)),
    )
}

fn company_set(companies: &[i64]) -> Command {
    Command::set(companies.iter().map(|c| RecId::from_real(*c).unwrap()))
}

async fn code_rows(tx: &mut Transaction, account: i64) -> Vec<(i64, String)> {
    tx.query(
        "SELECT company_id::int8, code FROM account_account_code WHERE account_id = $1 ORDER BY company_id",
        &[&account],
    )
    .await
    .unwrap()
    .iter()
    .map(|row| (row.get(0), row.get(1)))
    .collect()
}

#[tokio::test]
async fn codes_by_company_given_while_creating_the_account() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let root = company(&mut tx, "Root", None).await;
    let other = company(&mut tx, "Other", None).await;
    let stranger = company(&mut tx, "Stranger", None).await;
    let both = tx
        .env()
        .with_company(other)
        .unwrap()
        .with_company(root)
        .unwrap()
        .records("account.account")
        .unwrap();
    // the form's save: the account without a `code` value and one create command per row
    let vals = |name: &str, rows: Vec<Command>| {
        RawVals::new()
            .set_cell("name", text(name))
            .set_cell("account_type", text("asset_current"))
            .set_commands("company_ids", vec![company_set(&[root, other])])
            .set_commands("code_mapping_ids", rows)
    };
    // (a) the active company, (b) the other company of the account
    let account = create(
        &both,
        &mut tx,
        vals(
            "Two codes",
            vec![code_row(root, "1010"), code_row(other, "2020")],
        ),
    )
    .await
    .unwrap();
    assert_eq!(
        code_rows(&mut tx, account.ids()[0]).await,
        vec![(root, "1010".to_owned()), (other, "2020".to_owned())]
    );
    // (c) a company the account does not belong to
    let error = create(
        &both,
        &mut tx,
        vals("Foreign", vec![code_row(stranger, "3030")]),
    )
    .await
    .unwrap_err();
    assert!(
        error
            .message()
            .contains("link records of different companies")
            || error.kind() == ErrorKind::Access,
        "{error:?}"
    );
    // (d) the access answer for a caller who does not hold the company needs a non-superuser
    // fixture (the harness loads no groups or access rows): not covered here. What is covered:
    // an account of the active company alone cannot carry a row of another company.
    let error = create(
        &accounts(&tx, root),
        &mut tx,
        vals("Unheld", vec![code_row(other, "4040")])
            .set_commands("company_ids", vec![company_set(&[root])]),
    )
    .await
    .unwrap_err();
    assert!(
        error
            .message()
            .contains("link records of different companies"),
        "{error:?}"
    );
    // the plain `code` and a row for the same root in one call: the explicit row wins
    let account = create(
        &both,
        &mut tx,
        vals("Both", vec![code_row(root, "5050")]).set_cell("code", text("6060")),
    )
    .await
    .unwrap();
    assert_eq!(
        code_rows(&mut tx, account.ids()[0]).await,
        vec![(root, "5050".to_owned())]
    );
}
