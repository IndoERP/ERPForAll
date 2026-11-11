//! User/contact linkage for native bootstrap, without changing legacy user rows.

use erp_sdk::prelude::*;

/// The contact backing a user identity.
#[extend(ResUsers)]
pub struct ResUsersPartner {
    /// Nullable for pre-existing users. New administrator initialization supplies a real
    /// partner in the same activation transaction; deleting a linked contact is restricted.
    /// This is not self-writable and does not delegate or mirror user fields yet.
    #[field(many2one, comodel = "res.partner", ondelete = "restrict", index = true)]
    pub partner_id: Option<RecId>,
}
