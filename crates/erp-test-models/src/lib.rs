//! Each module is one addon; tests choose the addons of their registry.
//! Never part of a production addon set.

pub mod bench;
pub mod test_compute;
pub mod test_convert;
pub mod test_convert_ui;
pub mod test_defaults;
pub mod test_exact_numeric;
pub mod test_impex;
pub mod test_partition;
pub mod test_partition_large;
pub mod test_scale_excl;
pub mod test_scale_upsert;

pub mod base_users {
    use erp_sdk::prelude::*;

    erp_sdk::addon!(name = "base_users");

    /// Adds the timezone used by environments (M3 reads it).
    #[extend(ResUsers, addon = "base_users")]
    pub struct UsersTz {
        #[field(char)]
        pub tz: Option<Arc<str>>,
    }
}

pub mod test_orm {
    use erp_sdk::prelude::*;

    erp_sdk::addon!(name = "test_orm");

    /// `test_orm.mixed`: one field per scalar type.
    #[model(
        name = "test_orm.mixed",
        description = "Test ORM Mixed",
        addon = "test_orm"
    )]
    pub struct Mixed {
        #[field(boolean)]
        pub boolean: bool,
        #[field(json)]
        pub json: Option<erp_sdk::serde_json::Value>,
        #[field(integer)]
        pub integer: i32,
        #[field(float)]
        pub float: f64,
        #[field(float, digits = 0)]
        pub numeric: Decimal,
        #[field(float, digits = (0, 2))]
        pub number: Decimal,
        #[field(float, default = 1.25)]
        pub float_default: f64,
        #[field(monetary, currency_field = "currency_id")]
        pub monetary: Decimal,
        #[field(many2one, comodel = "res.currency")]
        pub currency_id: Option<RecId>,
        #[field(selection, selection = [("option_1", "Option 1")])]
        pub selection: Option<Arc<str>>,
        #[field(selection, selection_method = "_get_selection")]
        pub selection_str: Option<Arc<str>>,
        #[field(date)]
        pub date: Option<Date>,
        #[field(datetime)]
        pub moment: Option<DateTime>,
        #[field(char)]
        pub char: Option<Arc<str>>,
        #[field(html)]
        pub html: Option<Arc<str>>,
        #[field(text)]
        pub text: Option<Arc<str>>,
    }

    // Complete the callable-selection fixture now that metadata refuses absent providers.
    #[methods(Mixed, addon = "test_orm")]
    impl Mixed {
        #[selection]
        async fn _get_selection(
            _this: &RecordSet<Mixed>,
            ctx: &mut erp_sdk::SelectionContext<'_>,
        ) -> Result<()> {
            ctx.emit("option_1", "Option 1")
        }
    }

    /// `test_orm.partner` (S1 subset: no relational fields yet).
    #[model(
        name = "test_orm.partner",
        description = "Test ORM Partner",
        addon = "test_orm"
    )]
    pub struct Partner {
        #[field(char)]
        pub name: Option<Arc<str>>,
        #[field(char)]
        pub email: Option<Arc<str>>,
        #[field(char)]
        pub website: Option<Arc<str>>,
        #[field(boolean, default = true)]
        pub active: bool,
        #[field(integer)]
        pub number: i32,
    }

    /// `test_orm.country`.
    #[model(
        name = "test_orm.country",
        description = "Country",
        order = "name, id",
        addon = "test_orm"
    )]
    pub struct Country {
        #[field(char)]
        pub name: Option<Arc<str>>,
        #[field(char, string = "Country Code")]
        pub code: Option<Arc<str>>,
    }

    /// `test_orm.selection`.
    #[model(
        name = "test_orm.selection",
        description = "Selection",
        addon = "test_orm"
    )]
    pub struct Selection {
        #[field(selection, selection = [("foo", "Foo"), ("bar", "Bar")])]
        pub state: Option<Arc<str>>,
    }

    /// `test_orm.category` with a CHECK constraint.
    #[model(
        name = "test_orm.category",
        description = "Test ORM Category",
        addon = "test_orm",
        constraints = [("_positive_color", "CHECK(color >= 0)", "The color code must be positive!")]
    )]
    pub struct Category {
        #[field(char)]
        pub name: Option<Arc<str>>,
        #[field(integer)]
        pub color: i32,
        #[field(char, store = false)]
        pub dummy: Option<Arc<str>>,
    }

    /// `test_orm.schema_table`.
    #[model(
        name = "test_orm.schema_table",
        description = "Test ORM Schema Table",
        addon = "test_orm"
    )]
    pub struct SchemaTable {
        #[field(boolean)]
        pub boolean: bool,
        #[field(char)]
        pub char: Option<Arc<str>>,
        #[field(integer)]
        pub integer: i32,
        #[field(float)]
        pub float: f64,
    }

    /// `test_orm.schema` (/041).
    #[model(
        name = "test_orm.schema",
        description = "Test ORM Schema",
        addon = "test_orm"
    )]
    pub struct Schema {
        #[field(char, required)]
        pub required: Option<Arc<str>>,
        #[field(char, size = 3)]
        pub size: Option<Arc<str>>,
        #[field(char, index = "btree")]
        pub index_btree: Option<Arc<str>>,
        #[field(char, index = "btree_not_null")]
        pub index_btree_not_null: Option<Arc<str>>,
        #[field(char, index = "trigram")]
        pub index_trigram: Option<Arc<str>>,
        #[field(integer, index)]
        pub very_very_very_very_very_long_field_name_1: i32,
        #[field(integer, index)]
        pub very_very_very_very_very_long_field_name_2: i32,
    }

    /// `test_orm.schema_index`: a second table with a `btree` field.
    #[model(
        name = "test_orm.schema_index",
        description = "Test Schema Index",
        addon = "test_orm"
    )]
    pub struct SchemaIndex {
        #[field(char, index)]
        pub btree: Option<Arc<str>>,
    }

    /// A model without `name` (default `display_name` is `model,id`).
    #[model(name = "test_orm.noname", description = "No Name", addon = "test_orm")]
    pub struct NoName {
        #[field(char)]
        pub code: Option<Arc<str>>,
        /// A non-stored computed field: `code` in upper case.
        #[field(char, compute = "_compute_upper")]
        pub upper: Option<Arc<str>>,
    }

    #[methods(NoName, addon = "test_orm")]
    impl NoName {
        #[compute(depends = ["code"])]
        fn _compute_upper(batch: &mut Batch<'_, NoName>) -> Result<()> {
            for i in batch.indices() {
                let code = batch.get(i, NoName::CODE)?;
                batch.set(
                    i,
                    NoName::UPPER,
                    code.map(|c| Arc::<str>::from(c.to_uppercase())),
                )?;
            }
            Ok(())
        }
    }

    /// A model whose `_rec_name` is a datetime (display in the environment's timezone).
    #[model(
        name = "test_orm.moment",
        description = "Moment",
        rec_name = "moment",
        addon = "test_orm"
    )]
    pub struct Moment {
        #[field(datetime)]
        pub moment: Option<DateTime>,
    }

    /// A model whose `write` stores `priority + 1`.
    #[model(
        name = "test_orm.write_override",
        description = "Write Override",
        addon = "test_orm"
    )]
    pub struct WriteOverride {
        #[field(integer)]
        pub priority: i32,
        #[field(char)]
        pub body: Option<Arc<str>>,
        #[field(boolean, default = true)]
        pub active: bool,
        /// A JSON document, so `web_save_multi`'s grouping of `Value::Json` entries is observable
        /// through the recorded batch sizes.
        #[field(json)]
        pub meta: Option<erp_sdk::serde_json::Value>,
    }

    /// Batch sizes recorded by `WriteOverride::write`.
    pub static WRITE_BATCH_SIZES: std::sync::Mutex<Vec<usize>> = std::sync::Mutex::new(Vec::new());

    #[methods(WriteOverride, addon = "test_orm")]
    impl WriteOverride {
        #[method(override)]
        async fn write(
            this: &RecordSet<WriteOverride>,
            tx: &mut Transaction,
            mut vals: RawVals,
            sup: Super,
        ) -> Result<()> {
            if let Ok(mut lock) = WRITE_BATCH_SIZES.lock() {
                lock.push(this.as_records().len());
            }
            if let Some(Cell::Int(p)) = vals.get("priority").cloned() {
                vals.insert("priority".into(), Cell::Int(p + 1));
            }
            sup.call(tx, this.as_records(), (vals,)).await
        }
    }

    /// `test_rpc.model_b` (/042).
    #[model(
        name = "test_rpc.model_b",
        description = "Model B",
        addon = "test_orm",
        constraints = [("_qty_positive", "check (value > 0)", "The value must be positive")]
    )]
    pub struct ModelB {
        #[field(char, required)]
        pub name: Option<Arc<str>>,
        #[field(integer)]
        pub value: i32,
    }
}

pub mod test_access;
pub mod test_auth;
pub mod test_company;
pub mod test_company_access;
pub mod test_company_emit;
pub mod test_company_escape;
pub mod test_company_related;
pub mod test_cooldown;
pub mod test_depends;
pub mod test_field_access;
pub mod test_jobs_fixtures;
pub mod test_login_selection;
pub mod test_mixed_constraint;
pub mod test_numbering;
pub mod test_o2m_order;
pub mod test_read_group;
pub mod test_rel;
pub mod test_search;
pub mod test_sudo_commands;
pub mod test_transient_policy;
pub mod test_vacuum;
pub mod test_web;

pub mod test_large;
pub mod test_rpc;
pub mod test_rset;
pub mod test_scale_index;
pub mod test_token;
pub mod test_token_more;
pub mod test_tracking;

pub mod test_inherit_a {
    use erp_sdk::prelude::*;

    erp_sdk::addon!(name = "test_inherit_a");

    /// Model `test.inherit.a` defined by addon A.
    #[model(
        name = "test.inherit.a",
        description = "Inherit A",
        addon = "test_inherit_a"
    )]
    pub struct A {
        #[field(char)]
        pub name: Option<Arc<str>>,
    }

    #[methods(A, addon = "test_inherit_a")]
    impl A {
        /// `foo()` returns `self.bar()`; `bar` is defined by addon B, which A does not know.
        #[method]
        async fn foo(this: &RecordSet<A>, tx: &mut Transaction) -> Result<i64> {
            this.call::<(), i64>(tx, "bar", ()).await
        }
    }
}

/// Addon B extends `test.inherit.a` with `bar()` returning 42.
pub mod test_inherit_b {
    use erp_sdk::prelude::*;

    use crate::test_inherit_a::A;

    erp_sdk::addon!(name = "test_inherit_b", depends = ["test_inherit_a"]);

    /// B's extension.
    #[extend(A, addon = "test_inherit_b")]
    pub struct AByB {
        #[field(char)]
        pub field_b: Option<Arc<str>>,
    }

    #[methods(A, addon = "test_inherit_b")]
    impl AByB {
        #[method]
        async fn bar(_this: &RecordSet<A>, _tx: &mut Transaction) -> Result<i64> {
            Ok(42)
        }
    }
}

/// Addon C overrides `bar()` as `super().bar() * 2`.
pub mod test_inherit_c {
    use erp_sdk::prelude::*;

    use crate::test_inherit_a::A;

    erp_sdk::addon!(name = "test_inherit_c", depends = ["test_inherit_b"]);

    /// C's extension.
    #[extend(A, addon = "test_inherit_c")]
    pub struct AByC {
        #[field(char)]
        pub field_c: Option<Arc<str>>,
    }

    #[methods(A, addon = "test_inherit_c")]
    impl AByC {
        #[method(override)]
        async fn bar(this: &RecordSet<A>, tx: &mut Transaction, sup: Super) -> Result<i64> {
            Ok(sup.call(tx, this.as_records(), ()).await? * 2)
        }
    }
}

/// Prototype inheritance seeing later parent extensions.
pub mod test_inherit_parent {
    use erp_sdk::prelude::*;

    erp_sdk::addon!(name = "test_inherit_parent");

    /// Abstract parent.
    #[model(
        name = "test_inherit_parent",
        description = "Parent",
        kind = "abstract",
        addon = "test_inherit_parent"
    )]
    pub struct Parent {}

    #[methods(Parent, addon = "test_inherit_parent")]
    impl Parent {
        #[method]
        async fn stuff(_this: &RecordSet<Parent>, _tx: &mut Transaction) -> Result<String> {
            Ok("P1".to_owned())
        }
    }

    /// Child created from the parent.
    #[model(
        name = "test_inherit_child",
        description = "Child",
        inherit = [Parent],
        addon = "test_inherit_parent"
    )]
    pub struct Child {
        #[field(integer)]
        pub bar: i32,
    }

    #[methods(Child, addon = "test_inherit_parent")]
    impl Child {
        #[method(override)]
        async fn stuff(
            this: &RecordSet<Child>,
            tx: &mut Transaction,
            sup: Super,
        ) -> Result<String> {
            Ok(sup.call(tx, this.as_records(), ()).await? + "C1")
        }
    }
}

/// A later extension of the parent.
pub mod test_inherit_parent_ext {
    use erp_sdk::prelude::*;

    use crate::test_inherit_parent::Parent;

    erp_sdk::addon!(
        name = "test_inherit_parent_ext",
        depends = ["test_inherit_parent"]
    );

    /// Adds `foo`, a unique constraint and a `stuff` override to the parent.
    #[extend(
        Parent,
        addon = "test_inherit_parent_ext",
        constraints = [("_unique_foo", "UNIQUE(foo)", "foo must be unique")]
    )]
    pub struct ParentExt {
        #[field(integer)]
        pub foo: i32,
    }

    #[methods(Parent, addon = "test_inherit_parent_ext")]
    impl ParentExt {
        #[method(override)]
        async fn stuff(
            this: &RecordSet<Parent>,
            tx: &mut Transaction,
            sup: Super,
        ) -> Result<String> {
            Ok(sup.call(tx, this.as_records(), ()).await? + "P2")
        }
    }
}

/// Extending `base` reaches every model.
pub mod test_base_ext {
    use erp_sdk::prelude::*;

    erp_sdk::addon!(name = "test_base_ext");

    /// Adds a field and a method to every model.
    #[extend(Base, addon = "test_base_ext")]
    pub struct BaseExt {
        #[field(char)]
        pub note_like: Option<Arc<str>>,
    }

    #[methods(Base, addon = "test_base_ext")]
    impl BaseExt {
        #[method]
        async fn hello(this: &RecordSet<Base>, _tx: &mut Transaction) -> Result<String> {
            Ok(format!("hello from {}", this.model_name()))
        }
    }
}

/// Two addons for the link-time registration guard test.
pub mod guard_a {
    use erp_sdk::prelude::*;

    erp_sdk::addon!(name = "guard_a");

    /// First guard model.
    #[model(name = "guard.one", addon = "guard_a")]
    pub struct One {
        #[field(integer)]
        pub n: i32,
    }

    /// Second guard model.
    #[model(name = "guard.two", addon = "guard_a")]
    pub struct Two {}

    #[methods(One, addon = "guard_a")]
    impl One {
        #[method]
        async fn ping(_this: &RecordSet<One>, _tx: &mut Transaction) -> Result<i64> {
            Ok(1)
        }
    }
}

/// Second addon of the guard test.
pub mod guard_b {
    use erp_sdk::prelude::*;

    use crate::guard_a::One;

    erp_sdk::addon!(name = "guard_b", depends = ["guard_a"]);

    /// Extension of `guard.one`.
    #[extend(One, addon = "guard_b")]
    pub struct OneExt {}

    #[methods(One, addon = "guard_b")]
    impl OneExt {
        #[method(override)]
        async fn ping(this: &RecordSet<One>, tx: &mut Transaction, sup: Super) -> Result<i64> {
            Ok(sup.call(tx, this.as_records(), ()).await? + 1)
        }

        #[method]
        async fn pong(_this: &RecordSet<One>, _tx: &mut Transaction, x: i64) -> Result<i64> {
            Ok(x)
        }
    }
}

/// The addon guide's Rust examples, compiled and run as doctests (`cargo test --doc`), so the
/// guide cannot drift from the API.
#[cfg(doctest)]
mod addon_guide_models {}

/// `init` and `_register_hook` overridden with the SDK macros (/044).
pub mod test_model_hooks {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use erp_sdk::prelude::*;

    erp_sdk::addon!(name = "test_model_hooks");

    /// Times `_register_hook` ran in this process.
    pub static REGISTERED: AtomicUsize = AtomicUsize::new(0);

    /// `test.model_hooks`: creates an SQL view in `init()`.
    #[model(
        name = "test.model_hooks",
        description = "Model hooks",
        addon = "test_model_hooks"
    )]
    pub struct Hooked {
        #[field(char)]
        pub name: Option<Arc<str>>,
    }

    #[methods(Hooked, addon = "test_model_hooks")]
    impl Hooked {
        #[method(override)]
        async fn init(this: &RecordSet<Hooked>, tx: &mut Transaction, sup: Super) -> Result<()> {
            tx.batch(
                "CREATE OR REPLACE VIEW test_model_hooks_names AS \
                 SELECT id, upper(name) AS name FROM test_model_hooks",
            )
            .await?;
            sup.call(tx, this.as_records(), ()).await
        }

        #[method(override)]
        async fn _register_hook(
            this: &RecordSet<Hooked>,
            tx: &mut Transaction,
            sup: Super,
        ) -> Result<()> {
            REGISTERED.fetch_add(1, Ordering::SeqCst);
            sup.call(tx, this.as_records(), ()).await
        }
    }
}

/// The addon guide's index kinds page, compiled and run as doctests like the above.
#[cfg(doctest)]
mod addon_guide_index_kinds {}

/// The addon guide's "Keys, upsert and row versions" examples.
#[cfg(doctest)]
mod addon_guide_keys_upsert_versions {}

/// The addon guide's chapter on exclusion constraints and range fields.
#[cfg(doctest)]
mod addon_guide_exclusions {}

/// The addon guide's action and menu examples, compiled and run as doctests.
#[cfg(doctest)]
mod addon_guide_actions_menus {}

/// The addon guide's audit trail examples, compiled and run as doctests.
#[cfg(doctest)]
mod addon_guide_audit {}

/// The addon guide's capability examples, compiled and run as doctests.
#[cfg(doctest)]
mod addon_guide_capabilities {}

/// The jobs guide's Rust examples.
#[cfg(doctest)]
mod addon_guide_jobs {}

/// The addon guide's sequence examples, compiled and run as doctests.
#[cfg(doctest)]
mod addon_guide_sequences {}

/// The addon guide's field access examples, compiled and run as doctests.
#[cfg(doctest)]
mod addon_guide_field_access {}

/// The addon guide's RPC examples, compiled and run as doctests.
#[cfg(doctest)]
mod addon_guide_rpc {}

/// The addon guide's `<function>` / `<delete>` / CSV examples, compiled and run as
/// doctests.
#[cfg(doctest)]
mod addon_guide_data_functions {}

/// The addon guide's migration examples, compiled and run as doctests.
#[cfg(doctest)]
mod addon_guide_migrations {}

/// The addon guide's query AST examples, compiled and run as doctests.
#[cfg(doctest)]
mod addon_guide_query_ast {}

/// The addon guide's record-access examples, compiled and run as doctests.
#[cfg(doctest)]
mod addon_guide_access {}

/// The addon guide's company examples, compiled as doctests.
/// Fixture addon for view arch group filtering tests.
pub mod test_view_groups {
    use erp_sdk::prelude::*;

    erp_sdk::addon!(name = "test_view_groups", depends = ["base"]);

    #[model(
        name = "test_view_groups.model",
        description = "Test View Groups Model",
        addon = "test_view_groups"
    )]
    pub struct TestViewGroupModel {
        #[field(char)]
        pub a: Option<Arc<str>>,
        #[field(char)]
        pub b: Option<Arc<str>>,
        #[field(char)]
        pub c: Option<Arc<str>>,
        #[field(char, groups = "base.group_system")]
        pub restricted: Option<Arc<str>>,
        #[field(char)]
        pub name: Option<Arc<str>>,
        #[field(char)]
        pub company_id: Option<Arc<str>>,
    }

    #[model(
        name = "test_view_groups.denied",
        description = "Test View Groups Denied Model",
        addon = "test_view_groups"
    )]
    pub struct TestViewGroupDeniedModel {
        #[field(char)]
        pub name: Option<Arc<str>>,
    }
}

#[cfg(doctest)]
mod addon_guide_company_consistency {}
