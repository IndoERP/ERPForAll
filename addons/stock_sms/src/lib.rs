// SPDX-License-Identifier: MIT OR Apache-2.0
//! Addon `stock_sms` for ERPForAll: delivery email and SMS confirmation dispatch
//! triggered when outgoing deliveries are validated under company settings
//! `stock.stock_move_email_validation` and `stock.stock_text_confirmation`.

use std::collections::BTreeMap;

use erp_addon_base::ir_config_setting::read_values;
use erp_addon_stock::picking::StockPicking;
use erp_sdk::views::wire::{SettingsScope, WireId};
use erp_sdk::{ErrorKind, GroupValue, Operation, ReadGroup, prelude::*};

erp_sdk::addon!(
    name = "stock_sms",
    exact_only = true,
    depends = ["base", "stock"]
);

const EMAIL_KEY: &str = "stock.stock_move_email_validation";
const SMS_KEY: &str = "stock.stock_text_confirmation";

fn default_now(_ctx: &erp_sdk::DefaultContext) -> Value {
    Value::DateTime(DateTime::now())
}

fn db_err(e: impl std::error::Error + Send + Sync + 'static) -> Error {
    Error::db("Delivery notification data could not be read.").with_source(e)
}

/// `stock.delivery.notification`: audit log of delivery email and SMS confirmations sent
/// to customers upon validating outgoing transfers.
#[model(
    name = "stock.delivery.notification",
    description = "Delivery Notification",
    order = "sent_at desc, id desc",
    company_access = direct(field = "company_id", branch = "exact", empty = "deny")
)]
pub struct StockDeliveryNotification {
    /// Outgoing transfer that triggered this confirmation.
    #[field(
        many2one,
        comodel = "stock.picking",
        required = true,
        ondelete = "cascade",
        index = true,
        string = "Transfer"
    )]
    pub picking_id: Option<RecId>,
    /// Customer notified.
    #[field(many2one, comodel = "res.partner", string = "Recipient Partner")]
    pub partner_id: Option<RecId>,
    /// Owning company.
    #[field(
        many2one,
        comodel = "res.company",
        required = true,
        index = true,
        string = "Company"
    )]
    pub company_id: Option<RecId>,
    /// Delivery confirmation channel (`email` or `sms`).
    #[field(
        selection,
        selection = [("email", "Email"), ("sms", "SMS")],
        required = true,
        string = "Channel"
    )]
    pub channel: Option<Arc<str>>,
    /// Email address or phone number addressed.
    #[field(char, string = "Recipient")]
    pub recipient: Option<Arc<str>>,
    /// Rendered confirmation message body.
    #[field(text, string = "Message")]
    pub message: Option<Arc<str>>,
    /// Dispatch timestamp (UTC).
    #[field(datetime, required = true, default_fn = default_now, string = "Sent On")]
    pub sent_at: Option<DateTime>,
    /// Whether the confirmation was dispatched or skipped (missing contact info).
    #[field(
        selection,
        selection = [("sent", "Sent"), ("skipped", "Skipped")],
        default = "sent",
        required = true,
        string = "Status"
    )]
    pub state: Option<Arc<str>>,
}

async fn confirmation_flags(tx: &mut Transaction, company_id: i64) -> Result<(bool, bool)> {
    let env = tx.env().sudo(true).with_company(company_id)?;
    let wire_company = u64::try_from(company_id)
        .ok()
        .and_then(|id| WireId::try_from(id).ok())
        .ok_or_else(|| Error::validation("Invalid company identifier."))?;
    let read = read_values(
        &env,
        tx,
        SettingsScope::Company {
            company_id: wire_company,
        },
        vec![EMAIL_KEY.to_owned(), SMS_KEY.to_owned()],
    )
    .await?;
    let json = erp_sdk::serde_json::to_value(&read)
        .map_err(|e| Error::internal(format!("delivery confirmation setting read: {e}")))?;
    let values = json["values"].as_array().ok_or_else(|| {
        Error::internal("delivery confirmation settings response missing 'values'")
    })?;
    let flag = |key: &str| -> Result<bool> {
        let entry = values
            .iter()
            .find(|v| v["key"] == key)
            .ok_or_else(|| Error::internal(format!("setting '{key}' missing from response")))?;
        entry["value"]
            .as_bool()
            .ok_or_else(|| Error::internal(format!("setting '{key}' value is not a boolean")))
    };
    Ok((flag(EMAIL_KEY)?, flag(SMS_KEY)?))
}

#[extend(StockPicking, addon = "stock_sms")]
pub struct StockPickingSms {
    /// Count of delivery confirmation notifications logged for this transfer.
    #[field(
        integer,
        compute = "_compute_notification_count",
        string = "Notifications"
    )]
    pub notification_count: i32,
}

#[methods(StockPicking, addon = "stock_sms")]
impl StockPickingSms {
    #[compute(depends = [], depends_context = ["uid", "allowed_company_ids"])]
    async fn _compute_notification_count(
        this: &RecordSet<StockPicking>,
        tx: &mut Transaction,
        out: &mut Assign<'_, StockPicking>,
    ) -> Result<()> {
        let rec_ids = this.as_records().rec_ids();
        let real_ids: Vec<i64> = rec_ids
            .iter()
            .filter_map(|id| id.as_real().map(|r| r.get()))
            .collect();
        let mut counts: BTreeMap<i64, i32> = BTreeMap::new();
        if !real_ids.is_empty() {
            tx.flush().await?;
            let domain = Domain::field("picking_id")?.in_(real_ids);
            let read = this
                .env()
                .records("stock.delivery.notification")?
                ._read_group(
                    tx,
                    ReadGroup::new(domain)
                        .groupby(["picking_id"])
                        .aggregates(["__count"]),
                )
                .await;
            match read {
                Ok(groups) => {
                    for row in groups.rows() {
                        if let (Some(pid), Some(GroupValue::Int(cnt))) = (
                            row.first().and_then(|k| k.record_id().map(|id| id.get())),
                            row.last(),
                        ) {
                            let cnt_i32 = i32::try_from(*cnt).map_err(|_| {
                                Error::internal("Delivery notification count exceeds i32 range.")
                            })?;
                            counts.insert(pid, cnt_i32);
                        }
                    }
                }
                Err(error) if error.kind() == ErrorKind::Access => {}
                Err(error) => return Err(error),
            }
        }
        for (idx, rec_id) in rec_ids.iter().enumerate() {
            let cnt = rec_id
                .as_real()
                .and_then(|id| counts.get(&id.get()).copied())
                .unwrap_or(0);
            out.set(idx, StockPickingSms::NOTIFICATION_COUNT, cnt)?;
        }
        Ok(())
    }

    /// Open delivery confirmation notifications logged for this transfer.
    #[method]
    #[rpc(effect = read)]
    pub async fn action_view_notifications(
        this: &RecordSet<StockPicking>,
        tx: &mut Transaction,
    ) -> Result<Value> {
        let records = this.as_records();
        records.check_access(tx, Operation::Read).await?;
        let id = records.ensure_one()?.int()?;
        let mut action = erp_sdk::ValueMap::new();
        action.insert("type".into(), Value::str("ir.actions.act_window"));
        action.insert("name".into(), Value::str("Delivery Notifications"));
        action.insert(
            "res_model".into(),
            Value::str("stock.delivery.notification"),
        );
        action.insert(
            "views".into(),
            Value::list([
                Value::list([Value::False, Value::str("list")]),
                Value::list([Value::False, Value::str("form")]),
            ]),
        );
        action.insert("target".into(), Value::str("current"));
        action.insert(
            "domain".into(),
            Value::list(vec![Value::list(vec![
                Value::str("picking_id"),
                Value::str("="),
                Value::Int(id),
            ])]),
        );
        Ok(Value::Map(action.into()))
    }

    /// Dispatch delivery email / SMS confirmations when an outgoing transfer finishes validation
    /// across all validation paths (direct, draft, batch, backorder, RPC).
    #[method(override)]
    async fn button_validate(
        this: &RecordSet<StockPicking>,
        tx: &mut Transaction,
        sup: Super,
    ) -> Result<()> {
        sup.call(tx, this.as_records(), ()).await?;
        let ids = this.ids();
        if ids.is_empty() {
            return Ok(());
        }
        tx.flush().await?;
        let rows = tx
            .query(
                "SELECT p.id::int8, p.name, p.company_id::int8, p.partner_id::int8, \
                        p.origin, pt.code, rp.name, rp.email, rp.phone \
                 FROM stock_picking p \
                 JOIN stock_picking_type pt ON pt.id = p.picking_type_id \
                 LEFT JOIN res_partner rp ON rp.id = p.partner_id \
                 WHERE p.id = ANY($1) AND p.state = 'done' AND pt.code = 'outgoing'",
                &[&ids],
            )
            .await?;
        if rows.is_empty() {
            return Ok(());
        }
        let sudo_env = this.env().sudo(true);
        let notif_model = sudo_env.records("stock.delivery.notification")?;
        let mut to_create = Vec::new();
        for row in rows {
            let picking_id: i64 = row.try_get(0).map_err(db_err)?;
            let picking_name: String = row.try_get(1).map_err(db_err)?;
            let company_id: i64 = row.try_get(2).map_err(db_err)?;
            let partner_id: Option<i64> = row.try_get(3).map_err(db_err)?;
            let origin: Option<String> = row.try_get(4).map_err(db_err)?;
            let partner_name: Option<String> = row.try_get(6).map_err(db_err)?;
            let email: Option<String> = row.try_get(7).map_err(db_err)?;
            let phone: Option<String> = row.try_get(8).map_err(db_err)?;

            let (send_email, send_sms) = confirmation_flags(tx, company_id).await?;
            if !send_email && !send_sms {
                continue;
            }
            let body = format_confirmation_message(
                &picking_name,
                origin.as_deref(),
                partner_name.as_deref(),
            );
            for (enabled, channel, recipient) in [
                (send_email, "email", email.as_deref()),
                (send_sms, "sms", phone.as_deref()),
            ] {
                if !enabled {
                    continue;
                }
                let already_logged = !tx
                    .query(
                        "SELECT 1 FROM stock_delivery_notification \
                         WHERE picking_id = $1 AND channel = $2 LIMIT 1",
                        &[&picking_id, &channel],
                    )
                    .await?
                    .is_empty();
                if already_logged {
                    continue;
                }
                let cleaned = recipient.map(str::trim).filter(|s| !s.is_empty());
                let state = if cleaned.is_some() { "sent" } else { "skipped" };
                let mut vals = RawVals::new();
                vals.insert("picking_id".into(), Cell::Id(picking_id));
                if let Some(pid) = partner_id {
                    vals.insert("partner_id".into(), Cell::Id(pid));
                }
                vals.insert("company_id".into(), Cell::Id(company_id));
                vals.insert("channel".into(), Cell::Str(channel.into()));
                vals.insert(
                    "recipient".into(),
                    Cell::Str(cleaned.unwrap_or("(no contact address)").into()),
                );
                vals.insert("message".into(), Cell::Str(body.clone().into()));
                vals.insert("state".into(), Cell::Str(state.into()));
                to_create.push(vals);
            }
        }
        if !to_create.is_empty() {
            notif_model.create(tx, to_create).await?;
        }
        Ok(())
    }
}

/// Format a concise customer-facing delivery confirmation message.
pub fn format_confirmation_message(
    picking_name: &str,
    origin: Option<&str>,
    partner_name: Option<&str>,
) -> String {
    let greeting = partner_name
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|n| format!("Hello {n}, "))
        .unwrap_or_default();
    match origin.map(str::trim).filter(|s| !s.is_empty()) {
        Some(orig) => {
            format!("{greeting}your delivery {picking_name} for order {orig} has been shipped.")
        }
        None => format!("{greeting}your delivery {picking_name} has been shipped."),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_delivery_confirmation_with_and_without_origin() {
        assert_eq!(
            format_confirmation_message("Main/OUT/00010", Some("S00001"), Some("PT Nusantara")),
            "Hello PT Nusantara, your delivery Main/OUT/00010 for order S00001 has been shipped."
        );
        assert_eq!(
            format_confirmation_message("Main/OUT/00011", None, None),
            "your delivery Main/OUT/00011 has been shipped."
        );
    }
}
