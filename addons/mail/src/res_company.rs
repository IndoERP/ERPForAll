//! Companies get a chatter: `res.company` inherits the thread mixin.
//! The extension lives here; `base` is not edited.

use erp_addon_base::ResCompany;
use erp_sdk::Operation;
use erp_sdk::prelude::*;

use crate::mail_thread::MailThread;

/// `res.company` with the thread mixin's messages, followers and follower partners.
#[extend(ResCompany, addon = "mail", inherit = [MailThread])]
pub struct CompanyThread {}

#[methods(ResCompany, addon = "mail")]
impl CompanyThread {
    /// Posting on a company needs read access.
    #[method(override)]
    async fn _mail_post_access(
        _this: &RecordSet<ResCompany>,
        _tx: &mut Transaction,
        _sup: Super,
    ) -> Result<Operation> {
        Ok(Operation::Read)
    }
}
