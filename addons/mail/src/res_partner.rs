//! The extension lives here; `base` is not edited. Inside one addon, declarations compose in
//! source order (file path, then line), so this file sorts after `mail_thread.rs`, which defines
//! the mixin.

use erp_addon_base::ResPartner;
use erp_sdk::Operation;
use erp_sdk::prelude::*;

use crate::mail_thread::MailThread;

/// `res.partner` with the thread mixin's messages, followers and follower partners.
#[extend(ResPartner, addon = "mail", inherit = [MailThread])]
pub struct PartnerThread {}

#[methods(ResPartner, addon = "mail")]
impl PartnerThread {
    #[method(override)]
    async fn _mail_post_access(
        _this: &RecordSet<ResPartner>,
        _tx: &mut Transaction,
        _sup: Super,
    ) -> Result<Operation> {
        Ok(Operation::Read)
    }
}
