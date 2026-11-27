//! The extension lives here; `base` is not edited. Inside one addon, declarations compose in
//! source order (file path, then line), so this file sorts after `mail_thread.rs`, which defines
//! the mixin.

use erp_addon_base::ResPartner;
use erp_sdk::Operation;
use erp_sdk::prelude::*;

use crate::activity_mixin::MailActivityMixin;
use crate::mail_thread::MailThread;

/// `res.partner` with the thread and activity mixins' messages, followers, and scheduled activities.
#[extend(ResPartner, addon = "mail", inherit = [MailThread, MailActivityMixin])]
pub struct PartnerThread {}

#[methods(ResPartner, addon = "mail")]
impl PartnerThread {
    #[method(override, sealed)]
    async fn _mail_post_access(
        _this: &RecordSet<ResPartner>,
        _tx: &mut Transaction,
        _sup: Super,
    ) -> Result<Operation> {
        Ok(Operation::Read)
    }
}
