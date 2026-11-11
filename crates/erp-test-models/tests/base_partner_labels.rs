//! Contact labels, email formatting and native lookup acceptance.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

use erp_addon_base::ResPartner;
use erp_orm::{NameSearch, RecordSet, Records, Transaction};
use erp_sdk::{Cell, Domain, ErrorKind, Operator, RawVals, Value};
use erp_test_models::base_users;
use erp_testkit::Harness;

type Partner = RecordSet<ResPartner>;

async fn harness() -> Harness {
    Harness::builder([&base_users::ADDON, &erp_addon_base::ADDON])
        .build()
        .await
        .unwrap()
}

fn partners(tx: &Transaction) -> Records {
    tx.env().records("res.partner").unwrap()
}

async fn create(tx: &mut Transaction, values: RawVals) -> Partner {
    partners(tx)
        .create(tx, vec![values])
        .await
        .unwrap()
        .try_into()
        .unwrap()
}

fn named(name: &str) -> RawVals {
    RawVals::new().set_cell("name", Cell::Str(name.into()))
}

async fn child(tx: &mut Transaction, name: Option<&str>, parent: &Partner, kind: &str) -> Partner {
    let mut values = RawVals::new()
        .set_cell("parent_id", Cell::Id(parent.ids()[0]))
        .set_cell("type", Cell::Str(kind.into()));
    if let Some(name) = name {
        values = values.set_cell("name", Cell::Str(name.into()));
    }
    create(tx, values).await
}

async fn assert_complete(tx: &mut Transaction, partner: &Partner, expected: &str) {
    assert_eq!(
        partner
            .get(tx, ResPartner::COMPLETE_NAME)
            .await
            .unwrap()
            .as_deref(),
        Some(expected)
    );
}

async fn assert_display(tx: &mut Transaction, partner: &Partner, expected: &str) {
    assert_eq!(
        partner
            .get(tx, ResPartner::DISPLAY_NAME)
            .await
            .unwrap()
            .as_deref(),
        Some(expected)
    );
}

fn with_flags(partner: &Partner, flags: &[&str]) -> Partner {
    partner.with_context(
        flags
            .iter()
            .map(|flag| ((*flag).to_owned(), Value::Bool(true))),
    )
}

#[tokio::test]
async fn spec_partner_008_email_formatted_normalization_and_invalidation() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let partner = create(
        &mut tx,
        named("Vlad the Impaler").set_cell("email", Cell::Str("vlad@example.com".into())),
    )
    .await;
    assert_eq!(
        partner
            .get(&mut tx, ResPartner::EMAIL_FORMATTED)
            .await
            .unwrap()
            .as_deref(),
        Some("\"Vlad the Impaler\" <vlad@example.com>")
    );
    // These are actual field writes followed by a computed-field read, so each
    // row also checks invalidation of a previously warm formatted-email value.
    for name in [
        "Balázs",
        "Bike@Home",
        "Bike @ Home@Home",
        "Balázs <email.in.name@example.com>",
    ] {
        partner
            .as_records()
            .write(&mut tx, named(name))
            .await
            .unwrap();
        let expected = format!("\"{name}\" <vlad@example.com>");
        assert_eq!(
            partner
                .get(&mut tx, ResPartner::EMAIL_FORMATTED)
                .await
                .unwrap()
                .as_deref(),
            Some(expected.as_str())
        );
    }
    partner
        .as_records()
        .write(&mut tx, named("Balázs"))
        .await
        .unwrap();
    for (email, expected) in [
        (
            Some("Vlad the Impaler <vlad@example.com>"),
            Some("\"Balázs\" <vlad@example.com>"),
        ),
        (
            Some("\"Other Name\" <BALAZS@Example.COM>"),
            Some("\"Balázs\" <balazs@example.com>"),
        ),
        (
            Some("vlad@example.com, dragon@example.com"),
            Some("\"Balázs\" <vlad@example.com,dragon@example.com>"),
        ),
        (
            Some("not-an-address, dragon@example.com"),
            Some("\"Balázs\" <dragon@example.com>"),
        ),
        (
            Some("not-an-address, \"Dragon\" <dragon@example.com>"),
            Some("\"Balázs\" <dragon@example.com>"),
        ),
        (None, None),
        (Some(""), None),
        (Some(" "), Some("\"Balázs\" <@ >")),
        (Some("notanemail"), Some("\"Balázs\" <@notanemail>")),
    ] {
        let value = email.map_or(Cell::Null, |email| Cell::Str(email.into()));
        partner
            .as_records()
            .write(&mut tx, RawVals::new().set_cell("email", value))
            .await
            .unwrap();
        assert_eq!(
            partner
                .get(&mut tx, ResPartner::EMAIL_FORMATTED)
                .await
                .unwrap()
                .as_deref(),
            expected,
            "input {email:?}"
        );
    }
    for source in [
        "Balázs <negotiator@example.com>, vlad@example.com",
        "Balázs <negotiator@example.com>",
    ] {
        let (id, _) = partners(&tx).name_create(&mut tx, source).await.unwrap();
        let created: Partner = partners(&tx).browse([id]).unwrap().try_into().unwrap();
        assert_eq!(
            created
                .get(&mut tx, ResPartner::NAME)
                .await
                .unwrap()
                .as_deref(),
            Some("Balázs")
        );
        assert_eq!(
            created
                .get(&mut tx, ResPartner::EMAIL)
                .await
                .unwrap()
                .as_deref(),
            Some("negotiator@example.com")
        );
        assert_eq!(
            created
                .get(&mut tx, ResPartner::EMAIL_FORMATTED)
                .await
                .unwrap()
                .as_deref(),
            Some("\"Balázs\" <negotiator@example.com>")
        );
    }
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_partner_009_complete_name_uses_commercial_root_and_source_type_labels() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let root = create(&mut tx, named("GhostStep")).await;
    let contact = child(&mut tx, Some("Existing Contact"), &root, "contact").await;
    let grand = child(&mut tx, Some("  Grand  "), &contact, "contact").await;
    assert_complete(&mut tx, &root, "GhostStep").await;
    assert_complete(&mut tx, &contact, "GhostStep, Existing Contact").await;
    assert_complete(&mut tx, &grand, "GhostStep, Grand").await;
    for (kind, label) in [
        ("invoice", "Invoice"),
        ("delivery", "Delivery"),
        ("other", "Other"),
    ] {
        let address = child(&mut tx, None, &root, kind).await;
        assert_complete(&mut tx, &address, &format!("GhostStep, {label}")).await;
        assert_complete(
            &mut tx,
            &with_flags(&address, &["partner_display_name_hide_company"]),
            &format!("GhostStep, {label}"),
        )
        .await;
    }
    let nameless_parent = create(
        &mut tx,
        RawVals::new().set_cell("type", Cell::Str("other".into())),
    )
    .await;
    let nameless_child = child(&mut tx, None, &nameless_parent, "other").await;
    assert_complete(&mut tx, &nameless_child, "Other").await;
    assert_display(&mut tx, &nameless_child, "Other").await;
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_partner_010_rename_and_move_recompute_descendant_labels_and_search() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let root = create(&mut tx, named("GhostStep")).await;
    let other = create(&mut tx, named("Other Co")).await;
    let contact = child(&mut tx, Some("Existing Contact"), &root, "contact").await;
    let grand = child(&mut tx, Some("Grand"), &contact, "contact").await;
    assert_complete(&mut tx, &grand, "GhostStep, Grand").await;
    root.as_records()
        .write(&mut tx, named("Ghost Inc"))
        .await
        .unwrap();
    for (partner, suffix) in [(&contact, "Existing Contact"), (&grand, "Grand")] {
        let expected = format!("Ghost Inc, {suffix}");
        assert_complete(&mut tx, partner, &expected).await;
        let found = partners(&tx)
            .search(
                &mut tx,
                &Domain::condition("complete_name", Operator::Eq, expected).unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(found.ids(), partner.ids());
    }
    contact
        .as_records()
        .write(
            &mut tx,
            RawVals::new().set_cell("parent_id", Cell::Id(other.ids()[0])),
        )
        .await
        .unwrap();
    assert_complete(&mut tx, &contact, "Other Co, Existing Contact").await;
    assert_complete(&mut tx, &grand, "Other Co, Grand").await;
    let ids = (contact.ids()[0], grand.ids()[0]);
    tx.commit().await.unwrap();
    let mut fresh = h.begin().await.unwrap();
    for (id, expected) in [
        (ids.0, "Other Co, Existing Contact"),
        (ids.1, "Other Co, Grand"),
    ] {
        let partner: Partner = partners(&fresh).browse([id]).unwrap().try_into().unwrap();
        assert_complete(&mut fresh, &partner, expected).await;
        assert_eq!(
            partners(&fresh)
                .search(
                    &mut fresh,
                    &Domain::condition("complete_name", Operator::Eq, expected).unwrap()
                )
                .await
                .unwrap()
                .ids(),
            vec![id]
        );
    }
    drop(fresh);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_partner_011_english_display_and_stored_name_language_isolation() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    for (name, code, url) in [("English", "en_US", "en"), ("French", "fr_FR", "fr")] {
        tx.env()
            .records("res.lang")
            .unwrap()
            .create(
                &mut tx,
                vec![
                    RawVals::new()
                        .set_cell("name", Cell::Str(name.into()))
                        .set_cell("code", Cell::Str(code.into()))
                        .set_cell("url_code", Cell::Str(url.into()))
                        .set_cell("active", Cell::Bool(true))
                        .set_cell("direction", Cell::Str("ltr".into())),
                ],
            )
            .await
            .unwrap();
    }
    let root = create(&mut tx, named("Parent")).await;
    let address = child(&mut tx, None, &root, "other").await;
    let english = address.with_context([("lang", Value::str("en_US"))]);
    assert_display(&mut tx, &english, "Parent, Other").await;
    assert_complete(&mut tx, &english, "Parent, Other").await;
    let french = address.with_context([("lang", Value::str("fr_FR"))]);
    assert_complete(&mut tx, &french, "Parent, Other").await;
    assert_display(&mut tx, &english, "Parent, Other").await;
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_partner_012_context_flags_order_whitespace_and_invalidation() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let jethala = create(
        &mut tx,
        named("Jethala")
            .set_cell("street", Cell::Str("Powder gali".into()))
            .set_cell("street2", Cell::Str("Gokuldham Society".into())),
    )
    .await;
    let bhide = create(&mut tx, named("Atmaram Bhide")).await;
    assert_display(
        &mut tx,
        &with_flags(&jethala, &["show_address"]),
        "Jethala\nPowder gali\nGokuldham Society",
    )
    .await;
    assert_display(
        &mut tx,
        &with_flags(&bhide, &["show_address"]),
        "Atmaram Bhide",
    )
    .await;
    let z = create(
        &mut tx,
        named("Z")
            .set_cell("email", Cell::Str("z@example.com".into()))
            .set_cell("vat", Cell::Str("BE0477472701".into()))
            .set_cell("street", Cell::Str("Main Street 1".into()))
            .set_cell("city", Cell::Str("Ramillies".into()))
            .set_cell("zip", Cell::Str("1367".into())),
    )
    .await;
    for (flags, expected) in [
        (
            vec!["show_address"],
            "Z\nMain Street 1\nRamillies 1367".to_owned(),
        ),
        (vec!["show_email"], "Z <z@example.com>".to_owned()),
        (vec!["partner_show_db_id"], format!("Z ({})", z.ids()[0])),
        (vec!["show_identifier"], "Z - BE0477472701".to_owned()),
        (
            vec!["show_address", "show_identifier"],
            "Z\nMain Street 1\nRamillies 1367\n BE0477472701".to_owned(),
        ),
        (
            vec![
                "show_address",
                "show_email",
                "partner_show_db_id",
                "show_identifier",
            ],
            format!(
                "Z ({}) <z@example.com>\nMain Street 1\nRamillies 1367\n BE0477472701",
                z.ids()[0]
            ),
        ),
    ] {
        assert_display(&mut tx, &with_flags(&z, &flags), &expected).await;
        assert_display(&mut tx, &z, "Z").await;
    }
    let decorated = with_flags(&z, &["show_address", "show_email", "show_identifier"]);
    z.as_records()
        .write(
            &mut tx,
            RawVals::new()
                .set_cell("email", Cell::Str("new@example.com".into()))
                .set_cell("street", Cell::Str("New Street".into()))
                .set_cell("vat", Cell::Str("NEW-VAT".into())),
        )
        .await
        .unwrap();
    assert_display(
        &mut tx,
        &decorated,
        "Z <new@example.com>\nNew Street\nRamillies 1367\n NEW-VAT",
    )
    .await;
    z.as_records()
        .write(
            &mut tx,
            RawVals::new()
                .set_cell("street2", Cell::Str("Suite 2".into()))
                .set_cell("city", Cell::Str("New City".into()))
                .set_cell("zip", Cell::Str("9999".into())),
        )
        .await
        .unwrap();
    assert_display(
        &mut tx,
        &decorated,
        "Z <new@example.com>\nNew Street\nSuite 2\nNew City 9999\n NEW-VAT",
    )
    .await;
    assert_complete(&mut tx, &decorated, "Z").await;
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_partner_013_formatted_labels_use_direct_parent_and_email_precedence() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let root = create(
        &mut tx,
        named("GhostStep").set_cell("email", Cell::Str("root@example.com".into())),
    )
    .await;
    let contact = child(&mut tx, Some("Existing Contact"), &root, "contact").await;
    let grand = child(&mut tx, Some("Grand"), &contact, "contact").await;
    let invoice = child(&mut tx, None, &root, "invoice").await;
    assert_display(
        &mut tx,
        &with_flags(&invoice, &["formatted_display_name"]),
        "GhostStep \t --Invoice--",
    )
    .await;
    assert_display(
        &mut tx,
        &with_flags(&grand, &["formatted_display_name"]),
        "Existing Contact \t --Grand--",
    )
    .await;
    assert_display(
        &mut tx,
        &with_flags(&root, &["formatted_display_name", "show_email"]),
        "GhostStep \t --root@example.com--",
    )
    .await;
    assert_display(
        &mut tx,
        &with_flags(
            &root,
            &["formatted_display_name", "show_email", "partner_show_db_id"],
        ),
        "GhostStep \t --root@example.com--",
    )
    .await;
    assert_display(
        &mut tx,
        &with_flags(
            &grand,
            &["formatted_display_name", "show_email", "partner_show_db_id"],
        ),
        &format!("Existing Contact \t --Grand-- \t --{}--", grand.ids()[0]),
    )
    .await;
    assert_display(
        &mut tx,
        &with_flags(&contact, &["formatted_display_name", "partner_show_db_id"]),
        &format!(
            "GhostStep \t --Existing Contact-- \t --{}--",
            contact.ids()[0]
        ),
    )
    .await;
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_partner_014_hiding_company_never_changes_stored_complete_name() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let root = create(&mut tx, named("GhostStep")).await;
    let contact = child(&mut tx, Some("Existing Contact"), &root, "contact").await;
    let hidden = with_flags(&contact, &["partner_display_name_hide_company"]);
    assert_display(&mut tx, &contact, "GhostStep, Existing Contact").await;
    assert_display(&mut tx, &hidden, "Existing Contact").await;
    assert_complete(&mut tx, &hidden, "GhostStep, Existing Contact").await;
    assert_display(&mut tx, &contact, "GhostStep, Existing Contact").await;
    let second = child(&mut tx, Some("Second"), &root, "contact").await;
    let second_hidden = with_flags(&second, &["partner_display_name_hide_company"]);
    assert_display(&mut tx, &second_hidden, "Second").await;
    assert_display(&mut tx, &second, "GhostStep, Second").await;
    assert_display(&mut tx, &second_hidden, "Second").await;
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_partner_037_four_field_name_search() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let root = create(&mut tx, named("Acme")).await;
    let john = child(&mut tx, Some("John"), &root, "contact").await;
    let email = create(
        &mut tx,
        named("Email target").set_cell("email", Cell::Str("raoul@x.fr".into())),
    )
    .await;
    let reference = create(
        &mut tx,
        named("Reference target").set_cell("ref", Cell::Str("R-77".into())),
    )
    .await;
    let vat = create(
        &mut tx,
        named("VAT target").set_cell("vat", Cell::Str("BE0477472701".into())),
    )
    .await;
    create(&mut tx, named("Unrelated")).await;
    let model = tx.env().model::<ResPartner>().unwrap();
    for (query, record, label) in [
        ("John", &john, "Acme, John"),
        ("raoul@", &email, "Email target"),
        ("R-77", &reference, "Reference target"),
        ("0477", &vat, "VAT target"),
    ] {
        assert_eq!(
            model.name_search(&mut tx, query).await.unwrap(),
            vec![(record.ids()[0], Some(label.into()))]
        );
    }
    assert_eq!(
        model.name_search(&mut tx, "acme").await.unwrap(),
        vec![
            (root.ids()[0], Some("Acme".into())),
            (john.ids()[0], Some("Acme, John".into()))
        ]
    );
    let denied_by_domain =
        NameSearch::new().domain(Domain::condition("id", Operator::Eq, john.ids()[0]).unwrap());
    assert!(
        model
            .name_search_with(&mut tx, "raoul@", &denied_by_domain)
            .await
            .unwrap()
            .is_empty()
    );
    let operator = NameSearch::new().operator(Operator::EqILike);
    assert!(
        model
            .name_search_with(&mut tx, "John", &operator)
            .await
            .unwrap()
            .is_empty()
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_partner_038_name_search_order_limit_and_archived() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let model = tx.env().model::<ResPartner>().unwrap();
    let (archived, _) = model
        .name_create(&mut tx, "\"A Raoul Grosbedon\" <raoul@example.fr>")
        .await
        .unwrap();
    partners(&tx)
        .browse([archived])
        .unwrap()
        .write(
            &mut tx,
            RawVals::new().set_cell("active", Cell::Bool(false)),
        )
        .await
        .unwrap();
    let (b, _) = model
        .name_create(&mut tx, "B Raoul chirurgiens-dentistes.fr")
        .await
        .unwrap();
    let (c, _) = model
        .name_create(&mut tx, "C Raoul O'hara  <!@historicalsociety.museum>")
        .await
        .unwrap();
    let (lone, _) = model
        .name_create(&mut tx, "ryu+giga-Sushi@example.jp")
        .await
        .unwrap();
    assert_eq!(
        model.name_search(&mut tx, "Raoul").await.unwrap(),
        vec![
            (b, Some("B Raoul chirurgiens-dentistes.fr".into())),
            (c, Some("C Raoul O'hara".into()))
        ]
    );
    assert_eq!(
        model
            .name_search_with(&mut tx, "Raoul", &NameSearch::new().limit(Some(1)))
            .await
            .unwrap(),
        vec![(b, Some("B Raoul chirurgiens-dentistes.fr".into()))]
    );
    assert_eq!(
        model
            .name_search(&mut tx, "")
            .await
            .unwrap()
            .iter()
            .map(|(id, _)| *id)
            .collect::<Vec<_>>(),
        vec![b, c, lone]
    );
    let all = model
        .with_context([("active_test", Value::Bool(false))])
        .name_search(&mut tx, "Raoul")
        .await
        .unwrap();
    assert_eq!(
        all.iter().map(|(id, _)| *id).collect::<Vec<_>>(),
        vec![archived, b, c]
    );
    let first_tie = create(&mut tx, named("Tie")).await;
    let second_tie = create(&mut tx, named("Tie")).await;
    assert_eq!(
        model
            .name_search(&mut tx, "Tie")
            .await
            .unwrap()
            .iter()
            .map(|(id, _)| *id)
            .collect::<Vec<_>>(),
        vec![second_tie.ids()[0], first_tie.ids()[0]]
    );
    assert!(
        model
            .name_search_with(&mut tx, "", &NameSearch::new().operator(Operator::NotILike))
            .await
            .unwrap()
            .is_empty()
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_partner_039_explicit_user_link_relational_name_search() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let unlinked = create(&mut tx, named("Vlad the Impaler")).await;
    let linked = create(&mut tx, named("Vlad the Impaler")).await;
    // This fixture deliberately supplies the existing explicit identity link.
    // It does not claim that user creation already delegates to a new partner.
    tx.query("INSERT INTO res_users (name, login, email, active, partner_id) VALUES ($1, $2, $3, true, $4) RETURNING id", &[&"Vlad the Impaler", &"vlad-labels", &"vlad.the.impaler@example.com", &linked.ids()[0]]).await.unwrap();
    let domain = Domain::condition("user_ids.email", Operator::ILike, "vlad").unwrap();
    let model = tx.env().model::<ResPartner>().unwrap();
    assert_eq!(
        model
            .name_search(&mut tx, "Vlad")
            .await
            .unwrap()
            .iter()
            .map(|(id, _)| *id)
            .collect::<Vec<_>>(),
        vec![linked.ids()[0], unlinked.ids()[0]]
    );
    assert_eq!(
        model
            .name_search_with(&mut tx, "Vlad", &NameSearch::new().domain(domain.clone()))
            .await
            .unwrap()
            .iter()
            .map(|(id, _)| *id)
            .collect::<Vec<_>>(),
        linked.ids()
    );
    assert_eq!(
        model
            .with_user(99_i64)
            .sudo(true)
            .name_search_with(&mut tx, "Vlad", &NameSearch::new().domain(domain))
            .await
            .unwrap()
            .iter()
            .map(|(id, _)| *id)
            .collect::<Vec<_>>(),
        linked.ids()
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_partner_040_name_create_parses_defaults_and_force_email() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let model = tx.env().model::<ResPartner>().unwrap();
    for (source, name, email) in [
        (
            "\"Raoul Grosbedon\" <raoul@chirurgiens-dentistes.fr> ",
            "Raoul Grosbedon",
            "raoul@chirurgiens-dentistes.fr",
        ),
        (
            "ryu+giga-Sushi@aizubange.fukushima.jp",
            "ryu+giga-sushi@aizubange.fukushima.jp",
            "ryu+giga-sushi@aizubange.fukushima.jp",
        ),
        (
            "Raoul chirurgiens-dentistes.fr",
            "Raoul chirurgiens-dentistes.fr",
            "",
        ),
        (
            " Raoul O'hara  <!@historicalsociety.museum>",
            "Raoul O'hara",
            "!@historicalsociety.museum",
        ),
        (
            "Raoul Grosbedon <raoul@CHIRURGIENS-dentistes.fr> ",
            "Raoul Grosbedon",
            "raoul@chirurgiens-dentistes.fr",
        ),
        (
            "Raoul megaraoul@chirurgiens-dentistes.fr",
            "Raoul",
            "megaraoul@chirurgiens-dentistes.fr",
        ),
    ] {
        let (id, label) = model.name_create(&mut tx, source).await.unwrap();
        let created = model.browse([id]).unwrap();
        assert_eq!(
            created
                .get(&mut tx, ResPartner::NAME)
                .await
                .unwrap()
                .as_deref(),
            Some(name)
        );
        assert_eq!(
            created
                .get(&mut tx, ResPartner::EMAIL)
                .await
                .unwrap()
                .as_deref()
                .unwrap_or(""),
            email
        );
        assert_eq!(
            label.as_ref(),
            created
                .get(&mut tx, ResPartner::DISPLAY_NAME)
                .await
                .unwrap()
                .as_deref()
                .unwrap()
        );
    }
    let defaults = model.with_context([
        ("default_email", Value::str("John.Wick@example.com")),
        ("default_type", Value::str("invalid-address-type")),
    ]);
    for (input, expected) in [
        (
            "\"Raoulette Vachette\" <Raoul@Grosbedon.fr>",
            "raoul@grosbedon.fr",
        ),
        ("Raoulette Vachette", "John.Wick@example.com"),
    ] {
        let (id, _) = defaults.name_create(&mut tx, input).await.unwrap();
        let created = model.browse([id]).unwrap();
        assert_eq!(
            created
                .get(&mut tx, ResPartner::EMAIL)
                .await
                .unwrap()
                .as_deref(),
            Some(expected)
        );
        assert_eq!(
            created
                .get(&mut tx, ResPartner::TYPE)
                .await
                .unwrap()
                .as_deref(),
            Some("contact")
        );
    }
    let before = partners(&tx)
        .search_count(&mut tx, &Domain::TRUE, None)
        .await
        .unwrap();
    let err = tx
        .savepoint(async |tx: &mut Transaction| {
            defaults
                .with_context([("force_email", Value::Bool(true))])
                .name_create(tx, "Raoul chirurgiens-dentistes.fr")
                .await
        })
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Validation);
    assert_eq!(
        err.message(),
        "Couldn't create contact without email address!"
    );
    assert_eq!(
        partners(&tx)
            .search_count(&mut tx, &Domain::TRUE, None)
            .await
            .unwrap(),
        before
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_partner_079_nameless_partner_email_has_no_false_display_name() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let root = create(&mut tx, named("Acme")).await;
    let address = child(&mut tx, None, &root, "delivery").await;
    address
        .as_records()
        .write(
            &mut tx,
            RawVals::new().set_cell("email", Cell::Str("Ship@Acme.Example".into())),
        )
        .await
        .unwrap();
    assert_eq!(
        address
            .get(&mut tx, ResPartner::EMAIL_FORMATTED)
            .await
            .unwrap()
            .as_deref(),
        Some("ship@acme.example")
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn contact_name_create_admits_exact_byte_boundary_and_refuses_overflow_without_writes() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let model = tx.env().model::<ResPartner>().unwrap();
    // The bound is bytes, not Unicode scalar values.
    let exact = "é".repeat(32_768);
    let (id, label) = model.name_create(&mut tx, &exact).await.unwrap();
    assert_eq!(label.as_ref(), exact);
    let created = model.browse([id]).unwrap();
    assert_eq!(
        created
            .get(&mut tx, ResPartner::NAME)
            .await
            .unwrap()
            .as_deref(),
        Some(exact.as_str())
    );
    let before = partners(&tx)
        .search_count(&mut tx, &Domain::TRUE, None)
        .await
        .unwrap();
    let over = format!("{exact}x");
    let error = match tx
        .savepoint(async |tx: &mut Transaction| model.name_create(tx, &over).await)
        .await
    {
        Err(error) => error,
        Ok((id, label)) => panic!(
            "over-limit input created id={id}, label_bytes={}",
            label.len()
        ),
    };
    assert_eq!(error.kind(), ErrorKind::Validation);
    assert_eq!(
        error.message(),
        "Contact email input exceeds the 65536-byte limit"
    );
    assert_eq!(
        partners(&tx)
            .search_count(&mut tx, &Domain::TRUE, None)
            .await
            .unwrap(),
        before
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn contact_email_candidate_bound_checks_the_whole_input() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let addresses = (0..256)
        .map(|i| format!("mail{i}@example.com"))
        .collect::<Vec<_>>();
    let email = addresses.join(",");
    let partner = create(
        &mut tx,
        named("Many addresses").set_cell("email", Cell::Str(email.clone().into())),
    )
    .await;
    assert_eq!(
        partner
            .get(&mut tx, ResPartner::EMAIL_FORMATTED)
            .await
            .unwrap()
            .as_deref(),
        Some(format!("\"Many addresses\" <{email}>").as_str())
    );
    let model = tx.env().model::<ResPartner>().unwrap();
    let over = format!("{email},overflow@example.com");
    let before = partners(&tx)
        .search_count(&mut tx, &Domain::TRUE, None)
        .await
        .unwrap();
    let error = match tx
        .savepoint(async |tx: &mut Transaction| model.name_create(tx, &over).await)
        .await
    {
        Err(error) => error,
        Ok((id, label)) => panic!(
            "over-limit input created id={id}, label_bytes={}",
            label.len()
        ),
    };
    assert_eq!(error.kind(), ErrorKind::Validation);
    assert_eq!(
        error.message(),
        "Contact email input exceeds the 256-address limit"
    );
    assert_eq!(
        partners(&tx)
            .search_count(&mut tx, &Domain::TRUE, None)
            .await
            .unwrap(),
        before
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn contact_complete_label_output_has_an_exact_checked_bound() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let model = tx.env().model::<ResPartner>().unwrap();
    let root = create(&mut tx, named("R")).await;
    let own_name = "x".repeat(1_048_576 - 3);
    let exact = format!("R, {own_name}");
    // Unsaved records exercise compute admission without an indexed PostgreSQL
    // name/complete_name value imposing its unrelated index-entry size limit.
    // A persisted parent also keeps this fixture outside CORE's pre-existing
    // new-root commercial_partner_id conversion gap.
    let partner = model
        .new_record(
            &mut tx,
            model
                .vals()
                .set(ResPartner::NAME, own_name.as_str())
                .set(ResPartner::PARENT_ID, root.rec_ids()[0])
                .set(ResPartner::EMAIL, "x@example.com"),
        )
        .await
        .unwrap();
    assert_complete(&mut tx, &partner, &exact).await;
    let decorated = with_flags(&partner, &["show_email"]);
    let error = match tx
        .savepoint(async |tx: &mut Transaction| decorated.get(tx, ResPartner::DISPLAY_NAME).await)
        .await
    {
        Err(error) => error,
        Ok(label) => panic!(
            "over-limit display label accepted, label_bytes={:?}",
            label.as_deref().map(str::len)
        ),
    };
    assert_eq!(error.kind(), ErrorKind::Validation);
    assert_eq!(
        error.message(),
        "Contact label output exceeds the 1048576-byte limit"
    );
    let before = partners(&tx)
        .search_count(&mut tx, &Domain::TRUE, None)
        .await
        .unwrap();
    let over = format!("{own_name}x");
    let values = model
        .vals()
        .set(ResPartner::NAME, over.as_str())
        .set(ResPartner::PARENT_ID, root.rec_ids()[0]);
    let error = match tx
        .savepoint(async |tx: &mut Transaction| {
            let new = model.new_record(tx, values).await?;
            new.get(tx, ResPartner::COMPLETE_NAME).await
        })
        .await
    {
        Err(error) => error,
        Ok(label) => panic!(
            "over-limit complete label accepted, label_bytes={:?}",
            label.as_deref().map(str::len)
        ),
    };
    assert_eq!(error.kind(), ErrorKind::Validation);
    assert_eq!(
        error.message(),
        "Contact label output exceeds the 1048576-byte limit"
    );
    assert_eq!(
        partners(&tx)
            .search_count(&mut tx, &Domain::TRUE, None)
            .await
            .unwrap(),
        before
    );
    drop(tx);
    h.cleanup().await.unwrap();
}
