#![allow(
    dead_code,
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "shared test fixture"
)]
use erp_fields::Cell;
use erp_orm::{BeginOptions, OperationIdentity, RawVals, SUPERUSER_ID, Transaction, UpsertCall};
use erp_registry::{
    AddonDescriptor, FieldDecl, KeyDecl, ModelDecl, TableObjectDecl, TableObjectKind,
};
use erp_test_models::base_users;
use erp_testkit::Harness;

pub const MODEL: &str = "test.outbox.upsert";
pub const TABLE: &str = "test_outbox_upsert";
pub static ADDON: AddonDescriptor = AddonDescriptor {
    name: "test_outbox_upsert",
    depends: &["base"],
    exact_only: false,
};
static KEY: [TableObjectDecl; 1] = [TableObjectDecl {
    name: "_business",
    kind: TableObjectKind::Key(KeyDecl::new(&["code"], "Upsert code must be unique.")),
}];
pub async fn harness(eventful: bool) -> Harness {
    harness_with(eventful, Denial::None).await
}
pub async fn harness_with(eventful: bool, denial: Denial) -> Harness {
    let builder = Harness::builder([&base_users::ADDON, &ADDON])
        .with_decls([ModelDecl::new(ADDON.name, MODEL)
            .eventful(eventful)
            .row_version(true)
            .table(TABLE)
            .fields(
                const {
                    &[
                        FieldDecl::char("code"),
                        FieldDecl::char("note"),
                        FieldDecl::integer("number"),
                        FieldDecl::float("amount"),
                        FieldDecl::json("payload"),
                        FieldDecl::integer("twice")
                            .generated("number * 2")
                            .generated_storage(erp_registry::generated::GeneratedStorage::Stored),
                    ]
                },
            )
            .table_objects(&KEY)])
        .write_hook(erp_orm::WriteHook::new(MODEL, &["number"], number_hook))
        .with_impls([&CHECK_METHOD])
        .with_constraints([&CHECK])
        .config(erp_orm::OrmConfig {
            insert_batch: 128,
            ..erp_orm::OrmConfig::default()
        });
    let builder = if matches!(denial, Denial::Field) {
        builder.policy(erp_testkit::FieldPolicy::new().deny(
            MODEL,
            "number",
            erp_orm::FieldOp::Write,
        ))
    } else {
        builder.rules(Rules(denial))
    };
    let h = builder.build().await.unwrap();
    let mut tx = h.begin().await.unwrap();
    tx.install_event_schema().await.unwrap();
    tx.execute("INSERT INTO res_users (id,name,login,active) VALUES (7,'upsert writer','upsert-writer',true)", &[]).await.unwrap();
    tx.commit().await.unwrap();
    h
}
pub async fn begin(h: &Harness) -> Transaction {
    h.orm()
        .begin_opts(
            BeginOptions::new(SUPERUSER_ID)
                .operation_identity(OperationIdentity::generate().unwrap()),
        )
        .await
        .unwrap()
}
pub fn row(code: Option<&str>, number: i32, note: Option<&str>) -> RawVals {
    RawVals::new()
        .set_cell("code", code.map_or(Cell::Null, |s| Cell::Str(s.into())))
        .set_cell("number", Cell::Int(number))
        .set_cell("note", note.map_or(Cell::Null, |s| Cell::Str(s.into())))
}
pub fn call(rows: Vec<RawVals>) -> UpsertCall {
    UpsertCall::new("_business")
        .with_rows(rows)
        .with_update(["number", "note"])
}
pub async fn seed(h: &Harness) -> Vec<i64> {
    let mut tx = begin(h).await;
    let ids = tx
        .env()
        .records(MODEL)
        .unwrap()
        .create(
            &mut tx,
            vec![row(Some("a"), 1, Some("first")), row(Some("b"), 2, None)],
        )
        .await
        .unwrap()
        .ids();
    tx.commit().await.unwrap();
    ids
}
#[derive(Debug, PartialEq, Eq)]
pub struct Event {
    pub kind: String,
    pub ids: Vec<i64>,
    pub fields: Vec<String>,
    pub subject: String,
    pub operation: String,
}
pub async fn events(h: &Harness, op: &str) -> Vec<Event> {
    let mut tx = h.begin().await.unwrap();
    let rows = tx.query("SELECT kind, record_ids, field_names, subject, operation_id::text FROM erp_event.ir_event_outbox WHERE operation_id::text=$1 ORDER BY id", &[&op]).await.unwrap();
    let out = rows
        .iter()
        .map(|r| Event {
            kind: r.get(0),
            ids: serde_json::from_value(r.get(1)).unwrap(),
            fields: r.get(2),
            subject: r.get(3),
            operation: r.get(4),
        })
        .collect();
    tx.rollback().await.unwrap();
    out
}
pub async fn state(h: &Harness) -> Vec<(i64, Option<String>, i32, Option<String>, i32)> {
    let mut tx = h.begin().await.unwrap();
    let out = tx
        .query(
            "SELECT id,code,number,note,twice FROM test_outbox_upsert ORDER BY code NULLS LAST",
            &[],
        )
        .await
        .unwrap()
        .iter()
        .map(|r| (r.get(0), r.get(1), r.get(2), r.get(3), r.get(4)))
        .collect();
    tx.rollback().await.unwrap();
    out
}
pub fn inserts(tx: &Transaction) -> usize {
    tx.captured()
        .iter()
        .filter(|s| s.starts_with("INSERT INTO \"erp_event\".\"ir_event_outbox\""))
        .count()
}

const REJECT: &str = "upsert fixture rejects note";
type CheckFn = erp_orm::MethodFn<(), ()>;
fn check<'a>(
    tx: &'a mut Transaction,
    r: &'a erp_orm::Records,
    _: (),
    _: erp_orm::Super<(), ()>,
) -> erp_orm::BoxFuture<'a, erp_fields::Result<()>> {
    Box::pin(async move {
        for id in r.ids() {
            if r.browse([id])?.value(tx, "note").await? == erp_fields::Value::str("reject") {
                return Err(erp_fields::Error::validation(REJECT));
            }
        }
        Ok(())
    })
}
fn entry() -> &'static (dyn std::any::Any + Send + Sync) {
    static F: CheckFn = check;
    &F
}
static CHECK_METHOD: erp_orm::MethodImpl = erp_orm::MethodImpl {
    addon: ADDON.name,
    model: MODEL,
    method: "check_upsert_note",
    kind: erp_orm::ImplKind::Define,
    sig_name: std::any::type_name::<CheckFn>,
    entry,
    file: file!(),
    line: line!(),
};
static CHECK: erp_orm::ConstraintImpl = erp_orm::ConstraintImpl {
    addon: ADDON.name,
    model: MODEL,
    method: "check_upsert_note",
    fields: &["note"],
    file: file!(),
    line: line!(),
};

pub mod computed {
    use erp_sdk::prelude::*;
    /// Deferred output fixture: input scheduling must bind upsert provenance.
    #[model(name="test.outbox.upsert.computed", addon="test_outbox_upsert", eventful=true,
 keys=[key(name="_business",fields=["code"],message="Computed code unique.")])]
    pub struct Computed {
        #[field(char)]
        pub code: Option<Arc<str>>,
        #[field(integer)]
        pub source: i32,
        #[field(integer, compute = "_double", store, precompute = false, compute_sudo)]
        pub doubled: i32,
    }
    #[methods(Computed, addon = "test_outbox_upsert")]
    impl Computed {
        #[compute(depends=["source"])]
        async fn _double(
            this: &RecordSet<Computed>,
            tx: &mut Transaction,
            out: &mut Assign<'_, Computed>,
        ) -> Result<()> {
            let values = this.mapped(tx, Computed::SOURCE).await?;
            for (i, value) in values.into_iter().enumerate() {
                if value == 9 {
                    let err = this
                        .env()
                        .records(super::MODEL)?
                        .upsert(tx, super::call(vec![super::row(Some("nested"), 1, None)]))
                        .await
                        .unwrap_err();
                    assert_eq!(err.kind(), erp_fields::ErrorKind::Programming);
                    assert_eq!(
                        err.message(),
                        "a later release S7: nested event-relevant mutations during mutation or compute execution are not implemented"
                    );
                }
                out.set(i, Computed::DOUBLED, value * 2)?;
            }
            Ok(())
        }
    }
}

#[derive(Clone, Copy)]
pub enum Denial {
    None,
    Model,
    Field,
    Write,
    Read,
    Create,
}
struct Rules(Denial);
impl erp_testkit::TestRules for Rules {
    fn check_model(
        &self,
        _: &erp_orm::PrincipalBinding<'_>,
        model: &erp_registry::ModelSpec,
        op: erp_orm::Operation,
    ) -> erp_fields::Result<()> {
        if model.name == MODEL && op == erp_orm::Operation::Write && matches!(self.0, Denial::Model)
        {
            return Err(erp_fields::Error::access(
                "upsert fixture denies model write",
            ));
        }
        Ok(())
    }
    fn access_domain(
        &self,
        _: &erp_orm::PrincipalBinding<'_>,
        model: &erp_registry::ModelSpec,
        op: erp_orm::Operation,
    ) -> erp_fields::Result<erp_domain::Domain> {
        if model.name == MODEL
            && matches!(
                (self.0, op),
                (Denial::Write, erp_orm::Operation::Write)
                    | (Denial::Read, erp_orm::Operation::Read)
                    | (Denial::Create, erp_orm::Operation::Create)
            )
        {
            Ok(erp_domain::Domain::FALSE)
        } else {
            Ok(erp_domain::Domain::TRUE)
        }
    }
}
pub async fn user_begin(h: &Harness) -> Transaction {
    user_begin_companies(h, &[]).await
}
pub async fn user_begin_companies(h: &Harness, ids: &[i64]) -> Transaction {
    let companies = ids
        .iter()
        .map(|id| erp_orm::CompanyId::new(*id).unwrap())
        .collect::<Vec<_>>();
    let principal = erp_orm::PrincipalBuilder::user(erp_orm::UserId::new(7).unwrap())
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
    h.orm()
        .begin_opts(
            BeginOptions::for_principal(principal)
                .operation_identity(OperationIdentity::generate().unwrap()),
        )
        .await
        .unwrap()
}

// Only the hook-layout test updates a row to77; created duplicate-batch rows do not run hooks.
pub static HOOK_HITS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
fn number_hook<'a>(
    tx: &'a mut Transaction,
    batch: &'a erp_orm::WriteBatch,
) -> erp_orm::BoxFuture<'a, erp_fields::Result<()>> {
    Box::pin(async move {
        assert_eq!(batch.fields(), &["number"]);
        for row in batch.rows() {
            let (old, new) = &row.values()[0];
            if *new == Cell::Int(77) {
                assert_eq!(*old, Cell::Int(1));
                HOOK_HITS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
            if matches!(*new, Cell::Int(87 | 88)) {
                let error = tx
                    .savepoint(async |_tx| {
                        Err::<(), _>(erp_fields::Error::validation("inner hook rollback"))
                    })
                    .await
                    .unwrap_err();
                assert_eq!(error.kind(), erp_fields::ErrorKind::Validation);
                assert_eq!(error.message(), "inner hook rollback");
                if *new == Cell::Int(88) {
                    let r = tx.env().records(MODEL)?.browse([row.id().get()])?;
                    r.write(tx, RawVals::new().set_cell("number", Cell::Int(99)))
                        .await?;
                }
            }
            if *new == Cell::Int(-77) {
                return Err(erp_fields::Error::validation(
                    "upsert fixture rejects write hook",
                ));
            }
        }
        Ok(())
    })
}

pub mod cross_model {
    use erp_sdk::prelude::*;
    /// Non-eventful upsert source whose related output is eventful.
    #[model(name="test.outbox.upsert.source",addon="test_outbox_upsert",keys=[key(name="_business",fields=["code"],message="Source code unique.")])]
    pub struct Source {
        #[field(char)]
        pub code: Option<Arc<str>>,
        #[field(integer)]
        pub number: i32,
    }
    /// Stored dependency target for structural capture admission.
    #[model(
        name = "test.outbox.upsert.sink",
        addon = "test_outbox_upsert",
        eventful = true
    )]
    pub struct Sink {
        #[field(many2one, comodel = "test.outbox.upsert.source")]
        pub source_id: Option<RecId>,
        #[field(integer, related = "source_id.number", store)]
        pub copied: i32,
    }
}
