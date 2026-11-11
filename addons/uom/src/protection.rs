//! Seed-protection policy extension; transaction reference guards are independent.

use erp_sdk::Operation;
use erp_sdk::prelude::*;

use crate::models::Uom;

#[methods(Uom, addon = "uom")]
impl Uom {
    /// XML-ID names in the UoM catalog that may be deleted when otherwise unused.
    ///
    /// Native addon overrides compose through `super` and may remove exemptions.
    /// This policy never removes committed-reference protection.
    #[method]
    async fn _unprotected_uom_xml_ids(
        _this: &RecordSet<Uom>,
        _tx: &mut Transaction,
    ) -> Result<Vec<String>> {
        Ok([
            "product_uom_hour",
            "product_uom_dozen",
            "product_uom_pack_6",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect())
    }
    #[method(override)]
    async fn unlink(this: &RecordSet<Uom>, tx: &mut Transaction, sup: Super) -> Result<()> {
        if this.is_empty() {
            return sup.call(tx, this.as_records(), ()).await;
        }
        this.as_records()
            .check_access(tx, Operation::Unlink)
            .await?;
        let ids = crate::references::validated_ids(&this.ids())?;
        let exemptions: Vec<String> = this
            .as_records()
            .call(tx, "_unprotected_uom_xml_ids", ())
            .await?;
        if exemptions.len() > 4096 || exemptions.iter().any(|s| s.len() > 256) {
            return Err(Error::validation(
                "Unit deletion policy exceeds its limits.",
            ));
        }
        // Absence of metadata is not evidence that a catalog record is unprotected.
        if this.env().registry().model("ir.model.data").is_none() {
            return Err(Error::programming(
                "Unit deletion protection requires ir.model.data metadata.",
            ));
        }
        tx.flush().await?;
        acquire_fence(tx).await?;
        let rows = tx.query("WITH RECURSIVE closure(id) AS (SELECT unnest($1::bigint[]) UNION SELECT u.id::bigint FROM public.uom_uom u JOIN closure c ON u.relative_uom_id=c.id), affected AS MATERIALIZED (SELECT id FROM closure LIMIT 4097) SELECT EXISTS(SELECT 1 FROM affected a JOIN public.uom_reference_use m ON m.uom_id=a.id), EXISTS(SELECT 1 FROM affected a JOIN public.ir_model_data d ON d.res_id=a.id WHERE d.module='uom' AND d.model='uom.uom' AND NOT d.name=ANY($2::text[])), (SELECT count(*) FROM affected)::bigint", &[&ids,&exemptions]).await.map_err(crate::references::map_guard_error)?;
        let [row] = rows.as_slice() else {
            return Err(Error::programming(
                "Unit deletion guard returned no decision.",
            ));
        };
        let used: bool = row.try_get(0).map_err(|e| {
            Error::db("Cannot decode unit deletion reference decision.").with_source(e)
        })?;
        let protected: bool = row
            .try_get(1)
            .map_err(|e| Error::db("Cannot decode unit deletion seed decision.").with_source(e))?;
        let count: i64 = row
            .try_get(2)
            .map_err(|e| Error::db("Cannot decode unit deletion graph size.").with_source(e))?;
        if count > 4096 {
            return Err(Error::validation(
                "Unit reference guard graph exceeds 4096 identities.",
            ));
        }
        if used {
            return Err(Error::user(
                "A referenced unit of measure cannot be deleted. Archive it instead.",
            ));
        }
        if protected {
            return Err(Error::user(
                "System units of measure cannot be deleted. Archive them instead.",
            ));
        }
        tx.savepoint(async |tx| sup.call(tx, this.as_records(), ()).await)
            .await
            .map_err(crate::references::map_guard_error)
    }
}

pub(crate) async fn acquire_fence(tx: &mut Transaction) -> Result<()> {
    let count = tx
        .execute(
            "UPDATE public.uom_reference_fence SET singleton=singleton WHERE singleton=1",
            &[],
        )
        .await?;
    if count != 1 {
        return Err(Error::programming(
            "Unit reference guard state is missing or corrupt.",
        ));
    }
    Ok(())
}

/// Preflight before opening the model's operation savepoint.
pub(crate) async fn before_write(
    this: &RecordSet<Uom>,
    tx: &mut Transaction,
    vals: &RawVals,
) -> Result<()> {
    this.as_records().check_access(tx, Operation::Write).await?;
    if vals.contains("parent_path") {
        return Err(Error::validation(
            "Field uom.uom.parent_path is maintained by the unit hierarchy and cannot be assigned.",
        ));
    }
    if !vals.contains("relative_factor") && !vals.contains("relative_uom_id") {
        return Ok(());
    }
    let ids = crate::references::validated_ids(&this.ids())?;
    let parent = match vals.get("relative_uom_id") {
        None | Some(Cell::Null) => None,
        Some(Cell::Id(id)) => Some(*id),
        Some(_) => {
            return Err(Error::validation(
                "Unit reference parent must be a persistent identity or null.",
            ));
        }
    };
    let ratio = match vals.get("relative_factor") {
        None => Decimal::ONE.to_string(), // Unused bound parameter when the field is absent.
        Some(Cell::Decimal(value)) => value.to_string(),
        Some(_) => {
            return Err(Error::validation(
                "Unit reference ratio must be an exact decimal.",
            ));
        }
    };
    tx.flush().await?;
    let rows = tx
        .query(
            include_str!("guard_preflight.sql"),
            &[
                &ids,
                &vals.contains("relative_uom_id"),
                &parent,
                &vals.contains("relative_factor"),
                &ratio,
            ],
        )
        .await?;
    let [row] = rows.as_slice() else {
        return Err(Error::programming(
            "Unit reference guard returned no decision.",
        ));
    };
    let fences: i64 = row
        .try_get(0)
        .map_err(|e| Error::db("Cannot decode unit guard fence decision.").with_source(e))?;
    let affected: i64 = row
        .try_get(1)
        .map_err(|e| Error::db("Cannot decode unit guard graph decision.").with_source(e))?;
    let denied: bool = row
        .try_get(2)
        .map_err(|e| Error::db("Cannot decode unit guard reference decision.").with_source(e))?;
    let invalid: bool = row
        .try_get(3)
        .map_err(|e| Error::db("Cannot decode unit guard hierarchy decision.").with_source(e))?;
    if fences != 1 {
        return Err(Error::programming(
            "Unit reference guard state is missing or corrupt.",
        ));
    }
    if affected > 4096 {
        return Err(Error::validation(
            "Unit reference guard graph exceeds 4096 identities.",
        ));
    }
    if invalid {
        return Err(Error::validation("Unit of measure hierarchy is invalid."));
    }
    if denied {
        return Err(Error::user(
            "A referenced unit of measure would change. Create a new unit instead.",
        ));
    }
    Ok(())
}
