// SPDX-License-Identifier: MIT OR Apache-2.0
//! Barcode invariants exercised through actual ORM mutations and overlapping connections.
#![allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]
mod support;
use erp_sdk::prelude::*;
use erp_sdk::{ErrorKind, Operation};
use std::time::Duration;

async fn template(
    tx: &mut Transaction,
    name: &str,
    company: Option<i64>,
    barcode: Option<&str>,
) -> Records {
    let mut vals = RawVals::new()
        .set_cell("name", Cell::Str(name.into()))
        .set_cell("company_id", company.map_or(Cell::Null, Cell::Id));
    if let Some(barcode) = barcode {
        vals = vals.set_cell("barcode", Cell::Str(barcode.into()));
    }
    tx.env()
        .records("product.template")
        .unwrap()
        .create(tx, vec![vals])
        .await
        .unwrap()
}
async fn variant(tx: &mut Transaction, tmpl: &Records) -> Records {
    tx.env()
        .records("product.product")
        .unwrap()
        .search(
            tx,
            &Domain::condition("product_tmpl_id", Operator::Eq, tmpl.rec_ids()[0].get()).unwrap(),
        )
        .await
        .unwrap()
}
async fn packaging(
    tx: &mut Transaction,
    product: &Records,
    unit: i64,
    company: Option<i64>,
    barcode: &str,
) -> Result<Records> {
    tx.env()
        .records("product.uom")?
        .create(
            tx,
            vec![
                RawVals::new()
                    .set_cell("product_id", Cell::Id(product.rec_ids()[0].get()))
                    .set_cell("uom_id", Cell::Id(unit))
                    .set_cell("company_id", company.map_or(Cell::Null, Cell::Id))
                    .set_cell("barcode", Cell::Str(barcode.into())),
            ],
        )
        .await
}

#[tokio::test]
async fn supplied_bypass_flag_is_refused_even_when_false() {
    let h = support::base_harness().await;
    let mut tx = h.begin().await.unwrap();
    for value in [Value::Bool(true), Value::Bool(false), Value::str("yes")] {
        let model = tx
            .env()
            .records("product.template")
            .unwrap()
            .with_context([("skip_barcode_uniqueness_check", value)]);
        let error = tx
            .savepoint(async |tx| {
                model
                    .create(
                        tx,
                        vec![RawVals::new().set_cell("name", Cell::Str("Bypass".into()))],
                    )
                    .await
            })
            .await
            .expect_err("a supplied bypass key must fail");
        assert_eq!(error.kind(), ErrorKind::Validation);
        assert_eq!(
            error.message(),
            "skip_barcode_uniqueness_check is not supported: barcode uniqueness cannot be skipped."
        );
    }
}

#[tokio::test]
async fn company_only_changes_recheck_all_variant_barcodes() {
    let h = support::base_harness().await;
    let a = support::seed_company(&h, "Scope A").await;
    let b = support::seed_company(&h, "Scope B").await;
    let mut tx = h.begin().await.unwrap();
    let _first = template(&mut tx, "First", Some(a), Some("MOVE")).await;
    let second = template(&mut tx, "Second", Some(b), None).await;
    let sibling = variant(&mut tx, &second).await;
    let product = tx
        .env()
        .records("product.product")
        .unwrap()
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("product_tmpl_id", Cell::Id(second.rec_ids()[0].get()))
                    .set_cell("barcode", Cell::Str("FREE".into())),
            ],
        )
        .await
        .unwrap();
    sibling
        .write(
            &mut tx,
            RawVals::new().set_cell("barcode", Cell::Str("MOVE".into())),
        )
        .await
        .unwrap();
    // The selected variant's FREE barcode is safe; its delegated company move must still
    // reject the other sibling's MOVE collision, including when writing through the template.
    for rows in [second.clone(), product] {
        let error = tx
            .savepoint(async |tx| {
                rows.write(tx, RawVals::new().set_cell("company_id", Cell::Id(a)))
                    .await
            })
            .await
            .expect_err("company-only move must recheck the barcode");
        assert_eq!(error.kind(), ErrorKind::Validation);
        assert!(error.message().contains("MOVE"), "{error}");
    }
}

#[tokio::test]
async fn packaging_and_product_use_symmetric_company_scope() {
    let h = support::base_harness().await;
    let unit = support::unit_id(&h).await;
    let a = support::seed_company(&h, "Package A").await;
    let b = support::seed_company(&h, "Package B").await;
    let mut tx = h.begin().await.unwrap();
    let first = template(&mut tx, "Product A", Some(a), Some("CROSS")).await;
    let second = template(&mut tx, "Product B", Some(b), None).await;
    let p = variant(&mut tx, &second).await;
    let pack = packaging(&mut tx, &p, unit, Some(b), "CROSS")
        .await
        .expect("different non-null companies may reuse a product barcode");
    let error = tx
        .savepoint(async |tx| {
            pack.write(tx, RawVals::new().set_cell("company_id", Cell::Id(a)))
                .await
        })
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Validation);
    assert!(error.message().contains("CROSS"), "{error}");
    pack.unlink(&mut tx).await.unwrap();
    first
        .write(
            &mut tx,
            RawVals::new().set_cell("barcode", Cell::Str("REVERSE".into())),
        )
        .await
        .unwrap();
    packaging(&mut tx, &p, unit, Some(b), "REVERSE")
        .await
        .unwrap();
}

#[tokio::test]
async fn diagnostics_list_every_conflicting_product() {
    let h = support::base_harness().await;
    let a = support::seed_company(&h, "Diagnostic A").await;
    let b = support::seed_company(&h, "Diagnostic B").await;
    let mut tx = h.begin().await.unwrap();
    template(&mut tx, "Visible One", Some(a), Some("DETAIL")).await;
    template(&mut tx, "Visible Two", Some(b), Some("DETAIL")).await;
    let target = template(&mut tx, "Target", None, None).await;
    let error = target
        .write(
            &mut tx,
            RawVals::new().set_cell("barcode", Cell::Str("DETAIL".into())),
        )
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Validation);
    assert_eq!(
        error.message(),
        "These barcodes are already in use:\n\n- \"DETAIL\" is held by product: Visible One, product: Visible Two, product: Target\n\nHolders you have no access to are not named."
    );
}

/// not two application nodes or concurrent variant generation)
#[tokio::test]
async fn stale_snapshot_cannot_commit_a_second_barcode_holder() {
    let h = support::base_harness().await;
    let mut setup = h.begin().await.unwrap();
    let first = template(&mut setup, "Concurrent One", None, None)
        .await
        .rec_ids()[0]
        .get();
    let second = template(&mut setup, "Concurrent Two", None, None)
        .await
        .rec_ids()[0]
        .get();
    setup.commit().await.unwrap();
    let (mut one, mut two) = h.begin_pair().await.unwrap();
    let pid = erp_testkit::backend_pid(&mut two).await.unwrap();
    let p1 = one
        .env()
        .records("product.template")
        .unwrap()
        .browse(vec![first])
        .unwrap();
    let p2 = two
        .env()
        .records("product.template")
        .unwrap()
        .browse(vec![second])
        .unwrap();
    p1.write(
        &mut one,
        RawVals::new().set_cell("barcode", Cell::Str("RACE".into())),
    )
    .await
    .unwrap();
    let waiting = tokio::spawn(async move {
        let result = p2
            .write(
                &mut two,
                RawVals::new().set_cell("barcode", Cell::Str("RACE".into())),
            )
            .await;
        (result, two)
    });
    erp_testkit::wait_until_blocked(&h, pid, Duration::from_secs(30))
        .await
        .unwrap();
    one.commit().await.unwrap();
    let (result, mut two) = waiting.await.unwrap();
    let error = result.expect_err("the stale writer must fail");
    assert_eq!(error.kind(), ErrorKind::Concurrency, "{error}");
    // PostgreSQL supplies this text; wording may differ by server version/localization.
    assert!(error.message().contains("serialize"), "{error}");
    assert_eq!(error.sqlstate(), Some("40001"));
    two.rollback().await.unwrap();
    let mut fresh = h.begin().await.unwrap();
    let p2 = fresh
        .env()
        .records("product.template")
        .unwrap()
        .browse(vec![second])
        .unwrap();
    let error = fresh
        .savepoint(async |tx| {
            p2.write(
                tx,
                RawVals::new().set_cell("barcode", Cell::Str("RACE".into())),
            )
            .await
        })
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Validation);
    assert!(error.message().contains("RACE"), "{error}");
    let count: i64 = fresh
        .query(
            "SELECT count(*) FROM product_product WHERE barcode = 'RACE'",
            &[],
        )
        .await
        .unwrap()[0]
        .get(0);
    assert_eq!(count, 1);
}

#[tokio::test]
async fn claims_follow_mirrors_cascades_and_rollback() {
    let h = support::base_harness().await;
    let unit = support::unit_id(&h).await;
    let mut tx = h.begin().await.unwrap();
    let tmpl = template(&mut tx, "Claim owner", None, Some("MIRROR-CLAIM")).await;
    let product = variant(&mut tx, &tmpl).await;
    let claim = tx.query("SELECT variant_id, template_id FROM product_barcode_claim WHERE barcode = 'MIRROR-CLAIM'", &[]).await.unwrap();
    assert_eq!(claim.len(), 1);
    assert_eq!(
        claim[0].get::<_, Option<i64>>(0),
        Some(product.rec_ids()[0].get())
    );
    assert_eq!(claim[0].get::<_, Option<i64>>(1), None);
    packaging(&mut tx, &product, unit, None, "CASCADE-CLAIM")
        .await
        .unwrap();
    tx.savepoint(async |tx| {
        tmpl.unlink(tx).await?;
        let count: i64 = tx
            .query("SELECT count(*) FROM product_barcode_claim", &[])
            .await?[0]
            .get(0);
        assert_eq!(count, 0);
        Err::<(), _>(Error::validation("rollback deletion"))
    })
    .await
    .unwrap_err();
    assert_eq!(
        tx.query("SELECT count(*) FROM product_barcode_claim", &[])
            .await
            .unwrap()[0]
            .get::<_, i64>(0),
        2
    );
    tmpl.unlink(&mut tx).await.unwrap();
    let replacement = template(&mut tx, "Replacement", None, Some("CASCADE-CLAIM")).await;
    assert_eq!(variant(&mut tx, &replacement).await.len(), 1);
    assert_eq!(
        tx.query(
            "SELECT count(*) FROM product_barcode_scope WHERE barcode = 'CASCADE-CLAIM'",
            &[]
        )
        .await
        .unwrap()[0]
            .get::<_, i64>(0),
        1
    );
}

/// Exact-string policy: whitespace and Unicode are stored without rewriting.
#[tokio::test]
async fn exact_strings_and_archived_holders_keep_their_claims() {
    let h = support::base_harness().await;
    let mut tx = h.begin().await.unwrap();
    let first = template(&mut tx, "Exact", None, Some(" é ")).await;
    template(&mut tx, "Distinct", None, Some("é")).await;
    first
        .write(
            &mut tx,
            RawVals::new().set_cell("active", Cell::Bool(false)),
        )
        .await
        .unwrap();
    let other = template(&mut tx, "Other", None, None).await;
    let error = tx
        .savepoint(async |tx| {
            other
                .write(
                    tx,
                    RawVals::new().set_cell("barcode", Cell::Str(" é ".into())),
                )
                .await
        })
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Validation);
    assert!(error.message().contains(" é "), "{error}");
    first
        .write(&mut tx, RawVals::new().set_cell("barcode", Cell::Null))
        .await
        .unwrap();
    other
        .write(
            &mut tx,
            RawVals::new().set_cell("barcode", Cell::Str(" é ".into())),
        )
        .await
        .unwrap();
}

#[tokio::test]
async fn diagnostic_labels_do_not_disclose_hidden_holders() {
    let policy = erp_testkit::DomainPolicy::builder(erp_testkit::Otherwise::Allow)
        .rule(
            "product.product",
            Operation::Read,
            Domain::condition("name", Operator::Ne, "Secret owner").unwrap(),
        )
        .rule(
            "product.template",
            Operation::Read,
            Domain::condition("name", Operator::Ne, "Secret owner").unwrap(),
        )
        .build()
        .unwrap();
    let h = support::harness_with_policy(policy).await;
    let company = support::seed_company(&h, "Reader company").await;
    let mut tx = h.begin().await.unwrap();
    let secret = template(&mut tx, "Secret owner", Some(company), Some("PRIVATE")).await;
    let product = variant(&mut tx, &secret).await;
    assert!(matches!(
        product.cell(&mut tx, "display_name").await.unwrap(),
        Cell::Str(_)
    ));
    let target = template(&mut tx, "Visible target", Some(company), None).await;
    let user: i64 = tx.query("INSERT INTO res_users (name, login, active) VALUES ('Barcode reader', 'barcode-reader', true) RETURNING id", &[]).await.unwrap()[0].get(0);
    let reader = tx.env().with_user(user).with_company(company).unwrap();
    let target = target.with_env(reader);
    let error = target
        .write(
            &mut tx,
            RawVals::new().set_cell("barcode", Cell::Str("PRIVATE".into())),
        )
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Validation);
    assert_eq!(
        error.message(),
        format!(
            "These barcodes are already in use:\n\n- \"PRIVATE\" is held by product: Visible target, {}\n\nHolders you have no access to are not named.",
            erp_addon_product::barcode::HIDDEN_HOLDERS
        )
    );
}

async fn assign_race(
    tx: &mut Transaction,
    template_id: i64,
    package: bool,
    unit: i64,
    code: &str,
) -> Result<()> {
    let tmpl = tx
        .env()
        .records("product.template")?
        .browse(vec![template_id])?;
    if package {
        let product = variant(tx, &tmpl).await;
        let company = match tmpl.cell(tx, "company_id").await? {
            Cell::Id(id) => Some(id),
            Cell::Null => None,
            value => return Err(Error::programming(format!("Unexpected company {value:?}"))),
        };
        packaging(tx, &product, unit, company, code).await?;
        Ok(())
    } else {
        tmpl.write(
            tx,
            RawVals::new().set_cell("barcode", Cell::Str(code.into())),
        )
        .await
    }
}

/// connections; distinct-company retry success; no two-node variant generation)
#[tokio::test]
async fn overlapping_holder_scopes_fail_the_stale_writer_in_both_directions() {
    let h = support::base_harness().await;
    let unit = support::unit_id(&h).await;
    let a = support::seed_company(&h, "Race A").await;
    let b = support::seed_company(&h, "Race B").await;
    for (index, (left, right, lp, rp, conflicts)) in [
        (None, Some(a), false, false, true),
        (Some(a), None, false, false, true),
        (Some(a), Some(a), false, true, true),
        (Some(a), Some(a), true, false, true),
        (None, Some(a), true, false, true),
        (Some(a), None, false, true, true),
        (Some(a), Some(b), true, true, true),
        (Some(a), Some(b), false, false, false),
    ]
    .into_iter()
    .enumerate()
    {
        let code = format!("SCOPE-RACE-{index}");
        let mut setup = h.begin().await.unwrap();
        let first = template(&mut setup, "Race first", left, None)
            .await
            .rec_ids()[0]
            .get();
        let second = template(&mut setup, "Race second", right, None)
            .await
            .rec_ids()[0]
            .get();
        setup.commit().await.unwrap();
        let (mut one, mut two) = h.begin_pair().await.unwrap();
        let pid = erp_testkit::backend_pid(&mut two).await.unwrap();
        assign_race(&mut one, first, lp, unit, &code).await.unwrap();
        let attempted = code.clone();
        let waiter = tokio::spawn(async move {
            let result = assign_race(&mut two, second, rp, unit, &attempted).await;
            (result, two)
        });
        erp_testkit::wait_until_blocked(&h, pid, Duration::from_secs(30))
            .await
            .unwrap();
        one.commit().await.unwrap();
        let (result, mut two) = waiter.await.unwrap();
        let error = result.expect_err("every stale writer shares the barcode fence");
        assert_eq!(
            error.kind(),
            ErrorKind::Concurrency,
            "case {index}: {error}"
        );
        // PostgreSQL supplies this text; wording may differ by server version/localization.
        assert!(error.message().contains("serialize"), "{error}");
        assert_eq!(error.sqlstate(), Some("40001"));
        two.rollback().await.unwrap();
        let mut retry = h.begin().await.unwrap();
        let result = assign_race(&mut retry, second, rp, unit, &code).await;
        if conflicts {
            let error = result.expect_err("overlap must fail after a fresh snapshot");
            assert_eq!(error.kind(), ErrorKind::Validation, "case {index}: {error}");
            assert!(
                error.message().contains(&code)
                    || (lp && rp && error.message().contains("packaging")),
                "{error}"
            );
            retry.rollback().await.unwrap();
        } else {
            result.unwrap();
            retry.commit().await.unwrap();
        }
        drop(retry);
        drop(two);
        drop(one);
        drop(setup);
        let mut check = h.begin().await.unwrap();
        let count: i64 = check
            .query(
                "SELECT count(*) FROM product_barcode_claim WHERE barcode=$1",
                &[&code],
            )
            .await
            .unwrap()[0]
            .get(0);
        assert_eq!(count, if conflicts { 1 } else { 2 });
    }
}

/// A disconnected/rolled-back first writer cannot strand a barcode claim or its scope lock.
#[tokio::test]
async fn rolled_back_winner_releases_the_waiting_writer() {
    let h = support::base_harness().await;
    let mut setup = h.begin().await.unwrap();
    let first = template(&mut setup, "Rollback first", None, None)
        .await
        .rec_ids()[0]
        .get();
    let second = template(&mut setup, "Rollback second", None, None)
        .await
        .rec_ids()[0]
        .get();
    setup.commit().await.unwrap();
    let (mut one, mut two) = h.begin_pair().await.unwrap();
    let pid = erp_testkit::backend_pid(&mut two).await.unwrap();
    assign_race(&mut one, first, false, 0, "ABORT-RACE")
        .await
        .unwrap();
    let waiter = tokio::spawn(async move {
        let result = assign_race(&mut two, second, false, 0, "ABORT-RACE").await;
        (result, two)
    });
    erp_testkit::wait_until_blocked(&h, pid, Duration::from_secs(30))
        .await
        .unwrap();
    one.rollback().await.unwrap();
    let (result, mut two) = waiter.await.unwrap();
    result.unwrap();
    two.commit().await.unwrap();
    let mut check = h.begin().await.unwrap();
    assert_eq!(
        check
            .query(
                "SELECT count(*) FROM product_barcode_claim WHERE barcode='ABORT-RACE'",
                &[]
            )
            .await
            .unwrap()[0]
            .get::<_, i64>(0),
        1
    );
}

#[tokio::test]
async fn template_batch_reports_all_groups_and_rolls_back_whole() {
    let h = support::base_harness().await;
    let mut tx = h.begin().await.unwrap();
    template(&mut tx, "Existing first", None, Some("GROUP-ONE")).await;
    template(&mut tx, "Existing second", None, Some("GROUP-TWO")).await;
    let model = tx.env().records("product.template").unwrap();
    let error = tx
        .savepoint(async |tx| {
            model
                .create(
                    tx,
                    vec![
                        RawVals::new()
                            .set_cell("name", Cell::Str("Attempt first".into()))
                            .set_cell("barcode", Cell::Str("GROUP-ONE".into())),
                        RawVals::new()
                            .set_cell("name", Cell::Str("Attempt second".into()))
                            .set_cell("barcode", Cell::Str("GROUP-TWO".into())),
                    ],
                )
                .await
        })
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Validation);
    for text in [
        "GROUP-ONE",
        "GROUP-TWO",
        "Existing first",
        "Existing second",
    ] {
        assert!(error.message().contains(text), "{error}");
    }
    assert_eq!(
        tx.query("SELECT count(*) FROM product_template", &[])
            .await
            .unwrap()[0]
            .get::<_, i64>(0),
        2
    );
    assert_eq!(
        tx.query("SELECT count(*) FROM product_barcode_claim", &[])
            .await
            .unwrap()[0]
            .get::<_, i64>(0),
        2
    );
}

/// Internal integrity state is derived from holders; privileged ORM callers and loaders cannot
/// remove permanent fences or fabricate/re-scope claims. Schema-owner SQL remains an operator API.
#[tokio::test]
async fn integrity_state_refuses_privileged_orm_mutations() {
    let h = support::base_harness().await;
    let mut tx = h.begin().await.unwrap();
    template(&mut tx, "Protected state", None, Some("PROTECTED")).await;
    for model_name in ["product.barcode.scope", "product.barcode.claim"] {
        let model = tx.env().records(model_name).unwrap();
        let rows = model
            .search(&mut tx, &Domain::constant(true))
            .await
            .unwrap();
        assert_eq!(rows.len(), 1);
        let error = tx
            .savepoint(async |tx| rows.unlink(tx).await)
            .await
            .expect_err("even privileged callers cannot remove derived integrity state");
        assert_eq!(error.kind(), ErrorKind::Access);
        assert_eq!(
            error.message(),
            "Barcode integrity state is managed by product and packaging writes; direct changes are not allowed."
        );
        let error = tx
            .savepoint(async |tx| {
                rows.write(
                    tx,
                    RawVals::new().set_cell("barcode", Cell::Str("TAMPERED".into())),
                )
                .await
            })
            .await
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Access);
        assert_eq!(
            error.message(),
            "Barcode integrity state is managed by product and packaging writes; direct changes are not allowed."
        );
        let error = tx
            .savepoint(async |tx| {
                model
                    .create(
                        tx,
                        vec![RawVals::new().set_cell("barcode", Cell::Str("FABRICATED".into()))],
                    )
                    .await
            })
            .await
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Access);
        assert_eq!(
            error.message(),
            "Barcode integrity state is managed by product and packaging writes; direct changes are not allowed."
        );
    }
}

async fn template_barcode(tx: &mut Transaction, tmpl: &Records) -> Cell {
    tmpl.cell(tx, "barcode").await.unwrap()
}

async fn holders_of(tx: &mut Transaction, barcode: &str) -> (i64, i64) {
    let row = &tx
        .query(
            "SELECT (SELECT count(*) FROM product_barcode_claim WHERE barcode = $1),
                (SELECT count(*) FROM product_product WHERE barcode = $1)
                + (SELECT count(*) FROM product_template WHERE barcode = $1)",
            &[&barcode],
        )
        .await
        .unwrap()[0];
    (row.get(0), row.get(1))
}

#[tokio::test]
async fn second_variant_by_hand_moves_the_mirror_to_the_variant() {
    let h = support::base_harness().await;
    let mut tx = h.begin().await.unwrap();
    let tmpl = template(&mut tx, "Mirror by hand", None, Some("P1-HAND")).await;
    let first = variant(&mut tx, &tmpl).await;
    assert_eq!(first.len(), 1);
    // an old stamp, so the clear below must write its own
    tx.execute(
        "UPDATE product_template SET write_date = '2000-01-01', write_uid = NULL WHERE id = $1",
        &[&tmpl.rec_ids()[0].get()],
    )
    .await
    .unwrap();
    tx.env()
        .records("product.product")
        .unwrap()
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("product_tmpl_id", Cell::Id(tmpl.rec_ids()[0].get()))
                    .set_cell("active", Cell::Bool(false)),
            ],
        )
        .await
        .unwrap();
    assert_eq!(template_barcode(&mut tx, &tmpl).await, Cell::Null);
    let stamp = &tx
        .query(
            "SELECT write_date > '2000-01-02', write_uid FROM product_template WHERE id = $1",
            &[&tmpl.rec_ids()[0].get()],
        )
        .await
        .unwrap()[0];
    assert!(stamp.get::<_, bool>(0), "the clear stamps write_date");
    assert_eq!(stamp.get::<_, Option<i64>>(1), Some(tx.env().uid()));
    assert_eq!(
        first.cell(&mut tx, "barcode").await.unwrap(),
        Cell::Str("P1-HAND".into())
    );
    // one physical holder left, and it keeps its claim
    assert_eq!(holders_of(&mut tx, "P1-HAND").await, (1, 1));
    // the barcode is still unique: another product cannot take it
    let error = tx
        .savepoint(async |tx| {
            tx.env()
                .records("product.template")?
                .create(
                    tx,
                    vec![
                        RawVals::new()
                            .set_cell("name", Cell::Str("Taker".into()))
                            .set_cell("barcode", Cell::Str("P1-HAND".into())),
                    ],
                )
                .await
        })
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Validation);
    assert!(error.message().contains("P1-HAND"), "{error}");
}

/// Adds one multi-value attribute line (values S and M) to each template, in one create call.
async fn add_size_line(tx: &mut Transaction, templates: &[&Records]) {
    let env = tx.env().clone();
    let attribute = env
        .records("product.attribute")
        .unwrap()
        .create(
            tx,
            vec![
                RawVals::new()
                    .set_cell("name", Cell::Str("Mirror size".into()))
                    .set_cell("create_variant", Cell::Str("always".into())),
            ],
        )
        .await
        .unwrap()
        .rec_ids()[0]
        .get();
    let values = env
        .records("product.attribute.value")
        .unwrap()
        .create(
            tx,
            ["S", "M"]
                .iter()
                .map(|name| {
                    RawVals::new()
                        .set_cell("name", Cell::Str((*name).into()))
                        .set_cell("attribute_id", Cell::Id(attribute))
                })
                .collect(),
        )
        .await
        .unwrap();
    env.records("product.template.attribute.line")
        .unwrap()
        .create(
            tx,
            templates
                .iter()
                .map(|tmpl| {
                    RawVals::new()
                        .set_cell("product_tmpl_id", Cell::Id(tmpl.rec_ids()[0].get()))
                        .set_cell("attribute_id", Cell::Id(attribute))
                        .set_cell("value_ids", Cell::Ids(values.rec_ids().to_vec().into()))
                })
                .collect(),
        )
        .await
        .unwrap();
}

async fn family_with_barcode(tx: &mut Transaction, tmpl: &Records, barcode: &str) -> usize {
    let env = tx.env().clone();
    env.records("product.product")
        .unwrap()
        .with_context([("active_test", Value::Bool(false))])
        .search(
            tx,
            &(Domain::condition("product_tmpl_id", Operator::Eq, tmpl.rec_ids()[0].get()).unwrap()
                & Domain::condition("barcode", Operator::Eq, barcode).unwrap()),
        )
        .await
        .unwrap()
        .len()
}

#[tokio::test]
async fn attribute_line_on_a_barcoded_single_variant_template_keeps_the_barcode() {
    let h = support::base_harness().await;
    let mut tx = h.begin().await.unwrap();
    let first = template(&mut tx, "Mirror by line", None, Some("P1-LINE")).await;
    let second = template(&mut tx, "Mirror by line 2", None, Some("P1-LINE-2")).await;
    add_size_line(&mut tx, &[&first, &second]).await;
    for (tmpl, barcode) in [(&first, "P1-LINE"), (&second, "P1-LINE-2")] {
        assert_eq!(variant(&mut tx, tmpl).await.len(), 2);
        // the replaced variant is gone, so the template holds the barcode and its claim
        assert_eq!(
            template_barcode(&mut tx, tmpl).await,
            Cell::Str(barcode.into())
        );
        assert_eq!(family_with_barcode(&mut tx, tmpl, barcode).await, 0);
        assert_eq!(holders_of(&mut tx, barcode).await, (1, 1));
    }
    let error = tx
        .savepoint(async |tx| {
            tx.env()
                .records("product.template")?
                .create(
                    tx,
                    vec![
                        RawVals::new()
                            .set_cell("name", Cell::Str("Taker".into()))
                            .set_cell("barcode", Cell::Str("P1-LINE".into())),
                    ],
                )
                .await
        })
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Validation);
    assert!(error.message().contains("P1-LINE"), "{error}");
}

#[tokio::test]
async fn attribute_line_archiving_the_replaced_variant_leaves_the_barcode_on_it() {
    let h = support::base_harness().await;
    let mut tx = h.begin().await.unwrap();
    let tmpl = template(&mut tx, "Mirror archived", None, Some("P1-ARCH")).await;
    let replaced = variant(&mut tx, &tmpl).await.rec_ids()[0].get();
    // a reference the variant cannot be deleted under, so the generator archives it; the
    // table goes with the test transaction, which is never committed
    tx.batch("CREATE TABLE keep_replaced (product_id bigint REFERENCES product_product (id))")
        .await
        .unwrap();
    tx.execute("INSERT INTO keep_replaced VALUES ($1)", &[&replaced])
        .await
        .unwrap();
    add_size_line(&mut tx, &[&tmpl]).await;
    let archived = tx
        .env()
        .records("product.product")
        .unwrap()
        .browse(vec![replaced])
        .unwrap();
    assert_eq!(
        archived.cell(&mut tx, "active").await.unwrap(),
        Cell::Bool(false)
    );
    assert_eq!(
        archived.cell(&mut tx, "barcode").await.unwrap(),
        Cell::Str("P1-ARCH".into())
    );
    assert_eq!(template_barcode(&mut tx, &tmpl).await, Cell::Null);
    assert_eq!(holders_of(&mut tx, "P1-ARCH").await, (1, 1));
}

#[tokio::test]
async fn variant_moved_into_a_single_variant_template_ends_the_mirror() {
    let h = support::base_harness().await;
    let mut tx = h.begin().await.unwrap();
    let target = template(&mut tx, "Mirror target", None, Some("N2-MOVE")).await;
    let own = variant(&mut tx, &target).await;
    let source = template(&mut tx, "Mirror source", None, None).await;
    variant(&mut tx, &source)
        .await
        .write(
            &mut tx,
            RawVals::new().set_cell("product_tmpl_id", Cell::Id(target.rec_ids()[0].get())),
        )
        .await
        .unwrap();
    assert_eq!(variant(&mut tx, &target).await.len(), 2);
    assert_eq!(template_barcode(&mut tx, &target).await, Cell::Null);
    assert_eq!(
        own.cell(&mut tx, "barcode").await.unwrap(),
        Cell::Str("N2-MOVE".into())
    );
    assert_eq!(holders_of(&mut tx, "N2-MOVE").await, (1, 1));
}
