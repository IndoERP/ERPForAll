// SPDX-License-Identifier: MIT OR Apache-2.0
//! Owner-side M3 consumer. The only network notification is a coalesced, payload-free wake.
//! Maildir is a local handoff. External SMTP and platform push adapters can consume separately.

use erp_fields::{Error, Result};
use erp_orm::{Orm, OutboxReplay, SUPERUSER_ID};
use std::io::Write;
use std::path::Path;

pub(crate) async fn replay(owner: &Orm) -> Result<OutboxReplay> {
    owner.replay_outbox("mail.notifications", 16, |tx, events| Box::pin(async move {
        let ids: Vec<_> = events.into_iter().filter(|event| event.model == "mail.delivery.request" && event.kind == "create").flat_map(|event|event.record_ids).collect();
        if ids.is_empty() { return Ok(()); }
        tx.execute("WITH recipients AS (
           SELECT msg.id, p.id AS partner, msg.silent,
               EXISTS (SELECT 1 FROM res_users u WHERE u.partner_id=p.id AND u.active AND NOT u.share AND u.notification_type='inbox') AS inbox,
               EXISTS (SELECT 1 FROM res_users u WHERE u.partner_id=p.id AND u.active AND NOT u.share) AS internal,
               (p.email IS NOT NULL AND length(p.email) <= 254 AND p.email ~ '^[^[:space:]<>@]+@[^[:space:]<>@]+$') AS addressed
           FROM mail_delivery_request req JOIN mail_message msg ON msg.id=req.message_id JOIN mail_message_notified_rel r ON msg.id=r.message_id JOIN res_partner p ON p.id=r.partner_id
           WHERE req.id=ANY($1::bigint[]) AND p.active AND (msg.model IS DISTINCT FROM 'discuss.channel' OR EXISTS
             (SELECT 1 FROM discuss_channel_member cm JOIN res_users u ON u.id=cm.user_id JOIN discuss_channel c ON c.id=cm.channel_id
              JOIN res_company_users_rel co ON co.user_id=u.id AND co.cid=c.company_id WHERE cm.channel_id=msg.res_id AND u.partner_id=p.id AND u.active AND NOT u.share)))
          INSERT INTO mail_notification (message_id,partner_id,transport,state)
          SELECT id,partner,kind,CASE WHEN kind<>'email' THEN 'delivered' WHEN addressed THEN 'pending' ELSE 'failed' END FROM recipients
          CROSS JOIN LATERAL (SELECT CASE WHEN inbox THEN 'inbox' ELSE 'email' END AS kind UNION ALL SELECT 'push' WHERE internal AND NOT COALESCE(silent,false)) kinds
          ON CONFLICT (message_id,partner_id,transport) DO NOTHING", &[&ids]).await?;
        tx.notify_once(&erp_orm::NotifyChannel::new("erp_thread_changed")?).await?;
        Ok(())
    })).await
}

/// Persist an RFC 5322 notice in a Maildir using a deterministic delivery name. If the DB
/// transaction retries after publication, the existing file is the same delivery.
pub(crate) async fn handoff(owner: &Orm, maildir: &Path) -> Result<()> {
    let mut tx = owner.begin(SUPERUSER_ID).await?;
    let rows = tx.query("SELECT n.id::bigint, p.email::text, (p.active AND
        (msg.model IS DISTINCT FROM 'discuss.channel' OR EXISTS (SELECT 1 FROM discuss_channel_member cm JOIN res_users u ON u.id=cm.user_id
        JOIN discuss_channel c ON c.id=cm.channel_id JOIN res_company_users_rel co ON co.user_id=u.id AND co.cid=c.company_id
        WHERE cm.channel_id=msg.res_id AND u.partner_id=p.id AND u.active AND NOT u.share))),msg.message_id::text, to_char(CURRENT_TIMESTAMP AT TIME ZONE 'UTC', 'Dy, DD Mon YYYY HH24:MI:SS') || ' +0000'
        FROM mail_notification n JOIN res_partner p ON p.id=n.partner_id JOIN mail_message msg ON msg.id=n.message_id WHERE n.transport='email' AND n.state='pending'
        ORDER BY n.id LIMIT 16 FOR UPDATE OF n SKIP LOCKED", &[]).await?;
    for row in rows {
        let invalid = |_| Error::invariant("Invalid mail delivery row.");
        let id: i64 = row.try_get(0).map_err(invalid)?;
        let address: Option<String> = row.try_get(1).map_err(invalid)?;
        let eligible: bool = row.try_get(2).map_err(invalid)?;
        let message_key: String = row.try_get(3).map_err(invalid)?;
        let date: String = row.try_get(4).map_err(invalid)?;
        let state = if !eligible {
            "revoked"
        } else if let Some(address) = address.filter(|a| valid_address(a)) {
            let directory = maildir.to_owned();
            tokio::task::spawn_blocking(move || {
                publish(&directory, &message_key, id, &address, &date)
            })
            .await
            .map_err(|_| Error::db("The local Maildir handoff stopped."))??;
            "delivered"
        } else {
            "failed"
        };
        tx.execute(
            "UPDATE mail_notification SET state=$2::text WHERE id=$1::bigint",
            &[&id, &state],
        )
        .await?;
    }
    tx.commit().await?;
    Ok(())
}

fn valid_address(address: &str) -> bool {
    address.len() <= 254
        && address.is_ascii()
        && !address
            .bytes()
            .any(|b| b.is_ascii_control() || b.is_ascii_whitespace() || b"<>,;:".contains(&b))
        && address.split_once('@').is_some_and(|(local, domain)| {
            !local.is_empty() && !domain.is_empty() && !domain.contains('@')
        })
}

/// The committed receipt directory outlives Maildir clients moving or deleting a delivered
/// message. Moving its pending file into new/ consumes the right to publish exactly once.
fn publish(maildir: &Path, message_key: &str, id: i64, address: &str, date: &str) -> Result<()> {
    use sha2::{Digest, Sha256};
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
    let io = |_| Error::db("The local Maildir could not accept delivery.");
    let digest: String = Sha256::digest(message_key)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    let key = format!("{digest}.{id}");
    let receipt_root = maildir.join(".erp-receipts");
    let new = maildir.join("new");
    for dir in [
        &receipt_root,
        &new,
        &maildir.join("tmp"),
        &maildir.join("cur"),
    ] {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)
            .map_err(io)?;
    }
    let sync = |dir: &Path| {
        std::fs::File::open(dir)
            .and_then(|f| f.sync_all())
            .map_err(io)
    };
    let receipt = receipt_root.join(&key);
    if !receipt.try_exists().map_err(io)? {
        let stage = tempfile::tempdir_in(&receipt_root).map_err(io)?;
        let mut body = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(stage.path().join("pending"))
            .map_err(io)?;
        write!(body, "Date: {date}\r\nFrom: ERPForAll <no-reply@erpforall.invalid>\r\nTo: {address}\r\nMessage-ID: <{key}@erpforall.invalid>\r\nSubject: New ERPForAll message\r\nMIME-Version: 1.0\r\nContent-Type: text/plain; charset=utf-8\r\n\r\nSign in to ERPForAll to read your message.\r\n").map_err(io)?;
        body.sync_all().map_err(io)?;
        drop(body);
        sync(stage.path())?;
        std::fs::rename(stage.path(), &receipt).map_err(io)?;
        sync(&receipt_root)?;
    }
    let pending = receipt.join("pending");
    if pending.try_exists().map_err(io)? {
        std::fs::rename(pending, new.join(&key)).map_err(io)?;
    }
    sync(&new)?;
    sync(&receipt)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn replay_after_mail_client_consumes_notice_keeps_single_delivery() -> Result<()> {
        let root = tempfile::tempdir().map_err(|_| Error::db("test maildir"))?;
        publish(
            root.path(),
            "test-message",
            17,
            "test@example.invalid",
            "Fri, 27 Nov 2026 02:10:00 +0000",
        )?;
        let dir = root.path().join("new");
        let entries = || -> Result<Vec<_>> {
            std::fs::read_dir(&dir)
                .and_then(|items| items.map(|item| item.map(|item| item.path())).collect())
                .map_err(|_| Error::db("read test maildir"))
        };
        let first = entries()?;
        assert_eq!(first.len(), 1);
        std::fs::remove_file(&first[0]).map_err(|_| Error::db("consume test mail"))?;
        publish(
            root.path(),
            "test-message",
            17,
            "test@example.invalid",
            "Fri, 27 Nov 2026 02:10:00 +0000",
        )?;
        assert!(entries()?.is_empty());
        assert!(!valid_address(
            "first@example.invalid\r\nBcc: other@example.invalid"
        ));
        Ok(())
    }
}
