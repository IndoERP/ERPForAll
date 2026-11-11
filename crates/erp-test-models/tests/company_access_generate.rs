//! The generator and verifier of company restrictions (A1-D2/A1-D3; design
//! , §6.1, §11.2) over the fixture "company access". Pure: no
//! database; the PostgreSQL half is `company_access_shapes.rs`.
#![allow(clippy::unwrap_used, clippy::panic, reason = "tests")]

use erp_registry::{Registry, RegistryBuilder};
use erp_security::company_access::{
    Changed, GeneratedFinding, GeneratedIssue, GeneratedRestriction, GeneratedRow, generate, verify,
};
use erp_security::rows::RuleId;
use erp_test_models::test_company_access;

const P: &str = "test_company_access.";

fn registry() -> Registry {
    RegistryBuilder::new([&test_company_access::ADDON])
        .build()
        .unwrap()
}

fn one<'a>(all: &'a [GeneratedRestriction], short: &str) -> &'a GeneratedRestriction {
    all.iter()
        .find(|g| g.model_name() == format!("{P}{short}"))
        .unwrap_or_else(|| panic!("no generated restriction for {short}"))
}

/// The row a module operation would write for `g`.
fn row_of(id: i64, g: &GeneratedRestriction) -> GeneratedRow {
    GeneratedRow::new(
        RuleId::new(id).unwrap(),
        g.model_name(),
        g.name(),
        true,
        "crud",
    )
    .with_domain(g.text())
}

fn issues(found: &[GeneratedFinding]) -> Vec<(String, Option<i64>, GeneratedIssue)> {
    found
        .iter()
        .map(|f| {
            (
                f.model_name().replace(P, ""),
                f.row().map(RuleId::get),
                f.issue(),
            )
        })
        .collect()
}

#[test]
fn texts_of_each_form_and_chain() {
    let r = registry();
    let all = generate(&r).unwrap();
    // sorted by model name; the abstract mixin and unrestricted models
    // models generate nothing
    let names: Vec<_> = all.iter().map(|g| g.model_name().replace(P, "")).collect();
    assert_eq!(
        names,
        [
            "l", "l2", "m1", "m2", "m3", "m4", "m5", "m6", "m7", "m8", "p", "rc", "t", "xc"
        ]
    );
    let cid = "company_id";
    let table: [(&str, String, String, &str); 13] = [
        (
            "m1",
            format!("[('{cid}', 'in', company_ids)]"),
            format!("[('{cid}', 'in', company_ids)]"),
            "company_id, exact, deny",
        ),
        (
            "m2",
            format!("[('{cid}', 'parent_of', company_ids)]"),
            format!("[('{cid}', 'parent_of', company_ids)]"),
            "company_id, ancestor, deny",
        ),
        (
            "m3",
            format!("['|', ('{cid}', '=', False), ('{cid}', 'parent_of', company_ids)]"),
            format!("[('{cid}', 'parent_of', company_ids)]"),
            "company_id, ancestor, allow",
        ),
        (
            "m4",
            format!("[('{cid}', 'in', company_ids + [False])]"),
            format!("[('{cid}', 'in', company_ids)]"),
            "company_id, exact, allow",
        ),
        (
            "m5",
            "[('company_ids', 'parent_of', company_ids)]".into(),
            "[('company_ids', 'parent_of', company_ids)]".into(),
            "company_ids, ancestor, deny",
        ),
        (
            "m6",
            "[('company_ids', 'in', company_ids)]".into(),
            "[('company_ids', 'in', company_ids)]".into(),
            "company_ids, exact, deny",
        ),
        (
            "m7",
            "['|', ('company_ids', '=', False), ('company_ids', 'in', company_ids)]".into(),
            "[('company_ids', 'in', company_ids)]".into(),
            "company_ids, exact, allow",
        ),
        (
            "m8",
            "['|', ('company_ids', '=', False), ('company_ids', 'parent_of', company_ids)]".into(),
            "[('company_ids', 'parent_of', company_ids)]".into(),
            "company_ids, ancestor, allow",
        ),
        (
            "l",
            format!("[('doc_id', 'any', [('{cid}', 'in', company_ids)])]"),
            format!("[('doc_id', 'any', [('{cid}', 'in', company_ids)])]"),
            "through doc_id",
        ),
        (
            "l2",
            format!("[('line_id', 'any', [('doc_id', 'any', [('{cid}', 'in', company_ids)])])]"),
            format!("[('line_id', 'any', [('doc_id', 'any', [('{cid}', 'in', company_ids)])])]"),
            "through line_id",
        ),
        (
            "p",
            format!(
                "['&', '|', ('{cid}', '=', False), ('{cid}', 'parent_of', company_ids), \
                 ('owner_id', 'any', [('{cid}', 'parent_of', company_ids)])]"
            ),
            format!(
                "['&', ('{cid}', 'parent_of', company_ids), \
                 ('owner_id', 'any', [('{cid}', 'parent_of', company_ids)])]"
            ),
            "company_id, ancestor, allow; through owner_id",
        ),
        (
            "t",
            format!("[('{cid}', 'in', company_ids)]"),
            format!("[('{cid}', 'in', company_ids)]"),
            "company_id, exact, deny",
        ),
        (
            "xc",
            format!("[('{cid}', 'in', company_ids)]"),
            format!("[('{cid}', 'in', company_ids)]"),
            "company_id, exact, deny",
        ),
    ];
    for (short, text, deny, summary) in table {
        let g = one(&all, short);
        assert_eq!(g.text(), text, "{short} text");
        assert_eq!(g.deny_text(), deny, "{short} deny text");
        assert_eq!(
            g.name(),
            format!("Company restriction of {P}{short} ({summary})"),
            "{short} name"
        );
    }
    // deterministic: a second call gives the same rows
    assert_eq!(all, generate(&r).unwrap());
}

#[test]
fn every_generated_restriction_names_the_company_and_compiles() {
    // `generate` also refuses a text that does not compile or does not name the company; this
    // checks the result again through the public test hook, deny variant included
    let r = registry();
    let all = generate(&r).unwrap();
    assert_eq!(all.len(), 14);
    for g in &all {
        for text in [g.text(), g.deny_text()] {
            let (_, mentions) =
                erp_security::company_access::compile_for_tests(&r, g.model_name(), text).unwrap();
            assert!(mentions, "{} ({text})", g.model_name());
        }
    }
}

#[test]
fn verify_accepts_exactly_the_generated_rows() {
    let r = registry();
    let all = generate(&r).unwrap();
    let rows: Vec<_> = all
        .iter()
        .enumerate()
        .map(|(i, g)| row_of(i64::try_from(i).unwrap() + 1, g))
        .collect();
    assert_eq!(verify(&r, &rows).unwrap(), []);
}

#[test]
fn verify_reports_each_issue() {
    let r = registry();
    let all = generate(&r).unwrap();
    let m1 = one(&all, "m1");
    let rid = |n| RuleId::new(n).unwrap();
    // every other model has its row, so only the cases below show up
    let mut rows: Vec<GeneratedRow> = all
        .iter()
        .enumerate()
        .filter(|(_, g)| !g.model_name().ends_with(".m1"))
        .map(|(i, g)| row_of(i64::try_from(i).unwrap() + 100, g))
        .collect();

    // missing: no row for m1
    let found = verify(&r, &rows).unwrap();
    assert_eq!(
        issues(&found),
        [("m1".into(), None, GeneratedIssue::Missing)]
    );
    assert!(found[0].denies());
    assert_eq!(found[0].model(), r.model_id(&format!("{P}m1")));

    // archived
    rows.push(
        GeneratedRow::new(rid(1), m1.model_name(), m1.name(), false, "crud").with_domain(m1.text()),
    );
    assert_eq!(
        issues(&verify(&r, &rows).unwrap()),
        [("m1".into(), Some(1), GeneratedIssue::Archived)]
    );

    // changed domain (also a company-less text), operation, group
    rows.pop();
    let base = || GeneratedRow::new(rid(1), m1.model_name(), m1.name(), true, "crud");
    rows.push(base().with_domain("[('company_id', 'in', company_ids + [False])]"));
    assert_eq!(
        issues(&verify(&r, &rows).unwrap()),
        [(
            "m1".into(),
            Some(1),
            GeneratedIssue::Changed {
                what: Changed::Domain
            }
        )]
    );
    rows.pop();
    rows.push(base().with_domain("[]"));
    assert_eq!(
        issues(&verify(&r, &rows).unwrap()),
        [
            (
                "m1".into(),
                Some(1),
                GeneratedIssue::Changed {
                    what: Changed::Domain
                }
            ),
            ("m1".into(), Some(1), GeneratedIssue::NoCompanyMention),
        ]
    );
    rows.pop();
    rows.push(
        GeneratedRow::new(rid(1), m1.model_name(), m1.name(), true, "r").with_domain(m1.text()),
    );
    assert_eq!(
        issues(&verify(&r, &rows).unwrap()),
        [(
            "m1".into(),
            Some(1),
            GeneratedIssue::Changed {
                what: Changed::Operation
            }
        )]
    );
    rows.pop();
    rows.push(
        base()
            .with_domain(m1.text())
            .with_group(erp_orm::GroupId::new(7).unwrap()),
    );
    assert_eq!(
        issues(&verify(&r, &rows).unwrap()),
        [(
            "m1".into(),
            Some(1),
            GeneratedIssue::Changed {
                what: Changed::Group
            }
        )]
    );

    // duplicate
    rows.pop();
    rows.push(base().with_domain(m1.text()));
    rows.push(
        GeneratedRow::new(rid(2), m1.model_name(), m1.name(), true, "crud").with_domain(m1.text()),
    );
    assert_eq!(
        issues(&verify(&r, &rows).unwrap()),
        [("m1".into(), Some(1), GeneratedIssue::Duplicate { count: 2 })]
    );

    // renamed: report only; orphan rows (unknown model, model that no longer generates)
    rows.pop();
    rows.pop();
    rows.push(
        GeneratedRow::new(rid(1), m1.model_name(), "Other", true, "crud").with_domain(m1.text()),
    );
    rows.push(
        GeneratedRow::new(
            rid(3),
            &format!("{P}u"),
            "Company restriction of u",
            true,
            "crud",
        )
        .with_domain("[]"),
    );
    rows.push(
        GeneratedRow::new(
            rid(4),
            "no.such.model",
            "Company restriction of x",
            true,
            "crud",
        )
        .with_domain("[]"),
    );
    let found = verify(&r, &rows).unwrap();
    assert_eq!(
        issues(&found),
        [
            // sorted by the full model name: "no.such.model" before "test_company_access.*"
            ("no.such.model".into(), Some(4), GeneratedIssue::Orphan),
            ("m1".into(), Some(1), GeneratedIssue::Renamed),
            ("u".into(), Some(3), GeneratedIssue::Orphan),
        ]
    );
    assert!(found.iter().all(|f| !f.denies()));
    assert_eq!(found[0].model(), None);
}

/// An orphan generated row that carries a group is a permission, not a narrowing restriction: it
/// is reported as an orphan *and* as a changed group (review M1), and denies nothing by itself.
#[test]
fn an_orphan_with_a_group_is_reported_as_a_permission() {
    let r = registry();
    let all = generate(&r).unwrap();
    let mut rows: Vec<GeneratedRow> = all
        .iter()
        .enumerate()
        .map(|(i, g)| row_of(i64::try_from(i).unwrap() + 100, g))
        .collect();
    rows.push(
        GeneratedRow::new(
            RuleId::new(1).unwrap(),
            &format!("{P}u"),
            "Company restriction of u",
            true,
            "crud",
        )
        .with_group(erp_orm::GroupId::new(7).unwrap()),
    );
    let found = verify(&r, &rows).unwrap();
    assert_eq!(
        issues(&found),
        [
            (
                "u".into(),
                Some(1),
                GeneratedIssue::Changed {
                    what: Changed::Group
                }
            ),
            ("u".into(), Some(1), GeneratedIssue::Orphan),
        ]
    );
    // the model is unrestricted (it has no generated restriction to lose): nothing is denied
    assert!(found.iter().all(|f| f.model().is_none() && !f.denies()));
}
