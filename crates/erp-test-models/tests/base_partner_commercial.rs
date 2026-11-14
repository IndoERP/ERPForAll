// SPDX-License-Identifier: MIT OR Apache-2.0
//! Commercial field synchronization acceptance (commercial amendment).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]
use erp_addon_base::ResPartner;
use erp_sdk::prelude::*;
use erp_test_models::base_users;
use erp_testkit::Harness;

erp_sdk::addon!(name = "test_partner_commercial", depends = ["base"]);

#[extend(ResPartner, addon = "test_partner_commercial")]
pub struct CommercialExtension;

fn flag(this: &RecordSet<ResPartner>, key: &str) -> bool {
    this.env().context().get(key) == Some(Value::Bool(true))
}

#[methods(ResPartner, addon = "test_partner_commercial")]
impl CommercialExtension {
    #[method(override)]
    async fn _synced_commercial_fields(
        this: &RecordSet<ResPartner>,
        tx: &mut Transaction,
        sup: Super,
    ) -> Result<Vec<String>> {
        let mut fields: Vec<String> = sup.call(tx, this.as_records(), ()).await?;
        if flag(this, "commercial_sync_ref") {
            fields.push("ref".to_owned());
        }
        Ok(fields)
    }

    #[method(override)]
    async fn _commercial_fields(
        this: &RecordSet<ResPartner>,
        tx: &mut Transaction,
        sup: Super,
    ) -> Result<Vec<String>> {
        let mut fields: Vec<String> = sup.call(tx, this.as_records(), ()).await?;
        if flag(this, "commercial_tags") {
            fields.push("category_id".to_owned());
        }
        if flag(this, "commercial_children") {
            fields.push("child_ids".to_owned());
        }
        Ok(fields)
    }
}

async fn harness() -> Harness {
    Harness::builder([&base_users::ADDON, &erp_addon_base::ADDON, &ADDON])
        .build()
        .await
        .unwrap()
}

fn partners(tx: &Transaction) -> Records {
    tx.env().records("res.partner").unwrap()
}

async fn industry(tx: &mut Transaction, name: &str) -> i64 {
    tx.env()
        .records("res.partner.industry")
        .unwrap()
        .create(
            tx,
            vec![RawVals::new().set_cell("name", Cell::Str(name.into()))],
        )
        .await
        .unwrap()
        .ids()[0]
}

fn named(name: &str) -> RawVals {
    RawVals::new().set_cell("name", Cell::Str(name.into()))
}

fn commercial(values: RawVals, vat: &str, industry: i64) -> RawVals {
    values
        .set_cell("vat", Cell::Str(vat.into()))
        .set_cell("industry_id", Cell::Id(industry))
}

fn child_of(values: RawVals, parent: &Records) -> RawVals {
    values.set_cell("parent_id", Cell::Id(parent.ids()[0]))
}

async fn create(model: &Records, tx: &mut Transaction, values: RawVals) -> Records {
    model.create(tx, vec![values]).await.unwrap()
}

fn text(value: Option<&str>) -> Cell {
    value.map_or(Cell::Null, |value| Cell::Str(value.into()))
}

fn id(value: Option<i64>) -> Cell {
    value.map_or(Cell::Null, Cell::Id)
}

/// Assert vat and industry on every record, naming the record on failure.
async fn assert_commercial(
    tx: &mut Transaction,
    records: &[&Records],
    vat: Option<&str>,
    industry: Option<i64>,
) {
    for record in records {
        let name = record.value(tx, "name").await.unwrap();
        assert_eq!(
            record.cells(tx, "vat").await.unwrap(),
            vec![text(vat)],
            "vat of {name:?}"
        );
        assert_eq!(
            record.cells(tx, "industry_id").await.unwrap(),
            vec![id(industry)],
            "industry_id of {name:?}"
        );
    }
}

async fn assert_text(tx: &mut Transaction, record: &Records, field: &str, expected: Option<&str>) {
    let name = record.value(tx, "name").await.unwrap();
    assert_eq!(
        record.cells(tx, field).await.unwrap(),
        vec![text(expected)],
        "{field} of {name:?}"
    );
}

#[tokio::test]
async fn commercial_fields_follow_creation_reparenting_and_upstream_writes() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let model = partners(&tx);
    let i1 = industry(&mut tx, "Industry 1").await;
    let i2 = industry(&mut tx, "Industry 2").await;
    let company_1 = create(&model, &mut tx, commercial(named("company_1"), "V1", i1)).await;
    let company_2 = create(&model, &mut tx, commercial(named("company_2"), "V2", i1)).await;

    // 027: creation under company_1, whatever the type.
    let contact = create(&model, &mut tx, child_of(named("Contact"), &company_1)).await;
    let delivery = create(
        &model,
        &mut tx,
        child_of(named("Delivery"), &contact).set_cell("type", Cell::Str("delivery".into())),
    )
    .await;
    let grand = create(&model, &mut tx, child_of(named("Grand contact"), &contact)).await;
    assert_commercial(
        &mut tx,
        &[&contact, &delivery, &grand],
        Some("V1"),
        Some(i1),
    )
    .await;

    // 027: reparenting the first contact moves its whole subtree to company_2's values.
    contact
        .write(
            &mut tx,
            RawVals::new().set_cell("parent_id", Cell::Id(company_2.ids()[0])),
        )
        .await
        .unwrap();
    assert_commercial(
        &mut tx,
        &[&contact, &delivery, &grand],
        Some("V2"),
        Some(i1),
    )
    .await;
    assert_commercial(&mut tx, &[&company_1], Some("V1"), Some(i1)).await;

    // 027: a child added through a one2many create command. Only the res.partner
    // create override synchronizes a new child; company_2's own write changes no
    // commercial field, so the values below prove the nested create was dispatched.
    company_2
        .write(
            &mut tx,
            RawVals::new().set_commands("child_ids", [Command::create(named("Added"))]),
        )
        .await
        .unwrap();
    let children = company_2.related(&mut tx, "child_ids").await.unwrap();
    let added_ids: Vec<i64> = children
        .ids()
        .into_iter()
        .filter(|child| *child != contact.ids()[0])
        .collect();
    assert_eq!(added_ids.len(), 1);
    let added = model.browse(added_ids).unwrap();
    assert_eq!(
        added.cells(&mut tx, "parent_id").await.unwrap(),
        vec![Cell::Id(company_2.ids()[0])]
    );
    assert_commercial(&mut tx, &[&added], Some("V2"), Some(i1)).await;

    // 028: a commercial partner write reaches every descendant.
    company_2
        .write(
            &mut tx,
            RawVals::new()
                .set_cell("industry_id", Cell::Id(i2))
                .set_cell("vat", Cell::Str("BE0477472701".into())),
        )
        .await
        .unwrap();
    assert_commercial(
        &mut tx,
        &[&contact, &delivery, &grand, &added],
        Some("BE0477472701"),
        Some(i2),
    )
    .await;

    // 029: a synced field written on a contact flows up and back down.
    contact
        .write(
            &mut tx,
            RawVals::new().set_cell("vat", Cell::Str("BE445566".into())),
        )
        .await
        .unwrap();
    assert_commercial(
        &mut tx,
        &[&company_2, &contact, &delivery, &grand, &added],
        Some("BE445566"),
        Some(i2),
    )
    .await;
    assert_commercial(&mut tx, &[&company_1], Some("V1"), Some(i1)).await;
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn reparented_individual_fills_company_and_receives_extended_synced_fields() {
    let h = harness().await;
    for extended in [true, false] {
        let mut tx = h.begin().await.unwrap();
        let model = partners(&tx).with_context([("commercial_sync_ref", Value::Bool(extended))]);
        let i1 = industry(&mut tx, "Individual industry").await;
        let individual = create(
            &model,
            &mut tx,
            commercial(named("Individual"), "BE0477472701", i1)
                .set_cell("ref", Cell::Str("REFINDIVIDUAL".into()))
                .set_cell("street", Cell::Str("Rue des Bourlottes 9".into()))
                .set_cell("street2", Cell::Str("Building A".into()))
                .set_cell("zip", Cell::Str("1367".into()))
                .set_cell("city", Cell::Str("Ramillies".into())),
        )
        .await;
        let grand = create(
            &model,
            &mut tx,
            child_of(named("Individual child"), &individual),
        )
        .await;
        let company = create(
            &model,
            &mut tx,
            named("Company").set_cell("ref", Cell::Str("COMPANYREF".into())),
        )
        .await;
        individual
            .write(
                &mut tx,
                RawVals::new().set_cell("parent_id", Cell::Id(company.ids()[0])),
            )
            .await
            .unwrap();
        assert_text(&mut tx, &individual, "type", Some("contact")).await;
        // Up: vat and address, never the industry; the company keeps its reference.
        assert_commercial(&mut tx, &[&company], Some("BE0477472701"), None).await;
        assert_text(&mut tx, &company, "ref", Some("COMPANYREF")).await;
        for (field, value) in [
            ("street", "Rue des Bourlottes 9"),
            ("street2", "Building A"),
            ("zip", "1367"),
            ("city", "Ramillies"),
        ] {
            assert_text(&mut tx, &company, field, Some(value)).await;
            assert_text(&mut tx, &individual, field, Some(value)).await;
        }
        // The void company industry does not reset the individual itself (set values only).
        assert_commercial(&mut tx, &[&individual], Some("BE0477472701"), Some(i1)).await;
        let expected_ref = if extended {
            "COMPANYREF"
        } else {
            "REFINDIVIDUAL"
        };
        assert_text(&mut tx, &individual, "ref", Some(expected_ref)).await;
        // The vat
        // then comes back down from the company it was sent up to.
        let grand_industry = if extended { None } else { Some(i1) };
        assert_commercial(&mut tx, &[&grand], Some("BE0477472701"), grand_industry).await;
        // `ref` is commercial only with the extension; the child never had its own.
        let grand_ref = if extended { Some("COMPANYREF") } else { None };
        assert_text(&mut tx, &grand, "ref", grand_ref).await;
        drop(tx);
    }
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn voids_flow_down_from_the_commercial_partner_but_stay_local_on_a_child() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let model = partners(&tx);
    let i2 = industry(&mut tx, "Industry 2").await;
    let company = create(
        &model,
        &mut tx,
        commercial(named("Company"), "BE0477472701", i2),
    )
    .await;
    let individual = create(&model, &mut tx, named("Individual")).await;
    individual
        .write(
            &mut tx,
            RawVals::new().set_cell("parent_id", Cell::Id(company.ids()[0])),
        )
        .await
        .unwrap();
    assert_commercial(&mut tx, &[&individual], Some("BE0477472701"), Some(i2)).await;

    company
        .write(
            &mut tx,
            RawVals::new()
                .set_cell("industry_id", Cell::Null)
                .set_cell("vat", Cell::Null),
        )
        .await
        .unwrap();
    assert_commercial(&mut tx, &[&company, &individual], None, None).await;

    company
        .write(
            &mut tx,
            RawVals::new()
                .set_cell("industry_id", Cell::Id(i2))
                .set_cell("vat", Cell::Str("BE0477472701".into())),
        )
        .await
        .unwrap();
    assert_commercial(
        &mut tx,
        &[&company, &individual],
        Some("BE0477472701"),
        Some(i2),
    )
    .await;

    individual
        .write(
            &mut tx,
            RawVals::new()
                .set_cell("industry_id", Cell::Null)
                .set_cell("vat", Cell::Null),
        )
        .await
        .unwrap();
    assert_commercial(&mut tx, &[&individual], None, None).await;
    assert_commercial(&mut tx, &[&company], Some("BE0477472701"), Some(i2)).await;
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn one2many_commercial_field_is_refused_and_many2many_is_synchronized() {
    let h = harness().await;
    // many2many accepted, in its own transaction: the refusal below poisons its transaction.
    let mut tx = h.begin().await.unwrap();
    let tags = tx.env().records("res.partner.category").unwrap();
    let tag_ids: Vec<i64> = tags
        .create(&mut tx, vec![named("Tag A"), named("Tag B")])
        .await
        .unwrap()
        .ids();
    let model = partners(&tx).with_context([("commercial_tags", Value::Bool(true))]);
    let parent = create(
        &model,
        &mut tx,
        named("Tagged company").set_commands(
            "category_id",
            [Command::set(
                tag_ids.iter().map(|tag| RecId::from_real(*tag).unwrap()),
            )],
        ),
    )
    .await;
    let child = create(&model, &mut tx, child_of(named("Tagged child"), &parent)).await;
    let grand = create(
        &model,
        &mut tx,
        child_of(named("Tagged grandchild"), &child),
    )
    .await;
    let parent_tags = parent.cells(&mut tx, "category_id").await.unwrap();
    assert_eq!(
        parent_tags,
        vec![Cell::Ids(
            tag_ids
                .iter()
                .map(|tag| RecId::from_real(*tag).unwrap())
                .collect()
        )]
    );
    assert_eq!(
        child.cells(&mut tx, "category_id").await.unwrap(),
        parent_tags
    );
    assert_eq!(
        grand.cells(&mut tx, "category_id").await.unwrap(),
        parent_tags
    );
    // A commercial partner write of the many2many reaches every descendant, including empty.
    parent
        .write(
            &mut tx,
            RawVals::new().set_commands("category_id", [Command::set([])]),
        )
        .await
        .unwrap();
    for record in [&child, &grand] {
        assert_eq!(
            record.cells(&mut tx, "category_id").await.unwrap(),
            vec![Cell::Ids(Arc::from(Vec::<RecId>::new()))]
        );
    }
    drop(tx);

    let mut tx = h.begin().await.unwrap();
    let parent = create(&partners(&tx), &mut tx, named("Parent")).await;
    let error = partners(&tx)
        .with_context([("commercial_children", Value::Bool(true))])
        .create(&mut tx, vec![child_of(named("Refused child"), &parent)])
        .await
        .unwrap_err();
    assert_eq!(error.kind(), erp_sdk::ErrorKind::Programming);
    assert_eq!(
        error.message(),
        "One2Many fields cannot be synchronized as part of `commercial_fields` or `address fields`"
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn company_children_and_their_subtrees_keep_their_commercial_values() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let model = partners(&tx);
    let i1 = industry(&mut tx, "Industry 1").await;
    let i2 = industry(&mut tx, "Industry 2").await;
    let root = create(&model, &mut tx, commercial(named("Group"), "V1", i1)).await;
    let subsidiary = create(&model, &mut tx, child_of(named("Subsidiary"), &root)).await;
    let inner = create(
        &model,
        &mut tx,
        child_of(named("Subsidiary contact"), &subsidiary),
    )
    .await;
    let sibling = create(&model, &mut tx, child_of(named("Group contact"), &root)).await;
    assert_commercial(
        &mut tx,
        &[&subsidiary, &inner, &sibling],
        Some("V1"),
        Some(i1),
    )
    .await;
    // In base a child is never a company; simulate a
    // localisation-refined stored compute, as the address_get boundary test does.
    tx.flush().await.unwrap();
    tx.execute(
        "UPDATE res_partner SET is_company = true WHERE id = $1",
        &[&subsidiary.ids()[0]],
    )
    .await
    .unwrap();
    tx.invalidate_all().await.unwrap();
    root.write(
        &mut tx,
        RawVals::new()
            .set_cell("vat", Cell::Str("V2".into()))
            .set_cell("industry_id", Cell::Id(i2)),
    )
    .await
    .unwrap();
    assert_commercial(&mut tx, &[&sibling], Some("V2"), Some(i2)).await;
    assert_commercial(&mut tx, &[&subsidiary, &inner], Some("V1"), Some(i1)).await;
    drop(tx);
    h.cleanup().await.unwrap();
}

/// Caller authority, no elevation: a descendant the caller may read but not write
/// fails the commercial partner's write with the ordinary access error.
#[tokio::test]
async fn descendant_sync_runs_with_the_callers_authority() {
    let policy = erp_testkit::DomainPolicy::builder(erp_testkit::Otherwise::Allow)
        .rule(
            "res.partner",
            erp_sdk::Operation::Write,
            Domain::condition("name", Operator::Ne, "Z denied").unwrap(),
        )
        .build()
        .unwrap();
    let h = Harness::builder([&base_users::ADDON, &erp_addon_base::ADDON, &ADDON])
        .policy(policy)
        .build()
        .await
        .unwrap();
    let mut tx = h.begin().await.unwrap();
    tx.batch("INSERT INTO res_users (id, name, login, active, share) VALUES (2, 'Commercial writer', 'commercial-writer', true, false)").await.unwrap();
    let root = create(&partners(&tx), &mut tx, named("Root")).await;
    let denied = create(&partners(&tx), &mut tx, child_of(named("Z denied"), &root)).await;
    tx.flush().await.unwrap();
    let error = root
        .with_user(2)
        .write(
            &mut tx,
            RawVals::new().set_cell("vat", Cell::Str("BE0477472701".into())),
        )
        .await
        .unwrap_err();
    let expected = format!(
        "Your access rights do not let you modify 'Contact' (res.partner) records.\n\nRecords: res.partner({},), User: 2",
        denied.ids()[0]
    );
    assert_eq!(error.kind(), erp_sdk::ErrorKind::Access);
    assert_eq!(error.message(), expected);
    let commit_error = tx.commit().await.unwrap_err();
    assert_eq!(commit_error.kind(), erp_sdk::ErrorKind::Access);
    assert_eq!(commit_error.message(), expected);
    h.cleanup().await.unwrap();
}

/// A harness whose policy hides the partners named `hidden` from reading, for
/// every principal except System, and inserts user 2 to act through it.
async fn harness_hiding(hidden: &str) -> (Harness, Transaction) {
    let policy = erp_testkit::DomainPolicy::builder(erp_testkit::Otherwise::Allow)
        .rule(
            "res.partner",
            erp_sdk::Operation::Read,
            Domain::condition("name", Operator::Ne, hidden).unwrap(),
        )
        .build()
        .unwrap();
    let h = Harness::builder([&base_users::ADDON, &erp_addon_base::ADDON, &ADDON])
        .policy(policy)
        .build()
        .await
        .unwrap();
    let mut tx = h.begin().await.unwrap();
    tx.batch("INSERT INTO res_users (id, name, login, active, share) VALUES (2, 'Commercial reader', 'commercial-reader', true, false)").await.unwrap();
    (h, tx)
}

/// Caller authority, no elevation (proposed commercial amendment item 4):
/// a descendant the caller cannot read is not found by the descendant walk, so it
/// and its whole subtree keep their old values, a readable grandchild included,
/// while the visible descendants receive the new value and the save succeeds.
#[tokio::test]
async fn descendants_hidden_from_the_caller_keep_their_values_with_their_subtree() {
    let (h, mut tx) = harness_hiding("K hidden").await;
    let model = partners(&tx);
    let i1 = industry(&mut tx, "Industry 1").await;
    let root = create(
        &model,
        &mut tx,
        commercial(named("Commercial root"), "V1", i1),
    )
    .await;
    let hidden = create(&model, &mut tx, child_of(named("K hidden"), &root)).await;
    let below_hidden = create(&model, &mut tx, child_of(named("K1 visible"), &hidden)).await;
    let sibling = create(&model, &mut tx, child_of(named("S visible"), &root)).await;
    assert_commercial(
        &mut tx,
        &[&hidden, &below_hidden, &sibling],
        Some("V1"),
        Some(i1),
    )
    .await;
    tx.flush().await.unwrap();
    // As user 2: the hidden child is not found below the root, its child is.
    let reader = model.with_user(2);
    let root_children = reader
        .search(
            &mut tx,
            &Domain::condition("parent_id", Operator::Eq, root.ids()[0]).unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(root_children.ids(), sibling.ids());
    let hidden_children = reader
        .search(
            &mut tx,
            &Domain::condition("parent_id", Operator::Eq, hidden.ids()[0]).unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(hidden_children.ids(), below_hidden.ids());
    root.with_user(2)
        .write(
            &mut tx,
            RawVals::new().set_cell("vat", Cell::Str("V2".into())),
        )
        .await
        .unwrap();
    tx.flush().await.unwrap();
    assert_text(&mut tx, &root, "vat", Some("V2")).await;
    assert_text(&mut tx, &sibling, "vat", Some("V2")).await;
    assert_text(&mut tx, &hidden, "vat", Some("V1")).await;
    assert_text(&mut tx, &below_hidden, "vat", Some("V1")).await;
    tx.commit().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn creating_under_an_unreadable_commercial_partner_is_refused() {
    let (h, mut tx) = harness_hiding("Top hidden").await;
    let model = partners(&tx);
    let i1 = industry(&mut tx, "Industry 1").await;
    let top = create(&model, &mut tx, commercial(named("Top hidden"), "V1", i1)).await;
    let middle = create(&model, &mut tx, child_of(named("Middle visible"), &top)).await;
    tx.flush().await.unwrap();
    let error = model
        .with_user(2)
        .create(&mut tx, vec![child_of(named("New contact"), &middle)])
        .await
        .unwrap_err();
    let expected = format!(
        "Your access rights do not let you access 'Contact' (res.partner) records.\n\nRecords: res.partner({},), User: 2",
        top.ids()[0]
    );
    assert_eq!(error.kind(), erp_sdk::ErrorKind::Access, "{error:?}");
    assert_eq!(error.message(), expected);
    drop(tx);
    h.cleanup().await.unwrap();
}

/// The descendant walk is bounded: a level of more than 4096 non-company children
/// refuses the commercial write as a whole and poisons the transaction.
#[tokio::test]
async fn commercial_write_refuses_more_than_4096_children_in_one_level() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let i1 = industry(&mut tx, "Industry 1").await;
    let root = create(
        &partners(&tx),
        &mut tx,
        commercial(named("Wide root"), "V1", i1),
    )
    .await;
    tx.flush().await.unwrap();
    tx.execute(
        "INSERT INTO res_partner (name, type, active, parent_id, commercial_partner_id, is_company, vat, partner_share) SELECT 'commercial-width-' || n::text, 'contact', true, $1, $1, false, 'V1', false FROM generate_series(1,4097) AS n",
        &[&root.ids()[0]],
    )
    .await
    .unwrap();
    tx.invalidate_all().await.unwrap();
    let error = root
        .write(
            &mut tx,
            RawVals::new().set_cell("vat", Cell::Str("V2".into())),
        )
        .await
        .unwrap_err();
    assert_eq!(error.kind(), erp_sdk::ErrorKind::Validation);
    assert_eq!(
        error.message(),
        "Partner address maximum visited nodes limit of 4096 exceeded."
    );
    let commit_error = tx.commit().await.unwrap_err();
    assert_eq!(commit_error.kind(), error.kind());
    assert_eq!(commit_error.message(), error.message());
    h.cleanup().await.unwrap();
}

/// A 256-edge chain is synchronized to its leaf; a level beyond depth 256 is
/// refused (the test builds it by SQL; reparenting a deep chain can also build it).
#[tokio::test]
async fn commercial_write_reaches_a_256_edge_chain_and_refuses_one_more_level() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let model = partners(&tx);
    let i1 = industry(&mut tx, "Industry 1").await;
    let root = create(&model, &mut tx, commercial(named("Deep root"), "V1", i1)).await;
    tx.flush().await.unwrap();
    tx.execute(
        "WITH source AS MATERIALIZED (SELECT nextval(pg_get_serial_sequence('res_partner', 'id')) AS id, n FROM generate_series(1,256) AS n), links AS (SELECT id, COALESCE(lag(id) OVER (ORDER BY n), $1) AS parent_id, n FROM source) INSERT INTO res_partner (id, name, type, active, parent_id, commercial_partner_id, is_company, vat, partner_share) SELECT id, 'commercial-depth-' || n::text, 'contact', true, parent_id, $1, false, 'V1', false FROM links",
        &[&root.ids()[0]],
    )
    .await
    .unwrap();
    tx.invalidate_all().await.unwrap();
    let leaf_id: i64 = tx
        .query(
            "SELECT id FROM res_partner WHERE name = $1",
            &[&"commercial-depth-256"],
        )
        .await
        .unwrap()[0]
        .get(0);
    let middle_id: i64 = tx
        .query(
            "SELECT id FROM res_partner WHERE name = $1",
            &[&"commercial-depth-128"],
        )
        .await
        .unwrap()[0]
        .get(0);
    let leaf = model.browse([leaf_id]).unwrap();
    let middle = model.browse([middle_id]).unwrap();
    root.write(
        &mut tx,
        RawVals::new().set_cell("vat", Cell::Str("V2".into())),
    )
    .await
    .unwrap();
    assert_text(&mut tx, &middle, "vat", Some("V2")).await;
    assert_text(&mut tx, &leaf, "vat", Some("V2")).await;
    tx.flush().await.unwrap();
    tx.execute(
        "INSERT INTO res_partner (name, type, active, parent_id, commercial_partner_id, is_company, vat, partner_share) VALUES ('commercial-depth-257', 'contact', true, $1, $2, false, 'V2', false)",
        &[&leaf_id, &root.ids()[0]],
    )
    .await
    .unwrap();
    tx.invalidate_all().await.unwrap();
    let error = root
        .write(
            &mut tx,
            RawVals::new().set_cell("vat", Cell::Str("V3".into())),
        )
        .await
        .unwrap_err();
    assert_eq!(error.kind(), erp_sdk::ErrorKind::Validation);
    assert_eq!(
        error.message(),
        "Partner address maximum depth of 256 exceeded."
    );
    let commit_error = tx.commit().await.unwrap_err();
    assert_eq!(commit_error.kind(), error.kind());
    assert_eq!(commit_error.message(), error.message());
    h.cleanup().await.unwrap();
}
