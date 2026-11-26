// SPDX-License-Identifier: MIT OR Apache-2.0
//! Batch Payments (`account.batch.payment`) and exact-decimal check amount-in-words helper.

use erp_sdk::Operation;
use erp_sdk::prelude::*;

use crate::payment::{post_payments, service::validate_payments};

/// `account.batch.payment`: groups inbound or outbound payments on one bank/cash journal
/// for deposit slips or batch bank remittances.
#[model(
    name = "account.batch.payment",
    description = "Batch Payment",
    order = "date desc, id desc",
    check_company_auto = true,
    company_access = direct(field = "company_id", branch = "exact", empty = "deny")
)]
pub struct AccountBatchPayment {
    /// Auto-numbered reference (`BATCH/IN/00001` or `BATCH/OUT/00001`).
    #[field(
        char,
        required = true,
        readonly = true,
        copy = false,
        default = "New",
        string = "Reference"
    )]
    pub name: Option<Arc<str>>,
    /// Batch date.
    #[field(date, required = true, string = "Date")]
    pub date: Option<Date>,
    /// Inbound customer deposit or outbound vendor batch.
    #[field(
        selection,
        selection = [("inbound", "Inbound"), ("outbound", "Outbound")],
        required = true,
        default = "inbound",
        string = "Batch Type"
    )]
    pub batch_type: Option<Arc<str>>,
    /// Workflow status.
    #[field(
        selection,
        selection = [
            ("draft", "New"),
            ("sent", "Sent"),
            ("reconciled", "Reconciled"),
        ],
        required = true,
        readonly = true,
        copy = false,
        default = "draft",
        string = "Status"
    )]
    pub state: Option<Arc<str>>,
    /// Bank or cash journal of the batch.
    #[field(
        many2one,
        comodel = "account.journal",
        required = true,
        check_company = true,
        ondelete = "restrict",
        domain = [("type", "in", ["bank", "cash", "credit"])],
        string = "Bank"
    )]
    pub journal_id: Option<RecId>,
    /// Company derived from the journal.
    #[field(
        many2one,
        comodel = "res.company",
        related = "journal_id.company_id",
        store = true,
        string = "Company"
    )]
    pub company_id: Option<RecId>,
    /// Currency derived from the journal or its company.
    #[field(
        many2one,
        comodel = "res.currency",
        compute = "_compute_amount",
        store = true,
        string = "Currency"
    )]
    pub currency_id: Option<RecId>,
    /// Payments included in this batch.
    #[field(
        one2many,
        comodel = "account.payment",
        inverse_name = "batch_payment_id",
        string = "Payments"
    )]
    pub payment_ids: Vec<RecId>,
    /// Total exact decimal amount of payments in the batch.
    #[field(
        monetary,
        currency_field = "currency_id",
        compute = "_compute_amount",
        store = true,
        string = "Total"
    )]
    pub amount: Decimal,
    /// Number of payments in the batch.
    #[field(
        integer,
        compute = "_compute_amount",
        store = true,
        string = "Payments Count"
    )]
    pub payment_count: i32,
    /// Optional bank deposit slip or remittance memo.
    #[field(char, string = "Memo")]
    pub memo: Option<Arc<str>>,
}

fn db(err: impl std::fmt::Display) -> Error {
    Error::db(format!("Batch payment query failed: {err}"))
}

async fn resolve_default_today(tx: &mut Transaction, env: &Env) -> Result<Date> {
    if let Some(cid) = env.company_id()? {
        erp_addon_base::company_calendar::business_date(tx, cid).await
    } else {
        env.context_today()
    }
}

#[methods(AccountBatchPayment, addon = "account")]
impl AccountBatchPayment {
    #[method(override)]
    async fn default_get(
        this: &RecordSet<AccountBatchPayment>,
        tx: &mut Transaction,
        names: Vec<String>,
        sup: Super,
    ) -> Result<erp_sdk::ValueMap> {
        let mut values: erp_sdk::ValueMap =
            sup.call(tx, this.as_records(), (names.clone(),)).await?;
        if names.iter().any(|n| n == "date") && !values.contains_key("date") {
            let today = resolve_default_today(tx, this.env()).await?;
            values.insert("date".to_owned(), Value::Date(today));
        }
        Ok(values)
    }

    #[compute(depends = ["journal_id", "payment_ids", "payment_ids.amount", "payment_ids.currency_id"])]
    async fn _compute_amount(
        this: &RecordSet<AccountBatchPayment>,
        tx: &mut Transaction,
        out: &mut Assign<'_, AccountBatchPayment>,
    ) -> Result<()> {
        tx.flush().await?;
        let journals = this.mapped(tx, AccountBatchPayment::JOURNAL_ID).await?;
        let rec_ids = this.as_records().rec_ids();
        for (i, (rec_id, journal)) in rec_ids.iter().zip(journals).enumerate() {
            let mut currency_id = None;
            if let Some(jid) = journal {
                let rows = tx
                    .query(
                        "SELECT COALESCE(j.currency_id, c.currency_id)::int8 \
                         FROM account_journal j JOIN res_company c ON c.id = j.company_id \
                         WHERE j.id = $1",
                        &[&jid.get()],
                    )
                    .await?;
                if let Some(row) = rows.first() {
                    let cid: Option<i64> = row.try_get(0).map_err(db)?;
                    currency_id = cid.and_then(RecId::from_real);
                }
            }
            let (total, count) = if let Some(real) = rec_id.as_real() {
                let rows = tx
                    .query(
                        "SELECT COALESCE(SUM(amount), 0), count(*)::int4, \
                                count(DISTINCT currency_id)::int4, MIN(currency_id)::int8 \
                         FROM account_payment \
                         WHERE batch_payment_id = $1 AND state <> 'canceled'",
                        &[&real.get()],
                    )
                    .await?;
                if let Some(row) = rows.first() {
                    let sum: Decimal = row.try_get(0).map_err(db)?;
                    let cnt: i32 = row.try_get(1).map_err(db)?;
                    let distinct_cur: i32 = row.try_get(2).map_err(db)?;
                    let first_cur: Option<i64> = row.try_get(3).map_err(db)?;
                    if distinct_cur > 1
                        || (cnt > 0
                            && currency_id.is_some()
                            && first_cur.is_some()
                            && first_cur != currency_id.map(|c| c.get()))
                    {
                        return Err(Error::validation(
                            "All payments in a batch payment must share the batch's currency.",
                        ));
                    }
                    (sum, cnt)
                } else {
                    (Decimal::ZERO, 0)
                }
            } else {
                (Decimal::ZERO, 0)
            };
            out.set(i, AccountBatchPayment::CURRENCY_ID, currency_id)?;
            out.set(i, AccountBatchPayment::AMOUNT, total)?;
            out.set(i, AccountBatchPayment::PAYMENT_COUNT, count)?;
        }
        Ok(())
    }

    #[method(override)]
    async fn create(
        this: &RecordSet<AccountBatchPayment>,
        tx: &mut Transaction,
        mut vals_list: Vec<RawVals>,
        sup: Super,
    ) -> Result<Records> {
        let mut today_cache: Option<Date> = None;
        for vals in &mut vals_list {
            if !this.env().su()
                && let Some(Cell::Str(st)) = vals.get("state")
                && st.as_ref() != "draft"
            {
                return Err(Error::validation(
                    "Batch payments must start in Draft status.",
                ));
            }
            if !vals.contains("date") {
                let d = match today_cache {
                    Some(d) => d,
                    None => {
                        let d = resolve_default_today(tx, this.env()).await?;
                        today_cache = Some(d);
                        d
                    }
                };
                vals.insert("date".into(), Cell::Date(d));
            }
            let needs_name = match vals.get("name") {
                None | Some(Cell::Null) => true,
                Some(Cell::Str(s)) => s.trim().is_empty() || s.as_ref() == "New",
                _ => false,
            };
            if needs_name {
                let batch_type = match vals.get("batch_type") {
                    Some(Cell::Str(s)) if s.as_ref() == "outbound" => "outbound",
                    _ => "inbound",
                };
                let prefix = if batch_type == "outbound" {
                    "BATCH/OUT/"
                } else {
                    "BATCH/IN/"
                };
                let next: i64 = tx
                    .query(
                        "SELECT nextval(pg_get_serial_sequence('account_batch_payment', 'id'))::int8",
                        &[],
                    )
                    .await?
                    .first()
                    .ok_or_else(|| Error::invariant("Missing nextval row for batch payment."))?
                    .try_get(0)
                    .map_err(db)?;
                vals.insert(
                    "name".into(),
                    Cell::Str(format!("{prefix}{next:05}").into()),
                );
            }
        }
        sup.call(tx, this.as_records(), (vals_list,)).await
    }

    #[method(override)]
    async fn write(
        this: &RecordSet<AccountBatchPayment>,
        tx: &mut Transaction,
        vals: RawVals,
        sup: Super,
    ) -> Result<()> {
        if !this.env().su() {
            if vals.contains("state") {
                return Err(Error::validation(
                    "Batch payment status must be changed through Validate, Reconcile, or Reset actions.",
                ));
            }
            let ids: Vec<i64> = this.as_records().ids().to_vec();
            if !ids.is_empty() {
                tx.flush().await?;
                let locked = tx
                    .query(
                        "SELECT id::int8 FROM account_batch_payment \
                         WHERE id = ANY($1::int8[]) AND state IN ('sent', 'reconciled') LIMIT 1",
                        &[&ids],
                    )
                    .await?;
                if !locked.is_empty() {
                    return Err(Error::user(
                        "Sent or reconciled batch payments cannot be modified.",
                    ));
                }
            }
        }
        sup.call(tx, this.as_records(), (vals,)).await
    }

    #[method(override)]
    async fn unlink(
        this: &RecordSet<AccountBatchPayment>,
        tx: &mut Transaction,
        sup: Super,
    ) -> Result<()> {
        if !this.env().su() {
            let ids: Vec<i64> = this.as_records().ids().to_vec();
            if !ids.is_empty() {
                tx.flush().await?;
                let locked = tx
                    .query(
                        "SELECT id::int8 FROM account_batch_payment \
                         WHERE id = ANY($1::int8[]) AND state <> 'draft' LIMIT 1",
                        &[&ids],
                    )
                    .await?;
                if !locked.is_empty() {
                    return Err(Error::user("Only draft batch payments can be deleted."));
                }
            }
        }
        sup.call(tx, this.as_records(), ()).await
    }

    /// Validates the batch: verifies at least one payment, matching direction/journal/currency,
    /// posts any draft payments, and marks the batch Sent.
    #[method]
    #[rpc(effect = write)]
    pub async fn validate_batch(
        this: &RecordSet<AccountBatchPayment>,
        tx: &mut Transaction,
    ) -> Result<()> {
        this.as_records().check_access(tx, Operation::Write).await?;
        tx.flush().await?;
        let env = this.env().clone();
        let uid = env.uid();
        for batch in this.iter() {
            let bid = batch.as_records().ids()[0];
            let brows = tx
                .query(
                    "SELECT b.state::text, b.journal_id::int8, b.batch_type::text, \
                            COALESCE(j.currency_id, c.currency_id)::int8 \
                     FROM account_batch_payment b \
                     JOIN account_journal j ON j.id = b.journal_id \
                     JOIN res_company c ON c.id = j.company_id \
                     WHERE b.id = $1 FOR UPDATE OF b",
                    &[&bid],
                )
                .await?;
            let Some(brow) = brows.first() else {
                return Err(Error::missing("Batch payment not found."));
            };
            let state: String = brow.try_get(0).map_err(db)?;
            if state != "draft" {
                return Err(Error::user("Only draft batch payments can be validated."));
            }
            let journal_id: i64 = brow.try_get(1).map_err(db)?;
            let batch_type: String = brow.try_get(2).map_err(db)?;
            let batch_currency_id: i64 = brow.try_get(3).map_err(db)?;

            let rows = tx
                .query(
                    "SELECT id::int8, state::text, journal_id::int8, payment_type::text, \
                            currency_id::int8 \
                     FROM account_payment WHERE batch_payment_id = $1 ORDER BY id FOR UPDATE",
                    &[&bid],
                )
                .await?;
            if rows.is_empty() {
                return Err(Error::user(
                    "Add at least one payment before validating this batch.",
                ));
            }
            let mut draft_ids = Vec::new();
            for row in &rows {
                let pid: i64 = row.try_get(0).map_err(db)?;
                let pstate: String = row.try_get(1).map_err(db)?;
                let pjournal: i64 = row.try_get(2).map_err(db)?;
                let ptype: String = row.try_get(3).map_err(db)?;
                let pcur: i64 = row.try_get(4).map_err(db)?;
                if pjournal != journal_id {
                    return Err(Error::user(
                        "All payments in a batch must belong to the batch's bank journal.",
                    ));
                }
                if ptype != batch_type {
                    return Err(Error::user(
                        "All payments in a batch must match the batch direction (Inbound/Outbound).",
                    ));
                }
                if pcur != batch_currency_id {
                    return Err(Error::user(
                        "All payments in a batch must use the same currency as the batch journal.",
                    ));
                }
                if pstate == "canceled" || pstate == "rejected" {
                    return Err(Error::user(
                        "Cancelled or rejected payments cannot be validated in a batch.",
                    ));
                }
                if pstate == "draft" {
                    draft_ids.push(pid);
                }
            }
            if !draft_ids.is_empty() {
                let drafts = env.records("account.payment")?.browse(draft_ids)?;
                post_payments(tx, &drafts).await?;
            }
            tx.execute(
                "UPDATE account_batch_payment SET state = 'sent', write_uid = $2, \
                 write_date = now() AT TIME ZONE 'UTC' WHERE id = $1 AND state = 'draft'",
                &[&bid, &uid],
            )
            .await?;
            tx.invalidate_records(
                batch.as_records(),
                Some(&["state", "amount", "payment_count"]),
            )
            .await?;
        }
        Ok(())
    }

    /// Marks every Paid payment in a Sent batch as Reconciled and transitions the batch to Reconciled.
    #[method]
    #[rpc(effect = write)]
    pub async fn action_reconcile_batch(
        this: &RecordSet<AccountBatchPayment>,
        tx: &mut Transaction,
    ) -> Result<()> {
        this.as_records().check_access(tx, Operation::Write).await?;
        tx.flush().await?;
        let env = this.env().clone();
        let uid = env.uid();
        for batch in this.iter() {
            let bid = batch.as_records().ids()[0];
            let brows = tx
                .query(
                    "SELECT state::text FROM account_batch_payment WHERE id = $1 FOR UPDATE",
                    &[&bid],
                )
                .await?;
            let Some(brow) = brows.first() else {
                return Err(Error::missing("Batch payment not found."));
            };
            if brow.try_get::<_, String>(0).map_err(db)? != "sent" {
                return Err(Error::user("Only sent batch payments can be reconciled."));
            }
            let rows = tx
                .query(
                    "SELECT id::int8, state::text FROM account_payment \
                     WHERE batch_payment_id = $1 ORDER BY id FOR UPDATE",
                    &[&bid],
                )
                .await?;
            if rows.is_empty() {
                return Err(Error::user("Cannot reconcile an empty batch payment."));
            }
            let mut paid_ids = Vec::new();
            for r in &rows {
                let pid: i64 = r.try_get(0).map_err(db)?;
                let st: String = r.try_get(1).map_err(db)?;
                match st.as_str() {
                    "paid" => paid_ids.push(pid),
                    "in_process" | "posted" => paid_ids.push(pid),
                    "reconciled" => {}
                    other => {
                        return Err(Error::user(format!(
                            "Cannot reconcile batch: payment #{pid} is in status '{other}'."
                        )));
                    }
                }
            }
            if !paid_ids.is_empty() {
                let paid = env.records("account.payment")?.browse(paid_ids)?;
                validate_payments(tx, &paid).await?;
            }
            tx.execute(
                "UPDATE account_batch_payment SET state = 'reconciled', write_uid = $2, \
                 write_date = now() AT TIME ZONE 'UTC' WHERE id = $1 AND state = 'sent'",
                &[&bid, &uid],
            )
            .await?;
            tx.invalidate_records(batch.as_records(), Some(&["state"]))
                .await?;
        }
        Ok(())
    }

    /// Resets a Sent batch back to Draft (reconciled batches remain locked).
    #[method]
    #[rpc(effect = write)]
    pub async fn action_reset_draft(
        this: &RecordSet<AccountBatchPayment>,
        tx: &mut Transaction,
    ) -> Result<()> {
        this.as_records().check_access(tx, Operation::Write).await?;
        tx.flush().await?;
        let uid = this.env().uid();
        for batch in this.iter() {
            let bid = batch.as_records().ids()[0];
            let brows = tx
                .query(
                    "SELECT state::text FROM account_batch_payment WHERE id = $1 FOR UPDATE",
                    &[&bid],
                )
                .await?;
            let Some(brow) = brows.first() else {
                return Err(Error::missing("Batch payment not found."));
            };
            if brow.try_get::<_, String>(0).map_err(db)? != "sent" {
                return Err(Error::user(
                    "Only sent batch payments can be reset to draft.",
                ));
            }
            tx.execute(
                "UPDATE account_batch_payment SET state = 'draft', write_uid = $2, \
                 write_date = now() AT TIME ZONE 'UTC' WHERE id = $1 AND state = 'sent'",
                &[&bid, &uid],
            )
            .await?;
            tx.invalidate_records(batch.as_records(), Some(&["state"]))
                .await?;
        }
        Ok(())
    }
}

/// Converts an exact `Decimal` monetary amount to English words for check printing and
/// amount-in-words display (never uses floating-point arithmetic).
pub fn amount_to_words(amount: Decimal, currency_code: &str) -> String {
    amount_to_words_scaled(amount, currency_code, 2)
}

/// Currency-scale-aware English check amount-in-words formatter (`u128`-safe).
pub fn amount_to_words_scaled(amount: Decimal, currency_code: &str, scale: u32) -> String {
    let clamped_scale = scale.min(6);
    let abs = amount.abs().round_dp(clamped_scale);
    let whole_dec = abs.trunc();
    let factor = 10u128.pow(clamped_scale);
    let frac_dec = if clamped_scale == 0 {
        Decimal::ZERO
    } else {
        ((abs - whole_dec) * Decimal::from(factor)).round_dp(0)
    };
    let whole: u128 = whole_dec.to_string().parse().unwrap_or(0);
    let minor: u128 = frac_dec.to_string().parse().unwrap_or(0);

    let code = currency_code.to_uppercase();
    let (major_sg, major_pl, minor_sg, minor_pl) = match code.as_str() {
        "USD" => ("Dollar", "Dollars", "Cent", "Cents"),
        "EUR" => ("Euro", "Euros", "Cent", "Cents"),
        "GBP" => ("Pound", "Pounds", "Penny", "Pence"),
        "IDR" => ("Rupiah", "Rupiah", "Sen", "Sen"),
        "JPY" => ("Yen", "Yen", "Sen", "Sen"),
        "SGD" => ("Singapore Dollar", "Singapore Dollars", "Cent", "Cents"),
        other if !other.is_empty() => (other, other, "Cent", "Cents"),
        _ => ("Unit", "Units", "Cent", "Cents"),
    };

    let major_word = integer_to_words(whole);
    let major_label = if whole == 1 { major_sg } else { major_pl };
    let prefix = if amount.is_sign_negative() && !abs.is_zero() {
        "Minus "
    } else {
        ""
    };
    if minor == 0 || clamped_scale == 0 {
        format!("{prefix}{major_word} {major_label}")
    } else {
        let minor_word = integer_to_words(minor);
        let minor_label = if minor == 1 { minor_sg } else { minor_pl };
        format!("{prefix}{major_word} {major_label} and {minor_word} {minor_label}")
    }
}

fn integer_to_words(n: u128) -> String {
    if n == 0 {
        return "Zero".to_owned();
    }
    const SCALES: [&str; 7] = [
        "",
        "Thousand",
        "Million",
        "Billion",
        "Trillion",
        "Quadrillion",
        "Quintillion",
    ];
    let mut parts = Vec::new();
    let mut rem = n;
    let mut scale = 0usize;
    while rem > 0 {
        let chunk = (rem % 1000) as u64;
        if chunk > 0 {
            let chunk_str = chunk_to_words(chunk);
            if scale > 0 && scale < SCALES.len() {
                parts.push(format!("{chunk_str} {}", SCALES[scale]));
            } else {
                parts.push(chunk_str);
            }
        }
        rem /= 1000;
        scale += 1;
    }
    parts.reverse();
    parts.join(" ")
}

fn chunk_to_words(n: u64) -> String {
    const ONES: [&str; 20] = [
        "",
        "One",
        "Two",
        "Three",
        "Four",
        "Five",
        "Six",
        "Seven",
        "Eight",
        "Nine",
        "Ten",
        "Eleven",
        "Twelve",
        "Thirteen",
        "Fourteen",
        "Fifteen",
        "Sixteen",
        "Seventeen",
        "Eighteen",
        "Nineteen",
    ];
    const TENS: [&str; 10] = [
        "", "", "Twenty", "Thirty", "Forty", "Fifty", "Sixty", "Seventy", "Eighty", "Ninety",
    ];
    let mut out = Vec::new();
    let hundreds = n / 100;
    let rem = n % 100;
    if hundreds > 0 {
        out.push(format!("{} Hundred", ONES[hundreds as usize]));
    }
    if rem >= 20 {
        let t = TENS[(rem / 10) as usize];
        let o = ONES[(rem % 10) as usize];
        if o.is_empty() {
            out.push(t.to_owned());
        } else {
            out.push(format!("{t}-{o}"));
        }
    } else if rem > 0 {
        out.push(ONES[rem as usize].to_owned());
    }
    out.join(" ")
}

#[cfg(test)]
mod tests {
    use super::amount_to_words;
    use erp_sdk::Decimal;
    use std::str::FromStr;

    #[test]
    fn exact_decimal_amount_to_words_formats_dollars_and_cents() {
        let amt = Decimal::from_str("1234.56").expect("valid decimal");
        assert_eq!(
            amount_to_words(amt, "USD"),
            "One Thousand Two Hundred Thirty-Four Dollars and Fifty-Six Cents"
        );
    }

    #[test]
    fn exact_decimal_amount_to_words_handles_zero_and_idr() {
        assert_eq!(amount_to_words(Decimal::ZERO, "USD"), "Zero Dollars");
        let idr = Decimal::from_str("1500000").expect("valid decimal");
        assert_eq!(
            amount_to_words(idr, "IDR"),
            "One Million Five Hundred Thousand Rupiah"
        );
    }
}
