//! Addon-owned reference bookkeeping and owner-only lifecycle.

use erp_sdk::prelude::*;

use crate::models::Uom;

fn guard_reference_history(tx: &mut Transaction) -> erp_sdk::BoxFuture<'_, Result<()>> {
    Box::pin(async move {
        require_owner(tx).await?;
        crate::protection::acquire_fence(tx).await?;
        let rows = tx
            .query("SELECT EXISTS(SELECT 1 FROM public.uom_reference_use)", &[])
            .await?;
        let [row] = rows.as_slice() else {
            return Err(Error::programming(
                "Unit reference uninstall guard returned no decision.",
            ));
        };
        let used: bool = row.try_get(0).map_err(|e| {
            Error::db("Cannot decode unit reference uninstall decision.").with_source(e)
        })?;
        if used {
            return Err(Error::user(
                "Units of measure have permanent transaction references; uninstall is refused.",
            ));
        }
        tx.batch(include_str!("guard_uninstall.sql")).await
    })
}

#[erp_modules::hooks::linkme::distributed_slice(erp_modules::hooks::HOOKS)]
#[linkme(crate = erp_modules::hooks::linkme)]
static UNINSTALL_GUARD: erp_modules::hooks::HookImpl = erp_modules::hooks::HookImpl::new(
    "uom",
    erp_modules::hooks::HookKind::Uninstall,
    "guard_reference_history",
    guard_reference_history,
)
.at(file!(), line!());

#[model(name = "uom.reference.fence", description = "Unit Reference Fence", constraints = [
    ("_singleton_uniq", "UNIQUE(singleton)", "The unit reference fence must be unique."),
    ("_singleton_one", "CHECK(singleton = 1)", "The unit reference fence key must be one."),
])]
pub(crate) struct ReferenceFence {
    #[field(integer, required = true)]
    pub singleton: i32,
}

#[model(name = "uom.reference.use", description = "Unit Reference Use", constraints = [
    ("_unit_uniq", "UNIQUE(uom_id)", "A unit has only one permanent reference marker."),
])]
pub(crate) struct ReferenceUse {
    #[field(many2one, comodel = "uom.uom", required = true, ondelete = "restrict")]
    pub uom_id: Option<RecId>,
    #[field(many2one, comodel = "uom.uom", required = true, ondelete = "restrict")]
    pub root_id: Option<RecId>,
    #[field(float, digits = 0, required = true)]
    pub factor: Decimal,
}

fn private_state() -> Error {
    Error::access("Unit reference guard records cannot be modified through ordinary CRUD.")
}

#[methods(ReferenceFence, addon = "uom")]
impl ReferenceFence {
    #[method(override)]
    async fn create(
        _this: &RecordSet<ReferenceFence>,
        _tx: &mut Transaction,
        _vals: Vec<RawVals>,
        _sup: Super,
    ) -> Result<Records> {
        Err(private_state())
    }
    #[method(override)]
    async fn write(
        _this: &RecordSet<ReferenceFence>,
        _tx: &mut Transaction,
        _vals: RawVals,
        _sup: Super,
    ) -> Result<()> {
        Err(private_state())
    }
    #[method(override)]
    async fn unlink(
        _this: &RecordSet<ReferenceFence>,
        _tx: &mut Transaction,
        _sup: Super,
    ) -> Result<()> {
        Err(private_state())
    }
}

#[methods(ReferenceUse, addon = "uom")]
impl ReferenceUse {
    #[method(override)]
    async fn create(
        _this: &RecordSet<ReferenceUse>,
        _tx: &mut Transaction,
        _vals: Vec<RawVals>,
        _sup: Super,
    ) -> Result<Records> {
        Err(private_state())
    }
    #[method(override)]
    async fn write(
        _this: &RecordSet<ReferenceUse>,
        _tx: &mut Transaction,
        _vals: RawVals,
        _sup: Super,
    ) -> Result<()> {
        Err(private_state())
    }
    #[method(override)]
    async fn unlink(
        _this: &RecordSet<ReferenceUse>,
        _tx: &mut Transaction,
        _sup: Super,
    ) -> Result<()> {
        Err(private_state())
    }
}

async fn require_owner(tx: &mut Transaction) -> Result<()> {
    let rows = tx.query("SELECT pg_catalog.pg_has_role(current_user,c.relowner,'USAGE') FROM pg_catalog.pg_class c WHERE c.oid=pg_catalog.to_regclass('public.uom_reference_fence')", &[]).await?;
    let [row] = rows.as_slice() else {
        return Err(Error::programming(
            "Unit reference guard schema is incomplete.",
        ));
    };
    let owner: bool = row
        .try_get(0)
        .map_err(|e| Error::db("Cannot decode unit guard owner state.").with_source(e))?;
    if !owner {
        return Err(Error::access(
            "Unit reference guard initialization requires the schema-owner connection.",
        ));
    }
    Ok(())
}

#[methods(Uom, addon = "uom")]
impl Uom {
    #[method(override)]
    async fn init(this: &RecordSet<Uom>, tx: &mut Transaction, sup: Super) -> Result<()> {
        require_owner(tx).await?;
        let rows = tx
            .query(
                "SELECT latest_version FROM public.ir_module_module WHERE name='uom'",
                &[],
            )
            .await?;
        let [row] = rows.as_slice() else {
            return Err(Error::programming(
                "Unit reference guard initialization requires module lifecycle state.",
            ));
        };
        let previous: Option<String> = row
            .try_get(0)
            .map_err(|e| Error::db("Cannot decode unit module version.").with_source(e))?;
        if previous
            .as_deref()
            .is_some_and(|version| version != "1.0.0")
        {
            // Schema reconciliation precedes init(). Detect recreation via the missing
            // table-owned trigger rather than legitimizing an empty replacement marker table.
            let rows = tx.query("SELECT (SELECT count(*) FROM public.uom_reference_fence)=1 AND (SELECT count(*) FROM public.uom_reference_fence WHERE singleton=1)=1 AND (SELECT count(*) FROM pg_catalog.pg_proc p JOIN pg_catalog.pg_namespace n ON n.oid=p.pronamespace WHERE n.nspname='public' AND p.proname IN ('erp_uom_fence','erp_uom_resolve','erp_uom_descendants','erp_uom_register','erp_uom_before_mutation','erp_uom_after_mutation','erp_uom_private_state'))=7 AND (SELECT count(*) FROM pg_catalog.pg_trigger t WHERE NOT t.tgisinternal AND t.tgenabled='O' AND ((t.tgrelid='public.uom_uom'::regclass AND t.tgname IN ('erp_uom_before_insert','erp_uom_before_delete','erp_uom_before_update','erp_uom_after_insert','erp_uom_after_update','erp_uom_after_delete')) OR (t.tgrelid='public.uom_reference_use'::regclass AND t.tgname='erp_uom_marker_immutable') OR (t.tgrelid='public.uom_reference_fence'::regclass AND t.tgname='erp_uom_fence_permanent')))=8", &[]).await?;
            let [row] = rows.as_slice() else {
                return Err(Error::programming(
                    "Unit reference guard validation returned no decision.",
                ));
            };
            let intact: bool = row.try_get(0).map_err(|e| {
                Error::db("Cannot decode unit reference guard state.").with_source(e)
            })?;
            if !intact {
                return Err(Error::programming(
                    "Unit reference guard state is missing or corrupt.",
                ));
            }
        }
        tx.batch(include_str!("guard_sql.sql")).await?;
        sup.call(tx, this.as_records(), ()).await
    }

    /// Data loading calls this before catalog records; ordinary runtime roles cannot initialize.
    #[method]
    #[data_callable(model)]
    async fn _uom_guard_initialize(_this: &RecordSet<Uom>, tx: &mut Transaction) -> Result<()> {
        require_owner(tx).await?;
        let rows = tx
            .query(
                "SELECT latest_version FROM public.ir_module_module WHERE name='uom'",
                &[],
            )
            .await?;
        let [row] = rows.as_slice() else {
            return Err(Error::programming(
                "Unit reference guard initialization requires module lifecycle state.",
            ));
        };
        let installed: Option<String> = row
            .try_get(0)
            .map_err(|e| Error::db("Cannot decode unit module version.").with_source(e))?;
        let rows = tx
            .query(
                "SELECT count(*) FROM public.uom_reference_fence WHERE singleton=1",
                &[],
            )
            .await?;
        let [row] = rows.as_slice() else {
            return Err(Error::programming(
                "Unit reference fence validation returned no decision.",
            ));
        };
        let count: i64 = row
            .try_get(0)
            .map_err(|e| Error::db("Cannot decode unit fence state.").with_source(e))?;
        match count {
            1 => Ok(()),
            0 if installed.is_none() || installed.as_deref() == Some("1.0.0") => {
                tx.execute(
                    "INSERT INTO public.uom_reference_fence(singleton) VALUES (1)",
                    &[],
                )
                .await?;
                Ok(())
            }
            _ => Err(Error::programming(
                "Unit reference guard state is missing or corrupt.",
            )),
        }
    }
}
