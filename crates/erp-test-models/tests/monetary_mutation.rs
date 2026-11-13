//! Regression entry points for. These cases cover only the named portions
//! of FIELD-024 and FIELD-026; broader mutation-path coverage remains outstanding.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

use erp_sdk::prelude::*;
use erp_sdk::{ErrorKind, RawVals, UpsertCall, Value, ValueMap};
use erp_test_models::base_users;
use erp_testkit::Harness;

mod fixture {
    use super::*;
    pub static EMPTY_NATIVE_CALLS: std::sync::atomic::AtomicUsize =
        std::sync::atomic::AtomicUsize::new(0);

    erp_sdk::addon!(name = "test_monetary_mutation", depends = ["base"]);

    #[model(
        name = "test_monetary_mutation.line",
        addon = "test_monetary_mutation",
        keys = [key(name = "_name_key", fields = ["name"], message = "Duplicate line.")]
    )]
    pub struct Line {
        #[field(char, required)]
        pub name: Option<Arc<str>>,
        #[field(many2one, comodel = "res.currency")]
        pub currency_id: Option<RecId>,
        #[field(monetary, currency_field = "currency_id")]
        pub amount: Decimal,
        #[field(float, digits = 0, compute = "_observe_amount", store = true)]
        pub observed_amount: Decimal,
    }

    #[methods(Line, addon = "test_monetary_mutation")]
    impl Line {
        #[compute(depends = ["amount"])]
        fn _observe_amount(batch: &mut Batch<'_, Line>) -> Result<()> {
            for i in batch.indices() {
                let amount = batch.get(i, Line::AMOUNT)?;
                batch.set(i, Line::OBSERVED_AMOUNT, amount)?;
            }
            Ok(())
        }
    }

    #[model(
        name = "test_monetary_mutation.computed",
        addon = "test_monetary_mutation"
    )]
    pub struct Computed {
        #[field(many2one, comodel = "res.currency")]
        pub selected_currency_id: Option<RecId>,
        #[field(float, digits = 0)]
        pub raw: Decimal,
        #[field(many2one, comodel = "res.currency", compute = "_money", store = true)]
        pub currency_id: Option<RecId>,
        #[field(
            monetary,
            currency_field = "currency_id",
            compute = "_money",
            store = true
        )]
        pub amount: Decimal,
        #[field(float, digits = 0, compute = "_observe", store = true)]
        pub observed: Decimal,
        #[field(monetary, currency_field = "currency_id", compute = "_virtual")]
        pub virtual_amount: Decimal,
    }

    #[model(
        name = "test_monetary_mutation.related",
        addon = "test_monetary_mutation"
    )]
    pub struct Related {
        #[field(many2one, comodel = "test_monetary_mutation.line")]
        pub owner_id: Option<RecId>,
        #[field(many2one, comodel = "res.currency", related = "owner_id.currency_id")]
        pub currency_id: Option<RecId>,
        #[field(monetary, currency_field = "currency_id")]
        pub amount: Decimal,
    }

    #[model(
        name = "test_monetary_mutation.late_hop",
        addon = "test_monetary_mutation"
    )]
    pub struct LateHop {
        #[field(many2one, comodel = "test_monetary_mutation.line")]
        pub selected_owner_id: Option<RecId>,
        #[field(
            many2one,
            comodel = "test_monetary_mutation.line",
            compute = "_owner",
            store = true
        )]
        pub owner_id: Option<RecId>,
        #[field(many2one, comodel = "res.currency", related = "owner_id.currency_id")]
        pub currency_id: Option<RecId>,
        #[field(monetary, currency_field = "currency_id")]
        pub amount: Decimal,
        #[field(float, digits = 0, compute = "_observe", store = true)]
        pub observed: Decimal,
    }
    #[methods(LateHop, addon = "test_monetary_mutation")]
    impl LateHop {
        #[compute(depends = ["selected_owner_id"])]
        fn _owner(batch: &mut Batch<'_, LateHop>) -> Result<()> {
            for i in batch.indices() {
                batch.set(
                    i,
                    LateHop::OWNER_ID,
                    batch.get(i, LateHop::SELECTED_OWNER_ID)?,
                )?;
            }
            Ok(())
        }
        #[compute(depends = ["amount"])]
        fn _observe(batch: &mut Batch<'_, LateHop>) -> Result<()> {
            for i in batch.indices() {
                batch.set(i, LateHop::OBSERVED, batch.get(i, LateHop::AMOUNT)?)?;
            }
            Ok(())
        }
    }

    #[model(
        name = "test_monetary_mutation.private_compute",
        addon = "test_monetary_mutation"
    )]
    pub struct PrivateCompute {
        #[field(many2one, comodel = "res.currency")]
        pub selected_currency_id: Option<RecId>,
        #[field(many2one, comodel = "res.currency")]
        pub secret_currency_id: Option<RecId>,
        #[field(
            many2one,
            comodel = "res.currency",
            compute = "_currency",
            compute_sudo = false
        )]
        pub currency_id: Option<RecId>,
        #[field(monetary, currency_field = "currency_id")]
        pub amount: Decimal,
        #[field(
            many2one,
            comodel = "res.currency",
            compute = "_currency_sudo",
            compute_sudo = true
        )]
        pub currency_sudo_id: Option<RecId>,
        #[field(monetary, currency_field = "currency_sudo_id")]
        pub amount_sudo: Decimal,
    }
    #[methods(PrivateCompute, addon = "test_monetary_mutation")]
    impl PrivateCompute {
        #[compute(depends = ["selected_currency_id"], reads = ["secret_currency_id"])]
        fn _currency(batch: &mut Batch<'_, PrivateCompute>) -> Result<()> {
            for i in batch.indices() {
                batch.set(
                    i,
                    PrivateCompute::CURRENCY_ID,
                    batch.get(i, PrivateCompute::SECRET_CURRENCY_ID)?,
                )?;
            }
            Ok(())
        }
        #[compute(depends = ["selected_currency_id"], reads = ["secret_currency_id"])]
        fn _currency_sudo(batch: &mut Batch<'_, PrivateCompute>) -> Result<()> {
            for i in batch.indices() {
                batch.set(
                    i,
                    PrivateCompute::CURRENCY_SUDO_ID,
                    batch.get(i, PrivateCompute::SECRET_CURRENCY_ID)?,
                )?;
            }
            Ok(())
        }
    }

    #[model(
        name = "test_monetary_mutation.private_related",
        addon = "test_monetary_mutation"
    )]
    pub struct PrivateRelated {
        #[field(many2one, comodel = "test_monetary_mutation.private_compute")]
        pub parent_id: Option<RecId>,
        #[field(
            many2one,
            comodel = "res.currency",
            related = "parent_id.currency_id",
            compute_sudo = false
        )]
        pub currency_id: Option<RecId>,
        #[field(monetary, currency_field = "currency_id")]
        pub own_amount: Decimal,
    }
    #[model(name = "test_monetary_mutation.private_child", addon = "test_monetary_mutation", inherits = [("test_monetary_mutation.private_compute", "parent_id")])]
    pub struct PrivateChild {
        #[field(
            many2one,
            comodel = "test_monetary_mutation.private_compute",
            required,
            ondelete = "cascade"
        )]
        pub parent_id: Option<RecId>,
        #[field(monetary, currency_field = "currency_id")]
        pub own_amount: Decimal,
    }
    #[model(
        name = "test_monetary_mutation.private_link",
        addon = "test_monetary_mutation"
    )]
    pub struct PrivateLink {
        #[field(many2one, comodel = "test_monetary_mutation.private_compute")]
        pub secret_parent_id: Option<RecId>,
        #[field(
            many2one,
            comodel = "test_monetary_mutation.private_compute",
            compute = "_parent",
            store = true,
            precompute = false,
            compute_sudo = false
        )]
        pub parent_id: Option<RecId>,
    }
    #[methods(PrivateLink, addon = "test_monetary_mutation")]
    impl PrivateLink {
        #[compute(depends = ["secret_parent_id"])]
        fn _parent(batch: &mut Batch<'_, PrivateLink>) -> Result<()> {
            if matches!(
                batch.env().context().get("empty_hop_probe"),
                Some(Value::Bool(true))
            ) {
                EMPTY_NATIVE_CALLS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
            for i in batch.indices() {
                batch.set(
                    i,
                    PrivateLink::PARENT_ID,
                    batch.get(i, PrivateLink::SECRET_PARENT_ID)?,
                )?;
            }
            Ok(())
        }
    }
    #[model(
        name = "test_monetary_mutation.private_deep",
        addon = "test_monetary_mutation"
    )]
    pub struct PrivateDeep {
        #[field(many2one, comodel = "test_monetary_mutation.private_link")]
        pub link_id: Option<RecId>,
        #[field(
            many2one,
            comodel = "res.currency",
            related = "link_id.parent_id.currency_id",
            compute_sudo = false
        )]
        pub currency_id: Option<RecId>,
        #[field(monetary, currency_field = "currency_id")]
        pub own_amount: Decimal,
    }

    #[model(
        name = "test_monetary_mutation.alias",
        addon = "test_monetary_mutation"
    )]
    pub struct Alias {
        #[field(many2one, comodel = "res.currency")]
        pub actual_currency_id: Option<RecId>,
        #[field(many2one, comodel = "res.currency", related = "actual_currency_id")]
        pub currency_id: Option<RecId>,
        #[field(monetary, currency_field = "currency_id")]
        pub amount: Decimal,
    }

    #[model(
        name = "test_monetary_mutation.derived",
        addon = "test_monetary_mutation"
    )]
    pub struct Derived {
        #[field(many2one, comodel = "res.currency")]
        pub selected_currency_id: Option<RecId>,
        #[field(many2one, comodel = "res.currency")]
        pub read_currency_id: Option<RecId>,
        #[field(
            many2one,
            comodel = "res.currency",
            compute = "_selected",
            store = true
        )]
        pub intermediate_currency_id: Option<RecId>,
        #[field(
            many2one,
            comodel = "res.currency",
            compute = "_currency",
            store = true
        )]
        pub currency_id: Option<RecId>,
        #[field(monetary, currency_field = "currency_id")]
        pub amount: Decimal,
        #[field(float, digits = 0, compute = "_observe", store = true)]
        pub observed: Decimal,
    }

    #[methods(Derived, addon = "test_monetary_mutation")]
    impl Derived {
        #[compute(depends = ["amount"])]
        fn _observe(batch: &mut Batch<'_, Derived>) -> Result<()> {
            for i in batch.indices() {
                batch.set(i, Derived::OBSERVED, batch.get(i, Derived::AMOUNT)?)?;
            }
            Ok(())
        }

        #[compute(depends = ["selected_currency_id"])]
        fn _selected(batch: &mut Batch<'_, Derived>) -> Result<()> {
            for i in batch.indices() {
                batch.set(
                    i,
                    Derived::INTERMEDIATE_CURRENCY_ID,
                    batch.get(i, Derived::SELECTED_CURRENCY_ID)?,
                )?;
            }
            Ok(())
        }

        #[compute(depends = ["intermediate_currency_id"], reads = ["read_currency_id"])]
        fn _currency(batch: &mut Batch<'_, Derived>) -> Result<()> {
            for i in batch.indices() {
                let currency = batch
                    .get(i, Derived::READ_CURRENCY_ID)?
                    .or(batch.get(i, Derived::INTERMEDIATE_CURRENCY_ID)?);
                batch.set(i, Derived::CURRENCY_ID, currency)?;
            }
            Ok(())
        }
    }

    #[model(
        name = "test_monetary_mutation.money_dependency",
        addon = "test_monetary_mutation"
    )]
    pub struct MoneyDependency {
        #[field(monetary, currency_field = "currency_id")]
        pub amount: Decimal,
        #[field(monetary, currency_field = "input_currency_id")]
        pub input_amount: Decimal,
        #[field(many2one, comodel = "res.currency")]
        pub input_currency_id: Option<RecId>,
        #[field(many2one, comodel = "res.currency")]
        pub selected_currency_id: Option<RecId>,
        #[field(
            many2one,
            comodel = "res.currency",
            compute = "_currency",
            store = true
        )]
        pub currency_id: Option<RecId>,
    }

    #[methods(MoneyDependency, addon = "test_monetary_mutation")]
    impl MoneyDependency {
        #[compute(depends = ["input_amount"], reads = ["selected_currency_id", "input_currency_id"])]
        fn _currency(batch: &mut Batch<'_, MoneyDependency>) -> Result<()> {
            for i in batch.indices() {
                let currency = batch
                    .get(i, MoneyDependency::SELECTED_CURRENCY_ID)?
                    .or(batch.get(i, MoneyDependency::INPUT_CURRENCY_ID)?);
                batch.set(i, MoneyDependency::CURRENCY_ID, currency)?;
            }
            Ok(())
        }
    }

    #[model(name = "test_monetary_mutation.form", addon = "test_monetary_mutation")]
    pub struct Form {
        #[field(many2many, comodel = "test_monetary_mutation.line")]
        pub line_ids: Vec<RecId>,
    }

    #[model(
        name = "test_monetary_mutation.late_currency",
        addon = "test_monetary_mutation"
    )]
    pub struct LateCurrency {
        #[field(many2one, comodel = "res.currency")]
        pub selected_currency_id: Option<RecId>,
        #[field(
            many2one,
            comodel = "res.currency",
            compute = "_currency",
            store = true,
            precompute = false
        )]
        pub currency_id: Option<RecId>,
        #[field(monetary, currency_field = "currency_id")]
        pub amount: Decimal,
    }
    #[methods(LateCurrency, addon = "test_monetary_mutation")]
    impl LateCurrency {
        #[compute(depends = ["selected_currency_id"])]
        fn _currency(batch: &mut Batch<'_, LateCurrency>) -> Result<()> {
            if batch.ids().iter().any(|id| id.is_new()) {
                return Err(Error::programming("Late currency must run after INSERT"));
            }
            for i in batch.indices() {
                batch.set(
                    i,
                    LateCurrency::CURRENCY_ID,
                    batch.get(i, LateCurrency::SELECTED_CURRENCY_ID)?,
                )?;
            }
            Ok(())
        }
    }

    #[model(
        name = "test_monetary_mutation.async_currency",
        addon = "test_monetary_mutation"
    )]
    pub struct AsyncCurrency {
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

    #[methods(AsyncCurrency, addon = "test_monetary_mutation")]
    impl AsyncCurrency {
        #[compute(candidate, depends = ["selected_currency_id"])]
        async fn _currency(
            this: &RecordSet<AsyncCurrency>,
            tx: &mut ReadTx<'_>,
            out: &mut Assign<'_, AsyncCurrency>,
        ) -> Result<()> {
            let selected = tx.mapped(this, AsyncCurrency::SELECTED_CURRENCY_ID).await?;
            for (i, currency) in selected.into_iter().enumerate() {
                out.set(i, AsyncCurrency::CURRENCY_ID, currency)?;
            }
            Ok(())
        }
    }

    /// The same shape without `candidate`: an ordinary async compute, which keeps the
    /// same-call refusal.
    #[model(
        name = "test_monetary_mutation.async_plain_currency",
        addon = "test_monetary_mutation"
    )]
    pub struct AsyncPlainCurrency {
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

    #[methods(AsyncPlainCurrency, addon = "test_monetary_mutation")]
    impl AsyncPlainCurrency {
        #[compute(depends = ["selected_currency_id"])]
        async fn _currency(
            this: &RecordSet<AsyncPlainCurrency>,
            tx: &mut Transaction,
            out: &mut Assign<'_, AsyncPlainCurrency>,
        ) -> Result<()> {
            for (i, row) in this.iter().enumerate() {
                out.set(
                    i,
                    AsyncPlainCurrency::CURRENCY_ID,
                    row.get(tx, AsyncPlainCurrency::SELECTED_CURRENCY_ID)
                        .await?,
                )?;
            }
            Ok(())
        }
    }

    #[model(name = "test_monetary_mutation.child", addon = "test_monetary_mutation", inherits = [("test_monetary_mutation.line", "line_id")])]
    pub struct Child {
        #[field(
            many2one,
            comodel = "test_monetary_mutation.line",
            required,
            ondelete = "cascade"
        )]
        pub line_id: Option<RecId>,
        #[field(monetary, currency_field = "currency_id")]
        pub own_amount: Decimal,
    }

    #[model(
        name = "test_monetary_mutation.inverse",
        addon = "test_monetary_mutation"
    )]
    pub struct Inverse {
        #[field(many2one, comodel = "res.currency")]
        pub currency_id: Option<RecId>,
        #[field(monetary, currency_field = "currency_id", inverse = "_save_amount")]
        pub amount: Decimal,
        #[field(float, digits = 0)]
        pub observed: Decimal,
    }

    #[model(
        name = "test_monetary_mutation.scoped_owner",
        company_access = unrestricted(reason = "monetary mutation fixture exercises currency access separately"),
        addon = "test_monetary_mutation"
    )]
    pub struct ScopedOwner {
        #[field(many2one, comodel = "res.company")]
        pub company_id: Option<RecId>,
        #[field(many2one, comodel = "res.currency")]
        pub currency_id: Option<RecId>,
    }

    #[model(
        name = "test_monetary_mutation.scoped_line",
        addon = "test_monetary_mutation"
    )]
    pub struct ScopedLine {
        #[field(many2one, comodel = "test_monetary_mutation.scoped_owner")]
        pub owner_id: Option<RecId>,
        #[field(many2one, comodel = "res.currency", related = "owner_id.currency_id")]
        pub currency_id: Option<RecId>,
        #[field(monetary, currency_field = "currency_id")]
        pub amount: Decimal,
    }

    #[model(
        name = "test_monetary_mutation.scoped_compute",
        addon = "test_monetary_mutation"
    )]
    pub struct ScopedCompute {
        #[field(many2one, comodel = "test_monetary_mutation.scoped_owner")]
        pub owner_id: Option<RecId>,
        #[field(many2one, comodel = "res.currency")]
        pub trigger_currency_id: Option<RecId>,
        #[field(many2one, comodel = "res.currency", related = "owner_id.currency_id")]
        pub source_currency_id: Option<RecId>,
        #[field(
            many2one,
            comodel = "res.currency",
            compute = "_currency",
            compute_sudo = false
        )]
        pub currency_id: Option<RecId>,
        #[field(monetary, currency_field = "currency_id")]
        pub amount: Decimal,
        #[field(
            many2one,
            comodel = "res.currency",
            compute = "_currency_sudo",
            compute_sudo = true
        )]
        pub currency_sudo_id: Option<RecId>,
        #[field(monetary, currency_field = "currency_sudo_id")]
        pub amount_sudo: Decimal,
    }
    #[methods(ScopedCompute, addon = "test_monetary_mutation")]
    impl ScopedCompute {
        #[compute(depends = ["trigger_currency_id"], reads = ["source_currency_id"])]
        fn _currency(batch: &mut Batch<'_, ScopedCompute>) -> Result<()> {
            for i in batch.indices() {
                batch.set(
                    i,
                    ScopedCompute::CURRENCY_ID,
                    batch.get(i, ScopedCompute::SOURCE_CURRENCY_ID)?,
                )?;
            }
            Ok(())
        }
        #[compute(depends = ["trigger_currency_id"], reads = ["source_currency_id"])]
        fn _currency_sudo(batch: &mut Batch<'_, ScopedCompute>) -> Result<()> {
            for i in batch.indices() {
                batch.set(
                    i,
                    ScopedCompute::CURRENCY_SUDO_ID,
                    batch.get(i, ScopedCompute::SOURCE_CURRENCY_ID)?,
                )?;
            }
            Ok(())
        }
    }

    #[model(
        name = "test_monetary_mutation.scoped_related",
        addon = "test_monetary_mutation"
    )]
    pub struct ScopedRelated {
        #[field(many2one, comodel = "test_monetary_mutation.scoped_compute")]
        pub parent_id: Option<RecId>,
        #[field(
            many2one,
            comodel = "res.currency",
            related = "parent_id.currency_id",
            compute_sudo = false
        )]
        pub currency_id: Option<RecId>,
        #[field(monetary, currency_field = "currency_id")]
        pub amount: Decimal,
        #[field(
            many2one,
            comodel = "res.currency",
            related = "parent_id.currency_sudo_id",
            compute_sudo = false
        )]
        pub currency_sudo_id: Option<RecId>,
        #[field(monetary, currency_field = "currency_sudo_id")]
        pub amount_sudo: Decimal,
    }
    #[model(name = "test_monetary_mutation.scoped_child", addon = "test_monetary_mutation", inherits = [("test_monetary_mutation.scoped_compute", "parent_id")])]
    pub struct ScopedChild {
        #[field(
            many2one,
            comodel = "test_monetary_mutation.scoped_compute",
            required,
            ondelete = "cascade"
        )]
        pub parent_id: Option<RecId>,
        #[field(monetary, currency_field = "currency_id")]
        pub own_amount: Decimal,
        #[field(monetary, currency_field = "currency_sudo_id")]
        pub own_amount_sudo: Decimal,
    }

    #[methods(Inverse, addon = "test_monetary_mutation")]
    impl Inverse {
        #[method]
        async fn _save_amount(this: &RecordSet<Inverse>, tx: &mut Transaction) -> Result<()> {
            for id in this.rec_ids() {
                let row = this.browse([id.get()])?;
                let amount = row.get(tx, Inverse::AMOUNT).await?;
                row.set(tx, Inverse::OBSERVED, amount).await?;
            }
            Ok(())
        }
    }

    #[methods(Computed, addon = "test_monetary_mutation")]
    impl Computed {
        #[compute(depends = ["raw"])]
        fn _virtual(batch: &mut Batch<'_, Computed>) -> Result<()> {
            for i in batch.indices() {
                let raw = batch.get(i, Computed::RAW)?;
                batch.set(i, Computed::VIRTUAL_AMOUNT, raw)?;
            }
            Ok(())
        }
        #[compute(depends = ["raw", "selected_currency_id"])]
        fn _money(batch: &mut Batch<'_, Computed>) -> Result<()> {
            for i in batch.indices() {
                let currency = batch.get(i, Computed::SELECTED_CURRENCY_ID)?;
                let amount = batch.get(i, Computed::RAW)?;
                batch.set(i, Computed::CURRENCY_ID, currency)?;
                batch.set(i, Computed::AMOUNT, amount)?;
            }
            Ok(())
        }

        #[compute(depends = ["amount"])]
        fn _observe(batch: &mut Batch<'_, Computed>) -> Result<()> {
            for i in batch.indices() {
                let amount = batch.get(i, Computed::AMOUNT)?;
                batch.set(i, Computed::OBSERVED, amount)?;
            }
            Ok(())
        }
    }

    /// One currency, one amount: the smallest shape for record-count bounds.
    #[model(name = "test_monetary_mutation.bulk", addon = "test_monetary_mutation")]
    pub struct Bulk {
        #[field(many2one, comodel = "res.currency")]
        pub currency_id: Option<RecId>,
        #[field(monetary, currency_field = "currency_id")]
        pub amount: Decimal,
    }

    /// Ten monetary inputs (`in1..in10`) whose own currency is plain, and an `amount` whose
    /// currency is a stored compute reading all ten: a write of `amount` plus inputs stages the
    /// inputs again in the same-call dependency projection.
    #[model(name = "test_monetary_mutation.wide", addon = "test_monetary_mutation")]
    pub struct Wide {
        #[field(many2one, comodel = "res.currency")]
        pub input_currency_id: Option<RecId>,
        #[field(many2one, comodel = "res.currency")]
        pub selected_currency_id: Option<RecId>,
        #[field(
            many2one,
            comodel = "res.currency",
            compute = "_currency",
            store = true
        )]
        pub currency_id: Option<RecId>,
        #[field(monetary, currency_field = "input_currency_id")]
        pub in1: Decimal,
        #[field(monetary, currency_field = "input_currency_id")]
        pub in2: Decimal,
        #[field(monetary, currency_field = "input_currency_id")]
        pub in3: Decimal,
        #[field(monetary, currency_field = "input_currency_id")]
        pub in4: Decimal,
        #[field(monetary, currency_field = "input_currency_id")]
        pub in5: Decimal,
        #[field(monetary, currency_field = "input_currency_id")]
        pub in6: Decimal,
        #[field(monetary, currency_field = "input_currency_id")]
        pub in7: Decimal,
        #[field(monetary, currency_field = "input_currency_id")]
        pub in8: Decimal,
        #[field(monetary, currency_field = "input_currency_id")]
        pub in9: Decimal,
        #[field(monetary, currency_field = "input_currency_id")]
        pub in10: Decimal,
        #[field(monetary, currency_field = "currency_id")]
        pub amount: Decimal,
    }
    #[methods(Wide, addon = "test_monetary_mutation")]
    impl Wide {
        #[compute(depends = ["in1", "in2", "in3", "in4", "in5", "in6", "in7", "in8", "in9", "in10"], reads = ["selected_currency_id", "input_currency_id"])]
        fn _currency(batch: &mut Batch<'_, Wide>) -> Result<()> {
            for i in batch.indices() {
                let currency = batch
                    .get(i, Wide::SELECTED_CURRENCY_ID)?
                    .or(batch.get(i, Wide::INPUT_CURRENCY_ID)?);
                batch.set(i, Wide::CURRENCY_ID, currency)?;
            }
            Ok(())
        }
    }

    pub static FILL_RUNS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

    /// Eleven stored monetary fields precomputed by one method: a create that supplies none of
    /// them still stages eleven computed cells per row.
    #[model(
        name = "test_monetary_mutation.wide_computed",
        addon = "test_monetary_mutation"
    )]
    pub struct WideComputed {
        #[field(many2one, comodel = "res.currency")]
        pub selected_currency_id: Option<RecId>,
        #[field(float, digits = 0)]
        pub raw: Decimal,
        #[field(many2one, comodel = "res.currency", compute = "_fill", store = true)]
        pub currency_id: Option<RecId>,
        #[field(
            monetary,
            currency_field = "currency_id",
            compute = "_fill",
            store = true
        )]
        pub c1: Decimal,
        #[field(
            monetary,
            currency_field = "currency_id",
            compute = "_fill",
            store = true
        )]
        pub c2: Decimal,
        #[field(
            monetary,
            currency_field = "currency_id",
            compute = "_fill",
            store = true
        )]
        pub c3: Decimal,
        #[field(
            monetary,
            currency_field = "currency_id",
            compute = "_fill",
            store = true
        )]
        pub c4: Decimal,
        #[field(
            monetary,
            currency_field = "currency_id",
            compute = "_fill",
            store = true
        )]
        pub c5: Decimal,
        #[field(
            monetary,
            currency_field = "currency_id",
            compute = "_fill",
            store = true
        )]
        pub c6: Decimal,
        #[field(
            monetary,
            currency_field = "currency_id",
            compute = "_fill",
            store = true
        )]
        pub c7: Decimal,
        #[field(
            monetary,
            currency_field = "currency_id",
            compute = "_fill",
            store = true
        )]
        pub c8: Decimal,
        #[field(
            monetary,
            currency_field = "currency_id",
            compute = "_fill",
            store = true
        )]
        pub c9: Decimal,
        #[field(
            monetary,
            currency_field = "currency_id",
            compute = "_fill",
            store = true
        )]
        pub c10: Decimal,
        #[field(
            monetary,
            currency_field = "currency_id",
            compute = "_fill",
            store = true
        )]
        pub c11: Decimal,
    }
    #[methods(WideComputed, addon = "test_monetary_mutation")]
    impl WideComputed {
        #[compute(depends = ["raw", "selected_currency_id"])]
        fn _fill(batch: &mut Batch<'_, WideComputed>) -> Result<()> {
            FILL_RUNS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            for i in batch.indices() {
                let raw = batch.get(i, WideComputed::RAW)?;
                batch.set(
                    i,
                    WideComputed::CURRENCY_ID,
                    batch.get(i, WideComputed::SELECTED_CURRENCY_ID)?,
                )?;
                batch.set(i, WideComputed::C1, raw)?;
                batch.set(i, WideComputed::C2, raw)?;
                batch.set(i, WideComputed::C3, raw)?;
                batch.set(i, WideComputed::C4, raw)?;
                batch.set(i, WideComputed::C5, raw)?;
                batch.set(i, WideComputed::C6, raw)?;
                batch.set(i, WideComputed::C7, raw)?;
                batch.set(i, WideComputed::C8, raw)?;
                batch.set(i, WideComputed::C9, raw)?;
                batch.set(i, WideComputed::C10, raw)?;
                batch.set(i, WideComputed::C11, raw)?;
            }
            Ok(())
        }
    }

    pub static PAIR_RUNS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

    /// An editable stored monetary compute with a stored sibling in its group: writing `amount`
    /// protects the group and first brings the sibling's pending recompute up to date.
    #[model(
        name = "test_monetary_mutation.sibling",
        addon = "test_monetary_mutation"
    )]
    pub struct Sibling {
        #[field(float, digits = 0)]
        pub raw: Decimal,
        #[field(many2one, comodel = "res.currency")]
        pub currency_id: Option<RecId>,
        #[field(
            monetary,
            currency_field = "currency_id",
            compute = "_pair",
            store = true,
            readonly = false
        )]
        pub amount: Decimal,
        #[field(float, digits = 0, compute = "_pair", store = true)]
        pub twin: Decimal,
    }
    #[methods(Sibling, addon = "test_monetary_mutation")]
    impl Sibling {
        #[compute(depends = ["raw"])]
        fn _pair(batch: &mut Batch<'_, Sibling>) -> Result<()> {
            PAIR_RUNS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            for i in batch.indices() {
                let raw = batch.get(i, Sibling::RAW)?;
                batch.set(i, Sibling::AMOUNT, raw)?;
                batch.set(i, Sibling::TWIN, raw)?;
            }
            Ok(())
        }
    }

    /// Eleven amounts on one stored computed currency (`selected_currency_id` is its input): a
    /// create that supplies amounts but not the currency keeps them pending until the currency
    /// is computed.
    #[model(
        name = "test_monetary_mutation.multi",
        addon = "test_monetary_mutation"
    )]
    pub struct Multi {
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
        pub a1: Decimal,
        #[field(monetary, currency_field = "currency_id")]
        pub a2: Decimal,
        #[field(monetary, currency_field = "currency_id")]
        pub a3: Decimal,
        #[field(monetary, currency_field = "currency_id")]
        pub a4: Decimal,
        #[field(monetary, currency_field = "currency_id")]
        pub a5: Decimal,
        #[field(monetary, currency_field = "currency_id")]
        pub a6: Decimal,
        #[field(monetary, currency_field = "currency_id")]
        pub a7: Decimal,
        #[field(monetary, currency_field = "currency_id")]
        pub a8: Decimal,
        #[field(monetary, currency_field = "currency_id")]
        pub a9: Decimal,
        #[field(monetary, currency_field = "currency_id")]
        pub a10: Decimal,
        #[field(monetary, currency_field = "currency_id")]
        pub a11: Decimal,
    }
    #[methods(Multi, addon = "test_monetary_mutation")]
    impl Multi {
        #[compute(depends = ["selected_currency_id"])]
        fn _currency(batch: &mut Batch<'_, Multi>) -> Result<()> {
            for i in batch.indices() {
                batch.set(
                    i,
                    Multi::CURRENCY_ID,
                    batch.get(i, Multi::SELECTED_CURRENCY_ID)?,
                )?;
            }
            Ok(())
        }
    }

    /// Two amounts on a computed currency whose compute is triggered by `trig` and only READS
    /// seven monetary inputs: the inputs are staged by the same-call projection and by nothing
    /// else, so the projection's own charge is what bounds them.
    #[model(
        name = "test_monetary_mutation.proj_reads",
        addon = "test_monetary_mutation"
    )]
    pub struct ProjReads {
        #[field(many2one, comodel = "res.currency")]
        pub input_currency_id: Option<RecId>,
        #[field(many2one, comodel = "res.currency")]
        pub selected_currency_id: Option<RecId>,
        #[field(float, digits = 0)]
        pub trig: Decimal,
        #[field(
            many2one,
            comodel = "res.currency",
            compute = "_currency",
            store = true
        )]
        pub currency_id: Option<RecId>,
        #[field(monetary, currency_field = "input_currency_id")]
        pub in1: Decimal,
        #[field(monetary, currency_field = "input_currency_id")]
        pub in2: Decimal,
        #[field(monetary, currency_field = "input_currency_id")]
        pub in3: Decimal,
        #[field(monetary, currency_field = "input_currency_id")]
        pub in4: Decimal,
        #[field(monetary, currency_field = "input_currency_id")]
        pub in5: Decimal,
        #[field(monetary, currency_field = "input_currency_id")]
        pub in6: Decimal,
        #[field(monetary, currency_field = "input_currency_id")]
        pub in7: Decimal,
        #[field(monetary, currency_field = "currency_id")]
        pub amount: Decimal,
        #[field(monetary, currency_field = "currency_id")]
        pub amount2: Decimal,
    }
    #[methods(ProjReads, addon = "test_monetary_mutation")]
    impl ProjReads {
        #[compute(depends = ["trig"], reads = ["selected_currency_id", "input_currency_id", "in1", "in2", "in3", "in4", "in5", "in6", "in7"])]
        fn _currency(batch: &mut Batch<'_, ProjReads>) -> Result<()> {
            for i in batch.indices() {
                let currency = batch
                    .get(i, ProjReads::SELECTED_CURRENCY_ID)?
                    .or(batch.get(i, ProjReads::INPUT_CURRENCY_ID)?);
                batch.set(i, ProjReads::CURRENCY_ID, currency)?;
            }
            Ok(())
        }
    }
}

mod bad_provider {
    use super::*;
    use erp_addon_base::ResCurrency;
    erp_sdk::addon!(name = "test_monetary_bad_provider", depends = ["base"]);
    #[extend(ResCurrency, addon = "test_monetary_bad_provider")]
    pub struct BadProvider {}
    #[methods(ResCurrency, addon = "test_monetary_bad_provider")]
    impl BadProvider {
        #[method(override)]
        async fn _round_monetary(
            _this: &RecordSet<ResCurrency>,
            _tx: &mut Transaction,
            amounts: Vec<(RecId, Decimal)>,
            _sup: Super,
        ) -> Result<Vec<Decimal>> {
            if amounts
                .first()
                .is_some_and(|(_, amount)| *amount < Decimal::ZERO)
            {
                return Err(Error::new(
                    ErrorKind::Database,
                    "controlled monetary provider failure",
                ));
            }
            Ok(Vec::new())
        }
    }
}

mod counted_provider {
    use super::*;
    use erp_addon_base::ResCurrency;
    use std::sync::atomic::{AtomicUsize, Ordering};
    pub static ROWS: AtomicUsize = AtomicUsize::new(0);
    erp_sdk::addon!(name = "test_monetary_counted_provider", depends = ["base"]);
    #[extend(ResCurrency, addon = "test_monetary_counted_provider")]
    pub struct CountedProvider {}
    #[methods(ResCurrency, addon = "test_monetary_counted_provider")]
    impl CountedProvider {
        #[method(override)]
        async fn _round_monetary(
            this: &RecordSet<ResCurrency>,
            tx: &mut Transaction,
            amounts: Vec<(RecId, Decimal)>,
            sup: Super,
        ) -> Result<Vec<Decimal>> {
            ROWS.fetch_add(amounts.len(), Ordering::SeqCst);
            sup.call(tx, this.as_records(), (amounts,)).await
        }
    }
}

mod failing_provider {
    use super::*;
    use erp_addon_base::ResCurrency;
    erp_sdk::addon!(name = "test_monetary_failing_provider", depends = ["base"]);
    #[extend(ResCurrency, addon = "test_monetary_failing_provider")]
    pub struct FailingProvider {}
    #[methods(ResCurrency, addon = "test_monetary_failing_provider")]
    impl FailingProvider {
        /// Fails for the chunk holding the amount 666; every other chunk rounds normally.
        #[method(override)]
        async fn _round_monetary(
            this: &RecordSet<ResCurrency>,
            tx: &mut Transaction,
            amounts: Vec<(RecId, Decimal)>,
            sup: Super,
        ) -> Result<Vec<Decimal>> {
            if amounts.iter().any(|(_, amount)| *amount == dec("666")) {
                return Err(Error::new(
                    ErrorKind::Database,
                    "controlled monetary chunk failure",
                ));
            }
            sup.call(tx, this.as_records(), (amounts,)).await
        }
    }
}

mod empty_provider {
    use super::*;
    use erp_addon_base::ResCurrency;
    erp_sdk::addon!(name = "test_monetary_empty_provider", depends = ["base"]);
    #[extend(ResCurrency, addon = "test_monetary_empty_provider")]
    pub struct EmptyProvider {}
    #[methods(ResCurrency, addon = "test_monetary_empty_provider")]
    impl EmptyProvider {
        #[method(override)]
        async fn _round_monetary(
            _this: &RecordSet<ResCurrency>,
            _tx: &mut Transaction,
            _amounts: Vec<(RecId, Decimal)>,
            _sup: Super,
        ) -> Result<Vec<Decimal>> {
            Err(Error::programming(
                "Empty currency hop unexpectedly invoked rounding",
            ))
        }
    }
}

mod cycle {
    use super::*;
    use fixture::Line;
    erp_sdk::addon!(
        name = "test_monetary_cycle",
        depends = ["test_monetary_mutation"]
    );
    #[extend(Line, addon = "test_monetary_cycle")]
    pub struct CyclicCurrency {
        #[field(
            many2one,
            comodel = "res.currency",
            compute = "_currency_from_amount",
            store = true
        )]
        pub currency_id: Option<RecId>,
    }
    #[methods(Line, addon = "test_monetary_cycle")]
    impl CyclicCurrency {
        #[compute(depends = ["amount"])]
        fn _currency_from_amount(batch: &mut Batch<'_, Line>) -> Result<()> {
            for i in batch.indices() {
                batch.set(i, CyclicCurrency::CURRENCY_ID, None::<RecId>)?;
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

/// The refusal of an async currency compute that is not registered `candidate`.
fn refusal(what: &str, field: &str, when: &str) -> String {
    format!(
        "Cannot {what} async monetary currency {field} {when}: declare its compute `candidate` (#[compute(candidate, ...)], a read-only async compute) to join same-call monetary admission (a later release)"
    )
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

fn line(name: &str, currency: Option<RecId>) -> RawVals {
    RawVals::new()
        .set_cell("name", Cell::Str(name.into()))
        .set_cell(
            "currency_id",
            currency.map_or(Cell::Null, |id| Cell::Id(id.get())),
        )
        .set_cell("amount", Cell::Decimal(dec("2.675")))
}

async fn assert_line_amount(
    tx: &mut Transaction,
    rows: &RecordSet<fixture::Line>,
    expected: Decimal,
) {
    assert_eq!(
        rows.mapped(tx, fixture::Line::AMOUNT).await.unwrap(),
        vec![expected; rows.len()]
    );
    tx.flush().await.unwrap();
    tx.invalidate_all().await.unwrap();
    assert_eq!(
        rows.mapped(tx, fixture::Line::AMOUNT).await.unwrap(),
        vec![expected; rows.len()]
    );
}

#[tokio::test]
async fn scalar_currency_rounding_matrix_and_database_readback() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let cents = currency(&mut tx, "CNT", "0.01").await;
    let small = currency(&mut tx, "SML", "0.0001").await;
    let units = currency(&mut tx, "UNT", "1").await;
    let model = tx.env().model::<fixture::Line>().unwrap();
    for (i, (currency, expected)) in [
        (Some(cents), "14.70"),
        (Some(small), "14.7013"),
        (Some(units), "15.0"),
        (None, "14.70126"),
    ]
    .into_iter()
    .enumerate()
    {
        let amount = dec("14.70126");
        let expected = dec(expected);
        let rows = model
            .create(
                &mut tx,
                (0..2)
                    .map(|n| {
                        Vals::new()
                            .set(fixture::Line::NAME, format!("matrix-{i}-{n}"))
                            .set(fixture::Line::CURRENCY_ID, currency)
                            .set(fixture::Line::AMOUNT, amount)
                    })
                    .collect(),
            )
            .await
            .unwrap();
        assert_line_amount(&mut tx, &rows, expected).await;
        let one = rows.browse([rows.rec_ids()[0].get()]).unwrap();
        one.set(&mut tx, fixture::Line::AMOUNT, Decimal::ZERO)
            .await
            .unwrap();
        assert_line_amount(&mut tx, &one, Decimal::ZERO).await;
        one.set(&mut tx, fixture::Line::AMOUNT, amount)
            .await
            .unwrap();
        assert_line_amount(&mut tx, &one, expected).await;
        one.as_records()
            .write(
                &mut tx,
                RawVals::new()
                    .set_cell("amount", Cell::Decimal(Decimal::ZERO))
                    .set_cell("currency_id", Cell::Null),
            )
            .await
            .unwrap();
        assert_line_amount(&mut tx, &one, Decimal::ZERO).await;
        one.as_records()
            .write(
                &mut tx,
                RawVals::new()
                    .set_cell("amount", Cell::Decimal(amount))
                    .set_cell(
                        "currency_id",
                        currency.map_or(Cell::Null, |id| Cell::Id(id.get())),
                    ),
            )
            .await
            .unwrap();
        assert_line_amount(&mut tx, &one, expected).await;
        one.set(&mut tx, fixture::Line::AMOUNT, amount)
            .await
            .unwrap();
        assert_line_amount(&mut tx, &one, expected).await;
        rows.set(&mut tx, fixture::Line::AMOUNT, amount)
            .await
            .unwrap();
        assert_line_amount(&mut tx, &rows, expected).await;
    }
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn create_and_assignment_use_currency_increment() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let cent = currency(&mut tx, "TST", "0.01").await;
    let model = tx.env().records("test_monetary_mutation.line").unwrap();
    let rows = model
        .create(
            &mut tx,
            vec![line("rounded", Some(cent)), line("raw", None)],
        )
        .await
        .unwrap();
    assert_eq!(
        rows.values(&mut tx, "amount").await.unwrap(),
        vec![Value::Decimal(dec("2.68")), Value::Decimal(dec("2.675"))]
    );
    assert_eq!(
        rows.values(&mut tx, "observed_amount").await.unwrap(),
        vec![Value::Decimal(dec("2.68")), Value::Decimal(dec("2.675"))]
    );
    let typed = tx
        .env()
        .model::<fixture::Line>()
        .unwrap()
        .browse([rows.rec_ids()[0].get()])
        .unwrap();
    typed
        .set(&mut tx, fixture::Line::AMOUNT, dec("-2.675"))
        .await
        .unwrap();
    assert_eq!(
        typed.get(&mut tx, fixture::Line::AMOUNT).await.unwrap(),
        dec("-2.68")
    );
    let first = rows.browse([rows.rec_ids()[0].get()]).unwrap();
    first
        .write(
            &mut tx,
            RawVals::new().set_cell("amount", Cell::Decimal(dec("1.005"))),
        )
        .await
        .unwrap();
    first
        .set_value(&mut tx, "amount", Value::Decimal(dec("-1.005")))
        .await
        .unwrap();
    tx.flush().await.unwrap();
    tx.invalidate_all().await.unwrap();
    assert_eq!(
        first.values(&mut tx, "amount").await.unwrap(),
        vec![Value::Decimal(dec("-1.01"))]
    );
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn one_amount_across_different_currencies_is_refused() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let cent = currency(&mut tx, "TST", "0.01").await;
    let unit = currency(&mut tx, "UNT", "1").await;
    let model = tx.env().records("test_monetary_mutation.line").unwrap();
    let rows = model
        .create(
            &mut tx,
            vec![line("cent", Some(cent)), line("unit", Some(unit))],
        )
        .await
        .unwrap();
    let error = rows
        .write(
            &mut tx,
            RawVals::new().set_cell("amount", Cell::Decimal(dec("1.005"))),
        )
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Validation);
    assert_eq!(
        error.to_string(),
        "Got multiple currencies while assigning values of monetary field test_monetary_mutation.line.amount"
    );
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn computed_currency_and_amount_are_normalized_as_a_group() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let cent = currency(&mut tx, "TST", "0.01").await;
    let row = tx
        .env()
        .model::<fixture::Computed>()
        .unwrap()
        .create(
            &mut tx,
            vec![
                Vals::new()
                    .set(fixture::Computed::SELECTED_CURRENCY_ID, cent)
                    .set(fixture::Computed::RAW, dec("2.675")),
            ],
        )
        .await
        .unwrap();
    assert_eq!(
        row.get(&mut tx, fixture::Computed::AMOUNT).await.unwrap(),
        dec("2.68")
    );
    assert_eq!(
        row.get(&mut tx, fixture::Computed::OBSERVED).await.unwrap(),
        dec("2.68")
    );
    assert_eq!(
        row.get(&mut tx, fixture::Computed::VIRTUAL_AMOUNT)
            .await
            .unwrap(),
        dec("2.68")
    );
    row.set(&mut tx, fixture::Computed::RAW, dec("1.005"))
        .await
        .unwrap();
    tx.flush().await.unwrap();
    tx.invalidate_all().await.unwrap();
    assert_eq!(
        row.get(&mut tx, fixture::Computed::AMOUNT).await.unwrap(),
        dec("1.01")
    );
    assert_eq!(
        row.get(&mut tx, fixture::Computed::OBSERVED).await.unwrap(),
        dec("1.01")
    );
    assert_eq!(
        row.get(&mut tx, fixture::Computed::VIRTUAL_AMOUNT)
            .await
            .unwrap(),
        dec("1.01")
    );
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn default_related_and_delegated_currencies_are_effective_before_amounts() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let cent = currency(&mut tx, "CNT", "0.01").await;
    let unit = currency(&mut tx, "UNT", "1").await;
    let model = tx.env().records("test_monetary_mutation.line").unwrap();
    let defaults = model.with_context([("default_currency_id", Value::Int(cent.get()))]);
    let owner = defaults
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("name", Cell::Str("owner".into()))
                    .set_cell("amount", Cell::Decimal(dec("2.675"))),
            ],
        )
        .await
        .unwrap();
    assert_eq!(
        owner.values(&mut tx, "amount").await.unwrap(),
        vec![Value::Decimal(dec("2.68"))]
    );
    let related = tx
        .env()
        .model::<fixture::Related>()
        .unwrap()
        .create(
            &mut tx,
            vec![
                Vals::new()
                    .set(fixture::Related::OWNER_ID, owner.rec_ids()[0])
                    .set(fixture::Related::AMOUNT, dec("2.675")),
            ],
        )
        .await
        .unwrap();
    assert_eq!(
        related
            .get(&mut tx, fixture::Related::AMOUNT)
            .await
            .unwrap(),
        dec("2.68")
    );
    let unit_owner = model
        .create(&mut tx, vec![line("unit-owner", Some(unit))])
        .await
        .unwrap();
    related
        .as_records()
        .write(
            &mut tx,
            RawVals::new()
                .set_cell("owner_id", Cell::Id(unit_owner.rec_ids()[0].get()))
                .set_cell("amount", Cell::Decimal(dec("2.675"))),
        )
        .await
        .unwrap();
    assert_eq!(
        related
            .get(&mut tx, fixture::Related::AMOUNT)
            .await
            .unwrap(),
        dec("3")
    );
    let alias = tx
        .env()
        .model::<fixture::Alias>()
        .unwrap()
        .create(
            &mut tx,
            vec![
                Vals::new()
                    .set(fixture::Alias::ACTUAL_CURRENCY_ID, cent)
                    .set(fixture::Alias::AMOUNT, dec("2.675")),
            ],
        )
        .await
        .unwrap();
    assert_eq!(
        alias.get(&mut tx, fixture::Alias::AMOUNT).await.unwrap(),
        dec("2.68")
    );
    alias
        .as_records()
        .write(
            &mut tx,
            RawVals::new()
                .set_cell("actual_currency_id", Cell::Id(unit.get()))
                .set_cell("amount", Cell::Decimal(dec("2.675"))),
        )
        .await
        .unwrap();
    assert_eq!(
        alias.get(&mut tx, fixture::Alias::AMOUNT).await.unwrap(),
        dec("3")
    );
    let child = tx
        .env()
        .records("test_monetary_mutation.child")
        .unwrap()
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("name", Cell::Str("child".into()))
                    .set_cell("currency_id", Cell::Id(cent.get()))
                    .set_cell("own_amount", Cell::Decimal(dec("2.675"))),
            ],
        )
        .await
        .unwrap();
    assert_eq!(
        child.values(&mut tx, "own_amount").await.unwrap(),
        vec![Value::Decimal(dec("2.68"))]
    );
    child
        .write(
            &mut tx,
            RawVals::new()
                .set_cell("own_amount", Cell::Decimal(dec("2.675")))
                .set_cell("currency_id", Cell::Id(unit.get())),
        )
        .await
        .unwrap();
    assert_eq!(
        child.values(&mut tx, "own_amount").await.unwrap(),
        vec![Value::Decimal(dec("3"))]
    );
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn monetary_upsert_refuses_before_key_planning_or_sql() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let model = tx.env().records("test_monetary_mutation.line").unwrap();
    // Even an empty call with a missing key must hit the monetary boundary first.
    let error = model
        .upsert(&mut tx, UpsertCall::new("missing_key"))
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Programming);
    assert_eq!(
        error.to_string(),
        "upsert on 'test_monetary_mutation.line': monetary fields are not supported until a later release"
    );
    for call in [
        UpsertCall::new("_name_key").with_rows([line("insert", None)]),
        UpsertCall::new("_name_key")
            .with_rows([line("update", None)])
            .with_update(["amount"]),
        UpsertCall::new("_name_key")
            .with_rows([line("add", None)])
            .with_accumulate("amount"),
    ] {
        let error = model.upsert(&mut tx, call).await.unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Programming);
        assert_eq!(
            error.to_string(),
            "upsert on 'test_monetary_mutation.line': monetary fields are not supported until a later release"
        );
    }
    let typed = tx.env().model::<fixture::Line>().unwrap();
    let error = typed
        .upsert(&mut tx, Upsert::on(fixture::Line::NAME_KEY))
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Programming);
    assert_eq!(
        error.to_string(),
        "upsert on 'test_monetary_mutation.line': monetary fields are not supported until a later release"
    );
    let error = model
        .upsert_values(&mut tx, "_name_key", &[], &[], &[])
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Programming);
    assert_eq!(
        error.to_string(),
        "upsert on 'test_monetary_mutation.line': monetary fields are not supported until a later release"
    );
    assert!(
        model
            .search(&mut tx, &Domain::TRUE)
            .await
            .unwrap()
            .is_empty()
    );
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn builtin_direct_write_targets_have_no_monetary_fields() {
    let h = Harness::builder([
        &base_users::ADDON,
        &erp_addon_base::ADDON,
        &fixture::ADDON,
        &erp_data::model_data::ADDON,
    ])
    .with_decls([erp_data::model_data::ir_model_data_decl()])
    .build()
    .await
    .unwrap();
    let registry = h.registry();
    // Validate composed direct-write targets, including data-loader/import bindings.
    for name in [
        "ir.model.data",
        "ir.default",
        "ir.sequence",
        "ir.sequence.date_range",
    ] {
        let spec = registry.model(name).unwrap();
        assert!(
            spec.fields
                .iter()
                .all(|field| !matches!(field.ty, erp_sdk::FieldType::Monetary { .. })),
            "{name} now needs monetary direct-write integration"
        );
    }
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn provider_cardinality_and_error_kind_propagate_before_insert() {
    let h = Harness::builder([
        &base_users::ADDON,
        &erp_addon_base::ADDON,
        &fixture::ADDON,
        &bad_provider::ADDON,
    ])
    .build()
    .await
    .unwrap();
    let mut tx = h.begin().await.unwrap();
    let cent = currency(&mut tx, "CNT", "0.01").await;
    let model = tx.env().records("test_monetary_mutation.line").unwrap();
    let error = model
        .create(&mut tx, vec![line("bad", Some(cent))])
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Programming);
    assert_eq!(
        error.message(),
        "res.currency._round_monetary returned 0 amounts, expected 1"
    );
    assert!(
        model
            .search(&mut tx, &Domain::TRUE)
            .await
            .unwrap()
            .is_empty()
    );
    let error = model
        .create(
            &mut tx,
            vec![line("failure", Some(cent)).set_cell("amount", Cell::Decimal(dec("-2.675")))],
        )
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Database);
    assert_eq!(error.message(), "controlled monetary provider failure");
    assert!(
        model
            .search(&mut tx, &Domain::TRUE)
            .await
            .unwrap()
            .is_empty()
    );
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn in_memory_assignment_normalizes_and_new_currencies_fail_explicitly() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let cent = currency(&mut tx, "CNT", "0.01").await;
    let model = tx.env().records("test_monetary_mutation.line").unwrap();
    let row = tx
        .new_record(&model, line("new", Some(cent)), None, None)
        .await
        .unwrap();
    assert_eq!(
        row.values(&mut tx, "amount").await.unwrap(),
        vec![Value::Decimal(dec("2.68"))]
    );
    row.write(
        &mut tx,
        RawVals::new().set_cell("amount", Cell::Decimal(dec("1.005"))),
    )
    .await
    .unwrap();
    assert_eq!(
        row.values(&mut tx, "amount").await.unwrap(),
        vec![Value::Decimal(dec("1.01"))]
    );
    let currency_model = tx.env().records("res.currency").unwrap();
    let new_currency = tx
        .new_record(&currency_model, RawVals::new(), None, None)
        .await
        .unwrap();
    let error = tx
        .new_record(
            &model,
            line("invalid", Some(new_currency.rec_ids()[0])),
            None,
            None,
        )
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Programming);
    assert_eq!(
        error.message(),
        "Cannot normalize test_monetary_mutation.line.amount using an in-memory currency until a later release"
    );
    assert_eq!(
        row.values(&mut tx, "amount").await.unwrap(),
        vec![Value::Decimal(dec("1.01"))]
    );
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn currency_edits_and_savepoint_rollback_have_no_stale_rounding() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let cent = currency(&mut tx, "CNT", "0.01").await;
    let row = tx
        .env()
        .records("test_monetary_mutation.line")
        .unwrap()
        .create(&mut tx, vec![line("row", Some(cent))])
        .await
        .unwrap();
    let currency = tx.env().records("res.currency").unwrap().browse_ids([cent]);
    let error: Error = tx
        .savepoint(async |tx| {
            currency
                .write(
                    tx,
                    RawVals::new().set_cell("rounding", Cell::Decimal(dec("1"))),
                )
                .await?;
            row.write(
                tx,
                RawVals::new().set_cell("amount", Cell::Decimal(dec("2.675"))),
            )
            .await?;
            assert_eq!(
                row.values(tx, "amount").await?,
                vec![Value::Decimal(dec("3"))]
            );
            Err::<(), _>(Error::new(ErrorKind::Database, "rollback monetary edit"))
        })
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Database);
    assert_eq!(error.message(), "rollback monetary edit");
    row.write(
        &mut tx,
        RawVals::new().set_cell("amount", Cell::Decimal(dec("2.675"))),
    )
    .await
    .unwrap();
    assert_eq!(
        row.values(&mut tx, "amount").await.unwrap(),
        vec![Value::Decimal(dec("2.68"))]
    );
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn currency_lookup_sudo_allows_denied_currency_reads() {
    let policy = erp_testkit::DomainPolicy::builder(erp_testkit::Otherwise::Allow)
        .rule("res.currency", erp_sdk::Operation::Read, Domain::FALSE)
        .build()
        .unwrap();
    let h = Harness::builder([&base_users::ADDON, &erp_addon_base::ADDON, &fixture::ADDON])
        .policy(policy)
        .build()
        .await
        .unwrap();
    let mut tx = h.begin().await.unwrap();
    tx.batch("INSERT INTO res_users(id,name,login,active,share) VALUES (42,'Monetary reader','monetary-reader',true,false)").await.unwrap();
    let cent = currency(&mut tx, "CNT", "0.01").await;
    let model = tx.env().records("test_monetary_mutation.line").unwrap();
    let row = model
        .create(&mut tx, vec![line("warm", Some(cent))])
        .await
        .unwrap();
    // Test both cold and privileged-warm rounding inputs after changing principal.
    for warm in [false, true] {
        tx.invalidate_all().await.unwrap();
        if warm {
            tx.env()
                .records("res.currency")
                .unwrap()
                .browse_ids([cent])
                .read(&mut tx, &["name", "rounding", "decimal_places"])
                .await
                .unwrap();
        }
        row.with_user(42)
            .write(
                &mut tx,
                RawVals::new().set_cell("amount", Cell::Decimal(dec("1.005"))),
            )
            .await
            .unwrap();
        assert_eq!(
            row.values(&mut tx, "amount").await.unwrap(),
            vec![Value::Decimal(dec("1.01"))]
        );
    }
    let created = model
        .with_user(42)
        .create(&mut tx, vec![line("allowed-create", Some(cent))])
        .await
        .unwrap();
    assert_eq!(
        created.values(&mut tx, "amount").await.unwrap(),
        vec![Value::Decimal(dec("2.68"))]
    );
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn mixed_assignment_is_global_and_same_call_currency_replaces_it() {
    let config = erp_orm::OrmConfig {
        recompute_batch: 1,
        ..Default::default()
    };
    let h = Harness::builder([&base_users::ADDON, &erp_addon_base::ADDON, &fixture::ADDON])
        .config(config)
        .build()
        .await
        .unwrap();
    let mut tx = h.begin().await.unwrap();
    let cent = currency(&mut tx, "CNT", "0.01").await;
    let unit = currency(&mut tx, "UNT", "1").await;
    let model = tx.env().records("test_monetary_mutation.line").unwrap();
    let rows = model
        .create(
            &mut tx,
            vec![
                line("cent", Some(cent)),
                line("unit", Some(unit)),
                line("none", None),
            ],
        )
        .await
        .unwrap();
    let pair = rows.browse_ids([rows.rec_ids()[0], rows.rec_ids()[1]]);
    let error = tx
        .savepoint(async |tx| {
            pair.write(
                tx,
                RawVals::new().set_cell("amount", Cell::Decimal(dec("1.005"))),
            )
            .await
        })
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Validation);
    assert_eq!(
        error.message(),
        "Got multiple currencies while assigning values of monetary field test_monetary_mutation.line.amount"
    );
    let pair = rows.browse_ids([rows.rec_ids()[0], rows.rec_ids()[2]]);
    pair.write(
        &mut tx,
        RawVals::new().set_cell("amount", Cell::Decimal(dec("1.005"))),
    )
    .await
    .unwrap();
    tx.flush().await.unwrap();
    tx.invalidate_all().await.unwrap();
    assert_eq!(
        pair.values(&mut tx, "amount").await.unwrap(),
        vec![Value::Decimal(dec("1.01")); 2]
    );
    let raw = model
        .create(&mut tx, vec![line("raw-one", None), line("raw-two", None)])
        .await
        .unwrap();
    raw.write(
        &mut tx,
        RawVals::new().set_cell("amount", Cell::Decimal(dec("1.005"))),
    )
    .await
    .unwrap();
    tx.flush().await.unwrap();
    tx.invalidate_all().await.unwrap();
    assert_eq!(
        raw.values(&mut tx, "amount").await.unwrap(),
        vec![Value::Decimal(dec("1.005")); 2]
    );
    rows.write(
        &mut tx,
        RawVals::new()
            .set_cell("amount", Cell::Decimal(dec("1.005")))
            .set_cell("currency_id", Cell::Id(cent.get())),
    )
    .await
    .unwrap();
    assert_eq!(
        rows.values(&mut tx, "amount").await.unwrap(),
        vec![Value::Decimal(dec("1.01")); 3]
    );
    rows.write(
        &mut tx,
        RawVals::new().set_cell("currency_id", Cell::Id(unit.get())),
    )
    .await
    .unwrap();
    assert_eq!(
        rows.values(&mut tx, "amount").await.unwrap(),
        vec![Value::Decimal(dec("1.01")); 3]
    );
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn create_partitions_more_than_256_distinct_currencies_positionally() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let currencies = tx
        .env()
        .records("res.currency")
        .unwrap()
        .create(
            &mut tx,
            (0..257)
                .map(|i| {
                    let code =
                        String::from_utf8(vec![b'A' + (i / 26) as u8, b'A' + (i % 26) as u8, b'X'])
                            .unwrap();
                    RawVals::new()
                        .set_cell("name", Cell::Str(code.clone().into()))
                        .set_cell("symbol", Cell::Str(code.into()))
                        .set_cell(
                            "rounding",
                            Cell::Decimal(dec(if i % 2 == 0 { "0.01" } else { "1" })),
                        )
                })
                .collect(),
        )
        .await
        .unwrap();
    let model = tx.env().records("test_monetary_mutation.line").unwrap();
    let rows = model
        .create(
            &mut tx,
            currencies
                .rec_ids()
                .iter()
                .enumerate()
                .map(|(i, id)| line(&format!("row-{i}"), Some(*id)))
                .collect(),
        )
        .await
        .unwrap();
    let expected: Vec<Value> = (0..257)
        .map(|i| Value::Decimal(dec(if i % 2 == 0 { "2.68" } else { "3" })))
        .collect();
    assert_eq!(rows.values(&mut tx, "amount").await.unwrap(), expected);
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn inverse_and_lazy_new_defaults_observe_normalized_amounts() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let cent = currency(&mut tx, "CNT", "0.01").await;
    let model = tx.env().model::<fixture::Inverse>().unwrap();
    let row = model
        .create(
            &mut tx,
            vec![
                Vals::new()
                    .set(fixture::Inverse::CURRENCY_ID, cent)
                    .set(fixture::Inverse::AMOUNT, dec("2.675")),
            ],
        )
        .await
        .unwrap();
    assert_eq!(
        row.get(&mut tx, fixture::Inverse::OBSERVED).await.unwrap(),
        dec("2.68")
    );
    row.set(&mut tx, fixture::Inverse::AMOUNT, dec("1.005"))
        .await
        .unwrap();
    assert_eq!(
        row.get(&mut tx, fixture::Inverse::OBSERVED).await.unwrap(),
        dec("1.01")
    );
    let new_model = tx
        .env()
        .records("test_monetary_mutation.line")
        .unwrap()
        .with_context([
            ("default_amount", Value::Decimal(dec("2.675"))),
            ("default_currency_id", Value::Int(cent.get())),
        ]);
    let new = tx
        .new_record(&new_model, RawVals::new(), None, None)
        .await
        .unwrap();
    assert_eq!(
        new.values(&mut tx, "amount").await.unwrap(),
        vec![Value::Decimal(dec("2.68"))]
    );
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn currency_depending_on_its_unrounded_amount_is_a_clear_cycle_error() {
    let h = Harness::builder([
        &base_users::ADDON,
        &erp_addon_base::ADDON,
        &fixture::ADDON,
        &cycle::ADDON,
    ])
    .build()
    .await
    .unwrap();
    let mut tx = h.begin().await.unwrap();
    let model = tx.env().records("test_monetary_mutation.line").unwrap();
    let error = model
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("name", Cell::Str("cycle".into()))
                    .set_cell("amount", Cell::Decimal(dec("2.675"))),
            ],
        )
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Programming);
    assert_eq!(
        error.message(),
        "Monetary currency resolution cycle: test_monetary_mutation.line.currency_id requires amount before normalization"
    );
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn separate_orm_instances_do_not_keep_rounding_after_commit() {
    let h = harness().await;
    let mut setup = h.begin().await.unwrap();
    let cent = currency(&mut setup, "CNT", "0.01").await;
    let row = setup
        .env()
        .records("test_monetary_mutation.line")
        .unwrap()
        .create(&mut setup, vec![line("shared", Some(cent))])
        .await
        .unwrap()
        .rec_ids()[0];
    setup.commit().await.unwrap();
    let pool = erp_orm::pool::create_pool(h.db().config().clone(), 2).unwrap();
    let other = erp_orm::Orm::builder(Arc::clone(h.registry()), pool)
        .policy(erp_orm::StaticPolicy::AllowAll)
        .build()
        .unwrap();
    let mut first = other.begin(erp_orm::SUPERUSER_ID).await.unwrap();
    let records = first
        .env()
        .records("test_monetary_mutation.line")
        .unwrap()
        .browse_ids([row]);
    records
        .write(
            &mut first,
            RawVals::new().set_cell("amount", Cell::Decimal(dec("1.005"))),
        )
        .await
        .unwrap();
    assert_eq!(
        records.values(&mut first, "amount").await.unwrap(),
        vec![Value::Decimal(dec("1.01"))]
    );
    first.commit().await.unwrap();
    let mut edit = h.begin().await.unwrap();
    edit.env()
        .records("res.currency")
        .unwrap()
        .browse_ids([cent])
        .write(
            &mut edit,
            RawVals::new().set_cell("rounding", Cell::Decimal(dec("1"))),
        )
        .await
        .unwrap();
    edit.commit().await.unwrap();
    let mut fresh = other.begin(erp_orm::SUPERUSER_ID).await.unwrap();
    let records = fresh
        .env()
        .records("test_monetary_mutation.line")
        .unwrap()
        .browse_ids([row]);
    records
        .write(
            &mut fresh,
            RawVals::new().set_cell("amount", Cell::Decimal(dec("1.005"))),
        )
        .await
        .unwrap();
    assert_eq!(
        records.values(&mut fresh, "amount").await.unwrap(),
        vec![Value::Decimal(dec("1"))]
    );
    fresh.rollback().await.unwrap();
    drop(other);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn related_currency_preserves_declared_compute_authorization() {
    let policy = erp_testkit::DomainPolicy::builder(erp_testkit::Otherwise::Allow)
        .rule(
            "test_monetary_mutation.line",
            erp_sdk::Operation::Read,
            Domain::FALSE,
        )
        .build()
        .unwrap();
    let h = Harness::builder([&base_users::ADDON, &erp_addon_base::ADDON, &fixture::ADDON])
        .policy(policy)
        .build()
        .await
        .unwrap();
    let mut tx = h.begin().await.unwrap();
    tx.batch("INSERT INTO res_users(id,name,login,active,share) VALUES (42,'Related reader','related-reader',true,false)").await.unwrap();
    let cent = currency(&mut tx, "CNT", "0.01").await;
    let owner = tx
        .env()
        .records("test_monetary_mutation.line")
        .unwrap()
        .create(&mut tx, vec![line("owner", Some(cent))])
        .await
        .unwrap();
    let row = tx
        .env()
        .records("test_monetary_mutation.related")
        .unwrap()
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("owner_id", Cell::Id(owner.rec_ids()[0].get()))
                    .set_cell("amount", Cell::Decimal(dec("2.675"))),
            ],
        )
        .await
        .unwrap()
        .with_user(42);
    assert_eq!(
        row.values(&mut tx, "currency_id").await.unwrap(),
        vec![Value::Int(cent.get())]
    );
    row.write(
        &mut tx,
        RawVals::new().set_cell("amount", Cell::Decimal(dec("1.005"))),
    )
    .await
    .unwrap();
    assert_eq!(
        row.values(&mut tx, "amount").await.unwrap(),
        vec![Value::Decimal(dec("1.01"))]
    );
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn related_currency_keeps_the_readers_company_bound() {
    let h = harness().await;
    let mut setup = h.begin().await.unwrap();
    setup.batch("INSERT INTO res_users(id,name,login,active,share) VALUES (42,'Company reader','company-reader',true,false)").await.unwrap();
    let cent = currency(&mut setup, "CNT", "0.01").await;
    let companies = setup
        .env()
        .records("res.company")
        .unwrap()
        .create(
            &mut setup,
            ["Own", "Foreign"]
                .into_iter()
                .map(|name| {
                    RawVals::new()
                        .set_cell("name", Cell::Str(name.into()))
                        .set_cell("currency_id", Cell::Id(cent.get()))
                })
                .collect(),
        )
        .await
        .unwrap();
    let owner = setup
        .env()
        .records("test_monetary_mutation.scoped_owner")
        .unwrap()
        .create(
            &mut setup,
            vec![
                RawVals::new()
                    .set_cell("company_id", Cell::Id(companies.rec_ids()[1].get()))
                    .set_cell("currency_id", Cell::Id(cent.get())),
            ],
        )
        .await
        .unwrap()
        .rec_ids()[0];
    let row = setup
        .env()
        .records("test_monetary_mutation.scoped_line")
        .unwrap()
        .create(
            &mut setup,
            vec![
                RawVals::new()
                    .set_cell("owner_id", Cell::Id(owner.get()))
                    .set_cell("amount", Cell::Decimal(dec("2.675"))),
            ],
        )
        .await
        .unwrap()
        .rec_ids()[0];
    setup.commit().await.unwrap();
    let company = erp_orm::CompanyId::new(companies.rec_ids()[0].get()).unwrap();
    let resolution = erp_orm::Resolution::in_id_order(
        erp_orm::GroupSet::default(),
        [company],
        std::num::NonZeroU64::MIN,
    )
    .unwrap();
    let principal = erp_orm::PrincipalBuilder::user(erp_orm::UserId::new(42).unwrap())
        .resolution(resolution)
        .companies(vec![company])
        .build()
        .unwrap();
    let mut tx = h
        .orm()
        .begin_opts(
            erp_orm::BeginOptions::for_principal(principal).context(erp_orm::Context::new()),
        )
        .await
        .unwrap();
    let records = tx
        .env()
        .records("test_monetary_mutation.scoped_line")
        .unwrap()
        .browse_ids([row]);
    let baseline = records.values(&mut tx, "currency_id").await.unwrap_err();
    assert_eq!(baseline.kind(), ErrorKind::Access);
    let error = records
        .write(
            &mut tx,
            RawVals::new().set_cell("amount", Cell::Decimal(dec("1.005"))),
        )
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Access);
    assert_eq!(error.message(), baseline.message());
    assert_eq!(
        records.values(&mut tx, "amount").await.unwrap(),
        vec![Value::Decimal(dec("2.68"))]
    );
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn same_call_computed_currency_inputs_are_projected_before_rounding() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let cent = currency(&mut tx, "CNT", "0.01").await;
    let unit = currency(&mut tx, "UNT", "1").await;
    let row = tx
        .env()
        .model::<fixture::Derived>()
        .unwrap()
        .create(
            &mut tx,
            vec![
                Vals::new()
                    .set(fixture::Derived::SELECTED_CURRENCY_ID, cent)
                    .set(fixture::Derived::AMOUNT, dec("2.675")),
            ],
        )
        .await
        .unwrap();
    assert_eq!(
        row.get(&mut tx, fixture::Derived::AMOUNT).await.unwrap(),
        dec("2.68")
    );
    row.as_records()
        .write(
            &mut tx,
            RawVals::new()
                .set_cell("selected_currency_id", Cell::Id(unit.get()))
                .set_cell("amount", Cell::Decimal(dec("2.675"))),
        )
        .await
        .unwrap();
    assert_eq!(
        row.get(&mut tx, fixture::Derived::AMOUNT).await.unwrap(),
        dec("3")
    );
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn non_candidate_async_currency_refuses_and_names_the_candidate_flag() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let cent = currency(&mut tx, "CNT", "0.01").await;
    let unit = currency(&mut tx, "UNT", "1").await;
    let model = tx.env().model::<fixture::AsyncPlainCurrency>().unwrap();
    let error = model
        .create(
            &mut tx,
            vec![
                Vals::new()
                    .set(fixture::AsyncPlainCurrency::SELECTED_CURRENCY_ID, cent)
                    .set(fixture::AsyncPlainCurrency::AMOUNT, dec("2.675")),
            ],
        )
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Programming);
    assert_eq!(
        error.message(),
        &refusal(
            "resolve",
            "test_monetary_mutation.async_plain_currency.currency_id",
            "before create"
        )
    );
    assert!(
        model
            .as_records()
            .search(&mut tx, &Domain::TRUE)
            .await
            .unwrap()
            .is_empty()
    );
    let row = tx
        .env()
        .model::<fixture::AsyncPlainCurrency>()
        .unwrap()
        .create(
            &mut tx,
            vec![Vals::new().set(fixture::AsyncPlainCurrency::SELECTED_CURRENCY_ID, cent)],
        )
        .await
        .unwrap();
    row.set(&mut tx, fixture::AsyncPlainCurrency::AMOUNT, dec("2.675"))
        .await
        .unwrap();
    assert_eq!(
        row.get(&mut tx, fixture::AsyncPlainCurrency::AMOUNT)
            .await
            .unwrap(),
        dec("2.68")
    );
    tx.flush().await.unwrap();
    // No outbox model is installed in this harness; there is no outbox producer here.
    assert!(h.registry().model("ir.event.outbox").is_none());
    let error = row
        .as_records()
        .write(
            &mut tx,
            RawVals::new()
                .set_cell("selected_currency_id", Cell::Id(unit.get()))
                .set_cell("amount", Cell::Decimal(dec("2.675"))),
        )
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Programming);
    assert_eq!(
        error.message(),
        &refusal(
            "project same-call",
            "test_monetary_mutation.async_plain_currency.currency_id",
            "from changed inputs"
        )
    );
    assert_eq!(
        row.get(&mut tx, fixture::AsyncPlainCurrency::AMOUNT)
            .await
            .unwrap(),
        dec("2.68")
    );
    assert_eq!(
        row.get(&mut tx, fixture::AsyncPlainCurrency::SELECTED_CURRENCY_ID)
            .await
            .unwrap(),
        Some(cent)
    );
    tx.flush().await.unwrap();
    tx.invalidate_all().await.unwrap();
    assert_eq!(
        row.get(&mut tx, fixture::AsyncPlainCurrency::AMOUNT)
            .await
            .unwrap(),
        dec("2.68")
    );
    assert_eq!(
        row.get(&mut tx, fixture::AsyncPlainCurrency::SELECTED_CURRENCY_ID)
            .await
            .unwrap(),
        Some(cent)
    );
    assert_eq!(
        row.get(&mut tx, fixture::AsyncPlainCurrency::CURRENCY_ID)
            .await
            .unwrap(),
        Some(cent)
    );
    // Assigning the current input again is not a change and remains supported.
    row.as_records()
        .write(
            &mut tx,
            RawVals::new()
                .set_cell("selected_currency_id", Cell::Id(cent.get()))
                .set_cell("amount", Cell::Decimal(dec("1.005"))),
        )
        .await
        .unwrap();
    assert_eq!(
        row.get(&mut tx, fixture::AsyncPlainCurrency::AMOUNT)
            .await
            .unwrap(),
        dec("1.01")
    );
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn async_currency_same_call_create_rounds_and_persists() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let cent = currency(&mut tx, "ACR", "0.01").await;
    let row = tx
        .env()
        .model::<fixture::AsyncCurrency>()
        .unwrap()
        .create(
            &mut tx,
            vec![
                Vals::new()
                    .set(fixture::AsyncCurrency::SELECTED_CURRENCY_ID, cent)
                    .set(fixture::AsyncCurrency::AMOUNT, dec("2.675")),
            ],
        )
        .await
        .unwrap();
    assert_eq!(
        row.get(&mut tx, fixture::AsyncCurrency::AMOUNT)
            .await
            .unwrap(),
        dec("2.68")
    );
    assert_eq!(
        row.get(&mut tx, fixture::AsyncCurrency::CURRENCY_ID)
            .await
            .unwrap(),
        Some(cent)
    );
    tx.flush().await.unwrap();
    tx.invalidate_all().await.unwrap();
    assert_eq!(
        row.get(&mut tx, fixture::AsyncCurrency::AMOUNT)
            .await
            .unwrap(),
        dec("2.68")
    );
    assert_eq!(
        row.get(&mut tx, fixture::AsyncCurrency::CURRENCY_ID)
            .await
            .unwrap(),
        Some(cent)
    );
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn async_currency_same_call_write_rounds_and_persists() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let cent = currency(&mut tx, "AWC", "0.01").await;
    let unit = currency(&mut tx, "AWU", "1").await;
    let row = tx
        .env()
        .model::<fixture::AsyncCurrency>()
        .unwrap()
        .create(
            &mut tx,
            vec![Vals::new().set(fixture::AsyncCurrency::SELECTED_CURRENCY_ID, cent)],
        )
        .await
        .unwrap();
    row.set(&mut tx, fixture::AsyncCurrency::AMOUNT, dec("2.675"))
        .await
        .unwrap();
    tx.flush().await.unwrap();
    row.as_records()
        .write(
            &mut tx,
            RawVals::new()
                .set_cell("selected_currency_id", Cell::Id(unit.get()))
                .set_cell("amount", Cell::Decimal(dec("2.675"))),
        )
        .await
        .unwrap();
    assert_eq!(
        row.get(&mut tx, fixture::AsyncCurrency::AMOUNT)
            .await
            .unwrap(),
        dec("3")
    );
    assert_eq!(
        row.get(&mut tx, fixture::AsyncCurrency::CURRENCY_ID)
            .await
            .unwrap(),
        Some(unit)
    );
    tx.flush().await.unwrap();
    tx.invalidate_all().await.unwrap();
    assert_eq!(
        row.get(&mut tx, fixture::AsyncCurrency::AMOUNT)
            .await
            .unwrap(),
        dec("3")
    );
    assert_eq!(
        row.get(&mut tx, fixture::AsyncCurrency::CURRENCY_ID)
            .await
            .unwrap(),
        Some(unit)
    );
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn monetary_rounding_overflow_refuses_before_insert() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let units = currency(&mut tx, "TWO", "2").await;
    let model = tx.env().records("test_monetary_mutation.line").unwrap();
    let error = model
        .create(
            &mut tx,
            vec![line("overflow", Some(units)).set_cell("amount", Cell::Decimal(Decimal::MAX))],
        )
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Validation);
    assert_eq!(
        error.message(),
        "The monetary value is out of the supported range (quantize)."
    );
    assert!(
        model
            .search(&mut tx, &Domain::TRUE)
            .await
            .unwrap()
            .is_empty()
    );
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn currency_reads_without_triggers_do_not_change_rounding() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let cent = currency(&mut tx, "CNT", "0.01").await;
    let unit = currency(&mut tx, "UNT", "1").await;
    let row = tx
        .env()
        .model::<fixture::Derived>()
        .unwrap()
        .create(
            &mut tx,
            vec![
                Vals::new()
                    .set(fixture::Derived::SELECTED_CURRENCY_ID, cent)
                    .set(fixture::Derived::AMOUNT, dec("2.675")),
            ],
        )
        .await
        .unwrap();
    row.as_records()
        .write(
            &mut tx,
            RawVals::new()
                .set_cell("read_currency_id", Cell::Id(unit.get()))
                .set_cell("amount", Cell::Decimal(dec("2.675"))),
        )
        .await
        .unwrap();
    assert_eq!(
        row.get(&mut tx, fixture::Derived::CURRENCY_ID)
            .await
            .unwrap(),
        Some(cent)
    );
    assert_eq!(
        row.get(&mut tx, fixture::Derived::AMOUNT).await.unwrap(),
        dec("2.68")
    );
    tx.flush().await.unwrap();
    tx.invalidate_all().await.unwrap();
    assert_eq!(
        row.get(&mut tx, fixture::Derived::CURRENCY_ID)
            .await
            .unwrap(),
        Some(cent)
    );
    assert_eq!(
        row.get(&mut tx, fixture::Derived::AMOUNT).await.unwrap(),
        dec("2.68")
    );
    // A real dependency change projects both dependency and read-only candidate inputs.
    row.as_records()
        .write(
            &mut tx,
            RawVals::new()
                .set_cell("selected_currency_id", Cell::Id(unit.get()))
                .set_cell("read_currency_id", Cell::Id(cent.get()))
                .set_cell("amount", Cell::Decimal(dec("1.005"))),
        )
        .await
        .unwrap();
    tx.flush().await.unwrap();
    tx.invalidate_all().await.unwrap();
    assert_eq!(
        row.get(&mut tx, fixture::Derived::CURRENCY_ID)
            .await
            .unwrap(),
        Some(cent)
    );
    assert_eq!(
        row.get(&mut tx, fixture::Derived::AMOUNT).await.unwrap(),
        dec("1.01")
    );
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn currency_projection_compares_normalized_dependency_values() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let cent = currency(&mut tx, "CNT", "0.01").await;
    let unit = currency(&mut tx, "UNT", "1").await;
    let row = tx
        .env()
        .model::<fixture::MoneyDependency>()
        .unwrap()
        .create(
            &mut tx,
            vec![
                Vals::new()
                    .set(fixture::MoneyDependency::INPUT_CURRENCY_ID, cent)
                    .set(fixture::MoneyDependency::INPUT_AMOUNT, dec("1.01"))
                    .set(fixture::MoneyDependency::AMOUNT, dec("2.675")),
            ],
        )
        .await
        .unwrap();
    row.as_records()
        .write(
            &mut tx,
            RawVals::new()
                .set_cell("input_amount", Cell::Decimal(dec("1.005")))
                .set_cell("selected_currency_id", Cell::Id(unit.get()))
                .set_cell("amount", Cell::Decimal(dec("2.675"))),
        )
        .await
        .unwrap();
    tx.flush().await.unwrap();
    tx.invalidate_all().await.unwrap();
    assert_eq!(
        row.get(&mut tx, fixture::MoneyDependency::INPUT_AMOUNT)
            .await
            .unwrap(),
        dec("1.01")
    );
    assert_eq!(
        row.get(&mut tx, fixture::MoneyDependency::CURRENCY_ID)
            .await
            .unwrap(),
        Some(cent)
    );
    assert_eq!(
        row.get(&mut tx, fixture::MoneyDependency::AMOUNT)
            .await
            .unwrap(),
        dec("2.68")
    );
    row.as_records()
        .write(
            &mut tx,
            RawVals::new()
                .set_cell("input_amount", Cell::Decimal(dec("1.015")))
                .set_cell("amount", Cell::Decimal(dec("2.675"))),
        )
        .await
        .unwrap();
    tx.flush().await.unwrap();
    tx.invalidate_all().await.unwrap();
    assert_eq!(
        row.get(&mut tx, fixture::MoneyDependency::INPUT_AMOUNT)
            .await
            .unwrap(),
        dec("1.02")
    );
    assert_eq!(
        row.get(&mut tx, fixture::MoneyDependency::CURRENCY_ID)
            .await
            .unwrap(),
        Some(unit)
    );
    assert_eq!(
        row.get(&mut tx, fixture::MoneyDependency::AMOUNT)
            .await
            .unwrap(),
        dec("3")
    );
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn currency_projection_only_runs_for_rows_with_changed_dependencies() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let cent = currency(&mut tx, "CNT", "0.01").await;
    let unit = currency(&mut tx, "UNT", "1").await;
    let rows = tx
        .env()
        .model::<fixture::Derived>()
        .unwrap()
        .create(
            &mut tx,
            [cent, unit]
                .into_iter()
                .map(|cur| {
                    Vals::new()
                        .set(fixture::Derived::SELECTED_CURRENCY_ID, cur)
                        .set(fixture::Derived::AMOUNT, dec("2.675"))
                })
                .collect(),
        )
        .await
        .unwrap();
    let error = tx
        .savepoint(async |tx| {
            rows.as_records()
                .write(
                    tx,
                    RawVals::new()
                        .set_cell("selected_currency_id", Cell::Id(unit.get()))
                        .set_cell("read_currency_id", Cell::Id(cent.get()))
                        .set_cell("amount", Cell::Decimal(dec("2.675"))),
                )
                .await
        })
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Validation);
    assert_eq!(
        error.message(),
        "Got multiple currencies while assigning values of monetary field test_monetary_mutation.derived.amount"
    );
    assert_eq!(
        rows.mapped(&mut tx, fixture::Derived::SELECTED_CURRENCY_ID)
            .await
            .unwrap(),
        vec![Some(cent), Some(unit)]
    );
    assert_eq!(
        rows.mapped(&mut tx, fixture::Derived::AMOUNT)
            .await
            .unwrap(),
        vec![dec("2.68"), dec("3")]
    );
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn new_record_currency_compute_sees_initial_non_triggering_inputs() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let cent = currency(&mut tx, "CNT", "0.01").await;
    let unit = currency(&mut tx, "UNT", "1").await;
    let row = tx
        .env()
        .model::<fixture::Derived>()
        .unwrap()
        .new_record(
            &mut tx,
            Vals::new()
                .set(fixture::Derived::READ_CURRENCY_ID, unit)
                .set(fixture::Derived::AMOUNT, dec("2.675")),
        )
        .await
        .unwrap();
    assert_eq!(
        row.get(&mut tx, fixture::Derived::AMOUNT).await.unwrap(),
        dec("3")
    );
    assert_eq!(
        row.get(&mut tx, fixture::Derived::CURRENCY_ID)
            .await
            .unwrap(),
        Some(unit)
    );
    row.as_records()
        .write(
            &mut tx,
            RawVals::new()
                .set_cell("read_currency_id", Cell::Id(cent.get()))
                .set_cell("amount", Cell::Decimal(dec("2.675"))),
        )
        .await
        .unwrap();
    assert_eq!(
        row.get(&mut tx, fixture::Derived::AMOUNT).await.unwrap(),
        dec("3")
    );
    assert_eq!(
        row.get(&mut tx, fixture::Derived::CURRENCY_ID)
            .await
            .unwrap(),
        Some(unit)
    );
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn mixed_currency_zero_and_null_writes_clear_the_whole_set() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let cent = currency(&mut tx, "CNT", "0.01").await;
    let unit = currency(&mut tx, "UNT", "1").await;
    let rows = tx
        .env()
        .records("test_monetary_mutation.line")
        .unwrap()
        .create(
            &mut tx,
            vec![line("cent", Some(cent)), line("unit", Some(unit))],
        )
        .await
        .unwrap();
    for cell in [Cell::Decimal(Decimal::ZERO), Cell::Null] {
        let expected = if matches!(cell, Cell::Null) {
            Value::Float(0.0)
        } else {
            Value::Decimal(Decimal::ZERO)
        };
        rows.write(&mut tx, RawVals::new().set_cell("amount", cell))
            .await
            .unwrap();
        tx.flush().await.unwrap();
        tx.invalidate_all().await.unwrap();
        assert_eq!(
            rows.values(&mut tx, "amount").await.unwrap(),
            vec![expected; 2]
        );
    }
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn hidden_currency_lookup_does_not_elevate_amount_writes() {
    let policy = erp_testkit::FieldPolicy::new()
        .deny(
            "test_monetary_mutation.line",
            "currency_id",
            erp_orm::FieldOp::Read,
        )
        .deny(
            "test_monetary_mutation.line",
            "amount",
            erp_orm::FieldOp::Write,
        );
    let h = Harness::builder([&base_users::ADDON, &erp_addon_base::ADDON, &fixture::ADDON])
        .policy(policy)
        .build()
        .await
        .unwrap();
    let mut tx = h.begin().await.unwrap();
    tx.batch("INSERT INTO res_users(id,name,login,active,share) VALUES (42,'Amount reader','amount-reader',true,false)").await.unwrap();
    let cent = currency(&mut tx, "CNT", "0.01").await;
    let row = tx
        .env()
        .records("test_monetary_mutation.line")
        .unwrap()
        .create(&mut tx, vec![line("restricted", Some(cent))])
        .await
        .unwrap();
    let error = row
        .with_user(42)
        .write(
            &mut tx,
            RawVals::new().set_cell("amount", Cell::Decimal(dec("1.005"))),
        )
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Access);
    assert_eq!(
        error.message(),
        "You do not have enough rights to access the field \"amount\" on test_monetary_mutation.line (test_monetary_mutation.line). Please contact your system administrator.\n\nOperation: write"
    );
    assert_eq!(
        row.values(&mut tx, "amount").await.unwrap(),
        vec![Value::Decimal(dec("2.68"))]
    );
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn hidden_currency_identity_is_available_only_to_rounding() {
    let policy = erp_testkit::FieldPolicy::new().deny(
        "test_monetary_mutation.line",
        "currency_id",
        erp_orm::FieldOp::Read,
    );
    let h = Harness::builder([&base_users::ADDON, &erp_addon_base::ADDON, &fixture::ADDON])
        .policy(policy)
        .build()
        .await
        .unwrap();
    let mut tx = h.begin().await.unwrap();
    tx.batch("INSERT INTO res_users(id,name,login,active,share) VALUES (42,'Amount writer','amount-writer',true,false)").await.unwrap();
    let cent = currency(&mut tx, "CNT", "0.01").await;
    let row = tx
        .env()
        .records("test_monetary_mutation.line")
        .unwrap()
        .create(&mut tx, vec![line("hidden", Some(cent))])
        .await
        .unwrap();
    for warm in [false, true] {
        tx.invalidate_all().await.unwrap();
        if warm {
            row.values(&mut tx, "currency_id").await.unwrap();
        }
        let user_row = row.with_user(42);
        user_row
            .write(
                &mut tx,
                RawVals::new().set_cell("amount", Cell::Decimal(dec("1.005"))),
            )
            .await
            .unwrap();
        let error = user_row.values(&mut tx, "currency_id").await.unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Access);
        assert_eq!(
            error.message(),
            "You do not have enough rights to access the field \"currency_id\" on test_monetary_mutation.line (test_monetary_mutation.line). Please contact your system administrator.\n\nOperation: read"
        );
        assert_eq!(
            user_row.values(&mut tx, "amount").await.unwrap(),
            vec![Value::Decimal(dec("1.01"))]
        );
    }
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn denied_write_rules_precede_mixed_currency_resolution() {
    let policy = erp_testkit::DomainPolicy::builder(erp_testkit::Otherwise::Allow)
        .rule(
            "test_monetary_mutation.line",
            erp_sdk::Operation::Write,
            Domain::FALSE,
        )
        .build()
        .unwrap();
    let h = Harness::builder([&base_users::ADDON, &erp_addon_base::ADDON, &fixture::ADDON])
        .policy(policy)
        .build()
        .await
        .unwrap();
    let mut tx = h.begin().await.unwrap();
    tx.batch("INSERT INTO res_users(id,name,login,active,share) VALUES (42,'Denied writer','denied-writer',true,false)").await.unwrap();
    let cent = currency(&mut tx, "CNT", "0.01").await;
    let unit = currency(&mut tx, "UNT", "1").await;
    let rows = tx
        .env()
        .records("test_monetary_mutation.line")
        .unwrap()
        .create(
            &mut tx,
            vec![line("cent", Some(cent)), line("unit", Some(unit))],
        )
        .await
        .unwrap();
    let error = rows
        .with_user(42)
        .write(
            &mut tx,
            RawVals::new().set_cell("amount", Cell::Decimal(dec("1.005"))),
        )
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Access);
    assert_eq!(
        error.message(),
        "You are not allowed to write 'test_monetary_mutation.line' (test_monetary_mutation.line) records."
    );
    assert!(!error.message().contains("currencies"));
    assert_eq!(
        rows.values(&mut tx, "amount").await.unwrap(),
        vec![Value::Decimal(dec("2.68")), Value::Decimal(dec("3"))]
    );
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn precompute_normalizes_each_monetary_input_and_output_once() {
    use std::sync::atomic::Ordering;
    let h = Harness::builder([
        &base_users::ADDON,
        &erp_addon_base::ADDON,
        &fixture::ADDON,
        &counted_provider::ADDON,
    ])
    .config(erp_orm::OrmConfig {
        recompute_batch: 1,
        ..Default::default()
    })
    .build()
    .await
    .unwrap();
    let mut tx = h.begin().await.unwrap();
    let cent = currency(&mut tx, "CNT", "0.01").await;
    let model = tx.env().records("test_monetary_mutation.line").unwrap();
    counted_provider::ROWS.store(0, Ordering::SeqCst);
    let rows = model
        .create(
            &mut tx,
            vec![
                line("one", Some(cent)),
                line("two", Some(cent)),
                line("three", Some(cent)),
            ],
        )
        .await
        .unwrap();
    assert_eq!(
        counted_provider::ROWS.load(Ordering::SeqCst),
        3,
        "one provider input for each supplied monetary cell"
    );
    assert_eq!(
        rows.values(&mut tx, "observed_amount").await.unwrap(),
        vec![Value::Decimal(dec("2.68")); 3]
    );
    counted_provider::ROWS.store(0, Ordering::SeqCst);
    let derived = tx
        .env()
        .records("test_monetary_mutation.computed")
        .unwrap()
        .create(
            &mut tx,
            (0..3)
                .map(|_| {
                    RawVals::new()
                        .set_cell("selected_currency_id", Cell::Id(cent.get()))
                        .set_cell("raw", Cell::Decimal(dec("2.675")))
                })
                .collect(),
        )
        .await
        .unwrap();
    assert_eq!(
        derived.values(&mut tx, "observed").await.unwrap(),
        vec![Value::Decimal(dec("2.68")); 3]
    );
    assert_eq!(
        counted_provider::ROWS.load(Ordering::SeqCst),
        3,
        "derived monetary output normalized once before its consumer"
    );
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn pending_related_currency_waits_for_its_computed_first_hop() {
    let h = Harness::builder([&base_users::ADDON, &erp_addon_base::ADDON, &fixture::ADDON])
        .config(erp_orm::OrmConfig {
            recompute_batch: 1,
            ..Default::default()
        })
        .build()
        .await
        .unwrap();
    let mut tx = h.begin().await.unwrap();
    let cent = currency(&mut tx, "CNT", "0.01").await;
    let owner = tx
        .env()
        .records("test_monetary_mutation.line")
        .unwrap()
        .create(&mut tx, vec![line("owner", Some(cent))])
        .await
        .unwrap();
    let row = tx
        .env()
        .records("test_monetary_mutation.late_hop")
        .unwrap()
        .create(
            &mut tx,
            (0..3)
                .map(|_| {
                    RawVals::new()
                        .set_cell("selected_owner_id", Cell::Id(owner.rec_ids()[0].get()))
                        .set_cell("amount", Cell::Decimal(dec("2.675")))
                })
                .collect(),
        )
        .await
        .unwrap();
    assert_eq!(
        row.values(&mut tx, "amount").await.unwrap(),
        vec![Value::Decimal(dec("2.68")); 3]
    );
    assert_eq!(
        row.values(&mut tx, "observed").await.unwrap(),
        vec![Value::Decimal(dec("2.68")); 3]
    );
    tx.flush().await.unwrap();
    tx.invalidate_all().await.unwrap();
    assert_eq!(
        row.values(&mut tx, "amount").await.unwrap(),
        vec![Value::Decimal(dec("2.68")); 3]
    );
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn currency_lookup_does_not_elevate_native_compute_inputs() {
    let policy = erp_testkit::FieldPolicy::new().deny(
        "test_monetary_mutation.private_compute",
        "secret_currency_id",
        erp_orm::FieldOp::Read,
    );
    let h = Harness::builder([&base_users::ADDON, &erp_addon_base::ADDON, &fixture::ADDON])
        .policy(policy)
        .build()
        .await
        .unwrap();
    let mut tx = h.begin().await.unwrap();
    tx.batch("INSERT INTO res_users(id,name,login,active,share) VALUES (42,'Compute reader','compute-reader',true,false)").await.unwrap();
    let cent = currency(&mut tx, "CNT", "0.01").await;
    let row = tx
        .env()
        .records("test_monetary_mutation.private_compute")
        .unwrap()
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("selected_currency_id", Cell::Id(cent.get()))
                    .set_cell("secret_currency_id", Cell::Id(cent.get()))
                    .set_cell("amount", Cell::Decimal(dec("2.675"))),
            ],
        )
        .await
        .unwrap();
    for (change, warm) in [(false, false), (false, true), (true, false), (true, true)] {
        tx.invalidate_all().await.unwrap();
        if warm {
            row.values(&mut tx, "secret_currency_id").await.unwrap();
        }
        let baseline = row
            .with_user(42)
            .values(&mut tx, "currency_id")
            .await
            .unwrap_err();
        assert_eq!(baseline.kind(), ErrorKind::Access);
        let mut vals = RawVals::new().set_cell("amount", Cell::Decimal(dec("1.005")));
        if change {
            vals = vals.set_cell("selected_currency_id", Cell::Null);
        }
        let error = tx
            .savepoint(async |tx| row.with_user(42).write(tx, vals).await)
            .await
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Access);
        assert_eq!(error.message(), baseline.message());
        assert_eq!(
            row.values(&mut tx, "amount").await.unwrap(),
            vec![Value::Decimal(dec("2.68"))]
        );
    }
    row.with_user(42)
        .write(
            &mut tx,
            RawVals::new().set_cell("amount_sudo", Cell::Decimal(dec("1.005"))),
        )
        .await
        .unwrap();
    assert_eq!(
        row.values(&mut tx, "amount_sudo").await.unwrap(),
        vec![Value::Decimal(dec("1.01"))]
    );
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn native_currency_compute_keeps_its_inputs_company_bound() {
    let h = harness().await;
    let mut setup = h.begin().await.unwrap();
    setup.batch("INSERT INTO res_users(id,name,login,active,share) VALUES (42,'Company reader','company-reader',true,false)").await.unwrap();
    let cent = currency(&mut setup, "CNT", "0.01").await;
    let companies = setup
        .env()
        .records("res.company")
        .unwrap()
        .create(
            &mut setup,
            ["Own", "Foreign"]
                .into_iter()
                .map(|name| {
                    RawVals::new()
                        .set_cell("name", Cell::Str(name.into()))
                        .set_cell("currency_id", Cell::Id(cent.get()))
                })
                .collect(),
        )
        .await
        .unwrap();
    let owner = setup
        .env()
        .records("test_monetary_mutation.scoped_owner")
        .unwrap()
        .create(
            &mut setup,
            vec![
                RawVals::new()
                    .set_cell("company_id", Cell::Id(companies.rec_ids()[1].get()))
                    .set_cell("currency_id", Cell::Id(cent.get())),
            ],
        )
        .await
        .unwrap()
        .rec_ids()[0];
    let row = setup
        .env()
        .records("test_monetary_mutation.scoped_compute")
        .unwrap()
        .create(
            &mut setup,
            vec![
                RawVals::new()
                    .set_cell("owner_id", Cell::Id(owner.get()))
                    .set_cell("amount", Cell::Decimal(dec("2.675"))),
            ],
        )
        .await
        .unwrap()
        .rec_ids()[0];
    setup.commit().await.unwrap();
    let company = erp_orm::CompanyId::new(companies.rec_ids()[0].get()).unwrap();
    let resolution = erp_orm::Resolution::in_id_order(
        erp_orm::GroupSet::default(),
        [company],
        std::num::NonZeroU64::MIN,
    )
    .unwrap();
    let principal = erp_orm::PrincipalBuilder::user(erp_orm::UserId::new(42).unwrap())
        .resolution(resolution)
        .companies(vec![company])
        .build()
        .unwrap();
    let mut tx = h
        .orm()
        .begin_opts(
            erp_orm::BeginOptions::for_principal(principal).context(erp_orm::Context::new()),
        )
        .await
        .unwrap();
    let records = tx
        .env()
        .records("test_monetary_mutation.scoped_compute")
        .unwrap()
        .browse_ids([row]);
    for (change, warm) in [(false, false), (false, true), (true, false), (true, true)] {
        tx.invalidate_all().await.unwrap();
        if warm {
            records
                .sudo(true)
                .values(&mut tx, "source_currency_id")
                .await
                .unwrap();
        }
        let baseline = records.values(&mut tx, "currency_id").await.unwrap_err();
        assert_eq!(baseline.kind(), ErrorKind::Access);
        let mut vals = RawVals::new().set_cell("amount", Cell::Decimal(dec("1.005")));
        if change {
            vals = vals.set_cell("trigger_currency_id", Cell::Id(cent.get()));
        }
        let error = tx
            .savepoint(async |tx| records.write(tx, vals).await)
            .await
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Access);
        assert_eq!(error.message(), baseline.message());
        assert_eq!(
            records.values(&mut tx, "amount").await.unwrap(),
            vec![Value::Decimal(dec("2.68"))]
        );
    }
    records
        .write(
            &mut tx,
            RawVals::new().set_cell("amount_sudo", Cell::Decimal(dec("1.005"))),
        )
        .await
        .unwrap();
    assert_eq!(
        records.values(&mut tx, "amount_sudo").await.unwrap(),
        vec![Value::Decimal(dec("1.01"))]
    );
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn native_currency_compute_keeps_rights_across_related_and_delegated_hops() {
    let policy = erp_testkit::FieldPolicy::new()
        .deny(
            "test_monetary_mutation.private_compute",
            "secret_currency_id",
            erp_orm::FieldOp::Read,
        )
        .deny(
            "test_monetary_mutation.private_link",
            "secret_parent_id",
            erp_orm::FieldOp::Read,
        );
    let h = Harness::builder([&base_users::ADDON, &erp_addon_base::ADDON, &fixture::ADDON])
        .policy(policy)
        .build()
        .await
        .unwrap();
    let mut tx = h.begin().await.unwrap();
    tx.batch(
        "INSERT INTO res_users(id,name,login,active,share) VALUES (42,'Hop reader','hop-reader',true,false)",
    )
    .await
    .unwrap();
    let cent = currency(&mut tx, "CNT", "0.01").await;
    let parent = tx
        .env()
        .records("test_monetary_mutation.private_compute")
        .unwrap()
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("selected_currency_id", Cell::Id(cent.get()))
                    .set_cell("secret_currency_id", Cell::Id(cent.get())),
            ],
        )
        .await
        .unwrap();
    let link = tx
        .env()
        .records("test_monetary_mutation.private_link")
        .unwrap()
        .create(
            &mut tx,
            vec![RawVals::new().set_cell("secret_parent_id", Cell::Id(parent.rec_ids()[0].get()))],
        )
        .await
        .unwrap();
    for model in [
        "test_monetary_mutation.private_related",
        "test_monetary_mutation.private_child",
        "test_monetary_mutation.private_deep",
    ] {
        let vals = if model.ends_with("private_deep") {
            RawVals::new().set_cell("link_id", Cell::Id(link.rec_ids()[0].get()))
        } else {
            RawVals::new().set_cell("parent_id", Cell::Id(parent.rec_ids()[0].get()))
        };
        let row = tx
            .env()
            .records(model)
            .unwrap()
            .create(
                &mut tx,
                vec![vals.set_cell("own_amount", Cell::Decimal(dec("2.675")))],
            )
            .await
            .unwrap();
        for warm in [false, true] {
            tx.invalidate_all().await.unwrap();
            if warm {
                parent.values(&mut tx, "secret_currency_id").await.unwrap();
                link.values(&mut tx, "secret_parent_id").await.unwrap();
            }
            let user = row.with_user(42);
            let baseline = tx
                .savepoint(async |tx| {
                    if model.ends_with("private_deep") {
                        link.write(tx, RawVals::new().set_cell("secret_parent_id", Cell::Null))
                            .await?;
                    }
                    user.values(tx, "currency_id").await
                })
                .await
                .unwrap_err();
            assert_eq!(baseline.kind(), ErrorKind::Access);
            let error = tx
                .savepoint(async |tx| {
                    if model.ends_with("private_deep") {
                        link.write(tx, RawVals::new().set_cell("secret_parent_id", Cell::Null))
                            .await?;
                    }
                    user.write(
                        tx,
                        RawVals::new().set_cell("own_amount", Cell::Decimal(dec("1.005"))),
                    )
                    .await
                })
                .await
                .expect_err(&format!("model={model}, warm={warm}"));
            assert_eq!(error.kind(), ErrorKind::Access);
            assert_eq!(error.message(), baseline.message(), "{model}, warm={warm}");
            assert_eq!(
                row.values(&mut tx, "own_amount").await.unwrap(),
                vec![Value::Decimal(dec("2.68"))]
            );
        }
    }
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

async fn native_currency_company_hop_case(model: &str) {
    let h = harness().await;
    let mut setup = h.begin().await.unwrap();
    setup.batch("INSERT INTO res_users(id,name,login,active,share) VALUES (42,'Company reader','company-reader',true,false)").await.unwrap();
    let cent = currency(&mut setup, "CNT", "0.01").await;
    let companies = setup
        .env()
        .records("res.company")
        .unwrap()
        .create(
            &mut setup,
            ["Own", "Foreign"]
                .into_iter()
                .map(|name| {
                    RawVals::new()
                        .set_cell("name", Cell::Str(name.into()))
                        .set_cell("currency_id", Cell::Id(cent.get()))
                })
                .collect(),
        )
        .await
        .unwrap();
    let owner = setup
        .env()
        .records("test_monetary_mutation.scoped_owner")
        .unwrap()
        .create(
            &mut setup,
            vec![
                RawVals::new()
                    .set_cell("company_id", Cell::Id(companies.rec_ids()[1].get()))
                    .set_cell("currency_id", Cell::Id(cent.get())),
            ],
        )
        .await
        .unwrap()
        .rec_ids()[0];
    let row = setup
        .env()
        .records("test_monetary_mutation.scoped_compute")
        .unwrap()
        .create(
            &mut setup,
            vec![
                RawVals::new()
                    .set_cell("owner_id", Cell::Id(owner.get()))
                    .set_cell("amount", Cell::Decimal(dec("2.675"))),
            ],
        )
        .await
        .unwrap()
        .rec_ids()[0];
    let parent = row;
    let amount = if model.ends_with("scoped_child") {
        "own_amount"
    } else {
        "amount"
    };
    let amount_sudo = if model.ends_with("scoped_child") {
        "own_amount_sudo"
    } else {
        "amount_sudo"
    };
    let row = setup
        .env()
        .records(model)
        .unwrap()
        .create(
            &mut setup,
            vec![
                RawVals::new()
                    .set_cell("parent_id", Cell::Id(parent.get()))
                    .set_cell(amount, Cell::Decimal(dec("2.675"))),
            ],
        )
        .await
        .unwrap()
        .rec_ids()[0];
    setup.commit().await.unwrap();
    let company = erp_orm::CompanyId::new(companies.rec_ids()[0].get()).unwrap();
    let resolution = erp_orm::Resolution::in_id_order(
        erp_orm::GroupSet::default(),
        [company],
        std::num::NonZeroU64::MIN,
    )
    .unwrap();
    let principal = erp_orm::PrincipalBuilder::user(erp_orm::UserId::new(42).unwrap())
        .resolution(resolution)
        .companies(vec![company])
        .build()
        .unwrap();
    let mut tx = h
        .orm()
        .begin_opts(
            erp_orm::BeginOptions::for_principal(principal).context(erp_orm::Context::new()),
        )
        .await
        .unwrap();
    let records = tx.env().records(model).unwrap().browse_ids([row]);
    for warm in [false, true] {
        tx.invalidate_all().await.unwrap();
        if warm {
            tx.env()
                .records("test_monetary_mutation.scoped_compute")
                .unwrap()
                .browse_ids([parent])
                .sudo(true)
                .values(&mut tx, "source_currency_id")
                .await
                .unwrap();
        }
        let baseline = records.values(&mut tx, "currency_id").await.unwrap_err();
        assert_eq!(baseline.kind(), ErrorKind::Access);
        let vals = RawVals::new().set_cell(amount, Cell::Decimal(dec("1.005")));
        let error = tx
            .savepoint(async |tx| records.write(tx, vals).await)
            .await
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Access);
        assert_eq!(error.message(), baseline.message());
        assert_eq!(
            records.values(&mut tx, amount).await.unwrap(),
            vec![Value::Decimal(dec("2.68"))]
        );
    }
    records
        .write(
            &mut tx,
            RawVals::new().set_cell(amount_sudo, Cell::Decimal(dec("1.005"))),
        )
        .await
        .unwrap();
    assert_eq!(
        records.values(&mut tx, amount_sudo).await.unwrap(),
        vec![Value::Decimal(dec("1.01"))]
    );
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn native_currency_company_bound_inputs_survive_related_hops() {
    native_currency_company_hop_case("test_monetary_mutation.scoped_related").await;
}
#[tokio::test]
async fn native_currency_company_bound_inputs_survive_delegation() {
    native_currency_company_hop_case("test_monetary_mutation.scoped_child").await;
}

#[tokio::test]
async fn empty_currency_hops_do_not_settle_unrelated_native_computes() {
    let policy = erp_testkit::FieldPolicy::new().deny(
        "test_monetary_mutation.private_link",
        "secret_parent_id",
        erp_orm::FieldOp::Read,
    );
    let h = Harness::builder([
        &base_users::ADDON,
        &erp_addon_base::ADDON,
        &fixture::ADDON,
        &empty_provider::ADDON,
    ])
    .policy(policy)
    .build()
    .await
    .unwrap();
    let mut tx = h.begin().await.unwrap();
    tx.batch("INSERT INTO res_users(id,name,login,active,share) VALUES (42,'Empty hop writer','empty-hop-writer',true,false)").await.unwrap();
    let parent = tx
        .env()
        .records("test_monetary_mutation.private_compute")
        .unwrap()
        .create(&mut tx, vec![RawVals::new()])
        .await
        .unwrap();
    let link = tx
        .env()
        .records("test_monetary_mutation.private_link")
        .unwrap()
        .create(
            &mut tx,
            vec![RawVals::new().set_cell("secret_parent_id", Cell::Id(parent.rec_ids()[0].get()))],
        )
        .await
        .unwrap();
    let row = tx
        .env()
        .records("test_monetary_mutation.private_deep")
        .unwrap()
        .create(
            &mut tx,
            vec![RawVals::new().set_cell("own_amount", Cell::Decimal(dec("2.675")))],
        )
        .await
        .unwrap();
    tx.flush().await.unwrap();
    link.write(
        &mut tx,
        RawVals::new().set_cell("secret_parent_id", Cell::Null),
    )
    .await
    .unwrap();
    fixture::EMPTY_NATIVE_CALLS.store(0, std::sync::atomic::Ordering::SeqCst);
    row.with_user(42)
        .with_context([("empty_hop_probe", Value::Bool(true))])
        .write(
            &mut tx,
            RawVals::new().set_cell("own_amount", Cell::Decimal(dec("1.005"))),
        )
        .await
        .unwrap();
    assert_eq!(
        row.values(&mut tx, "own_amount").await.unwrap(),
        vec![Value::Decimal(dec("1.005"))]
    );
    assert_eq!(
        fixture::EMPTY_NATIVE_CALLS.load(std::sync::atomic::Ordering::SeqCst),
        0
    );
    let error = link
        .with_user(42)
        .with_context([("empty_hop_probe", Value::Bool(true))])
        .values(&mut tx, "parent_id")
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Access);
    assert_eq!(
        error.message(),
        "You do not have enough rights to access the field \"secret_parent_id\" on test_monetary_mutation.private_link (test_monetary_mutation.private_link). Please contact your system administrator.\n\nOperation: read"
    );
    assert_eq!(
        fixture::EMPTY_NATIVE_CALLS.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "denied input preload never entered the native method"
    );
    assert_eq!(
        link.with_context([("empty_hop_probe", Value::Bool(true))])
            .values(&mut tx, "parent_id")
            .await
            .unwrap(),
        vec![Value::False]
    );
    assert_eq!(
        fixture::EMPTY_NATIVE_CALLS.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "unrelated native compute remained pending and the allowed read executes it after denial"
    );
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn correction_onchange_rounds_root_before_dependent_computes() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let cent = currency(&mut tx, "OCN", "0.01").await;
    let model = tx.env().records("test_monetary_mutation.line").unwrap();
    let persisted = model
        .create(&mut tx, vec![line("persisted", Some(cent))])
        .await
        .unwrap();
    for records in [&model, &persisted] {
        let values = [
            ("name".to_owned(), Value::str("edited")),
            ("currency_id".to_owned(), Value::Int(cent.get())),
            ("amount".to_owned(), Value::Decimal(dec("1.005"))),
            ("observed_amount".to_owned(), Value::Decimal(Decimal::ZERO)),
        ]
        .into_iter()
        .collect();
        let result = records
            .onchange(
                &mut tx,
                values,
                vec!["amount".to_owned()],
                erp_orm::Spec::new()
                    .field("name")
                    .field("currency_id")
                    .field("amount")
                    .field("observed_amount"),
            )
            .await
            .unwrap();
        let Value::Map(result) = result else {
            panic!("onchange object")
        };
        let Some(Value::Map(values)) = result.get("value") else {
            panic!("onchange values")
        };
        // Changed fields are inserted into snapshot0; the dependent output proves
        // that their normalized value was consumed even if amount is omitted.
        assert_eq!(
            values.get("observed_amount"),
            Some(&Value::Decimal(dec("1.01")))
        );
    }
    for records in [&model, &persisted] {
        for (currency, expected) in [(Value::Int(cent.get()), "2.68"), (Value::False, "2.675")] {
            let values = [
                ("name".to_owned(), Value::str("form")),
                ("currency_id".to_owned(), currency),
                ("amount".to_owned(), Value::Decimal(dec("2.675"))),
            ]
            .into_iter()
            .collect();
            let result = records
                .onchange(
                    &mut tx,
                    values,
                    Vec::new(),
                    erp_orm::Spec::new()
                        .field("name")
                        .field("currency_id")
                        .field("amount")
                        .field("observed_amount"),
                )
                .await
                .unwrap();
            let Value::Map(result) = result else {
                panic!("onchange object")
            };
            let Some(Value::Map(values)) = result.get("value") else {
                panic!("onchange values")
            };
            assert_eq!(values.get("amount"), Some(&Value::Decimal(dec(expected))));
            assert_eq!(
                values.get("observed_amount"),
                Some(&Value::Decimal(dec(expected)))
            );
        }
    }
    assert_eq!(
        persisted.values(&mut tx, "amount").await.unwrap(),
        vec![Value::Decimal(dec("2.68"))]
    );
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn correction_zero_null_create_does_not_force_async_or_late_currency() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let cent = currency(&mut tx, "ZCN", "0.01").await;
    for name in [
        "test_monetary_mutation.async_currency",
        "test_monetary_mutation.async_plain_currency",
        "test_monetary_mutation.late_currency",
    ] {
        for amount in [Value::Decimal(Decimal::ZERO), Value::False] {
            for defaulted in [false, true] {
                let model = tx.env().records(name).unwrap().with_context([
                    ("default_amount", amount.clone()),
                    ("default_selected_currency_id", Value::Int(cent.get())),
                ]);
                let mut values = std::collections::BTreeMap::new();
                if !defaulted {
                    values.insert("amount".to_owned(), amount.clone());
                    values.insert("selected_currency_id".to_owned(), Value::Int(cent.get()));
                }
                let row = model.create_values(&mut tx, &[values]).await.unwrap();
                assert_eq!(
                    row.values(&mut tx, "amount").await.unwrap(),
                    vec![Value::Decimal(Decimal::ZERO)]
                );
                tx.flush().await.unwrap();
                assert_eq!(
                    row.values(&mut tx, "currency_id").await.unwrap(),
                    vec![Value::Int(cent.get())]
                );
            }
        }
    }
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn correction_onchange_normalizes_delegated_parent_and_nested_lines() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let cent = currency(&mut tx, "DCN", "0.01").await;
    let fields = erp_orm::Spec::new()
        .field("name")
        .field("currency_id")
        .field("amount")
        .field("observed_amount");
    let values: ValueMap = [
        ("name".to_owned(), Value::str("form")),
        ("currency_id".to_owned(), Value::Int(cent.get())),
        ("amount".to_owned(), Value::Decimal(dec("2.675"))),
    ]
    .into_iter()
    .collect();
    let mut child_values = values.clone();
    child_values.insert("own_amount".to_owned(), Value::Decimal(dec("2.675")));
    let child = tx.env().records("test_monetary_mutation.child").unwrap();
    let result = child
        .onchange(
            &mut tx,
            child_values,
            Vec::new(),
            fields.clone().field("own_amount"),
        )
        .await
        .unwrap();
    let Value::Map(result) = result else {
        panic!("onchange object")
    };
    let Some(Value::Map(result)) = result.get("value") else {
        panic!("onchange values")
    };
    for field in ["amount", "own_amount", "observed_amount"] {
        assert_eq!(result.get(field), Some(&Value::Decimal(dec("2.68"))));
    }
    let form = tx.env().records("test_monetary_mutation.form").unwrap();
    let command = Value::list([Value::Int(0), Value::Int(0), Value::Map(Arc::new(values))]);
    let form_values = [("line_ids".to_owned(), Value::list([command]))]
        .into_iter()
        .collect();
    let result = form
        .onchange(
            &mut tx,
            form_values,
            Vec::new(),
            erp_orm::Spec::new().with("line_ids", erp_orm::SpecItem::fields(fields)),
        )
        .await
        .unwrap();
    let Value::Map(result) = result else {
        panic!("onchange object")
    };
    let Some(Value::Map(result)) = result.get("value") else {
        panic!("onchange values")
    };
    let Some(Value::List(commands)) = result.get("line_ids") else {
        panic!("line commands")
    };
    let Value::List(command) = &commands[0] else {
        panic!("line command")
    };
    let Value::Map(line) = &command[2] else {
        panic!("line values")
    };
    assert_eq!(line.get("amount"), Some(&Value::Decimal(dec("2.68"))));
    assert_eq!(
        line.get("observed_amount"),
        Some(&Value::Decimal(dec("2.68")))
    );
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn correction_onchange_failure_keeps_origin_and_allows_next_call() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let cent = currency(&mut tx, "FCN", "0.01").await;
    let unit = currency(&mut tx, "FUN", "1").await;
    let model = tx
        .env()
        .records("test_monetary_mutation.async_plain_currency")
        .unwrap();
    let row = model
        .create(
            &mut tx,
            vec![RawVals::new().set_cell("selected_currency_id", Cell::Id(cent.get()))],
        )
        .await
        .unwrap();
    tx.flush().await.unwrap();
    let original_amount = row.values(&mut tx, "amount").await.unwrap();
    let fields = erp_orm::Spec::new()
        .field("selected_currency_id")
        .field("currency_id")
        .field("amount");
    let values = [
        ("selected_currency_id".to_owned(), Value::Int(unit.get())),
        ("amount".to_owned(), Value::Decimal(dec("2.675"))),
    ]
    .into_iter()
    .collect();
    let error = row
        .onchange(&mut tx, values, Vec::new(), fields.clone())
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Programming);
    assert_eq!(
        error.message(),
        &refusal(
            "project same-call",
            "test_monetary_mutation.async_plain_currency.currency_id",
            "from changed inputs"
        )
    );
    assert_eq!(
        row.values(&mut tx, "amount").await.unwrap(),
        original_amount
    );
    assert_eq!(
        row.values(&mut tx, "currency_id").await.unwrap(),
        vec![Value::Int(cent.get())]
    );
    let values = [
        ("selected_currency_id".to_owned(), Value::Int(cent.get())),
        ("currency_id".to_owned(), Value::Int(cent.get())),
        ("amount".to_owned(), Value::Decimal(dec("2.675"))),
    ]
    .into_iter()
    .collect();
    let result = row
        .onchange(&mut tx, values, Vec::new(), fields)
        .await
        .unwrap();
    let Value::Map(result) = result else {
        panic!("onchange object")
    };
    let Some(Value::Map(values)) = result.get("value") else {
        panic!("onchange values")
    };
    assert_eq!(values.get("amount"), Some(&Value::Decimal(dec("2.68"))));
    assert_eq!(
        row.values(&mut tx, "amount").await.unwrap(),
        original_amount
    );
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn candidate_currency_onchange_rounds_with_changed_input_and_keeps_origin() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let cent = currency(&mut tx, "OCC", "0.01").await;
    let unit = currency(&mut tx, "OCU", "1").await;
    let row = tx
        .env()
        .records("test_monetary_mutation.async_currency")
        .unwrap()
        .create(
            &mut tx,
            vec![RawVals::new().set_cell("selected_currency_id", Cell::Id(cent.get()))],
        )
        .await
        .unwrap();
    tx.flush().await.unwrap();
    let original_amount = row.values(&mut tx, "amount").await.unwrap();
    let fields = erp_orm::Spec::new()
        .field("selected_currency_id")
        .field("currency_id")
        .field("amount");
    let values = [
        ("selected_currency_id".to_owned(), Value::Int(unit.get())),
        ("amount".to_owned(), Value::Decimal(dec("2.675"))),
    ]
    .into_iter()
    .collect();
    let result = row
        .onchange(&mut tx, values, Vec::new(), fields)
        .await
        .unwrap();
    let Value::Map(result) = result else {
        panic!("onchange object")
    };
    let Some(Value::Map(values)) = result.get("value") else {
        panic!("onchange values")
    };
    assert_eq!(values.get("amount"), Some(&Value::Decimal(dec("3"))));
    assert_eq!(
        row.values(&mut tx, "amount").await.unwrap(),
        original_amount
    );
    assert_eq!(
        row.values(&mut tx, "currency_id").await.unwrap(),
        vec![Value::Int(cent.get())]
    );
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn correction_onchange_resolves_same_call_relation_links() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let cent = currency(&mut tx, "LCN", "0.01").await;
    let unit = currency(&mut tx, "LUN", "1").await;
    let owners = tx
        .env()
        .records("test_monetary_mutation.line")
        .unwrap()
        .create(
            &mut tx,
            vec![line("link-cent", Some(cent)), line("link-unit", Some(unit))],
        )
        .await
        .unwrap();
    for (name, link_field, extra) in [
        ("test_monetary_mutation.related", "owner_id", None),
        (
            "test_monetary_mutation.late_hop",
            "selected_owner_id",
            Some("observed"),
        ),
        (
            "test_monetary_mutation.child",
            "line_id",
            Some("own_amount"),
        ),
    ] {
        let model = tx.env().records(name).unwrap();
        let persisted = model
            .create(
                &mut tx,
                vec![RawVals::new().set_cell(link_field, Cell::Id(owners.rec_ids()[0].get()))],
            )
            .await
            .unwrap();
        for records in [&model, &persisted] {
            let mut fields = erp_orm::Spec::new()
                .field(link_field)
                .field("currency_id")
                .field("amount");
            let mut values: ValueMap = [
                (link_field.to_owned(), Value::Int(owners.rec_ids()[1].get())),
                ("amount".to_owned(), Value::Decimal(dec("2.675"))),
            ]
            .into_iter()
            .collect();
            if let Some(extra) = extra {
                fields = fields.field(extra);
                if extra == "own_amount" {
                    values.insert(extra.to_owned(), Value::Decimal(dec("2.675")));
                }
            }
            let result = records
                .onchange(&mut tx, values, Vec::new(), fields)
                .await
                .unwrap();
            let Value::Map(result) = result else {
                panic!("onchange object")
            };
            let Some(Value::Map(values)) = result.get("value") else {
                panic!("onchange values")
            };
            assert_eq!(
                values.get("amount"),
                Some(&Value::Decimal(dec("3"))),
                "{name}"
            );
            if let Some(extra) = extra {
                assert_eq!(
                    values.get(extra),
                    Some(&Value::Decimal(dec("3"))),
                    "{name}.{extra}"
                );
            }
        }
    }
    assert_eq!(
        owners.values(&mut tx, "amount").await.unwrap(),
        vec![Value::Decimal(dec("2.68")), Value::Decimal(dec("3"))]
    );
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn correction_initial_onchange_currency_sees_non_triggering_inputs() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let cent = currency(&mut tx, "ICN", "0.01").await;
    let unit = currency(&mut tx, "IUN", "1").await;
    let model = tx.env().records("test_monetary_mutation.derived").unwrap();
    let fields = erp_orm::Spec::new()
        .field("read_currency_id")
        .field("currency_id")
        .field("amount")
        .field("observed");
    let values = [
        ("read_currency_id".to_owned(), Value::Int(unit.get())),
        ("amount".to_owned(), Value::Decimal(dec("2.675"))),
    ]
    .into_iter()
    .collect();
    let result = model
        .onchange(&mut tx, values, Vec::new(), fields.clone())
        .await
        .unwrap();
    let Value::Map(result) = result else {
        panic!("onchange object")
    };
    let Some(Value::Map(values)) = result.get("value") else {
        panic!("onchange values")
    };
    assert_eq!(values.get("amount"), Some(&Value::Decimal(dec("3"))));
    assert_eq!(values.get("observed"), Some(&Value::Decimal(dec("3"))));
    let persisted = model
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("selected_currency_id", Cell::Id(unit.get()))
                    .set_cell("amount", Cell::Decimal(dec("2.675"))),
            ],
        )
        .await
        .unwrap();
    for records in [&model, &persisted] {
        let values = [
            ("read_currency_id".to_owned(), Value::Int(cent.get())),
            ("currency_id".to_owned(), Value::Int(unit.get())),
            ("amount".to_owned(), Value::Decimal(dec("2.675"))),
            ("observed".to_owned(), Value::Decimal(Decimal::ZERO)),
        ]
        .into_iter()
        .collect();
        let result = records
            .onchange(
                &mut tx,
                values,
                vec!["read_currency_id".to_owned(), "amount".to_owned()],
                fields.clone(),
            )
            .await
            .unwrap();
        let Value::Map(result) = result else {
            panic!("onchange object")
        };
        let Some(Value::Map(values)) = result.get("value") else {
            panic!("onchange values")
        };
        // A later form already carries a current currency. A reads-only edit must
        // not notify/reproject that compute even though its form record has a new id.
        assert_eq!(values.get("observed"), Some(&Value::Decimal(dec("3"))));
        assert!(!values.contains_key("currency_id"));
    }
    assert_eq!(
        persisted.values(&mut tx, "amount").await.unwrap(),
        vec![Value::Decimal(dec("3"))]
    );
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

// ---- bounded staging ---------------------------------
//
// The contract numbers come from the task decision of 2026-10-31: at most 200,000 target
// records and at most 2,000,000 staged monetary cells per operation. They are literals here on
// purpose: the engine keeps them private.

const MAX_RECORDS: usize = 200_000;
const MAX_CELLS: usize = 2_000_000;
const TASK: &str = "a later release";

/// The caller-facing refusal: kind Validation, the limit and the task named, no input count.
fn assert_refused(error: &Error, limit: &str, forbidden: &[String]) {
    assert_eq!(error.kind(), ErrorKind::Validation, "{error}");
    let message = error.to_string();
    assert!(
        message.contains(&format!("monetary staging {limit} exceeded")),
        "{message}"
    );
    assert!(message.contains(TASK), "{message}");
    for text in forbidden {
        assert!(!message.contains(text.as_str()), "{message}");
    }
}

fn records_refusal(error: &Error, input: usize) {
    assert_refused(
        error,
        &format!("target records limit ({MAX_RECORDS})"),
        &[input.to_string()],
    );
}

fn cells_refusal(error: &Error, input: usize) {
    assert_refused(
        error,
        &format!("cells limit ({MAX_CELLS})"),
        &[input.to_string()],
    );
}

/// Insert `n` rows by SQL and return the ids of the whole table in order. The rows are real,
/// distinct database records; only `amount` is set (to NULL).
async fn insert_rows(tx: &mut Transaction, table: &str, n: usize) -> Vec<i64> {
    let n = i64::try_from(n).unwrap();
    tx.execute(
        &format!(
            "INSERT INTO \"{table}\" (amount) SELECT NULL::numeric FROM generate_series(1, $1::int8)"
        ),
        &[&n],
    )
    .await
    .unwrap();
    tx.query(&format!("SELECT id FROM \"{table}\" ORDER BY id"), &[])
        .await
        .unwrap()
        .iter()
        .map(|row| row.get::<_, i64>(0))
        .collect()
}

/// `(rows, rows with a stored amount)` of `table`, straight from the database.
async fn table_state(tx: &mut Transaction, table: &str) -> (i64, i64) {
    let row = tx
        .query(
            &format!("SELECT count(*), count(amount) FROM \"{table}\""),
            &[],
        )
        .await
        .unwrap()
        .remove(0);
    (row.get::<_, i64>(0), row.get::<_, i64>(1))
}

/// The state of the table's id sequence, without consuming a value.
async fn sequence_state(tx: &mut Transaction, table: &str) -> (i64, bool) {
    let sequence: String = tx
        .query(
            "SELECT pg_get_serial_sequence($1, 'id')",
            &[&format!("\"{table}\"")],
        )
        .await
        .unwrap()
        .remove(0)
        .get(0);
    let row = tx
        .query(
            &format!("SELECT last_value, is_called FROM {sequence}"),
            &[],
        )
        .await
        .unwrap()
        .remove(0);
    (row.get::<_, i64>(0), row.get::<_, bool>(1))
}

/// stored values stay as they were; distinct existing records)
#[tokio::test]
async fn write_over_record_bound_is_refused_and_leaves_state_unchanged() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let cents = currency(&mut tx, "CNT", "0.01").await;
    let ids = insert_rows(&mut tx, "test_monetary_mutation_bulk", MAX_RECORDS + 1).await;
    assert_eq!(ids.len(), MAX_RECORDS + 1);
    let model = tx.env().model::<fixture::Bulk>().unwrap();
    // a prior, admitted write gives the cache and the pending SQL state something to preserve
    let few = model.browse(ids[..3].to_vec()).unwrap();
    few.as_records()
        .write(
            &mut tx,
            RawVals::new()
                .set_cell("currency_id", Cell::Id(cents.get()))
                .set_cell("amount", Cell::Decimal(dec("1.005"))),
        )
        .await
        .unwrap();
    let before = few.mapped(&mut tx, fixture::Bulk::AMOUNT).await.unwrap();
    let queries = tx.query_count();

    let all = model.browse(ids.clone()).unwrap();
    assert_eq!(all.len(), MAX_RECORDS + 1);
    let error = all
        .as_records()
        .write(
            &mut tx,
            RawVals::new().set_cell("amount", Cell::Decimal(dec("20.00"))),
        )
        .await
        .unwrap_err();
    records_refusal(&error, MAX_RECORDS + 1);
    assert_eq!(
        tx.query_count(),
        queries,
        "the refusal issued no SQL statement"
    );
    assert_eq!(
        few.mapped(&mut tx, fixture::Bulk::AMOUNT).await.unwrap(),
        before
    );
    tx.flush().await.unwrap();
    // only the three admitted rows hold an amount in the database
    assert_eq!(
        table_state(&mut tx, "test_monetary_mutation_bulk").await,
        (i64::try_from(MAX_RECORDS + 1).unwrap(), 3)
    );
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

/// computed monetary field; its pending recompute survives)
#[tokio::test]
async fn write_over_bound_keeps_pending_recompute_of_stored_sibling() {
    use std::sync::atomic::Ordering;
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let ids = insert_rows(&mut tx, "test_monetary_mutation_sibling", MAX_RECORDS + 1).await;
    let model = tx.env().model::<fixture::Sibling>().unwrap();
    let first = model.browse([ids[0]]).unwrap();
    // `raw` is an input of the group: the group of the first row now waits for a recompute
    first
        .set(&mut tx, fixture::Sibling::RAW, dec("7"))
        .await
        .unwrap();
    fixture::PAIR_RUNS.store(0, Ordering::SeqCst);

    let all = model.browse(ids).unwrap();
    let error = all
        .as_records()
        .write(
            &mut tx,
            RawVals::new().set_cell("amount", Cell::Decimal(dec("5"))),
        )
        .await
        .unwrap_err();
    // first the ordering claim, then the error shape
    assert_eq!(
        fixture::PAIR_RUNS.load(Ordering::SeqCst),
        0,
        "the refusal came before the sibling recompute was published"
    );
    records_refusal(&error, MAX_RECORDS + 1);
    // the recompute is still owed: reading the sibling runs it now, once
    assert_eq!(
        first.get(&mut tx, fixture::Sibling::TWIN).await.unwrap(),
        dec("7")
    );
    assert_eq!(fixture::PAIR_RUNS.load(Ordering::SeqCst), 1);
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn create_over_record_bound_is_refused_without_rows_or_ids() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let cents = currency(&mut tx, "CNT", "0.01").await;
    let model = tx.env().model::<fixture::Bulk>().unwrap();
    let kept = model
        .create(
            &mut tx,
            vec![
                Vals::new()
                    .set(fixture::Bulk::CURRENCY_ID, cents)
                    .set(fixture::Bulk::AMOUNT, dec("1.005")),
            ],
        )
        .await
        .unwrap();
    tx.flush().await.unwrap();
    let rows = table_state(&mut tx, "test_monetary_mutation_bulk").await;
    let sequence = sequence_state(&mut tx, "test_monetary_mutation_bulk").await;
    let kept_amount = kept.mapped(&mut tx, fixture::Bulk::AMOUNT).await.unwrap();

    let oversized: Vec<Vals<fixture::Bulk>> = (0..MAX_RECORDS + 1)
        .map(|_| {
            Vals::new()
                .set(fixture::Bulk::CURRENCY_ID, cents)
                .set(fixture::Bulk::AMOUNT, dec("10.00"))
        })
        .collect();
    let error = model.create(&mut tx, oversized).await.unwrap_err();
    records_refusal(&error, MAX_RECORDS + 1);
    assert_eq!(
        table_state(&mut tx, "test_monetary_mutation_bulk").await,
        rows
    );
    assert_eq!(
        sequence_state(&mut tx, "test_monetary_mutation_bulk").await,
        sequence,
        "no id was drawn"
    );
    assert_eq!(
        kept.mapped(&mut tx, fixture::Bulk::AMOUNT).await.unwrap(),
        kept_amount
    );
    // the engine is usable afterwards and hands out the next id
    let next = model
        .create(
            &mut tx,
            vec![
                Vals::new()
                    .set(fixture::Bulk::CURRENCY_ID, cents)
                    .set(fixture::Bulk::AMOUNT, dec("2.675")),
            ],
        )
        .await
        .unwrap();
    assert_eq!(next.rec_ids()[0].get(), kept.rec_ids()[0].get() + 1);
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

/// record count is within its bound; real operation, distinct existing records)
#[tokio::test]
async fn write_over_cell_bound_is_refused_through_a_real_operation() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let records = 190_000;
    let ids = insert_rows(&mut tx, "test_monetary_mutation_wide", records).await;
    let model = tx.env().model::<fixture::Wide>().unwrap();
    let all = model.browse(ids).unwrap();
    let mut vals = RawVals::new().set_cell("amount", Cell::Decimal(dec("1")));
    for k in 1..=10 {
        vals = vals.set_cell(format!("in{k}"), Cell::Decimal(dec("1")));
    }
    // 190,000 records stay under the record bound; 11 cells each make 2,090,000 > 2,000,000
    let error = all.as_records().write(&mut tx, vals).await.unwrap_err();
    cells_refusal(&error, records * 11);
    tx.flush().await.unwrap();
    assert_eq!(
        table_state(&mut tx, "test_monetary_mutation_wide").await,
        (i64::try_from(records).unwrap(), 0)
    );
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

/// bound; the supplied cells alone are admitted)
#[tokio::test]
async fn projection_cells_alone_push_a_write_over_the_cell_bound() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let cents = currency(&mut tx, "CNT", "0.01").await;
    let write = |inputs: usize| {
        let mut vals = RawVals::new().set_cell("amount", Cell::Decimal(dec("1.005")));
        for k in 1..=inputs {
            vals = vals.set_cell(format!("in{k}"), Cell::Decimal(dec("1.005")));
        }
        vals
    };
    // control: the same shape on a few rows is admitted (the refusal below is the bound, not
    // the shape)
    let small = insert_rows(&mut tx, "test_monetary_mutation_wide", 10).await;
    let model = tx.env().model::<fixture::Wide>().unwrap();
    let few = model.browse(small.clone()).unwrap();
    few.as_records()
        .write(
            &mut tx,
            write(7).set_cell("selected_currency_id", Cell::Id(cents.get())),
        )
        .await
        .unwrap();

    // 200,000 records x (amount + 7 inputs) = 1,600,000 supplied cells: within the bound. The
    // projection of the currency compute stages the 7 inputs again: 1,400,000 more.
    let ids = insert_rows(&mut tx, "test_monetary_mutation_wide", MAX_RECORDS).await;
    let all = model.browse(ids[small.len()..].to_vec()).unwrap();
    assert_eq!(all.len(), MAX_RECORDS);
    let error = all.as_records().write(&mut tx, write(7)).await.unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Validation, "{error}");
    assert!(
        error.to_string().contains("cells limit (2000000) exceeded"),
        "{error}"
    );
    assert!(error.to_string().contains(TASK), "{error}");
    tx.flush().await.unwrap();
    // nothing of the refused write was stored; only the control rows hold amounts
    assert_eq!(
        table_state(&mut tx, "test_monetary_mutation_wide").await.1,
        i64::try_from(small.len()).unwrap()
    );
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

/// bound; refused before a compute runs and before any row is inserted)
#[tokio::test]
async fn precompute_cells_alone_push_a_create_over_the_cell_bound() {
    use std::sync::atomic::Ordering;
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let cents = currency(&mut tx, "CNT", "0.01").await;
    let model = tx.env().model::<fixture::WideComputed>().unwrap();
    let sequence = sequence_state(&mut tx, "test_monetary_mutation_wide_computed").await;
    fixture::FILL_RUNS.store(0, Ordering::SeqCst);
    let records = 190_000;
    // no row supplies a monetary cell; 11 computed cells each make 2,090,000 > 2,000,000
    let vals: Vec<Vals<fixture::WideComputed>> = (0..records)
        .map(|_| {
            Vals::new()
                .set(fixture::WideComputed::SELECTED_CURRENCY_ID, cents)
                .set(fixture::WideComputed::RAW, dec("1.005"))
        })
        .collect();
    let error = model.create(&mut tx, vals).await.unwrap_err();
    cells_refusal(&error, records * 11);
    assert_eq!(
        fixture::FILL_RUNS.load(Ordering::SeqCst),
        0,
        "refused before the precompute ran"
    );
    let count: i64 = tx
        .query(
            "SELECT count(*) FROM \"test_monetary_mutation_wide_computed\"",
            &[],
        )
        .await
        .unwrap()
        .remove(0)
        .get(0);
    assert_eq!(count, 0);
    assert_eq!(
        sequence_state(&mut tx, "test_monetary_mutation_wide_computed").await,
        sequence
    );
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn write_at_exactly_the_record_bound_is_admitted() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let ids = insert_rows(&mut tx, "test_monetary_mutation_bulk", MAX_RECORDS).await;
    let model = tx.env().model::<fixture::Bulk>().unwrap();
    let all = model.browse(ids.clone()).unwrap();
    assert_eq!(all.len(), MAX_RECORDS);
    all.as_records()
        .write(
            &mut tx,
            RawVals::new().set_cell("amount", Cell::Decimal(dec("3.50"))),
        )
        .await
        .unwrap();
    let last = model.browse([ids[MAX_RECORDS - 1]]).unwrap();
    assert_eq!(
        last.mapped(&mut tx, fixture::Bulk::AMOUNT).await.unwrap(),
        vec![dec("3.50")]
    );
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

/// admitted by a real write)
#[tokio::test]
async fn write_at_exactly_the_cell_bound_is_admitted() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let ids = insert_rows(&mut tx, "test_monetary_mutation_wide", MAX_RECORDS).await;
    let model = tx.env().model::<fixture::Wide>().unwrap();
    let all = model.browse(ids.clone()).unwrap();
    let mut vals = RawVals::new();
    for k in 1..=10 {
        vals = vals.set_cell(format!("in{k}"), Cell::Decimal(dec("2.5")));
    }
    all.as_records().write(&mut tx, vals).await.unwrap();
    let last = model.browse([ids[MAX_RECORDS - 1]]).unwrap();
    assert_eq!(
        last.mapped(&mut tx, fixture::Wide::IN10).await.unwrap(),
        vec![dec("2.5")]
    );
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

/// width: 190,000 rows supplying one of eleven monetary fields are admitted)
#[tokio::test]
async fn create_counts_supplied_cells_not_model_width() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let cents = currency(&mut tx, "CNT", "0.01").await;
    let model = tx.env().model::<fixture::Wide>().unwrap();
    let rows = 190_000;
    let vals: Vec<Vals<fixture::Wide>> = (0..rows)
        .map(|_| {
            Vals::new()
                .set(fixture::Wide::SELECTED_CURRENCY_ID, cents)
                .set(fixture::Wide::AMOUNT, dec("1.005"))
        })
        .collect();
    let created = model.create(&mut tx, vals).await.unwrap();
    assert_eq!(created.len(), rows);
    let last = model.browse([created.rec_ids()[rows - 1].get()]).unwrap();
    assert_eq!(
        last.mapped(&mut tx, fixture::Wide::AMOUNT).await.unwrap(),
        vec![dec("1.01")]
    );
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

/// is refused; exactly the bound is admitted)
#[tokio::test]
async fn default_fill_is_bounded_by_the_record_bound() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let model = tx.env().model::<fixture::Bulk>().unwrap();
    let mut new_ids = Vec::with_capacity(MAX_RECORDS + 1);
    for _ in 0..=MAX_RECORDS {
        let record = model.new_record(&mut tx, Vals::new()).await.unwrap();
        new_ids.push(record.rec_ids()[0]);
    }
    let records = tx.env().records("test_monetary_mutation.bulk").unwrap();
    let all = records.browse_ids(new_ids.clone());
    let error = all.values(&mut tx, "amount").await.unwrap_err();
    records_refusal(&error, MAX_RECORDS + 1);
    let exact = records.browse_ids(new_ids[..MAX_RECORDS].to_vec());
    let values = exact.values(&mut tx, "amount").await.unwrap();
    assert_eq!(values.len(), MAX_RECORDS);
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

/// and no cached value; a failing write publishes nothing)
#[tokio::test]
async fn failing_chunk_publishes_nothing() {
    let h = Harness::builder([
        &base_users::ADDON,
        &erp_addon_base::ADDON,
        &fixture::ADDON,
        &failing_provider::ADDON,
    ])
    .config(erp_orm::OrmConfig {
        recompute_batch: 1,
        ..Default::default()
    })
    .build()
    .await
    .unwrap();
    let mut tx = h.begin().await.unwrap();
    let cent = currency(&mut tx, "CNT", "0.01").await;
    let model = tx.env().model::<fixture::Bulk>().unwrap();
    let kept = model
        .create(
            &mut tx,
            vec![
                Vals::new()
                    .set(fixture::Bulk::CURRENCY_ID, cent)
                    .set(fixture::Bulk::AMOUNT, dec("1.005")),
            ],
        )
        .await
        .unwrap();
    tx.flush().await.unwrap();
    let rows = table_state(&mut tx, "test_monetary_mutation_bulk").await;
    let sequence = sequence_state(&mut tx, "test_monetary_mutation_bulk").await;
    let kept_amount = kept.mapped(&mut tx, fixture::Bulk::AMOUNT).await.unwrap();
    let error = model
        .create(
            &mut tx,
            ["1.005", "666", "2.005"]
                .into_iter()
                .map(|amount| {
                    Vals::new()
                        .set(fixture::Bulk::CURRENCY_ID, cent)
                        .set(fixture::Bulk::AMOUNT, dec(amount))
                })
                .collect(),
        )
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Database, "{error}");
    assert!(
        error
            .to_string()
            .contains("controlled monetary chunk failure")
    );
    assert_eq!(
        table_state(&mut tx, "test_monetary_mutation_bulk").await,
        rows
    );
    assert_eq!(
        sequence_state(&mut tx, "test_monetary_mutation_bulk").await,
        sequence
    );
    assert_eq!(
        kept.mapped(&mut tx, fixture::Bulk::AMOUNT).await.unwrap(),
        kept_amount
    );
    // a failing write keeps the earlier values
    let pair = model
        .create(
            &mut tx,
            ["1.005", "2.005"]
                .into_iter()
                .map(|amount| {
                    Vals::new()
                        .set(fixture::Bulk::CURRENCY_ID, cent)
                        .set(fixture::Bulk::AMOUNT, dec(amount))
                })
                .collect(),
        )
        .await
        .unwrap();
    let before = pair.mapped(&mut tx, fixture::Bulk::AMOUNT).await.unwrap();
    let error = pair
        .as_records()
        .write(
            &mut tx,
            RawVals::new().set_cell("amount", Cell::Decimal(dec("666"))),
        )
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Database, "{error}");
    assert_eq!(
        pair.mapped(&mut tx, fixture::Bulk::AMOUNT).await.unwrap(),
        before
    );
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

/// compact buffers; the refusal publishes nothing, a single currency plus none is admitted)
#[tokio::test]
async fn mixed_currency_admission_spans_chunks_on_compact_buffers() {
    let h = Harness::builder([&base_users::ADDON, &erp_addon_base::ADDON, &fixture::ADDON])
        .config(erp_orm::OrmConfig {
            recompute_batch: 1,
            ..Default::default()
        })
        .build()
        .await
        .unwrap();
    let mut tx = h.begin().await.unwrap();
    let cent = currency(&mut tx, "CNT", "0.01").await;
    let unit = currency(&mut tx, "UNT", "1").await;
    let model = tx.env().model::<fixture::Bulk>().unwrap();
    let rows = model
        .create(
            &mut tx,
            vec![
                Vals::new().set(fixture::Bulk::CURRENCY_ID, cent),
                Vals::new().set(fixture::Bulk::CURRENCY_ID, cent),
                Vals::new().set(fixture::Bulk::CURRENCY_ID, unit),
                Vals::new(),
            ],
        )
        .await
        .unwrap();
    tx.flush().await.unwrap();
    let before = table_state(&mut tx, "test_monetary_mutation_bulk").await;
    // the third row's currency differs from the first two: one chunk per row, one decision
    let error = rows
        .as_records()
        .write(
            &mut tx,
            RawVals::new().set_cell("amount", Cell::Decimal(dec("1.005"))),
        )
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Validation, "{error}");
    assert_eq!(
        error.message(),
        "Got multiple currencies while assigning values of monetary field test_monetary_mutation.bulk.amount"
    );
    tx.flush().await.unwrap();
    assert_eq!(
        table_state(&mut tx, "test_monetary_mutation_bulk").await,
        before
    );
    // a subset with one currency (and a currency-less row) is admitted and rounded to it
    let ok =
        rows.as_records()
            .browse_ids([rows.rec_ids()[0], rows.rec_ids()[1], rows.rec_ids()[3]]);
    ok.write(
        &mut tx,
        RawVals::new().set_cell("amount", Cell::Decimal(dec("1.005"))),
    )
    .await
    .unwrap();
    assert_eq!(
        ok.values(&mut tx, "amount").await.unwrap(),
        vec![Value::Decimal(dec("1.01")); 3]
    );
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

/// computed currency keeps it pending across several precompute rounds; 200,000 rows x 10 supplied
/// amounts are exactly the cell bound and are admitted)
#[tokio::test]
async fn create_charges_supplied_cells_once_across_pending_rounds() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let cents = currency(&mut tx, "CNT", "0.01").await;
    let model = tx.env().model::<fixture::Multi>().unwrap();
    let rows = MAX_RECORDS;
    let vals: Vec<Vals<fixture::Multi>> = (0..rows)
        .map(|_| {
            let mut vals = Vals::new().set(fixture::Multi::SELECTED_CURRENCY_ID, cents);
            for field in [
                fixture::Multi::A1,
                fixture::Multi::A2,
                fixture::Multi::A3,
                fixture::Multi::A4,
                fixture::Multi::A5,
                fixture::Multi::A6,
                fixture::Multi::A7,
                fixture::Multi::A8,
                fixture::Multi::A9,
                fixture::Multi::A10,
            ] {
                vals = vals.set(field, dec("1.005"));
            }
            vals
        })
        .collect();
    let created = model.create(&mut tx, vals).await.unwrap();
    assert_eq!(created.len(), rows);
    let last = model.browse([created.rec_ids()[rows - 1].get()]).unwrap();
    assert_eq!(
        last.mapped(&mut tx, fixture::Multi::A10).await.unwrap(),
        vec![dec("1.01")]
    );
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

/// any row is inserted)
#[tokio::test]
async fn create_one_cell_over_the_bound_is_refused_without_rows() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let cents = currency(&mut tx, "CNT", "0.01").await;
    let model = tx.env().model::<fixture::Multi>().unwrap();
    let ten = |vals: Vals<fixture::Multi>| {
        [
            fixture::Multi::A1,
            fixture::Multi::A2,
            fixture::Multi::A3,
            fixture::Multi::A4,
            fixture::Multi::A5,
            fixture::Multi::A6,
            fixture::Multi::A7,
            fixture::Multi::A8,
            fixture::Multi::A9,
            fixture::Multi::A10,
        ]
        .into_iter()
        .fold(vals, |vals, field| vals.set(field, dec("1.005")))
    };
    let mut vals: Vec<Vals<fixture::Multi>> = (0..MAX_RECORDS - 1)
        .map(|_| ten(Vals::new().set(fixture::Multi::SELECTED_CURRENCY_ID, cents)))
        .collect();
    // 199,999 x 10 + 11 = 2,000,001 cells on 200,000 records
    vals.push(
        ten(Vals::new().set(fixture::Multi::SELECTED_CURRENCY_ID, cents))
            .set(fixture::Multi::A11, dec("1.005")),
    );
    let error = model.create(&mut tx, vals).await.unwrap_err();
    cells_refusal(&error, MAX_CELLS + 1);
    let count: i64 = tx
        .query("SELECT count(*) FROM \"test_monetary_mutation_multi\"", &[])
        .await
        .unwrap()
        .remove(0)
        .get(0);
    assert_eq!(count, 0);
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

/// projection paths, charge each projected input once per record-field: 8 direct + 3 projected
/// cells per record are admitted; charged per amount and per path they would exceed the bound)
#[tokio::test]
async fn projection_charges_each_record_field_once() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let ids = insert_rows(&mut tx, "test_monetary_mutation_proj_reads", MAX_RECORDS).await;
    let model = tx.env().model::<fixture::ProjReads>().unwrap();
    let all = model.browse(ids.clone()).unwrap();
    let mut vals = RawVals::new()
        .set_cell("trig", Cell::Decimal(dec("5")))
        .set_cell("amount", Cell::Decimal(dec("1.005")))
        .set_cell("amount2", Cell::Decimal(dec("2.005")));
    for k in 1..=3 {
        vals = vals.set_cell(format!("in{k}"), Cell::Decimal(dec("1.005")));
    }
    all.as_records().write(&mut tx, vals).await.unwrap();
    let last = model.browse([ids[MAX_RECORDS - 1]]).unwrap();
    assert_eq!(
        last.mapped(&mut tx, fixture::ProjReads::AMOUNT2)
            .await
            .unwrap(),
        vec![dec("2.005")]
    );
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

/// the projection stages: 1,600,000 direct cells pass every pre-check, the 1,400,000 read inputs
/// of the currency compute push the write over the cell bound)
#[tokio::test]
async fn charge_inside_the_projection_bounds_read_inputs() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let ids = insert_rows(&mut tx, "test_monetary_mutation_proj_reads", MAX_RECORDS).await;
    let model = tx.env().model::<fixture::ProjReads>().unwrap();
    let all = model.browse(ids).unwrap();
    let mut vals = RawVals::new()
        .set_cell("trig", Cell::Decimal(dec("5")))
        .set_cell("amount", Cell::Decimal(dec("1.005")));
    for k in 1..=7 {
        vals = vals.set_cell(format!("in{k}"), Cell::Decimal(dec("1.005")));
    }
    let error = all.as_records().write(&mut tx, vals).await.unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Validation, "{error}");
    assert!(
        error.to_string().contains("cells limit (2000000) exceeded"),
        "{error}"
    );
    tx.flush().await.unwrap();
    assert_eq!(
        table_state(&mut tx, "test_monetary_mutation_proj_reads")
            .await
            .1,
        0
    );
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

/// built even when it supplies no monetary value at all)
#[tokio::test]
async fn create_over_record_bound_without_supplied_cells_is_refused() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let cents = currency(&mut tx, "CNT", "0.01").await;
    let model = tx.env().model::<fixture::Multi>().unwrap();
    let vals: Vec<Vals<fixture::Multi>> = (0..MAX_RECORDS + 1)
        .map(|_| Vals::new().set(fixture::Multi::SELECTED_CURRENCY_ID, cents))
        .collect();
    let error = model.create(&mut tx, vals).await.unwrap_err();
    records_refusal(&error, MAX_RECORDS + 1);
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}
