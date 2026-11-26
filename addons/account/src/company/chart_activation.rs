// SPDX-License-Identifier: MIT OR Apache-2.0
//! Chart of Accounts template activation, Incoterms 2020 and Cash Rounding configuration.

use erp_addon_base::ResCompany;
use erp_sdk::{Operation, prelude::*};

fn db_err(e: impl std::error::Error + Send + Sync + 'static) -> Error {
    Error::db("Accounting setup data could not be read.").with_source(e)
}

/// `account.incoterms`: international commercial terms used on invoices, orders and company defaults.
#[model(
    name = "account.incoterms",
    description = "Incoterms",
    order = "code, name",
    constraints = [(
        "_unique_code",
        "UNIQUE (code)",
        "An Incoterm with this code already exists."
    )]
)]
pub struct AccountIncoterms {
    /// Three-letter Incoterms code (e.g. `EXW`, `FOB`, `CIF`, `DDP`).
    #[field(char, required = true, size = 3, string = "Code")]
    pub code: Option<Arc<str>>,
    /// Full commercial term name.
    #[field(char, required = true, string = "Name")]
    pub name: Option<Arc<str>>,
    /// Active flag.
    #[field(boolean, default = true)]
    pub active: bool,
}

/// `account.cash.rounding`: invoice total cash rounding rules.
#[model(
    name = "account.cash.rounding",
    description = "Account Cash Rounding",
    order = "name, id",
    company_access = direct(field = "company_id", branch = "exact", empty = "allow"),
    constraints = [(
        "_positive_rounding",
        "CHECK (rounding > 0)",
        "Cash rounding precision must be strictly greater than zero."
    )]
)]
pub struct AccountCashRounding {
    /// Rule name (e.g. `Round to 0.05`, `Round to 100 IDR`).
    #[field(char, required = true, string = "Name")]
    pub name: Option<Arc<str>>,
    /// Rounding step (e.g. `0.05`, `1`, `100`).
    #[field(
        float,
        digits = 0,
        required = true,
        default = 1,
        string = "Rounding Precision"
    )]
    pub rounding: Decimal,
    /// Rounding strategy.
    #[field(
        selection,
        selection = [
            ("biggest_tax", "Modify tax amount"),
            ("add_invoice_line", "Add a rounding line")
        ],
        default = "add_invoice_line",
        required = true,
        string = "Rounding Strategy"
    )]
    pub strategy: Option<Arc<str>>,
    /// Rounding direction.
    #[field(
        selection,
        selection = [
            ("UP", "Up"),
            ("DOWN", "Down"),
            ("HALF-UP", "Half towards nearest")
        ],
        default = "HALF-UP",
        required = true,
        string = "Rounding Method"
    )]
    pub rounding_method: Option<Arc<str>>,
    /// Profit account when rounding decreases payable / increases receivable.
    #[field(many2one, comodel = "account.account", string = "Profit Account")]
    pub profit_account_id: Option<RecId>,
    /// Loss account when rounding decreases receivable / increases payable.
    #[field(many2one, comodel = "account.account", string = "Loss Account")]
    pub loss_account_id: Option<RecId>,
    /// Optional owning company.
    #[field(many2one, comodel = "res.company", string = "Company")]
    pub company_id: Option<RecId>,
}

/// One seed account definition inside a Chart of Accounts template.
#[derive(Debug, Clone, Copy)]
pub struct ChartAccountSpec {
    /// Account code (e.g. `101000`).
    pub code: &'static str,
    /// Account name.
    pub name: &'static str,
    /// Account type from `account.account.account_type`.
    pub account_type: &'static str,
    /// Whether items on this account can be reconciled.
    pub reconcile: bool,
    /// Optional company role column to populate if currently empty.
    pub company_role: Option<&'static str>,
}

const GENERIC_COA: &[ChartAccountSpec] = &[
    ChartAccountSpec {
        code: "101000",
        name: "Cash",
        account_type: "asset_cash",
        reconcile: false,
        company_role: None,
    },
    ChartAccountSpec {
        code: "102000",
        name: "Bank",
        account_type: "asset_cash",
        reconcile: false,
        company_role: None,
    },
    ChartAccountSpec {
        code: "110000",
        name: "Accounts Receivable",
        account_type: "asset_receivable",
        reconcile: true,
        company_role: None,
    },
    ChartAccountSpec {
        code: "115000",
        name: "Bank Suspense Account",
        account_type: "asset_current",
        reconcile: false,
        company_role: Some("account_journal_suspense_account_id"),
    },
    ChartAccountSpec {
        code: "116000",
        name: "Liquidity Transfer",
        account_type: "asset_current",
        reconcile: true,
        company_role: Some("transfer_account_id"),
    },
    ChartAccountSpec {
        code: "121000",
        name: "Tax Receivable",
        account_type: "asset_current",
        reconcile: false,
        company_role: None,
    },
    ChartAccountSpec {
        code: "201000",
        name: "Accounts Payable",
        account_type: "liability_payable",
        reconcile: true,
        company_role: None,
    },
    ChartAccountSpec {
        code: "210000",
        name: "Tax Payable",
        account_type: "liability_current",
        reconcile: false,
        company_role: None,
    },
    ChartAccountSpec {
        code: "301000",
        name: "Share Capital",
        account_type: "equity",
        reconcile: false,
        company_role: None,
    },
    ChartAccountSpec {
        code: "400000",
        name: "Product Sales",
        account_type: "income",
        reconcile: false,
        company_role: None,
    },
    ChartAccountSpec {
        code: "410000",
        name: "Foreign Exchange Gain",
        account_type: "income_other",
        reconcile: false,
        company_role: Some("income_currency_exchange_account_id"),
    },
    ChartAccountSpec {
        code: "420000",
        name: "Cash Discount Gain",
        account_type: "income_other",
        reconcile: false,
        company_role: Some("account_journal_early_pay_discount_gain_account_id"),
    },
    ChartAccountSpec {
        code: "500000",
        name: "Cost of Goods Sold",
        account_type: "expense_direct_cost",
        reconcile: false,
        company_role: None,
    },
    ChartAccountSpec {
        code: "600000",
        name: "General Expenses",
        account_type: "expense",
        reconcile: false,
        company_role: None,
    },
    ChartAccountSpec {
        code: "610000",
        name: "Foreign Exchange Loss",
        account_type: "expense",
        reconcile: false,
        company_role: Some("expense_currency_exchange_account_id"),
    },
    ChartAccountSpec {
        code: "620000",
        name: "Cash Discount Loss",
        account_type: "expense",
        reconcile: false,
        company_role: Some("account_journal_early_pay_discount_loss_account_id"),
    },
];

const ID_SAK_COA: &[ChartAccountSpec] = &[
    ChartAccountSpec {
        code: "111100",
        name: "Kas",
        account_type: "asset_cash",
        reconcile: false,
        company_role: None,
    },
    ChartAccountSpec {
        code: "111200",
        name: "Bank",
        account_type: "asset_cash",
        reconcile: false,
        company_role: None,
    },
    ChartAccountSpec {
        code: "112100",
        name: "Piutang Usaha",
        account_type: "asset_receivable",
        reconcile: true,
        company_role: None,
    },
    ChartAccountSpec {
        code: "115100",
        name: "Akun Sementara Bank",
        account_type: "asset_current",
        reconcile: false,
        company_role: Some("account_journal_suspense_account_id"),
    },
    ChartAccountSpec {
        code: "116100",
        name: "Transfer Likuiditas",
        account_type: "asset_current",
        reconcile: true,
        company_role: Some("transfer_account_id"),
    },
    ChartAccountSpec {
        code: "117100",
        name: "PPN Masukan",
        account_type: "asset_current",
        reconcile: false,
        company_role: None,
    },
    ChartAccountSpec {
        code: "211100",
        name: "Utang Usaha",
        account_type: "liability_payable",
        reconcile: true,
        company_role: None,
    },
    ChartAccountSpec {
        code: "212100",
        name: "PPN Keluaran",
        account_type: "liability_current",
        reconcile: false,
        company_role: None,
    },
    ChartAccountSpec {
        code: "311000",
        name: "Modal Disetor",
        account_type: "equity",
        reconcile: false,
        company_role: None,
    },
    ChartAccountSpec {
        code: "411000",
        name: "Pendapatan Penjualan",
        account_type: "income",
        reconcile: false,
        company_role: None,
    },
    ChartAccountSpec {
        code: "421000",
        name: "Laba Selisih Kurs",
        account_type: "income_other",
        reconcile: false,
        company_role: Some("income_currency_exchange_account_id"),
    },
    ChartAccountSpec {
        code: "422000",
        name: "Potongan Pembayaran Awal",
        account_type: "income_other",
        reconcile: false,
        company_role: Some("account_journal_early_pay_discount_gain_account_id"),
    },
    ChartAccountSpec {
        code: "511000",
        name: "Beban Pokok Penjualan",
        account_type: "expense_direct_cost",
        reconcile: false,
        company_role: None,
    },
    ChartAccountSpec {
        code: "611000",
        name: "Beban Operasional",
        account_type: "expense",
        reconcile: false,
        company_role: None,
    },
    ChartAccountSpec {
        code: "621000",
        name: "Rugi Selisih Kurs",
        account_type: "expense",
        reconcile: false,
        company_role: Some("expense_currency_exchange_account_id"),
    },
    ChartAccountSpec {
        code: "622000",
        name: "Beban Diskon Pembayaran Awal",
        account_type: "expense",
        reconcile: false,
        company_role: Some("account_journal_early_pay_discount_loss_account_id"),
    },
];

/// Return the seed account list for a chart template key.
pub fn chart_template_accounts(template: &str) -> Result<&'static [ChartAccountSpec]> {
    match template {
        "generic_coa" => Ok(GENERIC_COA),
        "id_sak" => Ok(ID_SAK_COA),
        other => Err(Error::validation(format!(
            "Unknown chart of accounts template '{other}'. Choose 'generic_coa' or 'id_sak'."
        ))),
    }
}

#[extend(ResCompany, addon = "account")]
pub struct CompanyChartTemplateAction {}

#[methods(ResCompany, addon = "account")]
impl CompanyChartTemplateAction {
    /// Activate the selected Chart of Accounts template for this company, idempotently
    /// creating missing accounts and wiring default liquidity, exchange and discount accounts.
    #[method]
    #[rpc(effect = write)]
    pub async fn action_activate_chart_template(
        this: &RecordSet<ResCompany>,
        tx: &mut Transaction,
    ) -> Result<()> {
        let records = this.as_records().clone();
        records.check_access(tx, Operation::Write).await?;
        let env = records.env().clone();
        if !env.su() && !tx.has_group(&env, "account.group_account_manager").await? {
            return Err(Error::access(
                "Only accounting managers can activate a chart of accounts template.",
            ));
        }
        let company_id = records.ensure_one()?.int()?;
        tx.savepoint(async move |tx| {
            tx.flush().await?;
            let row = tx
                .query(
                    "SELECT chart_template_selection, chart_template \
                     FROM res_company WHERE id = $1 FOR UPDATE",
                    &[&company_id],
                )
                .await?;
            let Some(crow) = row.first() else {
                return Err(Error::missing("Company not found."));
            };
            let selected: Option<String> = crow.try_get(0).map_err(db_err)?;
            let existing: Option<String> = crow.try_get(1).map_err(db_err)?;
            let template_key = selected
                .as_deref()
                .filter(|s| !s.trim().is_empty())
                .or(existing.as_deref().filter(|s| !s.trim().is_empty()))
                .unwrap_or("generic_coa")
                .to_owned();
            let specs = chart_template_accounts(&template_key)?;

            let company_env = env.with_company(company_id)?;
            let account_model = company_env.records("account.account")?;
            let mut role_targets: Vec<(&'static str, i64)> = Vec::new();

            for spec in specs {
                let existing_acc = tx
                    .query(
                        "SELECT m.account_id::int8 \
                         FROM account_account_code m \
                         WHERE m.company_id = $1 AND m.code = $2 LIMIT 1",
                        &[&company_id, &spec.code],
                    )
                    .await?;
                let account_id = if let Some(erow) = existing_acc.first() {
                    erow.try_get::<_, i64>(0).map_err(db_err)?
                } else {
                    let mut vals = RawVals::new();
                    vals.insert("code".into(), Cell::Str(spec.code.into()));
                    vals.insert("name".into(), Cell::Str(spec.name.into()));
                    vals.insert("account_type".into(), Cell::Str(spec.account_type.into()));
                    vals.insert("reconcile".into(), Cell::Bool(spec.reconcile));
                    let created = account_model.create(tx, vec![vals]).await?;
                    created
                        .ids()
                        .into_iter()
                        .next()
                        .ok_or_else(|| Error::invariant("Created account returned no id."))?
                };
                if let Some(role_col) = spec.company_role {
                    role_targets.push((role_col, account_id));
                }
            }

            // Populate unset company default accounts from the activated template.
            for (role_col, account_id) in role_targets {
                let sql = format!(
                    "UPDATE res_company SET {role_col} = $2 \
                     WHERE id = $1 AND {role_col} IS NULL"
                );
                tx.execute(&sql, &[&company_id, &account_id]).await?;
            }

            // Wire unset payment method line outstanding accounts on the company's bank/cash
            // journals using the company's transfer account so payments can be registered right
            // after chart activation.
            tx.execute(
                "UPDATE account_payment_method_line l \
                 SET payment_account_id = c.transfer_account_id \
                 FROM account_journal j, res_company c \
                 WHERE j.id = l.journal_id AND j.company_id = $1 \
                   AND j.type IN ('bank', 'cash') \
                   AND c.id = $1 AND c.transfer_account_id IS NOT NULL \
                   AND l.payment_account_id IS NULL",
                &[&company_id],
            )
            .await?;
            tx.invalidate_model("account.payment.method.line", Some(&["payment_account_id"]))
                .await?;

            // Populate exchange difference journal if unset and a general journal exists.
            let jrows = tx
                .query(
                    "SELECT id::int8 FROM account_journal \
                     WHERE company_id = $1 AND type = 'general' AND active IS TRUE \
                     ORDER BY sequence, id LIMIT 1",
                    &[&company_id],
                )
                .await?;
            if let Some(jrow) = jrows.first() {
                let jid: i64 = jrow.try_get(0).map_err(db_err)?;
                tx.execute(
                    "UPDATE res_company SET currency_exchange_journal_id = $2 \
                     WHERE id = $1 AND currency_exchange_journal_id IS NULL",
                    &[&company_id, &jid],
                )
                .await?;
            }

            tx.execute(
                "UPDATE res_company SET chart_template = $2, chart_template_selection = $2 \
                 WHERE id = $1",
                &[&company_id, &template_key],
            )
            .await?;
            tx.invalidate_model("res.company", None).await?;
            Ok(())
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    #[test]
    fn chart_templates_have_unique_codes_and_core_accounts() {
        for key in ["generic_coa", "id_sak"] {
            let specs = chart_template_accounts(key).expect("template exists");
            let codes: BTreeSet<&str> = specs.iter().map(|s| s.code).collect();
            assert_eq!(codes.len(), specs.len(), "{key} has duplicate codes");
            assert!(specs.iter().any(|s| s.account_type == "asset_receivable"));
            assert!(specs.iter().any(|s| s.account_type == "liability_payable"));
            assert!(specs.iter().any(|s| s.account_type == "income"));
            assert!(specs.iter().any(|s| s.account_type == "expense"));
            assert!(
                specs
                    .iter()
                    .any(|s| s.company_role == Some("income_currency_exchange_account_id"))
            );
            assert!(
                specs
                    .iter()
                    .any(|s| s.company_role == Some("expense_currency_exchange_account_id"))
            );
        }
        assert!(chart_template_accounts("unknown_pack").is_err());
    }
}
