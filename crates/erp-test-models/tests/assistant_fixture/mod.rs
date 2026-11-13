// SPDX-License-Identifier: MIT OR Apache-2.0
//! Fixture addons of the assistant opt-in and tool schemas (AI1.REG-SCHEMA, (a), D6;
//! that do and do not opt in; `test_assistant_override` overrides one opted-in method without the
//! opt-in (withdrawn) and one with it (kept). `test_assistant_open` opts in a method with an open
//! parameter, `test_assistant_widen` widens an opted-in method to `external` and
//! `test_assistant_hide` hides one: all three must fail the ORM build. Only installed addons
//! count; the fixture is linked into `tests/assistant_tool_schema.rs` only.

use erp_sdk::Domain;
use erp_sdk::prelude::*;

erp_sdk::addon!(name = "test_assistant", depends = ["test_orm"]);

/// A task: one field per mapping rule of the `values` schema (design note R6).
#[model(
    name = "test_assistant.task",
    description = "Assistant Task",
    addon = "test_assistant"
)]
pub struct Task {
    #[field(
        char,
        size = 64,
        required,
        string = "Title",
        help = "Short title of the task"
    )]
    pub name: Option<Arc<str>>,
    #[field(char, required, string = "Code")]
    pub code: Option<Arc<str>>,
    #[field(text)]
    pub note: Option<Arc<str>>,
    #[field(selection, selection = [("2", "High"), ("0", "Low"), ("1", "Normal")])]
    pub priority: Option<Arc<str>>,
    #[field(boolean, default = true)]
    pub open: bool,
    #[field(integer)]
    pub estimate: i32,
    #[field(float)]
    pub weight: f64,
    #[field(date)]
    pub deadline: Option<Date>,
    #[field(many2one, comodel = "test_assistant.task")]
    pub parent_id: Option<RecId>,
    #[field(one2many, comodel = "test_assistant.task", inverse_name = "parent_id")]
    pub child_ids: Vec<RecId>,
    /// A credential: never in a schema.
    #[field(char, secret = true)]
    pub token: Option<Arc<str>>,
    /// Computed without inverse: not writable.
    #[field(char, compute = "_compute_upper")]
    pub upper: Option<Arc<str>>,
}

#[methods(Task, addon = "test_assistant")]
impl Task {
    #[compute(depends = ["name"])]
    fn _compute_upper(batch: &mut Batch<'_, Task>) -> Result<()> {
        for i in batch.indices() {
            let name = batch.get(i, Task::NAME)?;
            batch.set(
                i,
                Task::UPPER,
                name.map(|c| Arc::<str>::from(c.to_uppercase())),
            )?;
        }
        Ok(())
    }

    /// Offered: one parameter per closed shape.
    #[method]
    #[rpc(effect = write, assistant = offer)]
    async fn plan(
        _this: &RecordSet<Task>,
        _tx: &mut Transaction,
        domain: Domain,
        labels: Vec<String>,
        limit: Option<u64>,
        weight: f64,
        urgent: bool,
    ) -> Result<bool> {
        drop((domain, labels, limit, weight, urgent));
        Ok(true)
    }

    /// Offered here; `test_assistant_override` overrides it without the opt-in.
    #[method]
    #[rpc(effect = write, assistant = offer)]
    async fn set_priority(
        _this: &RecordSet<Task>,
        _tx: &mut Transaction,
        priority: String,
        note: Option<String>,
        #[arg(default = 1)] rank: i64,
    ) -> Result<bool> {
        drop((priority, note, rank));
        Ok(true)
    }

    /// Closed parameters, no opt-in: never offered.
    #[method]
    #[rpc(effect = write)]
    async fn rename(_this: &RecordSet<Task>, _tx: &mut Transaction, name: String) -> Result<bool> {
        drop(name);
        Ok(true)
    }

    /// Offered model method without parameters.
    #[method]
    #[rpc(effect = destructive, model, assistant = offer)]
    async fn purge_closed(_this: &RecordSet<Task>, _tx: &mut Transaction) -> Result<bool> {
        Ok(true)
    }
}

/// Overrides of `test_assistant.task` methods: one without the opt-in, one with it.
pub mod overrides {
    use erp_sdk::prelude::*;

    use super::Task;

    erp_sdk::addon!(
        name = "test_assistant_override",
        depends = ["test_assistant"]
    );

    /// Extension of `test_assistant.task`.
    #[extend(Task, addon = "test_assistant_override")]
    pub struct TaskOverride {}

    #[methods(Task, addon = "test_assistant_override")]
    impl TaskOverride {
        /// Adds behaviour without offering it: the offer is withdrawn.
        #[method(override)]
        async fn set_priority(
            this: &RecordSet<Task>,
            tx: &mut Transaction,
            priority: String,
            note: Option<String>,
            rank: i64,
            sup: Super,
        ) -> Result<bool> {
            sup.call(tx, this.as_records(), (priority, note, rank))
                .await
        }

        /// Opts in again: the offer stays.
        #[method(override)]
        #[rpc(effect = destructive, model, assistant = offer)]
        async fn purge_closed(
            this: &RecordSet<Task>,
            tx: &mut Transaction,
            sup: Super,
        ) -> Result<bool> {
            sup.call(tx, this.as_records(), ()).await
        }
    }
}

/// An opt-in on a method with an open parameter: the ORM build fails.
pub mod open {
    use erp_sdk::Value;
    use erp_sdk::prelude::*;

    use super::Task;

    erp_sdk::addon!(name = "test_assistant_open", depends = ["test_assistant"]);

    /// Extension of `test_assistant.task`.
    #[extend(Task, addon = "test_assistant_open")]
    pub struct TaskOpen {}

    #[methods(Task, addon = "test_assistant_open")]
    impl TaskOpen {
        /// `Value` has no closed schema.
        #[method]
        #[rpc(effect = write, assistant = offer)]
        async fn stash(
            _this: &RecordSet<Task>,
            _tx: &mut Transaction,
            label: String,
            payload: Value,
        ) -> Result<bool> {
            drop((label, payload));
            Ok(true)
        }
    }
}

/// An override widening an opted-in method to `external`: the ORM build fails.
pub mod widen {
    use erp_sdk::prelude::*;

    use super::Task;

    erp_sdk::addon!(name = "test_assistant_widen", depends = ["test_assistant"]);

    /// Extension of `test_assistant.task`.
    #[extend(Task, addon = "test_assistant_widen")]
    pub struct TaskWiden {}

    #[methods(Task, addon = "test_assistant_widen")]
    impl TaskWiden {
        /// Widened to `external`.
        #[method(override)]
        #[rpc(effect = external)]
        async fn set_priority(
            this: &RecordSet<Task>,
            tx: &mut Transaction,
            priority: String,
            note: Option<String>,
            rank: i64,
            sup: Super,
        ) -> Result<bool> {
            sup.call(tx, this.as_records(), (priority, note, rank))
                .await
        }
    }
}

/// An override hiding an opted-in method: the offer names a method that is not exposed, so the
/// ORM build fails.
pub mod hide {
    use erp_sdk::prelude::*;

    use super::Task;

    erp_sdk::addon!(name = "test_assistant_hide", depends = ["test_assistant"]);

    /// Extension of `test_assistant.task`.
    #[extend(Task, addon = "test_assistant_hide")]
    pub struct TaskHide {}

    #[methods(Task, addon = "test_assistant_hide")]
    impl TaskHide {
        /// Never callable over RPC.
        #[method(override)]
        #[rpc(hide)]
        async fn purge_closed(
            this: &RecordSet<Task>,
            tx: &mut Transaction,
            sup: Super,
        ) -> Result<bool> {
            sup.call(tx, this.as_records(), ()).await
        }
    }
}
