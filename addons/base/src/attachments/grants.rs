//! Explicit, expiring publication of one attachment. Only a digest of a link secret is stored.
use super::access;
use erp_sdk::{Env, Error, Operation, Result, Transaction};

const MAX_SECS: i64 = 7 * 24 * 3600;

// Reapply this restriction on redemption in case a field changed since the link was issued.
fn shareable_parent(env: &Env, target: &access::Target) -> bool {
    let Some((model, _)) = target.parent() else {
        return true;
    };
    !access::DENIED_PARENT_MODELS.contains(&model)
        && target.res_field.as_ref().is_none_or(|name| {
            env.registry()
                .model(model)
                .and_then(|m| m.field(name))
                .is_some_and(|field| field.groups.is_none())
        })
}

async fn can_manage(tx: &mut Transaction, id: i64) -> Result<(i64, i64)> {
    let env = tx.env();
    let uid = env.uid();
    if env.su()
        || env.principal().scope().is_some()
        || !tx.has_group(&env, "base.group_system").await?
    {
        return Err(Error::access(
            "Managing content grants requires a signed-in administrator.",
        ));
    }
    let company = env
        .company()?
        .ok_or_else(|| Error::access("Select a company to manage content grants."))?
        .get();
    let targets = access::load_targets_locked(tx, &[id]).await?;
    let [target] = targets.as_slice() else {
        return Err(Error::access("Attachment unavailable."));
    };
    if access::allowed(tx, &env, &targets, Operation::Write).await? != [true] {
        return Err(Error::access("Attachment unavailable."));
    }
    if !shareable_parent(&env, target) {
        return Err(Error::access("This attachment cannot be shared."));
    }
    Ok((uid, company))
}

pub(crate) async fn issue(
    tx: &mut Transaction,
    id: i64,
    public_read: bool,
    lifetime: i64,
) -> Result<String> {
    if !(1..=MAX_SECS).contains(&lifetime) {
        return Err(Error::validation(
            "Content grant lifetime must be between 1 and 604800 seconds.",
        ));
    }
    let (uid, company) = can_manage(tx, id).await?;
    // The two random UUIDs are generated once. Rotation replaces the digest in the same transaction.
    let rows = tx.query("WITH token AS MATERIALIZED (SELECT CASE WHEN $2 THEN NULL ELSE replace(gen_random_uuid()::text || gen_random_uuid()::text, '-', '') END AS secret),
        granted AS (INSERT INTO ir_attachment_content_grant (attachment_id,public_read,issuer_id,company_id,expires_at,secret_hash)
        SELECT a.id,$2,$3,$4,clock_timestamp()+make_interval(secs => $5::int8::float8),sha256(convert_to(token.secret,'UTF8'))
        FROM ir_attachment a CROSS JOIN token WHERE a.id=$1 AND a.type='binary' AND a.blob_id IS NOT NULL
        AND (a.company_id IS NULL OR a.company_id=$4) AND (NOT $2 OR a.public IS TRUE)
        ON CONFLICT (attachment_id,public_read) DO UPDATE SET issuer_id=excluded.issuer_id,company_id=excluded.company_id,
          expires_at=excluded.expires_at,secret_hash=excluded.secret_hash RETURNING attachment_id)
        SELECT token.secret FROM granted CROSS JOIN token", &[&id, &public_read, &uid, &company, &lifetime]).await?;
    let [row] = rows.as_slice() else {
        return Err(Error::validation(
            "A grant requires stored content in the selected company. A public grant also requires a public attachment.",
        ));
    };
    let secret: Option<String> = row
        .try_get(0)
        .map_err(|e| Error::db("invalid content grant result").with_source(e))?;
    Ok(match secret {
        Some(secret) => format!("/erp/content/{id}?access_token={secret}"),
        None => format!("/erp/content/{id}"),
    })
}

pub(crate) async fn revoke(tx: &mut Transaction, id: i64) -> Result<bool> {
    can_manage(tx, id).await?;
    Ok(tx
        .execute(
            "DELETE FROM ir_attachment_content_grant WHERE attachment_id=$1",
            &[&id],
        )
        .await?
        > 0)
}

/// Resolve only the authorizing subject and company. The transport rechecks this proof
/// inside that subject's current transaction before looking up the attachment.
pub async fn resolve(
    tx: &mut Transaction,
    id: i64,
    secret: Option<&str>,
) -> Result<Option<(i64, i64)>> {
    if secret.is_some_and(|s| {
        s.len() != 64
            || !s
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    }) {
        return Ok(None);
    }
    let rows = tx
        .query(
            "SELECT g.issuer_id,g.company_id FROM ir_attachment_content_grant g
        JOIN ir_attachment a ON a.id=g.attachment_id JOIN res_users u ON u.id=g.issuer_id
        WHERE g.attachment_id=$1 AND g.expires_at > clock_timestamp() AND u.active AND
        (a.company_id IS NULL OR a.company_id=g.company_id) AND
        (($2::text IS NULL AND g.public_read AND a.public IS TRUE) OR
         (NOT g.public_read AND g.secret_hash=sha256(convert_to($2::text,'UTF8'))))",
            &[&id, &secret],
        )
        .await?;
    rows.first()
        .map(|row| {
            let col = |i| {
                row.try_get(i)
                    .map_err(|e| Error::db("invalid content grant owner").with_source(e))
            };
            Ok((col(0)?, col(1)?))
        })
        .transpose()
}

/// Recheck publication authority in the issuer's selected company. A newly group-restricted
/// field cannot become public through a grant issued before the restriction.
pub async fn can_redeem(tx: &mut Transaction, id: i64) -> Result<bool> {
    let env = tx.env();
    if env.su() || !tx.has_group(&env, "base.group_system").await? {
        return Ok(false);
    }
    let targets = access::load_targets(tx, &[id]).await?;
    let [target] = targets.as_slice() else {
        return Ok(false);
    };
    if !shareable_parent(&env, target) {
        return Ok(false);
    }
    Ok(access::allowed(tx, &env, &targets, Operation::Write).await? == [true])
}
