use erp_sdk::prelude::*;

erp_sdk::addon!(name = "test_field_access");

/// `test_access_right.some_obj`: one field per kind of restriction.
#[model(
    name = "test_fa.item",
    description = "Object For Test Access Right",
    addon = "test_field_access"
)]
pub struct Item {
    #[field(char)]
    pub name: Option<Arc<str>>,
    #[field(integer)]
    pub val: i32,
    /// Several groups.
    #[field(integer, groups = "base.group_portal,test_fa.group_test")]
    pub forbidden: i32,
    /// One group.
    #[field(integer, groups = "test_fa.group_manager")]
    pub forbidden2: i32,
    #[field(integer, groups = ".")]
    pub forbidden3: i32,
    #[field(many2one, comodel = "test_fa.item")]
    pub parent_id: Option<RecId>,
    #[field(
        one2many,
        comodel = "test_fa.bank",
        inverse_name = "item_id",
        groups = "base.group_system"
    )]
    pub bank_ids: Vec<RecId>,
    #[field(char, groups = "base.group_system")]
    pub email: Option<Arc<str>>,
    /// Related without `groups`: takes the target's.
    #[field(integer, related = "parent_id.forbidden2")]
    pub mirror: i32,
    /// Related with its own `groups`: keeps them.
    #[field(integer, related = "parent_id.forbidden2", groups = "base.group_user")]
    pub mirror_open: i32,
    /// Related to a related field: takes the effective groups of `mirror`.
    #[field(integer, related = "parent_id.mirror")]
    pub mirror_chain: i32,
    /// A non-stored inverse field restricted to `base.group_system`.
    #[field(
        char,
        compute = "_compute_inv_name",
        inverse = "_inverse_inv_name",
        groups = "base.group_system"
    )]
    pub inv_name: Option<Arc<str>>,
}

#[methods(Item, addon = "test_field_access")]
impl Item {
    #[compute(depends = ["name"])]
    fn _compute_inv_name(batch: &mut Batch<'_, Item>) -> Result<()> {
        for i in batch.indices() {
            let name = batch.get(i, Item::NAME)?;
            batch.set(i, Item::INV_NAME, name)?;
        }
        Ok(())
    }

    #[method]
    async fn _inverse_inv_name(this: &RecordSet<Item>, tx: &mut Transaction) -> Result<()> {
        for rec in this.iter() {
            if let Some(val) = rec.get(tx, Item::INV_NAME).await? {
                rec.set(tx, Item::NAME, val.as_ref()).await?;
            }
        }
        Ok(())
    }
}

/// The comodel of `bank_ids`.
#[model(
    name = "test_fa.bank",
    description = "Test Bank",
    addon = "test_field_access"
)]
pub struct Bank {
    #[field(many2one, comodel = "test_fa.item", required, ondelete = "cascade")]
    pub item_id: Option<RecId>,
    #[field(char)]
    pub acc_number: Option<Arc<str>>,
    #[field(char, groups = "base.group_system")]
    pub secret: Option<Arc<str>>,
}

/// `test_access_right.inherits`: delegates to an item.
#[model(
    name = "test_fa.inherits",
    description = "Inherits",
    inherits = [("test_fa.item", "some_id")],
    addon = "test_field_access"
)]
pub struct Child {
    #[field(many2one, comodel = "test_fa.item", required, ondelete = "cascade")]
    pub some_id: Option<RecId>,
    #[field(char)]
    pub label: Option<Arc<str>>,
}

/// A model whose field access an override narrows (the way `res.users` limits self-writes).
#[model(
    name = "test_fa.guarded",
    description = "Guarded",
    addon = "test_field_access"
)]
pub struct Guarded {
    #[field(char)]
    pub name: Option<Arc<str>>,
    #[field(integer)]
    pub val: i32,
    #[field(char)]
    pub note: Option<Arc<str>>,
}

#[methods(Guarded, addon = "test_field_access")]
impl Guarded {
    /// `note` is unreadable outside group `test_fa.group_test`; `val` is read-only while the
    /// context says `fa_freeze`.
    #[field_access(context = ["fa_freeze"])]
    fn narrow(
        ctx: &FieldAccessCtx<'_>,
        op: FieldOp,
        mask: FieldMask,
        sup: FieldAccessSuper,
    ) -> Result<FieldMask> {
        let mut mask = sup.call(ctx, op, mask)?;
        if op == FieldOp::Read && !ctx.has_group("test_fa.group_test")? {
            mask.deny(ctx.field("note")?, FieldOp::Read)?;
        }
        if op == FieldOp::Write && ctx.context("fa_freeze")? == Some(&Value::Bool(true)) {
            mask.deny(ctx.field("val")?, FieldOp::Write)?;
        }
        Ok(mask)
    }
}

/// A model whose override hides `secret` from reads while the context says `fa_veil`
/// (a declared, context-dependent field verdict that guard evaluations must carry).
#[model(
    name = "test_fa.veiled",
    description = "Veiled",
    addon = "test_field_access"
)]
pub struct Veiled {
    #[field(char)]
    pub name: Option<Arc<str>>,
    #[field(char)]
    pub secret: Option<Arc<str>>,
}

#[methods(Veiled, addon = "test_field_access")]
impl Veiled {
    /// `secret` is unreadable while the context says `fa_veil`.
    #[field_access(context = ["fa_veil"])]
    fn veil(
        ctx: &FieldAccessCtx<'_>,
        op: FieldOp,
        mask: FieldMask,
        sup: FieldAccessSuper,
    ) -> Result<FieldMask> {
        let mut mask = sup.call(ctx, op, mask)?;
        if op == FieldOp::Read && ctx.context("fa_veil")? == Some(&Value::Bool(true)) {
            mask.deny(ctx.field("secret")?, FieldOp::Read)?;
        }
        Ok(mask)
    }
}

/// A model whose override widens what the policy denies (a `Programming` error).
#[model(
    name = "test_fa.widening",
    description = "Widening",
    addon = "test_field_access"
)]
pub struct Widening {
    #[field(char)]
    pub name: Option<Arc<str>>,
}

#[methods(Widening, addon = "test_field_access")]
impl Widening {
    #[field_access]
    fn widen(
        ctx: &FieldAccessCtx<'_>,
        op: FieldOp,
        mask: FieldMask,
        sup: FieldAccessSuper,
    ) -> Result<FieldMask> {
        let mut mask = sup.call(ctx, op, mask)?;
        mask.grant(ctx.field("name")?, op)?;
        Ok(mask)
    }
}

/// A model whose override widens the mask with `full()` before calling `super`.
#[model(
    name = "test_fa.widen_full",
    description = "Widen full",
    addon = "test_field_access"
)]
pub struct WidenFull {
    #[field(char)]
    pub name: Option<Arc<str>>,
}

#[methods(WidenFull, addon = "test_field_access")]
impl WidenFull {
    #[field_access]
    fn widen_full(
        ctx: &FieldAccessCtx<'_>,
        op: FieldOp,
        _mask: FieldMask,
        sup: FieldAccessSuper,
    ) -> Result<FieldMask> {
        sup.call(ctx, op, FieldMask::full(ctx.model().fields.len()))
    }
}

/// A model whose override grants a field before calling `super`.
#[model(
    name = "test_fa.widen_grant",
    description = "Widen grant",
    addon = "test_field_access"
)]
pub struct WidenGrant {
    #[field(char)]
    pub name: Option<Arc<str>>,
}

#[methods(WidenGrant, addon = "test_field_access")]
impl WidenGrant {
    #[field_access]
    fn widen_grant(
        ctx: &FieldAccessCtx<'_>,
        op: FieldOp,
        mut mask: FieldMask,
        sup: FieldAccessSuper,
    ) -> Result<FieldMask> {
        mask.grant(ctx.field("name")?, op)?;
        sup.call(ctx, op, mask)
    }
}

/// A model whose override calls `super` with another operation than the run's.
#[model(
    name = "test_fa.wrong_op",
    description = "Wrong op",
    addon = "test_field_access"
)]
pub struct WrongOp {
    #[field(char)]
    pub name: Option<Arc<str>>,
}

#[methods(WrongOp, addon = "test_field_access")]
impl WrongOp {
    #[field_access]
    fn wrong_op(
        ctx: &FieldAccessCtx<'_>,
        _op: FieldOp,
        mask: FieldMask,
        sup: FieldAccessSuper,
    ) -> Result<FieldMask> {
        sup.call(ctx, FieldOp::Write, mask)
    }
}

/// A model whose override reads a context key it did not declare.
#[model(
    name = "test_fa.sloppy",
    description = "Sloppy",
    addon = "test_field_access"
)]
pub struct Sloppy {
    #[field(char)]
    pub name: Option<Arc<str>>,
}

#[methods(Sloppy, addon = "test_field_access")]
impl Sloppy {
    #[field_access(context = ["declared_key"])]
    fn peek(
        ctx: &FieldAccessCtx<'_>,
        op: FieldOp,
        mask: FieldMask,
        sup: FieldAccessSuper,
    ) -> Result<FieldMask> {
        let _ = ctx.context("declared_key")?;
        let _ = ctx.context("undeclared_key")?;
        sup.call(ctx, op, mask)
    }
}

/// A later addon adding a second override to [`Guarded`].
pub mod test_field_access_ext {
    use erp_sdk::prelude::*;

    use super::Guarded;

    erp_sdk::addon!(
        name = "test_field_access_ext",
        depends = ["test_field_access"]
    );

    /// Extends the guarded model without adding fields.
    #[extend(Guarded, addon = "test_field_access_ext")]
    pub struct GuardedExt {}

    #[methods(Guarded, addon = "test_field_access_ext")]
    impl GuardedExt {
        /// `name` is read-only while the context says `fa_freeze_name`.
        #[field_access(context = ["fa_freeze_name"])]
        fn narrow_name(
            ctx: &FieldAccessCtx<'_>,
            op: FieldOp,
            mask: FieldMask,
            sup: FieldAccessSuper,
        ) -> Result<FieldMask> {
            let mut mask = sup.call(ctx, op, mask)?;
            if op == FieldOp::Write && ctx.context("fa_freeze_name")? == Some(&Value::Bool(true)) {
                mask.deny(ctx.field("name")?, FieldOp::Write)?;
            }
            Ok(mask)
        }
    }
}
