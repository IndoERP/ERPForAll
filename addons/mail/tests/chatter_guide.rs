//! Compiles the example "Change what a thread shows" (kept in sync by
//! hand: the guide says so).
#![allow(dead_code, reason = "the guide's example")]

use erp_addon_mail::{MailThread, ThreadFacts};
use erp_sdk::prelude::*;

erp_sdk::addon!(name = "helpdesk", depends = ["base", "mail"]);

#[model(name = "helpdesk.ticket", description = "Ticket", inherit = [MailThread])]
pub struct Ticket {
    #[field(char, required = true)]
    pub name: Option<Arc<str>>,
    #[field(char)]
    pub priority: Option<Arc<str>>,
}

#[methods(Ticket, addon = "helpdesk")]
impl Ticket {
    /// Customers also see the automated notifications posted on a ticket.
    #[method(override)]
    async fn _get_customer_portal_message_types(
        this: &RecordSet<Ticket>,
        tx: &mut Transaction,
        sup: Super,
    ) -> Result<Vec<String>> {
        let mut types: Vec<String> = sup.call(tx, this.as_records(), ()).await?;
        types.push("notification".to_owned());
        Ok(types)
    }

    /// The thread facts of a ticket carry its priority (the `thread` topic's `data.thread`).
    #[method(override)]
    async fn _store_thread_fields(
        this: &RecordSet<Ticket>,
        tx: &mut Transaction,
        facts: ThreadFacts,
        sup: Super,
    ) -> Result<ThreadFacts> {
        let mut facts: ThreadFacts = sup.call(tx, this.as_records(), (facts,)).await?;
        let rows = this
            .as_records()
            .read_with(tx, &["priority"], erp_sdk::Load::Raw)
            .await?;
        let priority = match rows.first().and_then(|row| row.get("priority")) {
            Some(Value::Str(priority)) => priority.to_string(),
            Some(Value::False) => String::new(),
            other => return Err(Error::invariant(format!("priority is not text: {other:?}"))),
        };
        facts.set("priority", erp_sdk::serde_json::json!(priority))?;
        Ok(facts)
    }
}

#[test]
fn the_guide_example_registers() {
    use erp_sdk::Model as _;
    assert_eq!(Ticket::MODEL_NAME, "helpdesk.ticket");
}
