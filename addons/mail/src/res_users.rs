//! The extension lives here; `base` is not edited.
//!
//! The notification choice (`notification_type`) is stored here and shown in the
//! My Preferences dialog by `views/res_users_views.xml`. No delivery step reads it yet.

use erp_sdk::Operation;
use erp_sdk::prelude::*;

use crate::mail_thread::MailThread;

/// The message of the share-user rule.
const SHARE_REFUSAL: &str = "A portal or public user is notified by e-mail only; choose E-mail.";

/// `res.users` with the thread mixin's messages, followers and follower partners, and the
/// notification choice.
#[extend(ResUsers, addon = "mail", inherit = [MailThread])]
pub struct UsersThread {
    /// How the user wants notifications for document messages delivered: `email` (to their
    /// address) or `inbox` (in the application). Default `email`, applied on create. Not required
    /// at the column, for the reason given on `res.users.color_scheme` (db defaults are
    /// boolean-only): `_check_notification_type_set` refuses an empty value on create and write.
    /// Schema sync adds the column with the default, so existing rows read `email` after an
    /// upgrade (read from `schema.rs`, not run); a row inserted by raw SQL after the column exists
    /// holds no value, which means `email`. `inbox` is refused for share users by
    /// `_check_notification_type_share`, on create and on user-side writes of the choice or the
    /// groups only (see that method).
    /// `user_writeable`: the user's
    /// own delivery choice, which reaches only their own notifications.
    #[field(
        selection,
        selection = [("email", "E-mail"), ("inbox", "Inbox")],
        default = "email",
        string = "Notification",
        help = "Where you are told about new messages on documents you follow.",
        user_writeable
    )]
    pub notification_type: Option<Arc<str>>,
}

#[methods(ResUsers, addon = "mail")]
impl UsersThread {
    #[method(override)]
    async fn _mail_post_access(
        _this: &RecordSet<ResUsers>,
        _tx: &mut Transaction,
        _sup: Super,
    ) -> Result<Operation> {
        Ok(Operation::Read)
    }

    /// A written notification choice is never empty. A row that holds no value
    /// is not touched until it is written.
    #[constrains("notification_type")]
    async fn _check_notification_type_set(
        this: &RecordSet<ResUsers>,
        tx: &mut Transaction,
    ) -> Result<()> {
        let rows = this.as_records().read(tx, &["notification_type"]).await?;
        for row in &rows {
            match row.get("notification_type") {
                Some(Value::Str(key)) if !key.is_empty() => {}
                _ => {
                    return Err(Error::validation(
                        "The notification choice cannot be empty.",
                    ));
                }
            }
        }
        Ok(())
    }

    /// A row with no value counts as `email`.
    ///
    /// Not covered: the `share` flag recomputed from the group side (`recompute_user_share` in
    /// `addons/base/src/group_graph.rs` writes it by SQL and notifies no dependents), and nobody
    /// writes `share` directly, so naming it here would be inert. Such a user can keep `inbox`
    /// until a delivery slice reads the value; that slice must treat share plus `inbox` as `email`.
    #[constrains("notification_type", "group_ids")]
    async fn _check_notification_type_share(
        this: &RecordSet<ResUsers>,
        tx: &mut Transaction,
    ) -> Result<()> {
        let rows = this
            .as_records()
            .read(tx, &["notification_type", "share"])
            .await?;
        for row in &rows {
            let inbox = matches!(
                row.get("notification_type"),
                Some(Value::Str(key)) if &**key == "inbox"
            );
            if !inbox {
                continue;
            }
            match row.get("share") {
                Some(Value::Bool(true)) => return Err(Error::validation(SHARE_REFUSAL)),
                Some(Value::Bool(false) | Value::False) => {}
                other => {
                    return Err(Error::programming(format!(
                        "res.users.share must be a boolean, got {other:?}"
                    )));
                }
            }
        }
        Ok(())
    }
}
