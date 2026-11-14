// SPDX-License-Identifier: MIT OR Apache-2.0
use crate::tax::AccountTax;
use erp_sdk::prelude::*;
#[methods(AccountTax, addon = "account")]
impl AccountTax {
    /// Filter the actual client picker before its domain, ordering and result limit run.
    #[method(override)]
    async fn web_name_search(
        this: &RecordSet<AccountTax>,
        tx: &mut Transaction,
        mut args: erp_sdk::web::WebNameSearch,
        sup: Super,
    ) -> Result<Vec<Value>> {
        match this.env().context().get("dynamic_fiscal_position_id") {
            None | Some(Value::False) => {}
            Some(Value::Int(id)) if id > 0 => {
                this.env()
                    .records("account.fiscal.position")?
                    .browse([id])?
                    .check_access(tx, erp_sdk::Operation::Read)
                    .await?;
                let field = Domain::field("fiscal_position_ids")?;
                args.domain = args.domain
                    & (field.eq(false)
                        | field.in_(erp_sdk::domain::Value::list([erp_sdk::domain::Value::Int(
                            id,
                        )])));
            }
            _ => {
                return Err(Error::validation(
                    "dynamic_fiscal_position_id must be false or a positive record id.",
                ));
            }
        }
        sup.call(tx, this.as_records(), (args,)).await
    }

    #[compute(depends = ["company_id.domestic_fiscal_position_id", "fiscal_position_ids"])]
    async fn _compute_fpos_domestic(
        this: &RecordSet<AccountTax>,
        tx: &mut Transaction,
        out: &mut Assign<'_, AccountTax>,
    ) -> Result<()> {
        for (i, tax) in this.iter().enumerate() {
            let positions = tax.as_records().related(tx, "fiscal_position_ids").await?;
            let company = tax.as_records().related(tx, "company_id").await?;
            let domestic = super::id(company.cell(tx, "domestic_fiscal_position_id").await?)?;
            out.set(
                i,
                AccountTax::IS_DOMESTIC,
                positions.is_empty() || domestic.is_some_and(|id| positions.ids().contains(&id)),
            )?;
        }
        Ok(())
    }
}
