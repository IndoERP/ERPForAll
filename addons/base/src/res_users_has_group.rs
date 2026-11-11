//! Anyone may ask about themselves. Asking about another user needs an internal user (a member
//! of `base.group_user`) or superuser mode, so a portal or public user cannot enumerate the
//! rights of the people around them over RPC. The answer is the user's effective groups, implied
//! groups included, from the policy the transaction pinned; `base.group_no_one` counts only when
//! the asking environment is in debug mode. Asking about oneself costs no statement; the first
//! question about another user costs the principal resolver's one statement.

use erp_sdk::prelude::*;

/// The methods of `res.users` that answer group questions (no columns of its own).
#[extend(ResUsers)]
pub struct ResUsersGroupQuestions {}

#[methods(ResUsers)]
impl ResUsersGroupQuestions {
    /// Whether the user is in the group with external id `group_ext_id` (`module.name`).
    ///
    /// # Errors
    ///
    /// `Access` for a non-internal user asking about another user (outside superuser mode);
    /// `Programming` for an external id that is malformed or names no group; the usual
    /// singleton error for anything but one record.
    #[method]
    #[rpc(effect = read)]
    async fn has_group(
        this: &RecordSet<ResUsers>,
        tx: &mut Transaction,
        group_ext_id: String,
    ) -> Result<bool> {
        let user = this.ensure_one()?.ids()[0];
        let env = this.env();
        let own = user == env.uid();
        if !(env.su() || own || tx.has_group(env, "base.group_user").await?) {
            return Err(Error::access(
                "You can ony call user.has_group() with your current user.",
            ));
        }
        let subject = if own {
            env.clone()
        } else {
            env.with_user(user).with_debug(env.principal().debug())
        };
        tx.has_group(&subject, &group_ext_id).await
    }
}
