// SPDX-License-Identifier: MIT OR Apache-2.0
//! Personal notifications use delivery receipts. Conversation unread state stays in membership.
use super::*;

const INBOX_SIZE: usize = 50;
const COUNT_LIMIT: usize = 1000;
const SCAN_SIZE: usize = 100;

struct Notice {
    message: i64,
    model: Option<String>,
    record: Option<i64>,
    subject: Option<String>,
    unread: bool,
}

#[methods(DiscussChannel, addon = "mail")]
impl DiscussChannel {
    /// Recent personal notifications, with unread entries first and one row per message.
    #[method]
    #[rpc(effect = read, model)]
    async fn messaging_notifications(
        this: &RecordSet<DiscussChannel>,
        tx: &mut Transaction,
    ) -> Result<Value> {
        let env = this.env();
        internal(tx, env).await?;
        let notices = notifications(tx, env).await?;
        let mut by_model = std::collections::BTreeMap::<&str, Vec<i64>>::new();
        for notice in &notices {
            if let (Some(model), Some(record)) = (&notice.model, notice.record) {
                by_model.entry(model).or_default().push(record);
            }
        }
        let mut readable = std::collections::BTreeMap::new();
        for (model, ids) in by_model {
            readable.insert(model, workspace::readable(tx, env, model, ids).await?);
        }
        let see_subject = !notices.is_empty()
            && tx
                .has_field_access(env, "mail.message", "subject", erp_sdk::FieldOp::Read)
                .await?;
        let items: Vec<_> = notices
            .iter()
            .map(|notice| {
                let available = notice.model.as_ref().is_some_and(|name| {
                    readable
                        .get(name.as_str())
                        .is_some_and(|ids| notice.record.is_some_and(|id| ids.contains(&id)))
                });
                json!({
                    "message": notice.message,
                    "model": notice.model.as_ref().filter(|_| available),
                    "res_id": notice.record.filter(|_| available),
                    "subject": notice.subject.as_ref().filter(|_| see_subject),
                    "unread": notice.unread,
                })
            })
            .collect();
        Ok(Value::Json(Arc::new(json!(items))))
    }

    /// Count readable unread notifications within the bounded recent unread window.
    #[method]
    #[rpc(effect = read, model)]
    async fn messaging_notification_count(
        this: &RecordSet<DiscussChannel>,
        tx: &mut Transaction,
    ) -> Result<Value> {
        let env = this.env();
        internal(tx, env).await?;
        let visible = recent_message_ids(tx, env, true, COUNT_LIMIT).await?;
        Ok(Value::Int(visible.len() as i64))
    }

    /// Acknowledge one currently readable message for the signed-in recipient.
    #[method]
    #[rpc(effect = write, model)]
    async fn messaging_notification_read(
        this: &RecordSet<DiscussChannel>,
        tx: &mut Transaction,
        message_id: i64,
    ) -> Result<()> {
        let env = this.env();
        internal(tx, env).await?;
        let denied = || Error::access("This notification is unavailable.");
        if message_id <= 0
            || workspace::readable(tx, env, "mail.message", vec![message_id])
                .await?
                .is_empty()
        {
            return Err(denied());
        }
        let rows = tx
            .query(
                "WITH owned AS MATERIALIZED (
                     SELECT n.id,n.inbox_read
                     FROM mail_notification n JOIN mail_message msg ON msg.id=n.message_id
                     WHERE n.partner_id=(SELECT partner_id FROM res_users WHERE id=$1::bigint)
                       AND n.message_id=$2::bigint AND msg.model IS DISTINCT FROM 'discuss.channel'
                       AND n.transport IN ('inbox','push') AND n.state='delivered'
                 ), changed AS (
                     UPDATE mail_notification SET inbox_read=true
                     WHERE id IN (SELECT id FROM owned) AND inbox_read IS NOT TRUE
                       AND NOT EXISTS (SELECT FROM owned WHERE inbox_read IS TRUE)
                     RETURNING id
                 )
                 SELECT EXISTS (SELECT FROM owned), EXISTS (SELECT FROM changed)",
                &[&env.uid(), &message_id],
            )
            .await?;
        let bad = || Error::invariant("Invalid notification acknowledgment.");
        let row = rows.first().ok_or_else(bad)?;
        if !row.try_get::<_, bool>(0).map_err(|_| bad())? {
            return Err(denied());
        }
        if row.try_get::<_, bool>(1).map_err(|_| bad())? {
            wake(tx).await?;
        }
        Ok(())
    }
}

/// Apply the message's current access rules before counting it toward the output limit.
/// Keyset batches keep each query small even if many newer deliveries are no longer readable.
async fn recent_message_ids(
    tx: &mut Transaction,
    env: &Env,
    unread: bool,
    limit: usize,
) -> Result<Vec<i64>> {
    if limit == 0 {
        return Ok(Vec::new());
    }
    // A second in-app delivery arriving after acknowledgment inherits the first delivery's read state.
    let predicate = if unread {
        "n.inbox_read IS NOT TRUE AND NOT EXISTS (
            SELECT FROM mail_notification receipt
            WHERE receipt.partner_id=n.partner_id AND receipt.message_id=n.message_id
              AND receipt.transport IN ('inbox','push') AND receipt.state='delivered'
              AND receipt.inbox_read IS TRUE
        )"
    } else {
        "n.inbox_read IS TRUE"
    };
    let statement = format!(
        "SELECT DISTINCT n.message_id::bigint FROM mail_notification n
         JOIN mail_message msg ON msg.id=n.message_id
         WHERE n.partner_id=(SELECT partner_id FROM res_users WHERE id=$1::bigint)
           AND n.message_id < $2::bigint
           AND n.transport IN ('inbox','push') AND n.state='delivered'
           AND msg.model IS DISTINCT FROM 'discuss.channel' AND {predicate}
         ORDER BY 1 DESC LIMIT $3::bigint"
    );
    let mut visible = Vec::new();
    let mut before = i64::MAX;
    let scan_size = SCAN_SIZE.min(limit);
    let scan = scan_size as i64;
    while visible.len() < limit {
        let rows = tx.query(&statement, &[&env.uid(), &before, &scan]).await?;
        let ids = rows
            .iter()
            .map(|row| {
                row.try_get::<_, i64>(0)
                    .map_err(|_| Error::invariant("Invalid notification message."))
            })
            .collect::<Result<Vec<_>>>()?;
        let Some(last) = ids.last() else { break };
        before = *last;
        let allowed = workspace::readable(tx, env, "mail.message", ids.clone()).await?;
        visible.extend(
            ids.into_iter()
                .filter(|id| allowed.contains(id))
                .take(limit - visible.len()),
        );
        if rows.len() < scan_size {
            break;
        }
    }
    Ok(visible)
}

async fn visible_inbox(tx: &mut Transaction, env: &Env) -> Result<(Vec<i64>, Vec<i64>)> {
    let unread = recent_message_ids(tx, env, true, INBOX_SIZE).await?;
    let read = recent_message_ids(tx, env, false, INBOX_SIZE - unread.len()).await?;
    Ok((unread, read))
}

async fn notifications(tx: &mut Transaction, env: &Env) -> Result<Vec<Notice>> {
    let (unread, read) = visible_inbox(tx, env).await?;
    if unread.is_empty() && read.is_empty() {
        return Ok(Vec::new());
    }
    let rows = tx.query(
        "SELECT id::bigint,model::text,res_id::bigint,left(subject::text,160),id=ANY($1::bigint[]) AS unread
         FROM mail_message WHERE id=ANY($1::bigint[]) OR id=ANY($2::bigint[])
         ORDER BY unread DESC,id DESC",
        &[&unread, &read],
    ).await?;
    let bad = || Error::invariant("Invalid personal notification.");
    let mut notices = Vec::with_capacity(rows.len());
    for row in rows {
        notices.push(Notice {
            message: row.try_get(0).map_err(|_| bad())?,
            model: row.try_get(1).map_err(|_| bad())?,
            record: row.try_get(2).map_err(|_| bad())?,
            subject: row.try_get(3).map_err(|_| bad())?,
            unread: row.try_get(4).map_err(|_| bad())?,
        });
    }
    Ok(notices)
}

pub(super) async fn extend_window(
    tx: &mut Transaction,
    env: &Env,
    window: &mut Vec<i64>,
) -> Result<()> {
    let (unread, read) = visible_inbox(tx, env).await?;
    window.extend(unread);
    window.push(-1);
    window.extend(read);
    Ok(())
}
