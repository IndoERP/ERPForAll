//! A thread model of the `test_mail` addon used only by the post tests: not flat, and recording
//! the post hook's call in its name. Kept out of `support` so the follower tests' statement
//! ceilings, which count one owner read per model inheriting the mixin, see the same models as
//! before. It also holds the `ir.model.data` stand-in the post path needs.
#![allow(dead_code, reason = "declaration types are never constructed")]

use erp_addon_mail::MailThread;
use erp_sdk::prelude::*;

use crate::support::text;

/// A test stand-in for `ir.model.data` with the columns `message_post` reads (module, name, model,
/// res_id). The real model is declared by `erp-data`, which an addon crate may not depend on, even
/// for tests (`scripts/check-layers.py`). The real install path of a post is covered
/// only by one smoke test (`crates/erp-cli/tests/mail_install.rs`,
/// `a_post_after_a_real_install_is_a_note`); the other post cases are NOT tested on a real install
/// yet (open item).
#[model(
    name = "ir.model.data",
    description = "Model Data",
    addon = "test_mail"
)]
pub struct ModelData {
    #[field(char, required = true)]
    pub name: Option<Arc<str>>,
    #[field(char, required = true)]
    pub model: Option<Arc<str>>,
    #[field(char, required = true)]
    pub module: Option<Arc<str>>,
    #[field(many2one_reference, model_field = "model")]
    pub res_id: i64,
}

/// A thread model that is not flat and records the post hook's call in its name.
#[model(
    name = "test_mail.chat",
    description = "Mail Test Chat",
    inherit = [MailThread],
    addon = "test_mail"
)]
pub struct Chat {
    #[field(char)]
    pub name: Option<Arc<str>>,
}

#[methods(Chat, addon = "test_mail")]
impl Chat {
    #[method(override)]
    async fn _mail_flat_thread(
        _this: &RecordSet<Chat>,
        _tx: &mut Transaction,
        _sup: Super,
    ) -> Result<bool> {
        Ok(false)
    }

    /// Writes `after hook: <message id>` into the record's name.
    #[method(override)]
    async fn _message_post_after_hook(
        this: &RecordSet<Chat>,
        tx: &mut Transaction,
        message_id: i64,
        sup: Super,
    ) -> Result<()> {
        sup.call(tx, this.as_records(), (message_id,)).await?;
        this.as_records()
            .write(
                tx,
                RawVals::new().set_cell("name", text(&format!("after hook: {message_id}"))),
            )
            .await
    }
}
