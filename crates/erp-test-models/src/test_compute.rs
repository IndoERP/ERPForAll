#![allow(
    clippy::disallowed_names,
    reason = "field names of the reference ERP's fixtures (foo, bar, baz)"
)]

use std::sync::{Mutex, PoisonError};

use erp_sdk::prelude::*;

erp_sdk::addon!(name = "test_compute");

static LOG: Mutex<Vec<(String, String)>> = Mutex::new(Vec::new());

/// Append `entry` to the log of the environment's `log` context tag, if any.
pub fn log(env: &Env, entry: &str) {
    if let Some(Value::Str(tag)) = env.context().get("log") {
        LOG.lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push((tag.to_string(), entry.to_owned()));
    }
}

/// Take the entries logged under `tag`.
pub fn take_log(tag: &str) -> Vec<String> {
    let mut log = LOG.lock().unwrap_or_else(PoisonError::into_inner);
    let (mine, rest): (Vec<_>, Vec<_>) = log.drain(..).partition(|(t, _)| t == tag);
    *log = rest;
    mine.into_iter().map(|(_, e)| e).collect()
}

fn text(v: &Option<Arc<str>>) -> &str {
    v.as_deref().unwrap_or("")
}

/// `test_orm.cascade`: `bar` (non-stored) depends on `foo`; `baz` (stored) depends on `bar`
/// .
#[model(
    name = "test_orm.cascade",
    description = "Test ORM Cascade",
    addon = "test_compute"
)]
pub struct Cascade {
    #[field(char)]
    pub foo: Option<Arc<str>>,
    #[field(char, compute = "_compute_bar")]
    pub bar: Option<Arc<str>>,
    #[field(char, compute = "_compute_baz", store)]
    pub baz: Option<Arc<str>>,
}

#[methods(Cascade, addon = "test_compute")]
impl Cascade {
    #[compute(depends = ["foo"])]
    fn _compute_bar(batch: &mut Batch<'_, Cascade>) -> Result<()> {
        log(batch.env(), "bar");
        for i in batch.indices() {
            let foo = batch.get(i, Cascade::FOO)?;
            batch.set(i, Cascade::BAR, format!("[{}]", text(&foo)))?;
        }
        Ok(())
    }

    #[compute(depends = ["bar"])]
    fn _compute_baz(batch: &mut Batch<'_, Cascade>) -> Result<()> {
        for i in batch.indices() {
            let bar = batch.get(i, Cascade::BAR)?;
            batch.set(i, Cascade::BAZ, format!("<{}>", text(&bar)))?;
        }
        Ok(())
    }
}

/// `test_orm.compute.readonly`: stored readonly `bar` = `foo`.
#[model(
    name = "test_orm.compute.readonly",
    description = "Model with a computed readonly field",
    addon = "test_compute"
)]
pub struct ComputeReadonly {
    #[field(char, default = "")]
    pub foo: Option<Arc<str>>,
    #[field(char, compute = "_compute_bar", store)]
    pub bar: Option<Arc<str>>,
}

#[methods(ComputeReadonly, addon = "test_compute")]
impl ComputeReadonly {
    #[compute(depends = ["foo"])]
    fn _compute_bar(batch: &mut Batch<'_, ComputeReadonly>) -> Result<()> {
        log(batch.env(), "compute");
        for i in batch.indices() {
            let foo = batch.get(i, ComputeReadonly::FOO)?;
            batch.set(i, ComputeReadonly::BAR, foo)?;
        }
        Ok(())
    }
}

/// `test_orm.compute.readwrite`: stored editable `bar` = `foo`.
#[model(
    name = "test_orm.compute.readwrite",
    description = "Model with a computed non-readonly field",
    addon = "test_compute"
)]
pub struct ComputeReadwrite {
    #[field(char)]
    pub foo: Option<Arc<str>>,
    #[field(char, compute = "_compute_bar", store, readonly = false)]
    pub bar: Option<Arc<str>>,
}

#[methods(ComputeReadwrite, addon = "test_compute")]
impl ComputeReadwrite {
    #[compute(depends = ["foo"])]
    fn _compute_bar(batch: &mut Batch<'_, ComputeReadwrite>) -> Result<()> {
        for i in batch.indices() {
            let foo = batch.get(i, ComputeReadwrite::FOO)?;
            batch.set(i, ComputeReadwrite::BAR, foo)?;
        }
        Ok(())
    }
}

/// `test_orm.compute.inverse`: `bar` computed from `foo`, its inverse sets `foo`; a constraint on
/// `bar` and `baz` (/035).
#[model(
    name = "test_orm.compute.inverse",
    description = "Model with a computed inversed field",
    addon = "test_compute"
)]
pub struct ComputeInverse {
    #[field(char)]
    pub foo: Option<Arc<str>>,
    #[field(char, compute = "_compute_bar", inverse = "_inverse_bar", store)]
    pub bar: Option<Arc<str>>,
    #[field(char)]
    pub baz: Option<Arc<str>>,
}

#[methods(ComputeInverse, addon = "test_compute")]
impl ComputeInverse {
    #[compute(depends = ["foo"])]
    fn _compute_bar(batch: &mut Batch<'_, ComputeInverse>) -> Result<()> {
        log(batch.env(), "compute");
        for i in batch.indices() {
            let foo = batch.get(i, ComputeInverse::FOO)?;
            batch.set(i, ComputeInverse::BAR, foo)?;
        }
        Ok(())
    }

    #[method]
    async fn _inverse_bar(this: &RecordSet<ComputeInverse>, tx: &mut Transaction) -> Result<()> {
        log(this.env(), "inverse");
        for rec in this.iter() {
            let bar = rec.get(tx, ComputeInverse::BAR).await?;
            rec.set(tx, ComputeInverse::FOO, bar).await?;
        }
        Ok(())
    }

    #[constrains("bar", "baz")]
    async fn _check_constraint(
        this: &RecordSet<ComputeInverse>,
        _tx: &mut Transaction,
    ) -> Result<()> {
        if this.env().context().get("log_constraint").is_some() {
            log(this.env(), "constraint");
        }
        Ok(())
    }
}

/// `test_orm.multi_compute_inverse`: three fields of one compute, two inverse methods
/// (/034).
#[model(
    name = "test_orm.multi_compute_inverse",
    description = "Test ORM Multi Compute Inverse",
    addon = "test_compute"
)]
pub struct MultiComputeInverse {
    #[field(char, default = "", required)]
    pub foo: Option<Arc<str>>,
    #[field(char, compute = "_compute_bars", inverse = "_inverse_bar1", store)]
    pub bar1: Option<Arc<str>>,
    #[field(char, compute = "_compute_bars", inverse = "_inverse_bar23", store)]
    pub bar2: Option<Arc<str>>,
    #[field(char, compute = "_compute_bars", inverse = "_inverse_bar23", store)]
    pub bar3: Option<Arc<str>>,
}

impl MultiComputeInverse {
    async fn write_foo(this: &RecordSet<MultiComputeInverse>, tx: &mut Transaction) -> Result<()> {
        for rec in this.iter() {
            let b1 = rec.get(tx, MultiComputeInverse::BAR1).await?;
            let b2 = rec.get(tx, MultiComputeInverse::BAR2).await?;
            let b3 = rec.get(tx, MultiComputeInverse::BAR3).await?;
            let foo = format!("{}/{}/{}", text(&b1), text(&b2), text(&b3));
            rec.set(tx, MultiComputeInverse::FOO, foo).await?;
        }
        Ok(())
    }
}

#[methods(MultiComputeInverse, addon = "test_compute")]
impl MultiComputeInverse {
    #[compute(depends = ["foo"])]
    fn _compute_bars(batch: &mut Batch<'_, MultiComputeInverse>) -> Result<()> {
        log(batch.env(), "compute");
        for i in batch.indices() {
            let foo = batch.get(i, MultiComputeInverse::FOO)?;
            let mut parts: Vec<String> = text(&foo).split('/').map(str::to_owned).collect();
            parts.resize(3.max(parts.len()), String::new());
            batch.set(i, MultiComputeInverse::BAR1, parts[0].clone())?;
            batch.set(i, MultiComputeInverse::BAR2, parts[1].clone())?;
            batch.set(i, MultiComputeInverse::BAR3, parts[2].clone())?;
        }
        Ok(())
    }

    #[method]
    async fn _inverse_bar1(
        this: &RecordSet<MultiComputeInverse>,
        tx: &mut Transaction,
    ) -> Result<()> {
        log(this.env(), "inverse1");
        MultiComputeInverse::write_foo(this, tx).await
    }

    #[method]
    async fn _inverse_bar23(
        this: &RecordSet<MultiComputeInverse>,
        tx: &mut Transaction,
    ) -> Result<()> {
        log(this.env(), "inverse23");
        MultiComputeInverse::write_foo(this, tx).await
    }
}

/// `test_orm.compute.unassigned`: computes that assign only when `foo == "assign"`
/// .
#[model(
    name = "test_orm.compute.unassigned",
    description = "Model with computed fields left unassigned",
    addon = "test_compute"
)]
pub struct ComputeUnassigned {
    #[field(char)]
    pub foo: Option<Arc<str>>,
    #[field(char, compute = "_compute_bar")]
    pub bar: Option<Arc<str>>,
    #[field(char, compute = "_compute_bare", readonly = false)]
    pub bare: Option<Arc<str>>,
    #[field(char, compute = "_compute_bars", store)]
    pub bars: Option<Arc<str>>,
    #[field(char, compute = "_compute_bares", readonly = false, store)]
    pub bares: Option<Arc<str>>,
}

#[methods(ComputeUnassigned, addon = "test_compute")]
impl ComputeUnassigned {
    #[compute(depends = ["foo"])]
    fn _compute_bar(batch: &mut Batch<'_, ComputeUnassigned>) -> Result<()> {
        for i in batch.indices() {
            let foo = batch.get(i, ComputeUnassigned::FOO)?;
            if text(&foo) == "assign" {
                batch.set(i, ComputeUnassigned::BAR, foo)?;
            }
        }
        Ok(())
    }

    #[compute(depends = ["foo"])]
    fn _compute_bare(batch: &mut Batch<'_, ComputeUnassigned>) -> Result<()> {
        for i in batch.indices() {
            let foo = batch.get(i, ComputeUnassigned::FOO)?;
            if text(&foo) == "assign" {
                batch.set(i, ComputeUnassigned::BARE, foo)?;
            }
        }
        Ok(())
    }

    #[compute(depends = ["foo"])]
    fn _compute_bars(batch: &mut Batch<'_, ComputeUnassigned>) -> Result<()> {
        for i in batch.indices() {
            let foo = batch.get(i, ComputeUnassigned::FOO)?;
            if text(&foo) == "assign" {
                batch.set(i, ComputeUnassigned::BARS, foo)?;
            }
        }
        Ok(())
    }

    #[compute(depends = ["foo"])]
    fn _compute_bares(batch: &mut Batch<'_, ComputeUnassigned>) -> Result<()> {
        for i in batch.indices() {
            let foo = batch.get(i, ComputeUnassigned::FOO)?;
            if text(&foo) == "assign" {
                batch.set(i, ComputeUnassigned::BARES, foo)?;
            }
        }
        Ok(())
    }
}

/// `test_orm.compute.sudo`: `name_for_uid` is the current user's name, computed as superuser
/// with the uid kept.
#[model(
    name = "test_orm.compute.sudo",
    description = "Model with a compute_sudo field",
    addon = "test_compute"
)]
pub struct ComputeSudo {
    #[field(char)]
    pub name: Option<Arc<str>>,
    #[field(char, compute = "_compute_name_for_uid", compute_sudo)]
    pub name_for_uid: Option<Arc<str>>,
}

#[methods(ComputeSudo, addon = "test_compute")]
impl ComputeSudo {
    #[compute(depends_context = ["uid"])]
    async fn _compute_name_for_uid(
        this: &RecordSet<ComputeSudo>,
        tx: &mut Transaction,
        out: &mut Assign<'_, ComputeSudo>,
    ) -> Result<()> {
        let env = this.env().clone();
        log(&env, if env.su() { "sudo" } else { "user" });
        let user = env.model::<ResUsers>()?.browse([env.uid()])?;
        let name = user.get(tx, ResUsers::NAME).await?;
        out.set_all(ComputeSudo::NAME_FOR_UID, name)
    }
}

/// `test_performance.base` (scalar part): `value_pc = value / 100` stored; `value_ctx` from the
/// context key `key`, one query per batch (/017).
#[model(
    name = "test_performance.base",
    description = "Test Performance Base",
    addon = "test_compute"
)]
pub struct PerfBase {
    #[field(char)]
    pub name: Option<Arc<str>>,
    #[field(integer, default = 0)]
    pub value: i32,
    #[field(float, compute = "_value_pc", store)]
    pub value_pc: f64,
    #[field(integer, compute = "_value_ctx")]
    pub value_ctx: i32,
    /// Follows the audit timestamp the flush sets (review item 6).
    #[field(char, compute = "_write_day")]
    pub write_day: Option<Arc<str>>,
}

#[methods(PerfBase, addon = "test_compute")]
impl PerfBase {
    #[compute(depends = ["value"])]
    fn _value_pc(batch: &mut Batch<'_, PerfBase>) -> Result<()> {
        log(batch.env(), &format!("value_pc x{}", batch.len()));
        for i in batch.indices() {
            let value = batch.get(i, PerfBase::VALUE)?;
            batch.set(i, PerfBase::VALUE_PC, f64::from(value) / 100.0)?;
        }
        Ok(())
    }

    #[compute(depends = ["write_date"])]
    fn _write_day(batch: &mut Batch<'_, PerfBase>) -> Result<()> {
        for i in batch.indices() {
            let stamp = batch.get(i, Base::WRITE_DATE)?;
            batch.set(
                i,
                PerfBase::WRITE_DAY,
                stamp.map(|d| Arc::<str>::from(d.to_string())),
            )?;
        }
        Ok(())
    }

    #[compute(depends_context = ["key"])]
    async fn _value_ctx(
        this: &RecordSet<PerfBase>,
        tx: &mut Transaction,
        out: &mut Assign<'_, PerfBase>,
    ) -> Result<()> {
        let key = match this.env().context().get("key") {
            Some(Value::Int(k)) => i32::try_from(k).unwrap_or(0),
            _ => 0,
        };
        if key == YIELD_KEY {
            YieldOnce(false).await;
        }
        tx.query("SELECT 42", &[]).await?;
        out.set_all(PerfBase::VALUE_CTX, key)
    }
}

/// With this `key` in the context, `value_ctx` suspends once inside its compute: a test can cancel
/// it there deterministically (S4a review decision E2).
pub const YIELD_KEY: i32 = 99;

/// A future that is pending once (and asks to be polled again), then ready.
struct YieldOnce(bool);

impl std::future::Future for YieldOnce {
    type Output = ();
    fn poll(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<()> {
        if self.0 {
            std::task::Poll::Ready(())
        } else {
            self.0 = true;
            cx.waker().wake_by_ref();
            std::task::Poll::Pending
        }
    }
}

/// `test_orm.compute.creator`: its stored compute creates a `test_orm.compute.created`, which has
/// a stored compute of its own.
#[model(
    name = "test_orm.compute.creator",
    description = "test_orm.compute.creator",
    addon = "test_compute"
)]
pub struct Creator {
    #[field(char)]
    pub name: Option<Arc<str>>,
    #[field(
        many2one,
        comodel = "test_orm.compute.created",
        compute = "_compute_created",
        store
    )]
    pub created_id: Option<RecId>,
}

/// `test_orm.compute.created`: `value` = length of `name`.
#[model(
    name = "test_orm.compute.created",
    description = "test_orm.compute.created",
    addon = "test_compute"
)]
pub struct Created {
    #[field(char)]
    pub name: Option<Arc<str>>,
    #[field(integer, compute = "_compute_value", store)]
    pub value: i32,
}

#[methods(Creator, addon = "test_compute")]
impl Creator {
    #[compute(depends = ["name"])]
    async fn _compute_created(
        this: &RecordSet<Creator>,
        tx: &mut Transaction,
        out: &mut Assign<'_, Creator>,
    ) -> Result<()> {
        let names = this.mapped(tx, Creator::NAME).await?;
        let model = this.env().model::<Created>()?;
        let vals = names
            .into_iter()
            .map(|n| model.vals().set(Created::NAME, n))
            .collect();
        let created = model.create(tx, vals).await?;
        for (i, rec) in created.iter().enumerate() {
            out.set(i, Creator::CREATED_ID, rec.id()?)?;
        }
        Ok(())
    }
}

#[methods(Created, addon = "test_compute")]
impl Created {
    #[compute(depends = ["name"])]
    fn _compute_value(batch: &mut Batch<'_, Created>) -> Result<()> {
        for i in batch.indices() {
            let name = batch.get(i, Created::NAME)?;
            let len = i32::try_from(text(&name).chars().count()).unwrap_or(i32::MAX);
            batch.set(i, Created::VALUE, len)?;
        }
        Ok(())
    }
}

/// `test_orm.precompute` (scalar part): `lower`/`upper` share a compute, `lowup` reads them
/// .
#[model(
    name = "test_orm.precompute",
    description = "model with precomputed fields",
    addon = "test_compute"
)]
pub struct Precompute {
    #[field(char, required)]
    pub name: Option<Arc<str>>,
    #[field(char, compute = "_compute_names", store, precompute)]
    pub lower: Option<Arc<str>>,
    #[field(char, compute = "_compute_names", store, precompute)]
    pub upper: Option<Arc<str>>,
    #[field(char, compute = "_compute_lowup", store, precompute)]
    pub lowup: Option<Arc<str>>,
}

#[methods(Precompute, addon = "test_compute")]
impl Precompute {
    #[compute(depends = ["name"])]
    fn _compute_names(batch: &mut Batch<'_, Precompute>) -> Result<()> {
        for i in batch.indices() {
            let name = batch.get(i, Precompute::NAME)?;
            batch.set(i, Precompute::LOWER, text(&name).to_lowercase())?;
            batch.set(i, Precompute::UPPER, text(&name).to_uppercase())?;
        }
        Ok(())
    }

    #[compute(depends = ["lower", "upper"])]
    fn _compute_lowup(batch: &mut Batch<'_, Precompute>) -> Result<()> {
        for i in batch.indices() {
            let lower = batch.get(i, Precompute::LOWER)?;
            let upper = batch.get(i, Precompute::UPPER)?;
            batch.set(
                i,
                Precompute::LOWUP,
                format!("{}{}", text(&lower), text(&upper)),
            )?;
        }
        Ok(())
    }
}

/// `test_orm.precompute.editable`.
#[model(
    name = "test_orm.precompute.editable",
    description = "yet another model with precomputed editable fields",
    addon = "test_compute"
)]
pub struct PrecomputeEditable {
    #[field(char)]
    pub foo: Option<Arc<str>>,
    #[field(char, compute = "_compute_bar", precompute, store, readonly = false)]
    pub bar: Option<Arc<str>>,
    #[field(char, compute = "_compute_baz", precompute, store, readonly = false)]
    pub baz: Option<Arc<str>>,
    #[field(char, compute = "_compute_baz2", precompute, store)]
    pub baz2: Option<Arc<str>>,
    #[field(char, compute = "_compute_boos", precompute, store, readonly = false)]
    pub boo1: Option<Arc<str>>,
    #[field(char, compute = "_compute_boos", precompute, store, readonly = false)]
    pub boo2: Option<Arc<str>>,
    #[field(char, compute = "_compute_choo", precompute, store)]
    pub choo: Option<Arc<str>>,
}

#[methods(PrecomputeEditable, addon = "test_compute")]
impl PrecomputeEditable {
    #[compute(depends = ["foo"])]
    fn _compute_bar(batch: &mut Batch<'_, PrecomputeEditable>) -> Result<()> {
        batch.set_all(PrecomputeEditable::BAR, "COMPUTED")
    }

    #[compute(depends = ["bar"])]
    fn _compute_baz(batch: &mut Batch<'_, PrecomputeEditable>) -> Result<()> {
        batch.set_all(PrecomputeEditable::BAZ, "COMPUTED")
    }

    #[compute(depends = ["baz"])]
    fn _compute_baz2(batch: &mut Batch<'_, PrecomputeEditable>) -> Result<()> {
        for i in batch.indices() {
            let baz = batch.get(i, PrecomputeEditable::BAZ)?;
            batch.set(i, PrecomputeEditable::BAZ2, baz)?;
        }
        Ok(())
    }

    #[compute(depends = ["foo"])]
    fn _compute_boos(batch: &mut Batch<'_, PrecomputeEditable>) -> Result<()> {
        batch.set_all(PrecomputeEditable::BOO1, "COMPUTED")?;
        batch.set_all(PrecomputeEditable::BOO2, "COMPUTED")
    }

    #[compute(depends = ["boo2"])]
    fn _compute_choo(batch: &mut Batch<'_, PrecomputeEditable>) -> Result<()> {
        for i in batch.indices() {
            let boo2 = batch.get(i, PrecomputeEditable::BOO2)?;
            batch.set(i, PrecomputeEditable::CHOO, boo2)?;
        }
        Ok(())
    }
}

/// `test_orm.precompute.readonly`.
#[model(
    name = "test_orm.precompute.readonly",
    description = "a model with precomputed readonly fields",
    addon = "test_compute"
)]
pub struct PrecomputeReadonly {
    #[field(char)]
    pub foo: Option<Arc<str>>,
    #[field(selection, selection = [("draft", "Draft"), ("confirmed", "Confirmed")], default = "draft")]
    pub state: Option<Arc<str>>,
    #[field(char, compute = "_compute_bar", precompute, store, readonly = true)]
    pub bar: Option<Arc<str>>,
    #[field(char, compute = "_compute_baz", precompute, store, readonly = false)]
    pub baz: Option<Arc<str>>,
}

#[methods(PrecomputeReadonly, addon = "test_compute")]
impl PrecomputeReadonly {
    #[compute(depends = ["foo"])]
    fn _compute_bar(batch: &mut Batch<'_, PrecomputeReadonly>) -> Result<()> {
        batch.set_all(PrecomputeReadonly::BAR, "COMPUTED")
    }

    #[compute(depends = ["bar"])]
    fn _compute_baz(batch: &mut Batch<'_, PrecomputeReadonly>) -> Result<()> {
        batch.set_all(PrecomputeReadonly::BAZ, "COMPUTED")
    }
}

/// `test_orm.precompute.combo`: readonly, editable and inversed precomputed
/// fields.
#[model(
    name = "test_orm.precompute.combo",
    description = "yet another model with precomputed fields",
    addon = "test_compute"
)]
pub struct PrecomputeCombo {
    #[field(char)]
    pub name: Option<Arc<str>>,
    #[field(char, compute = "_compute_reader", precompute, store)]
    pub reader: Option<Arc<str>>,
    #[field(char, compute = "_compute_editer", precompute, store, readonly = false)]
    pub editer: Option<Arc<str>>,
    #[field(
        char,
        compute = "_compute_setter",
        precompute,
        inverse = "_inverse_setter",
        store
    )]
    pub setter: Option<Arc<str>>,
}

#[methods(PrecomputeCombo, addon = "test_compute")]
impl PrecomputeCombo {
    #[compute(depends = ["name"])]
    fn _compute_reader(batch: &mut Batch<'_, PrecomputeCombo>) -> Result<()> {
        for i in batch.indices() {
            let name = batch.get(i, PrecomputeCombo::NAME)?;
            batch.set(i, PrecomputeCombo::READER, name)?;
        }
        Ok(())
    }

    #[compute(depends = ["name"])]
    fn _compute_editer(batch: &mut Batch<'_, PrecomputeCombo>) -> Result<()> {
        for i in batch.indices() {
            let name = batch.get(i, PrecomputeCombo::NAME)?;
            batch.set(i, PrecomputeCombo::EDITER, name)?;
        }
        Ok(())
    }

    #[compute(depends = ["name"])]
    fn _compute_setter(batch: &mut Batch<'_, PrecomputeCombo>) -> Result<()> {
        for i in batch.indices() {
            let name = batch.get(i, PrecomputeCombo::NAME)?;
            batch.set(i, PrecomputeCombo::SETTER, name)?;
        }
        Ok(())
    }

    #[method]
    async fn _inverse_setter(
        this: &RecordSet<PrecomputeCombo>,
        _tx: &mut Transaction,
    ) -> Result<()> {
        log(this.env(), "inverse_setter");
        Ok(())
    }
}

/// A stored compute with a constraint on its result (scalar variant).
#[model(
    name = "test_orm.constrained",
    description = "Constrained",
    addon = "test_compute"
)]
pub struct Constrained {
    #[field(char)]
    pub name: Option<Arc<str>>,
    #[field(char, compute = "_compute_upper", store)]
    pub upper: Option<Arc<str>>,
    #[field(char)]
    pub note: Option<Arc<str>>,
}

#[methods(Constrained, addon = "test_compute")]
impl Constrained {
    #[compute(depends = ["name"])]
    fn _compute_upper(batch: &mut Batch<'_, Constrained>) -> Result<()> {
        for i in batch.indices() {
            let name = batch.get(i, Constrained::NAME)?;
            batch.set(i, Constrained::UPPER, text(&name).to_uppercase())?;
        }
        Ok(())
    }

    #[constrains("upper", "note")]
    async fn _check_upper(this: &RecordSet<Constrained>, tx: &mut Transaction) -> Result<()> {
        log(this.env(), &format!("check su={}", this.env().su()));
        for rec in this.iter() {
            let upper = rec.get(tx, Constrained::UPPER).await?;
            if text(&upper) == "BAD" {
                return Err(Error::validation("upper may not be BAD"));
            }
            let note = rec.get(tx, Constrained::NOTE).await?;
            if text(&note) == "bad" {
                return Err(Error::validation("note may not be bad"));
            }
        }
        Ok(())
    }
}

/// A stored compute that fails on demand and reads its own old value.
#[model(
    name = "test_orm.compute.raises",
    description = "Raises",
    addon = "test_compute"
)]
pub struct ComputeRaises {
    #[field(char)]
    pub foo: Option<Arc<str>>,
    #[field(char, compute = "_compute_bar", store)]
    pub bar: Option<Arc<str>>,
}

#[methods(ComputeRaises, addon = "test_compute")]
impl ComputeRaises {
    #[compute(depends = ["foo"])]
    fn _compute_bar(batch: &mut Batch<'_, ComputeRaises>) -> Result<()> {
        for i in batch.indices() {
            let foo = batch.get(i, ComputeRaises::FOO)?;
            if text(&foo) == "boom" {
                return Err(Error::user("boom"));
            }
            let old = batch.get(i, ComputeRaises::BAR)?;
            let new = match old.as_deref() {
                None | Some("") => text(&foo).to_owned(),
                Some(old) => format!("{old}|{}", text(&foo)),
            };
            batch.set(i, ComputeRaises::BAR, new)?;
        }
        Ok(())
    }
}

/// A one-segment related field, editable (scalar part), and a cache-only field
/// with a default.
#[model(
    name = "test_orm.related_single",
    description = "Related single",
    addon = "test_compute"
)]
pub struct RelatedSingle {
    #[field(char)]
    pub name: Option<Arc<str>>,
    #[field(char, related = "name", readonly = false)]
    pub related_name: Option<Arc<str>>,
    #[field(integer, default = 0)]
    pub color: i32,
    #[field(char, store = false)]
    pub dummy: Option<Arc<str>>,
}

/// Dependencies accumulated over an override: the base compute depends on `a`.
#[model(
    name = "test_orm.depends_ext",
    description = "Depends ext",
    addon = "test_compute"
)]
pub struct DependsExt {
    #[field(char)]
    pub a: Option<Arc<str>>,
    #[field(char)]
    pub b: Option<Arc<str>>,
    #[field(char, compute = "_compute_x")]
    pub x: Option<Arc<str>>,
    #[field(char, compute = "_compute_y", store, depends = ["b"])]
    pub y: Option<Arc<str>>,
}

#[methods(DependsExt, addon = "test_compute")]
impl DependsExt {
    #[compute(depends = ["a"])]
    fn _compute_x(batch: &mut Batch<'_, DependsExt>) -> Result<()> {
        for i in batch.indices() {
            let a = batch.get(i, DependsExt::A)?;
            batch.set(i, DependsExt::X, text(&a).to_owned())?;
        }
        Ok(())
    }

    /// The explicit `depends = ["b"]` on the field replaces this method's.
    #[compute(depends = ["a"], reads = ["a"])]
    fn _compute_y(batch: &mut Batch<'_, DependsExt>) -> Result<()> {
        for i in batch.indices() {
            let a = batch.get(i, DependsExt::A)?;
            let b = batch.get(i, DependsExt::B)?;
            batch.set(i, DependsExt::Y, format!("{}+{}", text(&a), text(&b)))?;
        }
        Ok(())
    }
}

/// Counts the calls of a non-stored compute.
#[model(
    name = "test_orm.counted",
    description = "Counted",
    addon = "test_compute"
)]
pub struct Counted {
    #[field(integer)]
    pub n: i32,
    #[field(integer, compute = "_compute_double")]
    pub double: i32,
}

#[methods(Counted, addon = "test_compute")]
impl Counted {
    #[compute(depends = ["n"])]
    fn _compute_double(batch: &mut Batch<'_, Counted>) -> Result<()> {
        log(batch.env(), &format!("double x{}", batch.len()));
        for i in batch.indices() {
            let n = batch.get(i, Counted::N)?;
            batch.set(i, Counted::DOUBLE, n * 2)?;
        }
        Ok(())
    }
}

/// A compute reading a field it did not declare.
#[model(
    name = "test_orm.undeclared",
    description = "Undeclared",
    addon = "test_compute"
)]
pub struct Undeclared {
    #[field(char)]
    pub a: Option<Arc<str>>,
    #[field(char)]
    pub b: Option<Arc<str>>,
    #[field(char, compute = "_compute_c")]
    pub c: Option<Arc<str>>,
}

#[methods(Undeclared, addon = "test_compute")]
impl Undeclared {
    #[compute(depends = ["a"])]
    fn _compute_c(batch: &mut Batch<'_, Undeclared>) -> Result<()> {
        for i in batch.indices() {
            let b = batch.get(i, Undeclared::B)?;
            batch.set(i, Undeclared::C, b)?;
        }
        Ok(())
    }
}

/// A diamond: `b` and `c` (non-stored) both depend on `a`, `d` (stored) on both (review item 11).
#[model(
    name = "test_orm.diamond",
    description = "Diamond",
    addon = "test_compute"
)]
pub struct Diamond {
    #[field(char)]
    pub a: Option<Arc<str>>,
    #[field(char, compute = "_compute_b")]
    pub b: Option<Arc<str>>,
    #[field(char, compute = "_compute_c")]
    pub c: Option<Arc<str>>,
    #[field(char, compute = "_compute_d", store)]
    pub d: Option<Arc<str>>,
}

#[methods(Diamond, addon = "test_compute")]
impl Diamond {
    #[compute(depends = ["a"])]
    fn _compute_b(batch: &mut Batch<'_, Diamond>) -> Result<()> {
        log(batch.env(), "b");
        for i in batch.indices() {
            let a = batch.get(i, Diamond::A)?;
            batch.set(i, Diamond::B, format!("{}b", text(&a)))?;
        }
        Ok(())
    }

    #[compute(depends = ["a"])]
    fn _compute_c(batch: &mut Batch<'_, Diamond>) -> Result<()> {
        log(batch.env(), "c");
        for i in batch.indices() {
            let a = batch.get(i, Diamond::A)?;
            batch.set(i, Diamond::C, format!("{}c", text(&a)))?;
        }
        Ok(())
    }

    #[compute(depends = ["b", "c"])]
    fn _compute_d(batch: &mut Batch<'_, Diamond>) -> Result<()> {
        log(batch.env(), "d");
        for i in batch.indices() {
            let b = batch.get(i, Diamond::B)?;
            let c = batch.get(i, Diamond::C)?;
            batch.set(i, Diamond::D, format!("{}+{}", text(&b), text(&c)))?;
        }
        Ok(())
    }
}

/// Computes reading the built-in `display_name` (review item 3).
#[model(
    name = "test_orm.shouting",
    description = "Shouting",
    addon = "test_compute"
)]
pub struct Shouting {
    #[field(char)]
    pub name: Option<Arc<str>>,
    #[field(char, compute = "_compute_shout")]
    pub shout: Option<Arc<str>>,
    #[field(char, compute = "_compute_label", store)]
    pub label: Option<Arc<str>>,
}

#[methods(Shouting, addon = "test_compute")]
impl Shouting {
    #[compute(depends = ["display_name"])]
    fn _compute_shout(batch: &mut Batch<'_, Shouting>) -> Result<()> {
        for i in batch.indices() {
            let name = batch.get(i, Base::DISPLAY_NAME)?;
            batch.set(i, Shouting::SHOUT, text(&name).to_uppercase())?;
        }
        Ok(())
    }

    #[compute(depends = ["display_name"])]
    fn _compute_label(batch: &mut Batch<'_, Shouting>) -> Result<()> {
        for i in batch.indices() {
            let name = batch.get(i, Base::DISPLAY_NAME)?;
            batch.set(i, Shouting::LABEL, format!("<{}>", text(&name)))?;
        }
        Ok(())
    }
}

/// Precompute eligibility (review item 5): `p` reads the async `s` (so it cannot be computed before
/// the INSERT); `q` opts out with `precompute = false`.
#[model(
    name = "test_orm.precompute.reads",
    description = "Precompute reads",
    addon = "test_compute"
)]
pub struct PrecomputeReads {
    #[field(char)]
    pub name: Option<Arc<str>>,
    #[field(char, compute = "_compute_s", store)]
    pub s: Option<Arc<str>>,
    #[field(char, compute = "_compute_p", store)]
    pub p: Option<Arc<str>>,
    #[field(char, compute = "_compute_q", store, precompute = false)]
    pub q: Option<Arc<str>>,
}

#[methods(PrecomputeReads, addon = "test_compute")]
impl PrecomputeReads {
    #[compute(depends = ["name"])]
    async fn _compute_s(
        this: &RecordSet<PrecomputeReads>,
        tx: &mut Transaction,
        out: &mut Assign<'_, PrecomputeReads>,
    ) -> Result<()> {
        let names = this.mapped(tx, PrecomputeReads::NAME).await?;
        for (i, n) in names.iter().enumerate() {
            out.set(i, PrecomputeReads::S, format!("{}!", text(n)))?;
        }
        Ok(())
    }

    #[compute(depends = ["name"], reads = ["s"])]
    fn _compute_p(batch: &mut Batch<'_, PrecomputeReads>) -> Result<()> {
        for i in batch.indices() {
            let s = batch.get(i, PrecomputeReads::S)?;
            batch.set(i, PrecomputeReads::P, format!("p:{}", text(&s)))?;
        }
        Ok(())
    }

    #[compute(depends = ["name"])]
    fn _compute_q(batch: &mut Batch<'_, PrecomputeReads>) -> Result<()> {
        for i in batch.indices() {
            let name = batch.get(i, PrecomputeReads::NAME)?;
            batch.set(i, PrecomputeReads::Q, format!("q:{}", text(&name)))?;
        }
        Ok(())
    }
}

/// A required stored compute that opts out of precompute: computed after the INSERT, so a create
/// that gives no value is refused before the INSERT (note).
#[model(
    name = "test_orm.compute.required_late",
    description = "Required late",
    addon = "test_compute"
)]
pub struct RequiredLate {
    #[field(char)]
    pub name: Option<Arc<str>>,
    #[field(char, compute = "_compute_code", store, required, precompute = false)]
    pub code: Option<Arc<str>>,
}

#[methods(RequiredLate, addon = "test_compute")]
impl RequiredLate {
    #[compute(depends = ["name"])]
    fn _compute_code(batch: &mut Batch<'_, RequiredLate>) -> Result<()> {
        for i in batch.indices() {
            let name = batch.get(i, RequiredLate::NAME)?;
            batch.set(i, RequiredLate::CODE, format!("code:{}", text(&name)))?;
        }
        Ok(())
    }
}

/// A stored async compute without queries (review item 13: unchanged results write nothing).
#[model(
    name = "test_orm.async_len",
    description = "Async len",
    addon = "test_compute"
)]
pub struct AsyncLen {
    #[field(char)]
    pub name: Option<Arc<str>>,
    #[field(integer, compute = "_compute_len", store)]
    pub len: i32,
}

#[methods(AsyncLen, addon = "test_compute")]
impl AsyncLen {
    #[compute(depends = ["name"])]
    async fn _compute_len(
        this: &RecordSet<AsyncLen>,
        tx: &mut Transaction,
        out: &mut Assign<'_, AsyncLen>,
    ) -> Result<()> {
        let names = this.mapped(tx, AsyncLen::NAME).await?;
        for (i, n) in names.iter().enumerate() {
            out.set(
                i,
                AsyncLen::LEN,
                i32::try_from(text(n).len()).unwrap_or(i32::MAX),
            )?;
        }
        Ok(())
    }
}

/// A stored async compute that creates a record of its own model each time it runs, so a flush
/// never converges (COMP §7 D5).
#[model(
    name = "test_orm.pingpong",
    description = "Ping pong",
    addon = "test_compute"
)]
pub struct PingPong {
    #[field(integer)]
    pub n: i32,
    #[field(integer, compute = "_compute_m", store)]
    pub m: i32,
}

#[methods(PingPong, addon = "test_compute")]
impl PingPong {
    #[compute(depends = ["n"])]
    async fn _compute_m(
        this: &RecordSet<PingPong>,
        tx: &mut Transaction,
        out: &mut Assign<'_, PingPong>,
    ) -> Result<()> {
        let model = this.env().model::<PingPong>()?;
        model
            .create(tx, vec![model.vals().set(PingPong::N, 1)])
            .await?;
        out.set_all(PingPong::M, 1)
    }
}

/// Every context key kind on one non-sudo field.
#[model(
    name = "test_orm.ctx_key",
    description = "Context key",
    addon = "test_compute"
)]
pub struct CtxKey {
    #[field(
        char,
        compute = "_compute_all",
        depends_context = ["company", "uid", "lang", "active_test", "k"]
    )]
    pub all: Option<Arc<str>>,
}

#[methods(CtxKey, addon = "test_compute")]
impl CtxKey {
    #[compute]
    fn _compute_all(batch: &mut Batch<'_, CtxKey>) -> Result<()> {
        batch.set_all(CtxKey::ALL, "x")
    }
}

/// The extension of `test_orm.depends_ext`: overrides `_compute_x` with an extra
/// dependency on `b` and calls `super()`.
pub mod ext {
    use erp_sdk::prelude::*;

    use super::{DependsExt, text};

    erp_sdk::addon!(name = "test_compute_ext", depends = ["test_compute"]);

    /// The extension's declaration of the model.
    #[extend(DependsExt, addon = "test_compute_ext")]
    pub struct DependsExtB {}

    #[methods(DependsExt, addon = "test_compute_ext")]
    impl DependsExtB {
        #[compute(override, depends = ["b"])]
        fn _compute_x(batch: &mut Batch<'_, DependsExt>, sup: ComputeSuper) -> Result<()> {
            sup.call(batch)?;
            for i in batch.indices() {
                let x = batch.get(i, DependsExt::X)?;
                let b = batch.get(i, DependsExt::B)?;
                batch.set(i, DependsExt::X, format!("{}/{}", text(&x), text(&b)))?;
            }
            Ok(())
        }
    }
}
