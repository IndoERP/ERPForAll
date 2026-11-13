//! Async candidate currency computes in same-call monetary admission ("Async
//! candidate currency computes"; task). Each case
//! covers the named portion (rounding) or (whole-recordset
//! admission) only; the broader mutation-path coverage stays in `monetary_mutation.rs`.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

use std::sync::Mutex;

use erp_sdk::prelude::*;
use erp_sdk::{ErrorKind, RawVals, Value};
use erp_test_models::base_users;
use erp_testkit::Harness;

/// `(tag of the batch's first record, batch length)` for every run of the candidate. Tests use
/// their own tags, so parallel tests do not see each other's runs.
static RUNS: Mutex<Vec<(String, usize)>> = Mutex::new(Vec::new());
/// `(tag, ids asked, currency ids SQL saw)` for the SQL probe.
type SqlSeen = (String, usize, Vec<Option<i64>>);
static SQL_SEEN: Mutex<Vec<SqlSeen>> = Mutex::new(Vec::new());

fn runs(tag: &str) -> Vec<usize> {
    RUNS.lock()
        .unwrap()
        .iter()
        .filter(|(t, _)| t == tag)
        .map(|(_, n)| *n)
        .collect()
}

mod fixture {
    use super::*;

    erp_sdk::addon!(name = "test_monetary_candidate", depends = ["base"]);

    #[model(
        name = "test_monetary_candidate.cand",
        addon = "test_monetary_candidate"
    )]
    pub struct Cand {
        #[field(char)]
        pub tag: Option<Arc<str>>,
        #[field(many2one, comodel = "res.currency")]
        pub selected_currency_id: Option<RecId>,
        #[field(
            many2one,
            comodel = "res.currency",
            compute = "_currency",
            store = true
        )]
        pub currency_id: Option<RecId>,
        #[field(monetary, currency_field = "currency_id")]
        pub amount: Decimal,
    }

    #[methods(Cand, addon = "test_monetary_candidate")]
    impl Cand {
        #[compute(candidate, depends = ["selected_currency_id", "tag"])]
        async fn _currency(
            this: &RecordSet<Cand>,
            tx: &mut ReadTx<'_>,
            out: &mut Assign<'_, Cand>,
        ) -> Result<()> {
            let tags = tx.mapped(this, Cand::TAG).await?;
            let selected = tx.mapped(this, Cand::SELECTED_CURRENCY_ID).await?;
            let first = tags
                .first()
                .cloned()
                .flatten()
                .map(|t| t.to_string())
                .unwrap_or_default();
            RUNS.lock().unwrap().push((first.clone(), this.len()));
            if first.starts_with("fail") {
                return Err(Error::programming("candidate provider failed"));
            }
            if first.starts_with("c-cancel") {
                // a suspension point inside the candidate, where the cancellation test drops the write
                tokio::task::yield_now().await;
            }
            if first.starts_with("undeclared") {
                // AMOUNT is not a declared dependency of this compute.
                tx.mapped(this, Cand::AMOUNT).await?;
            }
            if first.starts_with("sql") {
                let ids: Vec<i64> = this
                    .rec_ids()
                    .iter()
                    .filter(|id| !id.is_new())
                    .map(|id| id.get())
                    .collect();
                let rows = tx
                    .query(
                        "SELECT selected_currency_id FROM test_monetary_candidate_cand \
                         WHERE id = ANY($1) ORDER BY id",
                        &[&ids],
                    )
                    .await?;
                let seen = rows.iter().map(|r| r.get::<_, Option<i64>>(0)).collect();
                SQL_SEEN
                    .lock()
                    .unwrap()
                    .push((first.clone(), ids.len(), seen));
            }
            if let Some(statement) = first.strip_prefix("stmt:") {
                tx.query(statement, &[]).await?;
            }
            for (i, currency) in selected.into_iter().enumerate() {
                out.set(i, Cand::CURRENCY_ID, currency)?;
            }
            Ok(())
        }
    }

    /// Two monetary fields sharing one candidate currency.
    #[model(
        name = "test_monetary_candidate.twin",
        addon = "test_monetary_candidate"
    )]
    pub struct Twin {
        #[field(char)]
        pub tag: Option<Arc<str>>,
        #[field(many2one, comodel = "res.currency")]
        pub selected_currency_id: Option<RecId>,
        #[field(
            many2one,
            comodel = "res.currency",
            compute = "_currency",
            store = true
        )]
        pub currency_id: Option<RecId>,
        #[field(monetary, currency_field = "currency_id")]
        pub amount_a: Decimal,
        #[field(monetary, currency_field = "currency_id")]
        pub amount_b: Decimal,
    }

    #[methods(Twin, addon = "test_monetary_candidate")]
    impl Twin {
        #[compute(candidate, depends = ["selected_currency_id", "tag"])]
        async fn _currency(
            this: &RecordSet<Twin>,
            tx: &mut ReadTx<'_>,
            out: &mut Assign<'_, Twin>,
        ) -> Result<()> {
            let tags = tx.mapped(this, Twin::TAG).await?;
            let selected = tx.mapped(this, Twin::SELECTED_CURRENCY_ID).await?;
            let first = tags
                .first()
                .cloned()
                .flatten()
                .map(|t| t.to_string())
                .unwrap_or_default();
            RUNS.lock().unwrap().push((first, this.len()));
            for (i, currency) in selected.into_iter().enumerate() {
                out.set(i, Twin::CURRENCY_ID, currency)?;
            }
            Ok(())
        }
    }

    #[model(
        name = "test_monetary_candidate.owner",
        addon = "test_monetary_candidate"
    )]
    pub struct Owner {
        #[field(many2one, comodel = "res.currency")]
        pub currency_id: Option<RecId>,
    }

    /// Reads its owner's currency through the view; the compute runs as the user.
    #[model(
        name = "test_monetary_candidate.owned_off",
        addon = "test_monetary_candidate"
    )]
    pub struct OwnedOff {
        #[field(many2one, comodel = "test_monetary_candidate.owner")]
        pub owner_id: Option<RecId>,
        #[field(
            many2one,
            comodel = "res.currency",
            compute = "_currency",
            store = true,
            compute_sudo = false
        )]
        pub currency_id: Option<RecId>,
        #[field(monetary, currency_field = "currency_id")]
        pub amount: Decimal,
    }

    /// The same, running with `compute_sudo`.
    #[model(
        name = "test_monetary_candidate.owned_on",
        addon = "test_monetary_candidate"
    )]
    pub struct OwnedOn {
        #[field(many2one, comodel = "test_monetary_candidate.owner")]
        pub owner_id: Option<RecId>,
        #[field(
            many2one,
            comodel = "res.currency",
            compute = "_currency",
            store = true,
            compute_sudo = true
        )]
        pub currency_id: Option<RecId>,
        #[field(monetary, currency_field = "currency_id")]
        pub amount: Decimal,
    }

    async fn owner_currencies(
        env: &Env,
        owners: Vec<Option<RecId>>,
        tx: &mut ReadTx<'_>,
    ) -> Result<Vec<Option<RecId>>> {
        let mut out = Vec::new();
        for owner in owners {
            let Some(owner) = owner else {
                out.push(None);
                continue;
            };
            let records: RecordSet<Owner> = RecordSet::try_from(
                env.records("test_monetary_candidate.owner")?
                    .browse_ids([owner]),
            )?;
            out.push(tx.get(&records, Owner::CURRENCY_ID).await?);
        }
        Ok(out)
    }

    #[methods(OwnedOff, addon = "test_monetary_candidate")]
    impl OwnedOff {
        #[compute(candidate, depends = ["owner_id"])]
        async fn _currency(
            this: &RecordSet<OwnedOff>,
            tx: &mut ReadTx<'_>,
            out: &mut Assign<'_, OwnedOff>,
        ) -> Result<()> {
            let owners = tx.mapped(this, OwnedOff::OWNER_ID).await?;
            let env = this.as_records().env().clone();
            for (i, c) in owner_currencies(&env, owners, tx)
                .await?
                .into_iter()
                .enumerate()
            {
                out.set(i, OwnedOff::CURRENCY_ID, c)?;
            }
            Ok(())
        }
    }

    #[methods(OwnedOn, addon = "test_monetary_candidate")]
    impl OwnedOn {
        #[compute(candidate, depends = ["owner_id"])]
        async fn _currency(
            this: &RecordSet<OwnedOn>,
            tx: &mut ReadTx<'_>,
            out: &mut Assign<'_, OwnedOn>,
        ) -> Result<()> {
            let owners = tx.mapped(this, OwnedOn::OWNER_ID).await?;
            let env = this.as_records().env().clone();
            for (i, c) in owner_currencies(&env, owners, tx)
                .await?
                .into_iter()
                .enumerate()
            {
                out.set(i, OwnedOn::CURRENCY_ID, c)?;
            }
            Ok(())
        }
    }
}

async fn harness() -> Harness {
    Harness::builder([&base_users::ADDON, &erp_addon_base::ADDON, &fixture::ADDON])
        .build()
        .await
        .unwrap()
}

async fn harness_chunked(batch: usize) -> Harness {
    Harness::builder([&base_users::ADDON, &erp_addon_base::ADDON, &fixture::ADDON])
        .config(erp_orm::OrmConfig {
            recompute_batch: batch,
            ..Default::default()
        })
        .build()
        .await
        .unwrap()
}

fn dec(text: &str) -> Decimal {
    erp_sdk::money::parse_decimal(text).unwrap()
}

async fn currency(tx: &mut Transaction, code: &str, increment: &str) -> RecId {
    tx.env()
        .records("res.currency")
        .unwrap()
        .create(
            tx,
            vec![
                RawVals::new()
                    .set_cell("name", Cell::Str(code.into()))
                    .set_cell("symbol", Cell::Str(code.into()))
                    .set_cell("rounding", Cell::Decimal(dec(increment))),
            ],
        )
        .await
        .unwrap()
        .rec_ids()[0]
}

fn cand(tag: &str, currency: Option<RecId>, amount: &str) -> Vals<fixture::Cand> {
    Vals::new()
        .set(fixture::Cand::TAG, Some(Arc::<str>::from(tag)))
        .set(fixture::Cand::SELECTED_CURRENCY_ID, currency)
        .set(fixture::Cand::AMOUNT, dec(amount))
}

type State = Vec<(Decimal, Option<RecId>)>;

async fn cache_state(tx: &mut Transaction, rows: &RecordSet<fixture::Cand>) -> State {
    let amounts = rows.mapped(tx, fixture::Cand::AMOUNT).await.unwrap();
    let currencies = rows.mapped(tx, fixture::Cand::CURRENCY_ID).await.unwrap();
    amounts.into_iter().zip(currencies).collect()
}

/// The cache state, then the database state after flush and invalidation; they must agree.
async fn readback(tx: &mut Transaction, rows: &RecordSet<fixture::Cand>) -> State {
    let cached = cache_state(tx, rows).await;
    tx.flush().await.unwrap();
    tx.invalidate_all().await.unwrap();
    let stored = cache_state(tx, rows).await;
    assert_eq!(cached, stored, "cache and database readback differ");
    stored
}

fn model(tx: &Transaction) -> RecordSet<fixture::Cand> {
    tx.env().model::<fixture::Cand>().unwrap()
}

/// candidate currency, which runs once and before the INSERT)
#[tokio::test]
async fn create_paths_round_with_the_candidate_currency_once() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let cent = currency(&mut tx, "CC1", "0.01").await;
    // typed
    let typed = model(&tx)
        .create(&mut tx, vec![cand("once-typed", Some(cent), "2.675")])
        .await
        .unwrap();
    assert_eq!(runs("once-typed"), vec![1]);
    assert_eq!(
        readback(&mut tx, &typed).await,
        vec![(dec("2.68"), Some(cent))]
    );
    assert_eq!(
        runs("once-typed"),
        vec![1],
        "flush and readback recompute nothing"
    );
    // dynamic
    let dynamic = tx
        .env()
        .records("test_monetary_candidate.cand")
        .unwrap()
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("tag", Cell::Str("once-dynamic".into()))
                    .set_cell("selected_currency_id", Cell::Id(cent.get()))
                    .set_cell("amount", Cell::Decimal(dec("2.675"))),
            ],
        )
        .await
        .unwrap();
    assert_eq!(runs("once-dynamic"), vec![1]);
    let dynamic = RecordSet::<fixture::Cand>::try_from(dynamic).unwrap();
    assert_eq!(
        readback(&mut tx, &dynamic).await,
        vec![(dec("2.68"), Some(cent))]
    );
    // values map
    let values = [
        ("tag".to_owned(), Value::Str(Arc::from("once-values"))),
        ("selected_currency_id".to_owned(), Value::Int(cent.get())),
        ("amount".to_owned(), Value::Decimal(dec("2.675"))),
    ]
    .into_iter()
    .collect();
    let by_values = tx
        .env()
        .records("test_monetary_candidate.cand")
        .unwrap()
        .create_values(&mut tx, &[values])
        .await
        .unwrap();
    assert_eq!(runs("once-values"), vec![1]);
    let by_values = RecordSet::<fixture::Cand>::try_from(by_values).unwrap();
    assert_eq!(
        readback(&mut tx, &by_values).await,
        vec![(dec("2.68"), Some(cent))]
    );
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn defaulted_inputs_round_with_the_candidate_currency() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let unit = currency(&mut tx, "CD1", "1").await;
    let rows = tx
        .env()
        .records("test_monetary_candidate.cand")
        .unwrap()
        .with_context([
            ("default_tag", Value::Str(Arc::from("default"))),
            ("default_selected_currency_id", Value::Int(unit.get())),
            ("default_amount", Value::Decimal(dec("2.675"))),
        ])
        .create_values(&mut tx, &[std::collections::BTreeMap::new()])
        .await
        .unwrap();
    let rows = RecordSet::<fixture::Cand>::try_from(rows).unwrap();
    assert_eq!(runs("default"), vec![1]);
    assert_eq!(readback(&mut tx, &rows).await, vec![(dec("3"), Some(unit))]);
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn write_paths_round_with_the_changed_candidate_input_once() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let cent = currency(&mut tx, "CW1", "0.01").await;
    let unit = currency(&mut tx, "CW2", "1").await;
    let rows = model(&tx)
        .create(
            &mut tx,
            vec![
                cand("w-create-typed", Some(cent), "0"),
                cand("w-create-dyn", Some(cent), "0"),
            ],
        )
        .await
        .unwrap();
    let typed = rows.browse_ids([rows.rec_ids()[0]]);
    let typed = RecordSet::<fixture::Cand>::try_from(typed).unwrap();
    let dynamic =
        RecordSet::<fixture::Cand>::try_from(rows.browse_ids([rows.rec_ids()[1]])).unwrap();
    tx.flush().await.unwrap();
    // typed write: currency input and amount in one call
    typed
        .write(
            &mut tx,
            typed
                .vals()
                .set(fixture::Cand::TAG, Some(Arc::<str>::from("w-typed")))
                .set(fixture::Cand::SELECTED_CURRENCY_ID, Some(unit))
                .set(fixture::Cand::AMOUNT, dec("2.675")),
        )
        .await
        .unwrap();
    assert_eq!(runs("w-typed"), vec![1]);
    assert_eq!(
        readback(&mut tx, &typed).await,
        vec![(dec("3"), Some(unit))]
    );
    assert_eq!(runs("w-typed"), vec![1], "published once, not recomputed");
    // dynamic write
    dynamic
        .as_records()
        .write(
            &mut tx,
            RawVals::new()
                .set_cell("tag", Cell::Str("w-dyn".into()))
                .set_cell("selected_currency_id", Cell::Id(unit.get()))
                .set_cell("amount", Cell::Decimal(dec("2.675"))),
        )
        .await
        .unwrap();
    assert_eq!(runs("w-dyn"), vec![1]);
    assert_eq!(
        readback(&mut tx, &dynamic).await,
        vec![(dec("3"), Some(unit))]
    );
    assert_eq!(runs("w-dyn"), vec![1]);
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn unchanged_and_changed_dependency_subsets() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let cent = currency(&mut tx, "CS1", "0.01").await;
    let unit = currency(&mut tx, "CS2", "1").await;
    let rows = model(&tx)
        .create(
            &mut tx,
            vec![cand("sub", Some(cent), "0"), cand("sub", Some(cent), "0")],
        )
        .await
        .unwrap();
    tx.flush().await.unwrap();
    let before = runs("sub").len();
    // an amount alone, and the current input again, are not dependency changes
    rows.write(
        &mut tx,
        rows.vals().set(fixture::Cand::AMOUNT, dec("2.675")),
    )
    .await
    .unwrap();
    rows.write(
        &mut tx,
        rows.vals()
            .set(fixture::Cand::SELECTED_CURRENCY_ID, Some(cent))
            .set(fixture::Cand::AMOUNT, dec("1.005")),
    )
    .await
    .unwrap();
    assert_eq!(
        runs("sub").len(),
        before,
        "no dependency changed: no candidate run"
    );
    assert_eq!(
        readback(&mut tx, &rows).await,
        vec![(dec("1.01"), Some(cent)); 2]
    );
    // one record of the two changes its input
    let first = RecordSet::<fixture::Cand>::try_from(rows.browse_ids([rows.rec_ids()[0]])).unwrap();
    first
        .write(
            &mut tx,
            first
                .vals()
                .set(fixture::Cand::SELECTED_CURRENCY_ID, Some(unit))
                .set(fixture::Cand::AMOUNT, dec("2.675")),
        )
        .await
        .unwrap();
    assert_eq!(runs("sub")[before..], [1]);
    assert_eq!(
        readback(&mut tx, &rows).await,
        vec![(dec("3"), Some(unit)), (dec("1.01"), Some(cent))]
    );
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

/// per chunk and publish every record)
#[tokio::test]
async fn many_records_across_chunks() {
    let h = harness_chunked(2).await;
    let mut tx = h.begin().await.unwrap();
    let cent = currency(&mut tx, "CK1", "0.01").await;
    let unit = currency(&mut tx, "CK2", "1").await;
    let rows = model(&tx)
        .create(
            &mut tx,
            (0..5)
                .map(|_| cand("chunk-create", Some(cent), "2.675"))
                .collect(),
        )
        .await
        .unwrap();
    assert_eq!(runs("chunk-create"), vec![2, 2, 1]);
    assert_eq!(
        readback(&mut tx, &rows).await,
        vec![(dec("2.68"), Some(cent)); 5]
    );
    rows.write(
        &mut tx,
        rows.vals()
            .set(fixture::Cand::TAG, Some(Arc::<str>::from("chunk-write")))
            .set(fixture::Cand::SELECTED_CURRENCY_ID, Some(unit))
            .set(fixture::Cand::AMOUNT, dec("2.675")),
    )
    .await
    .unwrap();
    assert_eq!(runs("chunk-write"), vec![2, 2, 1]);
    assert_eq!(
        readback(&mut tx, &rows).await,
        vec![(dec("3"), Some(unit)); 5]
    );
    assert_eq!(
        runs("chunk-write"),
        vec![2, 2, 1],
        "published once, not recomputed"
    );
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

/// for the whole recordset across chunks and nothing is published)
#[tokio::test]
async fn mixed_candidate_currencies_are_refused_whole_and_publish_nothing() {
    let h = harness_chunked(2).await;
    let mut tx = h.begin().await.unwrap();
    let cent = currency(&mut tx, "CM1", "0.01").await;
    let unit = currency(&mut tx, "CM2", "1").await;
    let rows = model(&tx)
        .create(
            &mut tx,
            vec![
                cand("mix-create", Some(cent), "0"),
                cand("mix-create", Some(cent), "0"),
                cand("mix-create", Some(unit), "0"),
            ],
        )
        .await
        .unwrap();
    tx.flush().await.unwrap();
    let original = readback(&mut tx, &rows).await;
    // the last record resolves to another currency, in a later chunk
    let error = rows
        .write(
            &mut tx,
            rows.vals()
                .set(fixture::Cand::TAG, Some(Arc::<str>::from("mix-write")))
                .set(fixture::Cand::AMOUNT, dec("2.675")),
        )
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Validation);
    assert_eq!(
        error.message(),
        "Got multiple currencies while assigning values of monetary field test_monetary_candidate.cand.amount"
    );
    assert_eq!(
        runs("mix-write"),
        vec![2, 1],
        "every chunk ran before admission"
    );
    assert_eq!(cache_state(&mut tx, &rows).await, original);
    assert_eq!(
        rows.mapped(&mut tx, fixture::Cand::TAG).await.unwrap(),
        vec![Some(Arc::<str>::from("mix-create")); 3],
        "no input was published either"
    );
    // a create supplies each row its own amount, so each normalizes with its own candidate
    // currency; the mixed refusal belongs to one assigned amount
    let created = model(&tx)
        .create(
            &mut tx,
            vec![
                cand("mix-new", Some(cent), "2.675"),
                cand("mix-new", Some(unit), "2.675"),
                cand("mix-new", Some(cent), "2.675"),
            ],
        )
        .await
        .unwrap();
    assert_eq!(runs("mix-new"), vec![2, 1]);
    assert_eq!(
        readback(&mut tx, &created).await,
        vec![
            (dec("2.68"), Some(cent)),
            (dec("3"), Some(unit)),
            (dec("2.68"), Some(cent))
        ]
    );
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

/// absent amounts need no currency)
#[tokio::test]
async fn null_currency_and_zero_or_absent_amounts() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let cent = currency(&mut tx, "CN1", "0.01").await;
    let none = model(&tx)
        .create(&mut tx, vec![cand("null-cur", None, "2.675")])
        .await
        .unwrap();
    assert_eq!(readback(&mut tx, &none).await, vec![(dec("2.675"), None)]);
    let zero = model(&tx)
        .create(&mut tx, vec![cand("zero-amt", Some(cent), "0")])
        .await
        .unwrap();
    assert_eq!(readback(&mut tx, &zero).await, vec![(dec("0"), Some(cent))]);
    let absent = model(&tx)
        .create(
            &mut tx,
            vec![
                Vals::new()
                    .set(fixture::Cand::TAG, Some(Arc::<str>::from("absent-amt")))
                    .set(fixture::Cand::SELECTED_CURRENCY_ID, Some(cent)),
            ],
        )
        .await
        .unwrap();
    assert_eq!(
        readback(&mut tx, &absent).await,
        vec![(dec("0"), Some(cent))]
    );
    assert_eq!(runs("absent-amt"), vec![1], "still exactly one run");
    // switching the currency away: the amount keeps its validated precision
    zero.write(
        &mut tx,
        zero.vals()
            .set(fixture::Cand::SELECTED_CURRENCY_ID, None::<RecId>)
            .set(fixture::Cand::AMOUNT, dec("1.005")),
    )
    .await
    .unwrap();
    assert_eq!(readback(&mut tx, &zero).await, vec![(dec("1.005"), None)]);
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn provider_error_leaves_nothing_published() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let cent = currency(&mut tx, "CE1", "0.01").await;
    let unit = currency(&mut tx, "CE2", "1").await;
    let rows = model(&tx)
        .create(&mut tx, vec![cand("err-ok", Some(cent), "2.675")])
        .await
        .unwrap();
    let original = readback(&mut tx, &rows).await;
    let error = rows
        .write(
            &mut tx,
            rows.vals()
                .set(fixture::Cand::TAG, Some(Arc::<str>::from("fail-write")))
                .set(fixture::Cand::SELECTED_CURRENCY_ID, Some(unit))
                .set(fixture::Cand::AMOUNT, dec("9.999")),
        )
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Programming);
    assert_eq!(error.message(), "candidate provider failed");
    assert_eq!(runs("fail-write"), vec![1]);
    assert_eq!(readback(&mut tx, &rows).await, original);
    assert_eq!(
        rows.mapped(&mut tx, fixture::Cand::TAG).await.unwrap(),
        vec![Some(Arc::<str>::from("err-ok"))]
    );
    let before = model(&tx)
        .search_count(&mut tx, &Domain::TRUE, None)
        .await
        .unwrap();
    let error = model(&tx)
        .create(&mut tx, vec![cand("fail-create", Some(cent), "2.675")])
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Programming);
    assert_eq!(error.message(), "candidate provider failed");
    assert_eq!(
        model(&tx)
            .search_count(&mut tx, &Domain::TRUE, None)
            .await
            .unwrap(),
        before,
        "nothing was inserted"
    );
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

/// pre-mutation database, and undeclared batch fields are refused)
#[tokio::test]
async fn view_visibility_rules() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let cent = currency(&mut tx, "CV1", "0.01").await;
    let unit = currency(&mut tx, "CV2", "1").await;
    let rows = model(&tx)
        .create(&mut tx, vec![cand("sql-create", Some(cent), "2.675")])
        .await
        .unwrap();
    // before INSERT the row has no id in the database: SQL saw no row
    assert_eq!(
        SQL_SEEN
            .lock()
            .unwrap()
            .iter()
            .filter(|(t, _, _)| t == "sql-create")
            .map(|(_, asked, seen)| (*asked, seen.clone()))
            .collect::<Vec<_>>(),
        vec![(0, vec![])]
    );
    tx.flush().await.unwrap();
    rows.write(
        &mut tx,
        rows.vals()
            .set(fixture::Cand::TAG, Some(Arc::<str>::from("sql-write")))
            .set(fixture::Cand::SELECTED_CURRENCY_ID, Some(unit))
            .set(fixture::Cand::AMOUNT, dec("2.675")),
    )
    .await
    .unwrap();
    // the ORM read saw the staged `unit` (the amount rounds to 3); SQL saw the stored `cent`
    assert_eq!(readback(&mut tx, &rows).await, vec![(dec("3"), Some(unit))]);
    assert_eq!(
        SQL_SEEN
            .lock()
            .unwrap()
            .iter()
            .filter(|(t, _, _)| t == "sql-write")
            .map(|(_, asked, seen)| (*asked, seen.clone()))
            .collect::<Vec<_>>(),
        vec![(1, vec![Some(cent.get())])]
    );
    // a field of the batch that is not declared is refused, whatever the value
    let error = model(&tx)
        .create(&mut tx, vec![cand("undeclared", Some(cent), "2.675")])
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Programming);
    assert_eq!(
        error.message(),
        "a candidate compute read test_monetary_candidate.cand.amount of a record in its own batch, which is not among its declared depends or reads: declare it"
    );
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

/// and the listed effectful functions; it is a lexical deny-list, not a proof of read-only)
#[tokio::test]
async fn the_view_refuses_non_select_sql() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let cent = currency(&mut tx, "CQ1", "0.01").await;
    let not_select = "a candidate compute may only run SELECT statements through its read-only view: the statement is not a plain SELECT/WITH/SHOW/EXPLAIN, changes data, or cannot be read with certainty";
    let cases: [(&str, String); 10] = [
        ("DELETE FROM test_monetary_candidate_cand", not_select.to_owned()),
        (
            "SELECT id FROM test_monetary_candidate_cand FOR UPDATE",
            not_select.to_owned(),
        ),
        ("SELECT 1 INTO candidate_tmp_x", not_select.to_owned()),
        (
            "SELECT set_config('candidate.x', '1', false)",
            "a candidate compute may only run SELECT statements through its read-only view: set_config() has an effect that outlives the call".to_owned(),
        ),
        (
            "SELECT setval('test_monetary_candidate_seq', 1)",
            refusal_fn("setval"),
        ),
        (
            "SELECT nextval('test_monetary_candidate_seq')",
            refusal_fn("nextval"),
        ),
        // a comment between the name and the parenthesis is still a call
        (
            "SELECT nextval/**/('test_monetary_candidate_seq')",
            refusal_fn("nextval"),
        ),
        (
            "SELECT setval/* a /* nested */ b */ ('test_monetary_candidate_seq', 1)",
            refusal_fn("setval"),
        ),
        ("SELECT pg_sleep(0)", refusal_fn("pg_sleep")),
        ("SELECT lo_unlink(1)", refusal_fn("lo_unlink")),
    ];
    // identifiers that merely start like a large-object function are not calls
    let allowed = model(&tx)
        .create(
            &mut tx,
            vec![cand(
                "stmt:SELECT 1 AS lo_limit, 2 AS \"lo_quoted\", 3 AS \"nextval\"",
                Some(cent),
                "2.675",
            )],
        )
        .await
        .unwrap();
    assert_eq!(
        allowed
            .mapped(&mut tx, fixture::Cand::AMOUNT)
            .await
            .unwrap(),
        vec![dec("2.68")]
    );
    for (n, (statement, message)) in cases.into_iter().enumerate() {
        let error = model(&tx)
            .create(
                &mut tx,
                vec![cand(&format!("stmt:{statement}"), Some(cent), "2.675")],
            )
            .await
            .unwrap_err();
        assert_eq!(
            error.kind(),
            ErrorKind::Programming,
            "case {n}: {statement}"
        );
        assert_eq!(error.message(), message, "case {n}: {statement}");
    }
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

fn refusal_fn(function: &str) -> String {
    format!(
        "a candidate compute may only run SELECT statements through its read-only view: {function}() changes sequence or large-object state or waits, which a read-only view does not allow"
    )
}

#[tokio::test]
async fn two_amounts_sharing_a_candidate_currency_run_it_once() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let cent = currency(&mut tx, "CT1", "0.01").await;
    let unit = currency(&mut tx, "CT2", "1").await;
    let twin = tx.env().model::<fixture::Twin>().unwrap();
    let rows = twin
        .create(
            &mut tx,
            vec![
                twin.vals()
                    .set(fixture::Twin::TAG, Some(Arc::<str>::from("twin-create")))
                    .set(fixture::Twin::SELECTED_CURRENCY_ID, Some(cent))
                    .set(fixture::Twin::AMOUNT_A, dec("2.675"))
                    .set(fixture::Twin::AMOUNT_B, dec("1.005")),
            ],
        )
        .await
        .unwrap();
    assert_eq!(runs("twin-create"), vec![1]);
    rows.write(
        &mut tx,
        rows.vals()
            .set(fixture::Twin::TAG, Some(Arc::<str>::from("twin-write")))
            .set(fixture::Twin::SELECTED_CURRENCY_ID, Some(unit))
            .set(fixture::Twin::AMOUNT_A, dec("2.675"))
            .set(fixture::Twin::AMOUNT_B, dec("1.505")),
    )
    .await
    .unwrap();
    assert_eq!(runs("twin-write"), vec![1]);
    tx.flush().await.unwrap();
    tx.invalidate_all().await.unwrap();
    assert_eq!(
        rows.mapped(&mut tx, fixture::Twin::AMOUNT_A).await.unwrap(),
        vec![dec("3")]
    );
    assert_eq!(
        rows.mapped(&mut tx, fixture::Twin::AMOUNT_B).await.unwrap(),
        vec![dec("2")]
    );
    assert_eq!(runs("twin-write"), vec![1]);
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

async fn sudo_harness() -> Harness {
    let policy = erp_testkit::DomainPolicy::builder(erp_testkit::Otherwise::Allow)
        .rule(
            "test_monetary_candidate.owner",
            erp_sdk::Operation::Read,
            Domain::FALSE,
        )
        .rule(
            "test_monetary_candidate.cand",
            erp_sdk::Operation::Write,
            Domain::FALSE,
        )
        .build()
        .unwrap();
    Harness::builder([&base_users::ADDON, &erp_addon_base::ADDON, &fixture::ADDON])
        .policy(policy)
        .build()
        .await
        .unwrap()
}

/// refuses with Access; the currency lookup elevation does not widen either)
#[tokio::test]
async fn compute_sudo_decides_denied_inputs() {
    let h = sudo_harness().await;
    let mut tx = h.begin().await.unwrap();
    tx.batch("INSERT INTO res_users(id,name,login,active,share) VALUES (42,'Candidate reader','candidate-reader',true,false)")
        .await
        .unwrap();
    let cent = currency(&mut tx, "CU1", "0.01").await;
    let owner = tx
        .env()
        .records("test_monetary_candidate.owner")
        .unwrap()
        .create(
            &mut tx,
            vec![RawVals::new().set_cell("currency_id", Cell::Id(cent.get()))],
        )
        .await
        .unwrap();
    let owner_id = owner.rec_ids()[0];
    // sudo compute: the user needs no read right on the owner
    let on = tx
        .env()
        .records("test_monetary_candidate.owned_on")
        .unwrap()
        .with_user(42)
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("owner_id", Cell::Id(owner_id.get()))
                    .set_cell("amount", Cell::Decimal(dec("2.675"))),
            ],
        )
        .await
        .unwrap()
        .sudo(true);
    assert_eq!(
        on.values(&mut tx, "amount").await.unwrap(),
        vec![Value::Decimal(dec("2.68"))]
    );
    assert_eq!(
        on.values(&mut tx, "currency_id").await.unwrap(),
        vec![Value::Int(cent.get())]
    );
    // non-sudo compute: the caller's denied read is an Access error, nothing is inserted
    let error = tx
        .env()
        .records("test_monetary_candidate.owned_off")
        .unwrap()
        .with_user(42)
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("owner_id", Cell::Id(owner_id.get()))
                    .set_cell("amount", Cell::Decimal(dec("2.675"))),
            ],
        )
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Access);
    assert!(
        error.message().contains("test_monetary_candidate.owner"),
        "{}",
        error.message()
    );
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn denied_amount_write_precedes_the_candidate() {
    let h = sudo_harness().await;
    let mut tx = h.begin().await.unwrap();
    tx.batch("INSERT INTO res_users(id,name,login,active,share) VALUES (42,'Candidate writer','candidate-writer',true,false)")
        .await
        .unwrap();
    let cent = currency(&mut tx, "CX1", "0.01").await;
    let unit = currency(&mut tx, "CX2", "1").await;
    let rows = model(&tx)
        .create(&mut tx, vec![cand("denied", Some(cent), "2.675")])
        .await
        .unwrap();
    tx.flush().await.unwrap();
    let before = runs("denied-write").len();
    let error = rows
        .with_user(42)
        .write(
            &mut tx,
            rows.vals()
                .set(fixture::Cand::TAG, Some(Arc::<str>::from("denied-write")))
                .set(fixture::Cand::SELECTED_CURRENCY_ID, Some(unit))
                .set(fixture::Cand::AMOUNT, dec("1.005")),
        )
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Access);
    assert_eq!(
        error.message(),
        "You are not allowed to write 'test_monetary_candidate.cand' (test_monetary_candidate.cand) records."
    );
    assert_eq!(
        runs("denied-write").len(),
        before,
        "the candidate never ran"
    );
    assert_eq!(
        cache_state(&mut tx, &rows).await,
        vec![(dec("2.68"), Some(cent))]
    );
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

/// rolled back, leaves no candidate result that a later write or recompute could reuse)
#[tokio::test]
async fn a_cancelled_candidate_admission_leaves_nothing_to_reuse() {
    use std::future::{Future, poll_fn};
    use std::task::Poll;

    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let mut cancelled_at_least_once = false;
    for budget in 1..=60usize {
        // the rollback below removes the currencies too, so each round creates its own
        let cent = currency(&mut tx, "CC1", "0.01").await;
        let unit = currency(&mut tx, "CC2", "1").await;
        let rows = model(&tx)
            .create(&mut tx, vec![cand("c-setup", Some(cent), "0")])
            .await
            .unwrap();
        tx.flush().await.unwrap();
        let tag = format!("c-cancel-{budget}");
        let finished = {
            let vals = rows
                .vals()
                .set(fixture::Cand::TAG, Some(Arc::<str>::from(tag.as_str())))
                .set(fixture::Cand::SELECTED_CURRENCY_ID, Some(unit))
                .set(fixture::Cand::AMOUNT, dec("2.675"));
            let mut write = Box::pin(rows.write(&mut tx, vals));
            let mut polls = 0usize;
            poll_fn(|cx| {
                if polls == budget {
                    return Poll::Ready(false);
                }
                polls += 1;
                match write.as_mut().poll(cx) {
                    Poll::Ready(result) => {
                        result.unwrap();
                        Poll::Ready(true)
                    }
                    Poll::Pending => Poll::Pending,
                }
            })
            .await
            // the unfinished write future is dropped here
        };
        cancelled_at_least_once |= !finished;
        tx.rollback().await.unwrap();
        if finished {
            break;
        }
        // after the rollback the same shape of write rounds with the new currency, from a
        // candidate run of its own
        let cent = currency(&mut tx, "CC1", "0.01").await;
        let unit = currency(&mut tx, "CC2", "1").await;
        let fresh = model(&tx)
            .create(&mut tx, vec![cand("c-after", Some(cent), "0")])
            .await
            .unwrap();
        fresh
            .write(
                &mut tx,
                fresh
                    .vals()
                    .set(fixture::Cand::TAG, Some(Arc::<str>::from("c-after-w")))
                    .set(fixture::Cand::SELECTED_CURRENCY_ID, Some(unit))
                    .set(fixture::Cand::AMOUNT, dec("2.675")),
            )
            .await
            .unwrap();
        assert_eq!(runs("c-after-w"), vec![1], "budget {budget}");
        assert_eq!(
            readback(&mut tx, &fresh).await,
            vec![(dec("3"), Some(unit))],
            "budget {budget}"
        );
        tx.rollback().await.unwrap();
        RUNS.lock().unwrap().retain(|(t, _)| t != "c-after-w");
    }
    assert!(cancelled_at_least_once, "no await point was cancelled");
    h.cleanup().await.unwrap();
}
