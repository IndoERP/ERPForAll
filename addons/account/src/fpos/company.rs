// SPDX-License-Identifier: MIT OR Apache-2.0
use super::id;
use erp_sdk::prelude::*;

#[extend(ResCompany, addon = "account")]
struct FposCompany {
    /// Internal database write fence; denied by the field-access chain, absent from views.
    #[field(
        integer,
        default = 0,
        required = true,
        readonly = true,
        groups = ".",
        copy = false
    )]
    pub fpos_revision: i32,
    /// Rules owned directly by this company.
    #[field(
        one2many,
        comodel = "account.fiscal.position",
        inverse_name = "company_id"
    )]
    pub fiscal_position_ids: Vec<RecId>,
    /// Lowest-sequence domestic rule, preferring a country over a group on ties.
    #[field(
        many2one,
        comodel = "account.fiscal.position",
        compute = "_compute_fpos_company"
    )]
    pub domestic_fiscal_position_id: Option<RecId>,
    /// Countries with an active foreign VAT registration.
    #[field(many2many, comodel = "res.country", compute = "_compute_fpos_company")]
    pub multi_vat_foreign_country_ids: Vec<RecId>,
    /// Optional receipt default for document consumers.
    #[field(many2one, comodel = "account.fiscal.position", check_company = true)]
    pub account_purchase_receipt_fiscal_position_id: Option<RecId>,
}
#[methods(ResCompany, addon = "account")]
impl FposCompany {
    #[field_access]
    fn _fpos_field_access(
        ctx: &FieldAccessCtx<'_>,
        op: FieldOp,
        mask: FieldMask,
        sup: FieldAccessSuper<'_>,
    ) -> Result<FieldMask> {
        let mut mask = sup.call(ctx, op, mask)?;
        let field = ctx
            .model()
            .field_id("fpos_revision")
            .ok_or_else(|| Error::programming("Missing fpos_revision field."))?;
        mask.deny(field, op)?;
        Ok(mask)
    }
    #[compute(depends = ["partner_id.country_id", "fiscal_position_ids.sequence", "fiscal_position_ids.country_id", "fiscal_position_ids.country_group_id.country_ids", "fiscal_position_ids.foreign_vat", "fiscal_position_ids.active"])]
    async fn _compute_fpos_company(
        this: &RecordSet<ResCompany>,
        tx: &mut Transaction,
        out: &mut Assign<'_, ResCompany>,
    ) -> Result<()> {
        for (i, company) in this.iter().enumerate() {
            let country = id(company
                .as_records()
                .related(tx, "partner_id")
                .await?
                .cell(tx, "country_id")
                .await?)?;
            let positions = company
                .as_records()
                .related(tx, "fiscal_position_ids")
                .await?;
            let mut domestic = Vec::new();
            let mut foreign = std::collections::BTreeSet::new();
            for position in positions.iter() {
                if position.cell(tx, "active").await? != Cell::Bool(true) {
                    continue;
                }
                let pc = id(position.cell(tx, "country_id").await?)?;
                let group = position.related(tx, "country_group_id").await?;
                let group_matches = country.is_some_and(|country| !group.is_empty() && country > 0)
                    && group
                        .related(tx, "country_ids")
                        .await?
                        .ids()
                        .iter()
                        .any(|id| Some(*id) == country);
                if country.is_some() && (pc == country || (pc.is_none() && group_matches)) {
                    let seq = match position.cell(tx, "sequence").await? {
                        Cell::Int(n) => n,
                        other => {
                            return Err(Error::programming(format!(
                                "Invalid position sequence: {other:?}"
                            )));
                        }
                    };
                    domestic.push((seq, pc.is_none(), position.ids()[0]));
                }
                if !super::text(position.cell(tx, "foreign_vat").await?)?.is_empty()
                    && let Some(country) = pc
                {
                    foreign.insert(country);
                }
            }
            domestic.sort_unstable();
            let first = domestic
                .first()
                .map(|entry| entry.2)
                .and_then(RecId::from_real);
            out.set(i, FposCompany::DOMESTIC_FISCAL_POSITION_ID, first)?;
            let foreign = foreign
                .into_iter()
                .map(|id| {
                    RecId::from_real(id)
                        .ok_or_else(|| Error::internal("Invalid foreign country id."))
                })
                .collect::<Result<Vec<_>>>()?;
            out.set(i, FposCompany::MULTI_VAT_FOREIGN_COUNTRY_IDS, foreign)?;
        }
        Ok(())
    }
}
/// Existing companies begin with a zero fence revision; install schema defaults cover new rows.
#[erp_sdk::migration(addon = "account", version = "1.1", stage = post)]
pub async fn initialize_fpos_revision(
    tx: &mut Transaction,
    _from: &erp_sdk::migrations::SemverVersion,
) -> Result<()> {
    tx.execute(
        "UPDATE res_company SET fpos_revision=0 WHERE fpos_revision IS NULL",
        &[],
    )
    .await?;
    Ok(())
}
