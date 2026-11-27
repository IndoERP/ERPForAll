// SPDX-License-Identifier: MIT OR Apache-2.0
//! Durable recipient receipts. The outbox consumer writes these with its acknowledgement.

use erp_sdk::prelude::*;

#[model(name = "mail.notification", description = "Message Delivery", addon = "mail",
    company_access = parent(edge = "partner_id"),
    keys = [key(name = "_recipient_transport", fields = ["message_id", "partner_id", "transport"], message = "This message already has a delivery for this recipient and transport.")],
    indexes = [
        ("_pending", "(state, transport, id)"),
        ("_in_app_unread_message", "(partner_id, message_id DESC) WHERE transport IN ('inbox','push') AND state='delivered' AND inbox_read IS NOT TRUE"),
        ("_in_app_read_message", "(partner_id, message_id DESC) WHERE transport IN ('inbox','push') AND state='delivered' AND inbox_read IS TRUE")
    ])]
pub struct MailNotification {
    #[field(
        many2one,
        comodel = "mail.message",
        required = true,
        ondelete = "cascade",
        index = true
    )]
    pub message_id: Option<RecId>,
    #[field(
        many2one,
        comodel = "res.partner",
        required = true,
        ondelete = "cascade",
        index = true
    )]
    pub partner_id: Option<RecId>,
    #[field(selection, selection = [("email", "E-mail"), ("inbox", "Inbox"), ("push", "Push")], required = true)]
    pub transport: Option<Arc<str>>,
    #[field(selection, selection = [("pending", "Pending"), ("delivered", "Delivered"), ("failed", "Failed"), ("revoked", "Revoked")], required = true)]
    pub state: Option<Arc<str>>,
    /// The recipient acknowledged this message in Discuss.
    #[field(boolean, default = false)]
    pub inbox_read: bool,
}

/// The fan-out request is eventful. History-only messages and ordinary chatter reads incur no
/// outbox work. The native host installs this hook next to the chatter history hook.
#[model(name = "mail.delivery.request", description = "Message Fan-out Request", addon = "mail", eventful = true,
    keys = [key(name = "_message", fields = ["message_id"], message = "A message has one fan-out request.")])]
pub struct MailDeliveryRequest {
    #[field(
        many2one,
        comodel = "mail.message",
        required = true,
        ondelete = "cascade"
    )]
    pub message_id: Option<RecId>,
}

#[derive(Default)]
struct Posted(std::collections::BTreeSet<i64>);

pub(crate) fn posted(tx: &mut Transaction, id: i64) -> Result<()> {
    let pending = &mut tx.extension::<Posted>().0;
    if pending.len() >= 4096 && !pending.contains(&id) {
        return Err(Error::new(
            erp_sdk::ErrorKind::ResourceExhausted,
            "A transaction may fan out up to 4096 messages.",
        ));
    }
    pending.insert(id);
    Ok(())
}

/// Register on every runtime that delivers notifications from chatter or scheduled posts.
pub const DELIVERY_HOOK: erp_sdk::PrecommitHook =
    erp_sdk::PrecommitHook::new("mail.delivery", persist_requests);

fn persist_requests(tx: &mut Transaction) -> erp_sdk::BoxFuture<'_, Result<()>> {
    Box::pin(async move {
        let ids: Vec<_> = std::mem::take(&mut tx.extension::<Posted>().0)
            .into_iter()
            .collect();
        if ids.is_empty() {
            return Ok(());
        }
        // A saved-point post may have rolled back or a caller may have deleted its message.
        let rows = tx
            .query(
                "SELECT id::bigint FROM mail_message WHERE id=ANY($1::bigint[]) ORDER BY id",
                &[&ids],
            )
            .await?;
        let mut values = Vec::with_capacity(rows.len());
        for row in rows {
            let id: i64 = row
                .try_get(0)
                .map_err(|_| Error::invariant("Invalid fan-out message."))?;
            values.push(RawVals::new().set_cell("message_id", Cell::Id(id)));
        }
        if !values.is_empty() {
            tx.env()
                .with_context_replaced(erp_sdk::Context::new())
                .sudo(true)
                .records("mail.delivery.request")?
                .create(tx, values)
                .await?;
        }
        Ok(())
    })
}
