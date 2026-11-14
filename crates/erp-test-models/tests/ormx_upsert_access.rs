//! Upsert on the real access engine (with;
//! rows read by the real `PolicySource`, the principals come from the real `Resolver`, and the
//! texts are the engine's SEC-008 and SEC-009 messages. Every compared value is what our code
//! returns; the message templates are the spec's.
#![allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]

use std::sync::Arc;

use erp_addon_base::IrAccess;
use erp_orm::{
    ApiKeyId, MethodAllow, ModelAllow, Operation, Orm, PolicyProvider, PrincipalBuilder, Scope,
    UserId,
};
use erp_sdk::{ErrorKind, RecId, Transaction, Upsert, UpsertOutcome, Vals};
use erp_security::resolver::Resolver;
use erp_security::source::PolicySource;
use erp_test_models::base_users;
use erp_test_models::test_scale_upsert::{
    self as scale, ScaleCompany, ScaleLocation, ScaleProduct, ScaleQuant,
};
use erp_testkit::tokio_postgres::types::ToSql;
use erp_testkit::tokio_postgres::{Client, NoTls};
use erp_testkit::{Harness, assert_query_count};

/// Business key on `ir.access` for the case: an access is named once per model.
/// `ir.access` is a policy table (`PolicySourceWrite` of `base`, bump trigger on the table), so an
/// upsert on it is a policy write.
mod access_key {
    use erp_addon_base::IrAccess;
    use erp_sdk::prelude::*;

    erp_sdk::addon!(
        name = "test_scale_upsert_access",
        depends = ["base", "test_scale_upsert"]
    );

    /// The extension carrying the key.
    #[extend(
        IrAccess,
        addon = "test_scale_upsert_access",
        keys = [key(
            name = "_access_key",
            fields = ["model_id", "name"],
            message = "An access of this name already exists for the model."
        )]
    )]
    pub struct AccessKeys;
}

use access_key::AccessKeys;

async fn harness() -> Harness {
    Harness::builder([
        &erp_addon_base::ADDON,
        &erp_data::model_data::ADDON,
        &base_users::ADDON,
        &scale::ADDON,
        &access_key::ADDON,
    ])
    .with_decls([erp_data::ir_model_data_decl()])
    .build()
    .await
    .expect("harness")
}

async fn connect(h: &Harness) -> Client {
    let (client, connection) = h.db().config().connect(NoTls).await.unwrap();
    tokio::spawn(async move {
        if let Err(error) = connection.await {
            eprintln!("ormx_upsert_access: connection closed with an error: {error}");
        }
    });
    client
}

async fn id_of(client: &Client, sql: &str, params: &[&(dyn ToSql + Sync)]) -> i64 {
    client.query_one(sql, params).await.unwrap().get(0)
}

/// One `ir_access` row: a permission of `group`, or a restriction for everyone (`None`).
struct Rule {
    name: &'static str,
    group: Option<i64>,
    operation: &'static str,
    domain: Option<String>,
}

fn permission(name: &'static str, group: i64, operation: &'static str) -> Rule {
    Rule {
        name,
        group: Some(group),
        operation,
        domain: None,
    }
}

fn restriction(name: &'static str, operation: &'static str, domain: String) -> Rule {
    Rule {
        name,
        group: None,
        operation,
        domain: Some(domain),
    }
}

/// The world of the cases: three users in three groups, `scale.quant` in `ir_model`, and three
/// quants Q1 (quantity 3), Q2 and Q3 on two locations.
struct Fixture {
    h: Harness,
    client: Client,
    /// "Quant Editors": create, read, update and delete quants (user U).
    editors: i64,
    /// "Quant Creators": create and read (user X).
    creators: i64,
    /// "Quant Writers": create and update, no read (user V).
    blind: i64,
    u: i64,
    v: i64,
    x: i64,
    model: i64,
    locations: Vec<RecId>,
    products: Vec<RecId>,
    quants: Vec<RecId>,
}

const U_NAME: &str = "Ursula";
const V_NAME: &str = "Victor";

async fn fixture() -> Fixture {
    let h = harness().await;
    let client = connect(&h).await;
    let group = async |name: &str| -> i64 {
        id_of(
            &client,
            "INSERT INTO res_groups (name) VALUES ($1) RETURNING id::int8",
            &[&name],
        )
        .await
    };
    let internal = group("Internal User").await;
    client
        .execute(
            "INSERT INTO ir_model_data (module, name, model, res_id) \
             VALUES ('base', 'group_user', 'res.groups', $1)",
            &[&internal],
        )
        .await
        .unwrap();
    let editors = group("Quant Editors").await;
    let creators = group("Quant Creators").await;
    let blind = group("Quant Writers").await;
    let mut users = Vec::new();
    for (name, login, groups) in [
        (U_NAME, "u", vec![internal, editors]),
        (V_NAME, "v", vec![internal, blind]),
        ("Xavier", "x", vec![internal, creators]),
    ] {
        let id = id_of(
            &client,
            "INSERT INTO res_users (name, login, active, share) VALUES ($1, $2, true, false) \
             RETURNING id::int8",
            &[&name, &login],
        )
        .await;
        for gid in groups {
            client
                .execute(
                    "INSERT INTO res_groups_users_rel (uid, gid) VALUES ($1, $2)",
                    &[&id, &gid],
                )
                .await
                .unwrap();
        }
        users.push(id);
    }
    let model = id_of(
        &client,
        "INSERT INTO ir_model (name, model, state, transient) \
         VALUES ('Scale Quant', 'scale.quant', 'base', false) RETURNING id::int8",
        &[],
    )
    .await;
    // the quants: Q1 (P0, L0), Q2 (P1, L1), Q3 (P2, L1), all of quantity 3
    let (locations, products, quants) = h
        .run(async |tx| {
            let env = tx.env();
            let companies = env
                .model::<ScaleCompany>()?
                .create(
                    tx,
                    ["C1", "C2"]
                        .into_iter()
                        .map(|n| Vals::new().set(ScaleCompany::NAME, n))
                        .collect(),
                )
                .await?
                .rec_ids()
                .to_vec();
            let locations = env
                .model::<ScaleLocation>()?
                .create(
                    tx,
                    companies
                        .iter()
                        .enumerate()
                        .map(|(i, c)| {
                            Vals::new()
                                .set(ScaleLocation::NAME, format!("L{i}"))
                                .set(ScaleLocation::COMPANY_ID, *c)
                        })
                        .collect(),
                )
                .await?
                .rec_ids()
                .to_vec();
            let products = env
                .model::<ScaleProduct>()?
                .create(
                    tx,
                    (0..4)
                        .map(|i| Vals::new().set(ScaleProduct::NAME, format!("P{i}")))
                        .collect(),
                )
                .await?
                .rec_ids()
                .to_vec();
            let quants = env
                .model::<ScaleQuant>()?
                .upsert(
                    tx,
                    Upsert::on(ScaleQuant::QUANT_KEY).rows([
                        key_row(products[0], locations[0]).set(ScaleQuant::QUANTITY, 3.0),
                        key_row(products[1], locations[1]).set(ScaleQuant::QUANTITY, 3.0),
                        key_row(products[2], locations[1]).set(ScaleQuant::QUANTITY, 3.0),
                    ]),
                )
                .await?
                .into_result()
                .ids();
            Ok((locations, products, quants))
        })
        .await
        .unwrap();
    Fixture {
        h,
        client,
        editors,
        creators,
        blind,
        u: users[0],
        v: users[1],
        x: users[2],
        model,
        locations,
        products,
        quants,
    }
}

fn key_row(product: RecId, location: RecId) -> Vals<ScaleQuant> {
    Vals::new()
        .set(ScaleQuant::PRODUCT_ID, product)
        .set(ScaleQuant::LOCATION_ID, location)
        .set(ScaleQuant::LOT_ID, None::<RecId>)
}

fn quants(tx: &Transaction) -> erp_sdk::RecordSet<ScaleQuant> {
    tx.env().model::<ScaleQuant>().unwrap()
}

impl Fixture {
    /// Replace the quant accesses with `rules` and return an ORM deciding with the policy now in
    /// the database, resolving principals through the real resolver.
    async fn orm(&self, rules: &[Rule]) -> Orm {
        self.client
            .execute("DELETE FROM ir_access WHERE model_id = $1", &[&self.model])
            .await
            .unwrap();
        for rule in rules {
            self.client
                .execute(
                    "INSERT INTO ir_access (name, model_id, group_id, operation, domain, active) \
                     VALUES ($1, $2, $3, $4, $5, true)",
                    &[
                        &rule.name,
                        &self.model,
                        &rule.group,
                        &rule.operation,
                        &rule.domain,
                    ],
                )
                .await
                .unwrap();
        }
        let pool = || erp_orm::pool::create_pool(self.h.db().config(), 2).unwrap();
        let source = PolicySource::build(Arc::clone(self.h.registry()), pool())
            .await
            .unwrap();
        let provider: Arc<dyn PolicyProvider> = Arc::new(source);
        Orm::builder(Arc::clone(self.h.registry()), pool())
            .policy(provider)
            .resolver(Resolver::id_order())
            .build()
            .unwrap()
    }

    async fn begin(&self, orm: &Orm, user: i64) -> Transaction {
        orm.begin(UserId::new(user).unwrap()).await.unwrap()
    }

    /// The stored `(quantity, row_version)` of every quant, in id order.
    async fn stored(&self) -> Vec<(f64, i64)> {
        self.client
            .query(
                "SELECT quantity, row_version FROM scale_quant ORDER BY id",
                &[],
            )
            .await
            .unwrap()
            .iter()
            .map(|r| (r.get(0), r.get(1)))
            .collect()
    }

    async fn quant_rows(&self) -> i64 {
        id_of(&self.client, "SELECT count(*)::int8 FROM scale_quant", &[]).await
    }
}

/// `[('id', 'in', [ids])]`.
fn ids_in(ids: &[RecId]) -> String {
    let list: Vec<String> = ids.iter().map(|i| i.get().to_string()).collect();
    format!("[('id', 'in', [{}])]", list.join(", "))
}

/// The SEC-009 record-level text (normal mode) of `op` for `user`.
fn record_refused(name: &str, user: i64, op: &str) -> String {
    format!(
        "Access rules protect some of these records.\n\n{name} (id={user}) has no '{op}' access to:\n- Scale Quant (scale.quant)\n\nAsk an administrator if you need access to these records."
    )
}

/// The SEC-008 model-level text of `verb` for an operation the listed groups allow.
fn model_refused(verb: &str, groups: &[&str]) -> String {
    let list: Vec<String> = groups.iter().map(|g| format!("\t- {g}")).collect();
    format!(
        "Your access rights do not let you {verb} 'Scale Quant' (scale.quant) records.\n\nGroups that may do this:\n{}\n\nAsk an administrator if you need this access.",
        list.join("\n")
    )
}

/// existing rows before the statement, the create check on the inserted rows after it, the
/// SEC-008 and SEC-009 texts of `explain`, the `read_only` API-key scope at the model check; the
/// create check is a separate statement (not fused into the INSERT), and a field outside the
/// caller's write mask failing with the SEC-023 text is's)
#[tokio::test]
async fn spec_ormx_008_upsert_access_in_create_and_write_shapes() {
    let f = fixture().await;
    let q1 = f.quants[0];
    let orm = f
        .orm(&[
            permission("editors", f.editors, "crud"),
            permission("creators", f.creators, "cr"),
            restriction("write only Q1", "u", ids_in(&[q1])),
            restriction(
                "no negative quantity",
                "c",
                "[('quantity', '>=', 0)]".to_owned(),
            ),
        ])
        .await;
    let key0 = || key_row(f.products[0], f.locations[0]);

    // (a) Q1 (writable) and a new key with quantity 1: pre-check, upsert, create check
    let mut tx = f.begin(&orm, f.u).await;
    let model = quants(&tx);
    let out = assert_query_count!(tx, >= 3, <= 3, {
        model
            .upsert(
                &mut tx,
                Upsert::on(ScaleQuant::QUANT_KEY)
                    .rows([
                        key0().set(ScaleQuant::QUANTITY, 2.0),
                        key_row(f.products[3], f.locations[0]).set(ScaleQuant::QUANTITY, 1.0),
                    ])
                    .accumulate(ScaleQuant::QUANTITY),
            )
            .await
            .unwrap()
    });
    assert_eq!(out.rows()[0].id(), q1);
    assert_eq!(out.rows()[0].outcome(), UpsertOutcome::Updated);
    assert_eq!(out.rows()[1].outcome(), UpsertOutcome::Inserted);
    tx.rollback().await.unwrap();
    drop(tx);

    // (b) Q1 and Q2 (not writable): refused before the upsert statement with the SEC-009 text,
    // nothing written
    let mut tx = f.begin(&orm, f.u).await;
    let model = quants(&tx);
    tx.capture_statements(true);
    let err = model
        .upsert(
            &mut tx,
            Upsert::on(ScaleQuant::QUANT_KEY)
                .rows([
                    key0().set(ScaleQuant::QUANTITY, 2.0),
                    key_row(f.products[1], f.locations[1]).set(ScaleQuant::QUANTITY, 2.0),
                ])
                .accumulate(ScaleQuant::QUANTITY),
        )
        .await
        .unwrap_err();
    tx.capture_statements(false);
    assert_eq!(err.kind(), ErrorKind::Access);
    assert_eq!(err.message(), record_refused(U_NAME, f.u, "write"));
    assert!(
        tx.captured()
            .iter()
            .all(|s| !s.contains("INSERT INTO \"scale_quant\"")),
        "the upsert statement was not sent: {:#?}",
        tx.captured()
    );
    assert!(!tx.is_poisoned(), "nothing was written");
    tx.rollback().await.unwrap();
    drop(tx);
    assert_eq!(f.stored().await, vec![(3.0, 1); 3]);

    // (c) a new key with quantity -1: refused after the statement by the create check; the
    // transaction cannot commit and nothing remains after the rollback
    let mut tx = f.begin(&orm, f.u).await;
    let model = quants(&tx);
    let err = model
        .upsert(
            &mut tx,
            Upsert::on(ScaleQuant::QUANT_KEY)
                .rows([key_row(f.products[3], f.locations[0]).set(ScaleQuant::QUANTITY, -1.0)])
                .accumulate(ScaleQuant::QUANTITY),
        )
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Access);
    assert_eq!(err.message(), record_refused(U_NAME, f.u, "create"));
    assert!(tx.is_poisoned());
    let commit = tx.commit().await.unwrap_err();
    assert_eq!(commit.kind(), ErrorKind::Access);
    tx.rollback().await.unwrap();
    drop(tx);
    assert_eq!(f.quant_rows().await, 3, "the refused row was rolled back");

    // a user who may create but not write fails at the model check, before any statement
    let mut tx = f.begin(&orm, f.x).await;
    let model = quants(&tx);
    let err = assert_query_count!(tx, <= 0, {
        model
            .upsert(
                &mut tx,
                Upsert::on(ScaleQuant::QUANT_KEY).rows([key0().set(ScaleQuant::QUANTITY, 1.0)]),
            )
            .await
            .unwrap_err()
    });
    assert_eq!(err.kind(), ErrorKind::Access);
    assert_eq!(err.message(), model_refused("modify", &["Quant Editors"]));
    tx.rollback().await.unwrap();
    drop(tx);

    // (d) an API key scoped read-only fails at the model check with the engine's scope text
    let scope = Scope::builder(ApiKeyId::new(7).unwrap(), "ci key")
        .models(ModelAllow::All)
        .methods(MethodAllow::All)
        .read_only(true)
        .build()
        .unwrap();
    let principal = PrincipalBuilder::user(UserId::new(f.u).unwrap())
        .scope(scope)
        .build()
        .unwrap();
    let mut tx = f.begin(&orm, f.u).await;
    let scoped = tx.env().with_principal_for_tests(principal);
    let model = scoped.model::<ScaleQuant>().unwrap();
    tx.capture_statements(true);
    let err = model
        .upsert(
            &mut tx,
            Upsert::on(ScaleQuant::QUANT_KEY).rows([key0().set(ScaleQuant::QUANTITY, 1.0)]),
        )
        .await
        .unwrap_err();
    tx.capture_statements(false);
    assert_eq!(err.kind(), ErrorKind::Access);
    assert_eq!(
        err.message(),
        "The API key 'ci key' is read-only: it may not create 'Scale Quant' (scale.quant)."
    );
    assert!(
        tx.captured().iter().all(|s| !s.contains("scale_quant")),
        "no statement touched the model: {:#?}",
        tx.captured()
    );
    tx.rollback().await.unwrap();
    drop(tx);
    assert_eq!(f.stored().await, vec![(3.0, 1); 3]);
    drop(orm);
    f.h.cleanup().await.unwrap();
}

/// read restriction or by the model check: the key's declared message without any id, and the
/// readable denied rows named alone by the engine's SEC-009 text; the rest of the case is in the
/// test above)
#[tokio::test]
async fn spec_ormx_008_unreadable_existing_rows_are_never_named() {
    let f = fixture().await;
    let (q1, q2) = (f.quants[0], f.quants[1]);
    // U writes Q1 only and reads Q1 and Q2; V writes Q1 only and may not read the model at all
    let orm = f
        .orm(&[
            permission("editors", f.editors, "crud"),
            permission("writers", f.blind, "cu"),
            restriction("write only Q1", "u", ids_in(&[q1])),
            restriction("read Q1 and Q2", "r", ids_in(&[q1, q2])),
        ])
        .await;
    let rows = [
        key_row(f.products[0], f.locations[0]),
        key_row(f.products[1], f.locations[1]),
        key_row(f.products[2], f.locations[1]),
    ];
    let spec = f.h.registry().model("scale.quant").unwrap().clone();
    let hidden = format!(
        "The database refused the change: {}",
        spec.business_key("_quant_key").unwrap().message
    );
    let upsert = async |tx: &mut Transaction, picked: &[usize]| {
        quants(tx)
            .upsert(
                tx,
                Upsert::on(ScaleQuant::QUANT_KEY)
                    .rows(
                        picked
                            .iter()
                            .map(|&i| rows[i].clone().set(ScaleQuant::QUANTITY, 2.0)),
                    )
                    .accumulate(ScaleQuant::QUANTITY),
            )
            .await
    };

    // Q1 (writable) with Q3 (U cannot read it): the key's message, no id; the pre-check is the
    // only statement, and the Validation error poisons the transaction like a create's would
    for picked in [&[0, 2][..], &[1, 2], &[2]] {
        let mut tx = f.begin(&orm, f.u).await;
        let err = assert_query_count!(tx, >= 1, <= 1, {
            upsert(&mut tx, picked).await.unwrap_err()
        });
        assert_eq!(
            (err.kind(), err.message()),
            (ErrorKind::Validation, hidden.as_str()),
            "rows {picked:?}"
        );
        assert!(tx.is_poisoned());
        tx.rollback().await.unwrap();
    }

    // Q1 and Q2 (readable, not writable): the engine's text for the write, no INSERT sent
    let mut tx = f.begin(&orm, f.u).await;
    tx.capture_statements(true);
    let err = upsert(&mut tx, &[0, 1]).await.unwrap_err();
    tx.capture_statements(false);
    assert_eq!(err.kind(), ErrorKind::Access);
    assert_eq!(err.message(), record_refused(U_NAME, f.u, "write"));
    assert!(
        tx.captured()
            .iter()
            .all(|s| !s.contains("INSERT INTO \"scale_quant\"")),
        "{:#?}",
        tx.captured()
    );
    tx.rollback().await.unwrap();
    drop(tx);

    // Q1 alone: the pre-check lets it through, then the statement (no create check: nothing
    // inserted)
    let mut tx = f.begin(&orm, f.u).await;
    let out = assert_query_count!(tx, >= 2, <= 2, { upsert(&mut tx, &[0]).await.unwrap() });
    assert_eq!(
        (out.rows()[0].id(), out.rows()[0].outcome()),
        (q1, UpsertOutcome::Updated)
    );
    tx.rollback().await.unwrap();
    drop(tx);

    // V may not read the model: its denied row (Q2) is never named either
    let mut tx = f.begin(&orm, f.v).await;
    let err = assert_query_count!(tx, >= 1, <= 1, {
        upsert(&mut tx, &[0, 1]).await.unwrap_err()
    });
    assert_eq!(
        (err.kind(), err.message()),
        (ErrorKind::Validation, hidden.as_str())
    );
    tx.rollback().await.unwrap();
    drop(tx);

    // nothing was written
    assert_eq!(f.stored().await, vec![(3.0, 1); 3]);
    drop(orm);
    f.h.cleanup().await.unwrap();
}

/// The `policy` generation rows the transaction sees (its own uncommitted ones included).
async fn policy_rows(tx: &mut Transaction) -> i64 {
    let rows = tx
        .query(
            "SELECT count(*)::int8 FROM erp_signal_generation WHERE key = 'policy'",
            &[],
        )
        .await
        .unwrap();
    rows[0].get(0)
}

fn access_row(model: i64, name: &str, operation: &str, domain: &str) -> Vals<IrAccess> {
    Vals::new()
        .set(IrAccess::MODEL_ID, RecId::from_real(model).unwrap())
        .set(IrAccess::NAME, name)
        .set(IrAccess::OPERATION, operation)
        .set(IrAccess::DOMAIN, domain)
}

/// flushed, the transaction rebuilds its policy privately before its next decision, which denies
/// with the SEC-009 text; the table's trigger writes exactly one `policy` generation row per
/// transaction for an insert and an update in one statement; a savepoint rollback removes the row
/// and the rebuilt policy; the part that the writing process does not credit the bump to its own
/// applied generations needs an ORM with a `Signal` installed, which the harness cannot build, and
/// is's rule for trigger bumps in `signal.rs`)
#[tokio::test]
async fn spec_ormx_033_upsert_on_a_policy_table_bumps_policy_once() {
    let f = fixture().await;
    let q = f.quants[0];
    let orm = f
        .orm(&[
            permission("editors", f.editors, "crud"),
            restriction("floor", "r", "[('quantity', '>=', 0)]".to_owned()),
        ])
        .await;
    let accesses = |tx: &Transaction| tx.env().model::<IrAccess>().unwrap().sudo(true);
    let quant = |tx: &Transaction| {
        tx.env()
            .records("scale.quant")
            .unwrap()
            .browse([q.get()])
            .unwrap()
    };

    let mut tx = f.begin(&orm, f.u).await;
    quant(&tx)
        .check_access(&mut tx, Operation::Write)
        .await
        .unwrap();
    let before = policy_rows(&mut tx).await;
    // one statement: inserts a restriction hiding Q from writes, updates the read floor
    let out = accesses(&tx)
        .upsert(
            &mut tx,
            Upsert::on(AccessKeys::ACCESS_KEY)
                .rows([
                    access_row(f.model, "min quantity", "u", "[('quantity', '>', 5)]"),
                    access_row(f.model, "floor", "r", "[('quantity', '>=', 1)]"),
                ])
                .update([IrAccess::OPERATION, IrAccess::DOMAIN]),
        )
        .await
        .unwrap();
    assert_eq!(
        out.rows().iter().map(|r| r.outcome()).collect::<Vec<_>>(),
        vec![UpsertOutcome::Inserted, UpsertOutcome::Updated]
    );
    assert_eq!(policy_rows(&mut tx).await, before + 1);
    // the next decision of U sees the new restriction
    let err = quant(&tx)
        .check_access(&mut tx, Operation::Write)
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Access);
    assert_eq!(err.message(), record_refused(U_NAME, f.u, "write"));
    // further policy writes of the same transaction add no row
    assert_eq!(policy_rows(&mut tx).await, before + 1);

    // a savepoint that loosens the restriction and rolls back: inside, the decision follows the
    // loosened policy; after the rollback, the restriction of the first upsert again
    let err = tx
        .savepoint(async |tx| {
            accesses(tx)
                .upsert(
                    tx,
                    Upsert::on(AccessKeys::ACCESS_KEY)
                        .rows([access_row(
                            f.model,
                            "min quantity",
                            "u",
                            "[('quantity', '>', 0)]",
                        )])
                        .update([IrAccess::OPERATION, IrAccess::DOMAIN]),
                )
                .await?;
            quant(tx).check_access(tx, Operation::Write).await?;
            Err::<(), _>(erp_sdk::Error::validation("roll back"))
        })
        .await
        .unwrap_err();
    assert_eq!(err.message(), "roll back");
    let err = quant(&tx)
        .check_access(&mut tx, Operation::Write)
        .await
        .unwrap_err();
    assert_eq!(err.message(), record_refused(U_NAME, f.u, "write"));
    tx.commit().await.unwrap();
    drop(tx);
    let committed = id_of(
        &f.client,
        "SELECT count(*)::int8 FROM erp_signal_generation WHERE key = 'policy'",
        &[],
    )
    .await;
    assert_eq!(
        committed,
        before + 1,
        "the transaction's one row is committed"
    );

    // a savepoint rollback of the transaction's first policy write removes its row
    let mut tx = f.begin(&orm, f.u).await;
    let start = policy_rows(&mut tx).await;
    let err = tx
        .savepoint(async |tx| {
            accesses(tx)
                .upsert(
                    tx,
                    Upsert::on(AccessKeys::ACCESS_KEY).rows([access_row(
                        f.model,
                        "another",
                        "u",
                        "[('quantity', '>', 1)]",
                    )]),
                )
                .await?;
            assert_eq!(policy_rows(tx).await, start + 1);
            Err::<(), _>(erp_sdk::Error::validation("roll back"))
        })
        .await
        .unwrap_err();
    assert_eq!(err.message(), "roll back");
    assert_eq!(policy_rows(&mut tx).await, start);
    accesses(&tx)
        .upsert(
            &mut tx,
            Upsert::on(AccessKeys::ACCESS_KEY).rows([access_row(
                f.model,
                "another",
                "u",
                "[('quantity', '>', 1)]",
            )]),
        )
        .await
        .unwrap();
    assert_eq!(policy_rows(&mut tx).await, start + 1);
    tx.rollback().await.unwrap();
    drop(tx);
    drop(orm);
    f.h.cleanup().await.unwrap();
}

/// readable, not only the rows the write rule denies. A row the caller may write but not read, or
/// any matched row when the caller has no read ACL on the model, fails with the key's declared
/// message and no id, for get-or-create, `update` and an `accumulate` of delta 0; a key that
/// matches nothing still inserts)
#[tokio::test]
async fn spec_ormx_008_every_matched_row_must_be_readable() {
    let f = fixture().await;
    let (q1, q2, q3) = (f.quants[0], f.quants[1], f.quants[2]);
    // U writes everything (no write restriction) and reads Q2 and Q3 only; V creates and writes
    // but has no read ACL at all
    let orm = f
        .orm(&[
            permission("editors", f.editors, "crud"),
            permission("writers", f.blind, "cu"),
            restriction("read Q2 and Q3", "r", ids_in(&[q2, q3])),
        ])
        .await;
    let spec = f.h.registry().model("scale.quant").unwrap().clone();
    let hidden = format!(
        "The database refused the change: {}",
        spec.business_key("_quant_key").unwrap().message
    );
    let key_q1 = || key_row(f.products[0], f.locations[0]);
    type Call<'a> = Box<dyn Fn() -> Upsert<ScaleQuant> + 'a>;
    let calls: Vec<(&str, Call<'_>)> = vec![
        (
            "get-or-create",
            Box::new(|| Upsert::on(ScaleQuant::QUANT_KEY).rows([key_q1()])),
        ),
        (
            "update",
            Box::new(|| {
                Upsert::on(ScaleQuant::QUANT_KEY)
                    .rows([key_q1().set(ScaleQuant::NOTE, "x")])
                    .update([ScaleQuant::NOTE])
            }),
        ),
        (
            "accumulate of delta 0",
            Box::new(|| {
                Upsert::on(ScaleQuant::QUANT_KEY)
                    .rows([key_q1().set(ScaleQuant::QUANTITY, 0.0)])
                    .accumulate(ScaleQuant::QUANTITY)
            }),
        ),
    ];
    for user in [f.u, f.v] {
        for (label, call) in &calls {
            let mut tx = f.begin(&orm, user).await;
            let model = quants(&tx);
            tx.capture_statements(true);
            let err = model.upsert(&mut tx, call()).await.unwrap_err();
            tx.capture_statements(false);
            assert_eq!(
                (err.kind(), err.message()),
                (ErrorKind::Validation, hidden.as_str()),
                "{label} as user {user}"
            );
            assert!(
                tx.captured()
                    .iter()
                    .all(|s| !s.contains("INSERT INTO \"scale_quant\"")),
                "{label}: the upsert statement was not sent: {:#?}",
                tx.captured()
            );
            tx.rollback().await.unwrap();
        }
    }
    // a readable matched row (Q2) is still updated, and V (no read ACL) can still insert a key
    // that matches nothing
    let mut tx = f.begin(&orm, f.u).await;
    let out = quants(&tx)
        .upsert(
            &mut tx,
            Upsert::on(ScaleQuant::QUANT_KEY)
                .rows([key_row(f.products[1], f.locations[1]).set(ScaleQuant::QUANTITY, 1.0)])
                .accumulate(ScaleQuant::QUANTITY),
        )
        .await
        .unwrap();
    assert_eq!(out.rows()[0].id(), q2);
    assert_eq!(out.rows()[0].outcome(), UpsertOutcome::Updated);
    tx.rollback().await.unwrap();
    let mut tx = f.begin(&orm, f.v).await;
    let out = quants(&tx)
        .upsert(
            &mut tx,
            Upsert::on(ScaleQuant::QUANT_KEY)
                .rows([key_row(f.products[3], f.locations[0]).set(ScaleQuant::QUANTITY, 1.0)]),
        )
        .await
        .unwrap();
    assert_eq!(out.rows()[0].outcome(), UpsertOutcome::Inserted);
    tx.rollback().await.unwrap();
    drop(orm);
    let _ = q1;
    f.h.cleanup().await.unwrap();
}

/// Review SEC-S6: the caller must be able to read the records it expects versions of, since a
/// later `Conflict` names them with their current versions.
#[tokio::test]
async fn expect_versions_needs_read_access_to_the_records() {
    let f = fixture().await;
    let (q1, q2) = (f.quants[0], f.quants[1]);
    let orm = f
        .orm(&[
            permission("editors", f.editors, "crud"),
            restriction("read Q2", "r", ids_in(&[q2])),
        ])
        .await;
    let mut tx = f.begin(&orm, f.u).await;
    let model = quants(&tx);
    let err = model
        .browse([q1.get()])
        .unwrap()
        .expect_versions(&mut tx, [(q1, 1)])
        .await
        .unwrap_err();
    assert_eq!(
        (err.kind(), err.message()),
        (
            ErrorKind::Access,
            record_refused(U_NAME, f.u, "read").as_str()
        )
    );
    model
        .browse([q2.get()])
        .unwrap()
        .expect_versions(&mut tx, [(q2, 1)])
        .await
        .unwrap();
    tx.rollback().await.unwrap();
    drop(orm);
    f.h.cleanup().await.unwrap();
}

/// Review SF-S3: a required column that no default fills and a row does not give is named before
/// the statement, not left to a bare NOT NULL violation.
#[tokio::test]
async fn an_upsert_names_a_required_column_a_row_does_not_give() {
    let f = fixture().await;
    let mut tx = f.h.begin().await.unwrap();
    let err = tx
        .env()
        .model::<IrAccess>()
        .unwrap()
        .upsert(
            &mut tx,
            Upsert::on(AccessKeys::ACCESS_KEY).rows([Vals::<IrAccess>::new()
                .set(IrAccess::MODEL_ID, RecId::from_real(f.model).unwrap())
                .set(IrAccess::NAME, "no operation")]),
        )
        .await
        .unwrap_err();
    assert_eq!(
        (err.kind(), err.message()),
        (
            ErrorKind::Programming,
            "upsert on 'ir.access': row 0 does not set required field 'operation', which has no \
             default (the INSERT would fail with a NOT NULL violation)"
        )
    );
    tx.rollback().await.unwrap();
    drop(tx);
    f.h.cleanup().await.unwrap();
}
