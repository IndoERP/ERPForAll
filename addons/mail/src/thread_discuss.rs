// SPDX-License-Identifier: MIT OR Apache-2.0
//! Company-scoped, invitation-only conversations. Membership is the single source of read access.

use crate::MailThread;
use erp_sdk::prelude::*;
use erp_sdk::serde_json::json;
use erp_sdk::{Context, Operation, SearchOptions};

pub(crate) const MAX_MEMBERS: usize = 100;
const PAGE: u64 = 50;
mod notifications;
mod workspace;

#[model(name = "discuss.channel", description = "Conversation", inherit = [MailThread], addon = "mail",
    company_access = direct(field = "company_id", branch = "exact", empty = "deny"))]
pub struct DiscussChannel {
    #[field(char, required = true)]
    pub name: Option<Arc<str>>,
    /// Addons extend this selection and override conversation methods on the same channel model.
    #[field(selection, selection = [("group", "Group"),("direct","Direct message")], default = "group", required = true)]
    pub channel_type: Option<Arc<str>>,
    #[field(
        many2one,
        comodel = "res.company",
        required = true,
        ondelete = "cascade",
        index = true
    )]
    pub company_id: Option<RecId>,
    #[field(
        one2many,
        comodel = "discuss.channel.member",
        inverse_name = "channel_id"
    )]
    pub member_ids: Vec<RecId>,
}

/// Private rendezvous for concurrent DM creation. Only the invitation workflow uses this model.
#[model(name = "discuss.channel.pair", description = "Direct conversation pair", addon = "mail",
    company_access = direct(field = "company_id", branch = "exact", empty = "deny"),
    keys = [key(name = "_direct_pair", fields = ["company_id", "first_user_id", "second_user_id"],
        message = "A direct conversation already exists for these participants.")])]
pub struct DiscussChannelPair {
    #[field(
        many2one,
        comodel = "res.company",
        required = true,
        ondelete = "cascade"
    )]
    pub company_id: Option<RecId>,
    #[field(many2one, comodel = "res.users", required = true, ondelete = "cascade")]
    pub first_user_id: Option<RecId>,
    #[field(many2one, comodel = "res.users", required = true, ondelete = "cascade")]
    pub second_user_id: Option<RecId>,
    #[field(
        many2one,
        comodel = "discuss.channel",
        ondelete = "cascade",
        index = true
    )]
    pub channel_id: Option<RecId>,
}

#[model(name = "discuss.channel.member", description = "Conversation Member", addon = "mail",
    company_access = parent(edge = "channel_id"),
    keys = [key(name = "_channel_user", fields = ["channel_id", "user_id"], message = "A user already belongs to this conversation.")])]
pub struct DiscussChannelMember {
    #[field(
        many2one,
        comodel = "discuss.channel",
        required = true,
        ondelete = "cascade",
        index = true
    )]
    pub channel_id: Option<RecId>,
    #[field(
        many2one,
        comodel = "res.users",
        required = true,
        ondelete = "cascade",
        index = true
    )]
    pub user_id: Option<RecId>,
    #[field(boolean, default = false)]
    pub is_owner: bool,
    // A high-water mark must survive deletion of the message it acknowledged.
    #[field(bigint, default = 0)]
    pub seen_message_id: erp_sdk::BigInt,
}

async fn internal(tx: &mut Transaction, env: &Env) -> Result<()> {
    if !env.su() && !tx.has_group(env, "base.group_user").await? {
        return Err(Error::access("Conversations require an internal user."));
    }
    Ok(())
}

async fn wake(tx: &mut Transaction) -> Result<()> {
    tx.notify_once(&erp_sdk::NotifyChannel::new("erp_thread_changed")?)
        .await
        .map(|_| ())
}

fn channel_id(this: &RecordSet<DiscussChannel>) -> Result<i64> {
    Ok(this.as_records().ensure_one()?.ids()[0])
}

async fn add_member(
    tx: &mut Transaction,
    env: &Env,
    channel: i64,
    uid: i64,
    owner: bool,
) -> Result<()> {
    let members = env
        .with_context_replaced(Context::new())
        .sudo(true)
        .records("discuss.channel.member")?;
    members
        .create(
            tx,
            vec![
                RawVals::new()
                    .set_cell("channel_id", Cell::Id(channel))
                    .set_cell("user_id", Cell::Id(uid))
                    .set_cell("is_owner", Cell::Bool(owner)),
            ],
        )
        .await?;
    wake(tx).await
}

/// Freeze the post audience before creating its row, so the initial reply and a replay
/// report the same recipients. Explicit recipients must already belong to the channel.
pub(crate) async fn recipient_partners(
    tx: &mut Transaction,
    channel: i64,
    uid: i64,
    author: i64,
    explicit: &[i64],
) -> Result<Vec<i64>> {
    // A departure removes this row. Serialize it with the last post by this participant.
    if tx.query("SELECT id FROM discuss_channel_member WHERE channel_id=$1::bigint AND user_id=$2::bigint FOR KEY SHARE", &[&channel, &uid]).await?.is_empty() {
        return Err(Error::access("Current membership is required to post in a conversation."));
    }
    let rows = tx
        .query(
            "SELECT DISTINCT p.id::bigint FROM discuss_channel_member cm
        JOIN discuss_channel c ON c.id=cm.channel_id JOIN res_users u ON u.id=cm.user_id
        JOIN res_company_users_rel co ON co.user_id=u.id AND co.cid=c.company_id
        JOIN res_partner p ON p.id=u.partner_id WHERE cm.channel_id=$1::bigint
        AND u.active AND NOT u.share AND p.active ORDER BY p.id::bigint",
            &[&channel],
        )
        .await?;
    let mut partners = Vec::with_capacity(rows.len());
    for row in rows {
        partners.push(
            row.try_get::<_, i64>(0)
                .map_err(|_| Error::invariant("Invalid conversation recipient."))?,
        );
    }
    if explicit
        .iter()
        .any(|id| partners.binary_search(id).is_err())
    {
        return Err(Error::access(
            "Only current participants can be addressed in a conversation.",
        ));
    }
    partners.retain(|id| *id != author);
    Ok(partners)
}

#[methods(DiscussChannel, addon = "mail")]
impl DiscussChannel {
    #[method(override, sealed)]
    async fn _mail_post_access(
        _this: &RecordSet<DiscussChannel>,
        _tx: &mut Transaction,
        _sup: Super,
    ) -> Result<Operation> {
        Ok(Operation::Read)
    }

    /// The current user's most recent in-app deliveries, with current message visibility reapplied.
    #[method]
    #[rpc(effect = read, model)]
    async fn messaging_inbox(
        this: &RecordSet<DiscussChannel>,
        tx: &mut Transaction,
    ) -> Result<Value> {
        internal(tx, this.env()).await?;
        let rows = visible_notifications(tx, this.env()).await?;
        Ok(Value::Json(Arc::new(json!(rows))))
    }

    #[method]
    #[rpc(effect = read)]
    async fn conversation_receipts(
        this: &RecordSet<DiscussChannel>,
        tx: &mut Transaction,
    ) -> Result<Value> {
        let channel = channel_id(this)?;
        this.as_records().check_access(tx, Operation::Read).await?;
        let env = this.env();
        let rows = tx.query(
            "SELECT m.user_id::bigint, COALESCE(m.seen_message_id,0)::bigint FROM discuss_channel_member m
             JOIN discuss_channel c ON c.id=m.channel_id JOIN discuss_channel_member me ON me.channel_id=c.id
             WHERE c.id=$1::bigint AND me.user_id=$2::bigint AND c.company_id=$3::bigint ORDER BY m.id LIMIT 100",
            &[&channel, &env.uid(), &env.company_id()?],
        ).await?;
        let mut receipts = Vec::new();
        for row in rows {
            let user: i64 = row
                .try_get(0)
                .map_err(|_| Error::invariant("Invalid member."))?;
            let seen: i64 = row
                .try_get(1)
                .map_err(|_| Error::invariant("Invalid receipt."))?;
            receipts.push(json!({"user":user,"seen":seen}));
        }
        Ok(Value::Json(Arc::new(json!(receipts))))
    }

    #[method]
    #[rpc(effect = write, model)]
    async fn conversation_create(
        this: &RecordSet<DiscussChannel>,
        tx: &mut Transaction,
        name: String,
        channel_type: Option<String>,
    ) -> Result<i64> {
        let env = this.env();
        internal(tx, env).await?;
        if channel_type.as_deref() == Some("direct") {
            return Err(Error::validation(
                "Select a person to start a direct message.",
            ));
        }
        let name = name.trim();
        if name.is_empty() || name.len() > 120 {
            return Err(Error::validation(
                "A conversation name needs 1 to 120 bytes.",
            ));
        }
        let company = env
            .company_id()?
            .ok_or_else(|| Error::validation("Select a company for the conversation."))?;
        let channel = env
            .with_context_replaced(Context::new())
            .sudo(true)
            .records("discuss.channel")?
            .create(
                tx,
                vec![
                    RawVals::new()
                        .set_cell(
                            "channel_type",
                            Cell::Str(channel_type.as_deref().unwrap_or("group").into()),
                        )
                        .set_cell("name", Cell::Str(name.into()))
                        .set_cell("company_id", Cell::Id(company)),
                ],
            )
            .await?
            .ids()[0];
        add_member(tx, env, channel, env.uid(), true).await?;
        Ok(channel)
    }

    #[method]
    #[rpc(effect = write)]
    async fn conversation_invite(
        this: &RecordSet<DiscussChannel>,
        tx: &mut Transaction,
        user_id: i64,
    ) -> Result<()> {
        let channel = channel_id(this)?;
        this.as_records().check_access(tx, Operation::Read).await?;
        let env = this.env();
        internal(tx, env).await?;
        tx.flush().await?;
        let rows = tx.query("UPDATE discuss_channel c SET name=c.name FROM discuss_channel_member m WHERE m.channel_id=c.id AND c.id=$1::bigint AND m.user_id=$2::bigint AND m.is_owner AND c.channel_type IS DISTINCT FROM 'direct' RETURNING c.company_id::bigint", &[&channel, &env.uid()]).await?;
        let Some(row) = rows.first() else {
            return Err(Error::access(
                "Only a current group conversation owner can invite members.",
            ));
        };
        let company: i64 = row
            .try_get(0)
            .map_err(|_| Error::invariant("Invalid channel company."))?;
        let eligible = tx.query("SELECT u.partner_id::bigint FROM res_users u JOIN res_company_users_rel r ON r.user_id=u.id WHERE u.id=$1::bigint AND u.active AND NOT u.share AND r.cid=$2::bigint", &[&user_id,&company]).await?;
        if eligible.is_empty() {
            return Err(Error::validation(
                "Invite an active internal user of this company.",
            ));
        }
        let partner: i64 = eligible[0]
            .try_get(0)
            .map_err(|_| Error::validation("An invited user needs a contact."))?;
        env.records("res.partner")?
            .browse([partner])?
            .check_access(tx, Operation::Read)
            .await?;
        let members = tx.query("SELECT user_id::bigint FROM discuss_channel_member WHERE channel_id=$1::bigint LIMIT 101", &[&channel]).await?;
        for member in &members {
            if member
                .try_get::<_, i64>(0)
                .map_err(|_| Error::invariant("Invalid member user."))?
                == user_id
            {
                return Ok(());
            }
        }
        if members.len() >= MAX_MEMBERS {
            return Err(Error::validation("A conversation holds up to 100 members."));
        }
        add_member(tx, env, channel, user_id, false).await
    }

    #[method]
    #[rpc(effect = write)]
    async fn conversation_leave(
        this: &RecordSet<DiscussChannel>,
        tx: &mut Transaction,
    ) -> Result<()> {
        let id = channel_id(this)?;
        this.as_records().check_access(tx, Operation::Read).await?;
        let uid = this.env().uid();
        tx.flush().await?;
        // Serialize owner succession with invitations and other departures.
        let group=tx.query(
            "UPDATE discuss_channel SET name=name WHERE id=$1::bigint AND channel_type IS DISTINCT FROM 'direct' RETURNING id",
            &[&id],
        ).await?;
        if group.is_empty() {
            return Err(Error::validation(
                "Direct conversations have a fixed pair of participants.",
            ));
        }
        let deleted = tx.query("DELETE FROM discuss_channel_member WHERE channel_id=$1::bigint AND user_id=$2::bigint RETURNING is_owner", &[&id,&uid]).await?;
        let member = deleted
            .first()
            .ok_or_else(|| Error::access("Conversation membership is required."))?;
        if member
            .try_get::<_, bool>(0)
            .map_err(|_| Error::invariant("Invalid conversation owner."))?
        {
            tx.execute("UPDATE discuss_channel_member SET is_owner=true WHERE id=(SELECT id FROM discuss_channel_member WHERE channel_id=$1::bigint ORDER BY id LIMIT 1)", &[&id]).await?;
        }
        wake(tx).await
    }

    #[method]
    #[rpc(effect = write)]
    async fn conversation_seen(
        this: &RecordSet<DiscussChannel>,
        tx: &mut Transaction,
        message_id: i64,
    ) -> Result<()> {
        let channel = checked_message(this, tx, message_id).await?;
        tx.execute("UPDATE discuss_channel_member SET seen_message_id=GREATEST(COALESCE(seen_message_id,0),$3::bigint) WHERE channel_id=$1::bigint AND user_id=$2::bigint", &[&channel,&this.env().uid(),&message_id]).await?;
        wake(tx).await
    }

    #[method]
    #[rpc(effect = write)]
    async fn conversation_bookmark(
        this: &RecordSet<DiscussChannel>,
        tx: &mut Transaction,
        message_id: i64,
        saved: bool,
    ) -> Result<()> {
        checked_message(this, tx, message_id).await?;
        let uid = this.env().uid();
        if saved {
            tx.execute("INSERT INTO mail_message_res_partner_bookmarked_rel (mail_message_id,res_partner_id) SELECT $1::bigint,partner_id FROM res_users WHERE id=$2::bigint ON CONFLICT DO NOTHING", &[&message_id,&uid]).await?;
        } else {
            tx.execute("DELETE FROM mail_message_res_partner_bookmarked_rel WHERE mail_message_id=$1::bigint AND res_partner_id=(SELECT partner_id FROM res_users WHERE id=$2::bigint)", &[&message_id,&uid]).await?;
        }
        wake(tx).await
    }

    #[method]
    #[rpc(effect = read, model)]
    async fn conversation_list(
        this: &RecordSet<DiscussChannel>,
        tx: &mut Transaction,
        after: Option<i64>,
    ) -> Result<Value> {
        let snapshot = snapshot(tx, this.env(), after.unwrap_or(0)).await?;
        Ok(Value::Json(Arc::new(json!(snapshot))))
    }
}

async fn checked_message(
    this: &RecordSet<DiscussChannel>,
    tx: &mut Transaction,
    message_id: i64,
) -> Result<i64> {
    let channel = channel_id(this)?;
    this.as_records().check_access(tx, Operation::Read).await?;
    this.env()
        .records("mail.message")?
        .browse([message_id])?
        .check_access(tx, Operation::Read)
        .await?;
    tx.flush().await?;
    let env = this.env();
    let messages = tx.query(
        "SELECT 1 FROM mail_message msg JOIN discuss_channel c ON msg.model='discuss.channel' AND msg.res_id=c.id
         JOIN discuss_channel_member m ON m.channel_id=c.id AND m.user_id=$3::bigint
         WHERE msg.id=$1::bigint AND c.id=$2::bigint AND c.company_id=$4::bigint AND msg.message_type<>'user_notification'
         FOR KEY SHARE OF m",
        &[&message_id, &channel, &env.uid(), &env.company_id()?],
    ).await?;
    if messages.is_empty() {
        return Err(Error::validation(
            "Select a visible message in your current conversation.",
        ));
    }
    Ok(channel)
}

async fn snapshot(
    tx: &mut Transaction,
    env: &Env,
    after: i64,
) -> Result<Vec<erp_sdk::serde_json::Value>> {
    internal(tx, env).await?;
    if after < 0 {
        return Err(Error::validation(
            "The conversation cursor must be nonnegative.",
        ));
    }
    let domain = Domain::condition("id", Operator::Gt, after)?;
    let channels = env
        .records("discuss.channel")?
        .search_with(
            tx,
            &domain,
            &SearchOptions {
                limit: Some(PAGE),
                order: Some("id asc".into()),
                ..SearchOptions::default()
            },
        )
        .await?
        .ids();
    tx.flush().await?;
    let rows = tx.query("SELECT c.id::bigint,c.name::text,COALESCE(m.seen_message_id,0)::bigint,m.is_owner, \
        (SELECT max(id)::bigint FROM mail_message WHERE model='discuss.channel' AND res_id=c.id AND message_type<>'user_notification'), \
        (SELECT count(*) FROM (SELECT 1 FROM mail_message WHERE model='discuss.channel' AND res_id=c.id AND message_type<>'user_notification' AND author_id IS DISTINCT FROM u.partner_id AND id>COALESCE(m.seen_message_id,0) LIMIT 1000) unseen), \
        (SELECT count(*) FROM (SELECT 1 FROM mail_message_res_partner_bookmarked_rel b JOIN mail_message msg ON msg.id=b.mail_message_id WHERE b.res_partner_id=u.partner_id AND msg.model='discuss.channel' AND msg.res_id=c.id LIMIT 1000) saved) \
        FROM discuss_channel c JOIN discuss_channel_member m ON m.channel_id=c.id JOIN res_users u ON u.id=m.user_id WHERE c.id=ANY($1::bigint[]) AND u.id=$2::bigint ORDER BY c.id", &[&channels,&env.uid()]).await?;
    rows.iter().map(|r| {
        let invalid = |_| Error::invariant("Invalid conversation summary.");
        Ok(json!({"id":r.try_get::<_,i64>(0).map_err(invalid)?,"name":r.try_get::<_,String>(1).map_err(invalid)?,"seen":r.try_get::<_,i64>(2).map_err(invalid)?,"owner":r.try_get::<_,bool>(3).map_err(invalid)?,"latest":r.try_get::<_,Option<i64>>(4).map_err(invalid)?,"unread":r.try_get::<_,i64>(5).map_err(invalid)?,"bookmarks":r.try_get::<_,i64>(6).map_err(invalid)?}))
    }).collect()
}

/// A payload-free websocket rechecks the authenticated user’s page in a fresh transaction.
pub(crate) async fn change_window(tx: &mut Transaction, page: i64) -> Result<Vec<i64>> {
    let env = tx.env();
    // The workspace uses an explicit sentinel to track its recent-first list.
    if page == 9_007_199_254_740_991 {
        let mut window = workspace::recent_window(tx, &env).await?;
        window.push(-1);
        notifications::extend_window(tx, &env, &mut window).await?;
        return Ok(window);
    }
    let mut window = Vec::new();
    for row in snapshot(
        tx,
        &env,
        page.checked_sub(1)
            .ok_or_else(|| Error::validation("Invalid conversation page."))?,
    )
    .await?
    {
        let owner = row["owner"]
            .as_bool()
            .ok_or_else(|| Error::invariant("Invalid conversation owner."))?;
        window.push(i64::from(owner));
        for key in ["id", "seen", "latest", "unread", "bookmarks"] {
            window.push(if key == "latest" && row[key].is_null() {
                0
            } else {
                row[key]
                    .as_i64()
                    .ok_or_else(|| Error::invariant("Invalid conversation window."))?
            });
        }
    }
    window.push(-1);
    for notice in visible_notifications(tx, &env).await? {
        window.push(
            notice["id"]
                .as_i64()
                .ok_or_else(|| Error::invariant("Invalid notification id."))?,
        );
    }
    Ok(window)
}

async fn visible_notifications(
    tx: &mut Transaction,
    env: &Env,
) -> Result<Vec<erp_sdk::serde_json::Value>> {
    let rows = tx.query(
        "SELECT n.id::bigint,n.message_id::bigint,n.transport::text,msg.model::text,msg.res_id::bigint,left(msg.subject::text,160)
         FROM mail_notification n JOIN res_users u ON u.partner_id=n.partner_id JOIN mail_message msg ON msg.id=n.message_id
         WHERE u.id=$1::bigint AND n.transport IN ('inbox','push') AND n.state='delivered'
           AND (msg.model IS DISTINCT FROM 'discuss.channel' OR EXISTS (
               SELECT FROM discuss_channel c JOIN discuss_channel_member cm ON cm.channel_id=c.id
               WHERE c.id=msg.res_id AND c.company_id=$2::bigint AND cm.user_id=u.id))
         ORDER BY n.id DESC LIMIT 50",
        &[&env.uid(), &env.company_id()?],
    ).await?;
    let bad = || Error::invariant("Invalid delivery notification.");
    let mut notices = Vec::with_capacity(rows.len());
    for row in rows {
        notices.push((
            row.try_get::<_, i64>(0).map_err(|_| bad())?,
            row.try_get::<_, i64>(1).map_err(|_| bad())?,
            row.try_get::<_, String>(2).map_err(|_| bad())?,
            row.try_get::<_, Option<String>>(3).map_err(|_| bad())?,
            row.try_get::<_, Option<i64>>(4).map_err(|_| bad())?,
            row.try_get::<_, Option<String>>(5).map_err(|_| bad())?,
        ));
    }
    let messages = workspace::readable(
        tx,
        env,
        "mail.message",
        notices.iter().map(|r| r.1).collect(),
    )
    .await?;
    notices.retain(|v| messages.contains(&v.1));
    let mut models = std::collections::BTreeMap::<String, Vec<i64>>::new();
    for (_, _, _, model, record, _) in &notices {
        if let (Some(model), Some(record)) = (model, record) {
            models.entry(model.clone()).or_default().push(*record);
        }
    }
    let mut readable_models = std::collections::BTreeMap::new();
    for (model, ids) in models {
        let visible = workspace::readable(tx, env, &model, ids).await?;
        readable_models.insert(model, visible);
    }
    let see_subject = !notices.is_empty()
        && tx
            .has_field_access(env, "mail.message", "subject", erp_sdk::FieldOp::Read)
            .await?;
    Ok(notices
        .into_iter()
        .map(|(id, message, transport, model, record, subject)| {
            let available = model
                .as_ref()
                .and_then(|name| readable_models.get(name))
                .is_some_and(|ids| record.is_some_and(|id| ids.contains(&id)));
            let channel =
                record.filter(|_| available && model.as_deref() == Some("discuss.channel"));
            json!({
                "id":id, "message":message, "transport":transport, "channel_id":channel,
                "model":model.filter(|_| available), "res_id":record.filter(|_| available),
                "subject":subject.filter(|_| see_subject),
            })
        })
        .collect())
}
