//! The link step of the write protocol, inside the business transaction
//! and set-based per batch. Every multi-row statement sorts its input, so two batches never take
//! locks in opposite orders.
//!
//! 1. the uploads are consumed (`ready` -> `consumed`) if they are unexpired and bound to the
//!    caller's subject and actor; fewer rows than ids is one error for every cause;
//! 2. the blob rows of the digests are locked `FOR KEY SHARE` (linkers of popular content never
//!    block each other; the lock conflicts with the `FOR UPDATE` of the garbage collector);
//! 3. absent digests are inserted `ON CONFLICT DO NOTHING`;
//! 4. new rows get their bytes before the commit (inline: a row; external: the staged object
//!    is promoted, create-only); `deleting` rows are revived only after their bytes are proved
//!    present (promoted from this batch's staged bytes, a `head`, or a full hash when a sweep
//!    ever started on them);
//! 5. the caller writes the `ir_attachment` rows with the returned `blob_id`s.
//!
//! Invariant I1: the bytes of a `live` row exist.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use erp_sdk::blob::{BlobError, ContentKey, StagedBlob, StagingKey};
use erp_sdk::prelude::*;
use erp_sdk::{Env, Transaction};

use super::runtime::{StoreRef, blob_error, col_error, retryable, runtime, store_by_id};
use super::types::{MAX_BATCH, UploadId, upload_not_usable};
use super::upload::actor_text;

macro_rules! col {
    ($row:expr, $i:expr) => {
        $row.try_get($i)
            .map_err(|e| col_error("attachment link", e))?
    };
}

/// Where the content of one link item comes from.
#[derive(Clone)]
pub(crate) enum Source {
    /// No content.
    Empty,
    /// Bytes at or below the inline threshold, already digested and typed.
    Inline {
        bytes: Vec<u8>,
        sha256: [u8; 32],
        sha1: [u8; 20],
        mimetype: String,
    },
    /// A staged upload.
    Upload(UploadId),
    /// An existing blob (a copy references the same content).
    Blob(i64),
}

/// What a source linked to.
#[derive(Debug, Clone)]
pub(crate) struct Linked {
    pub blob_id: Option<i64>,
    pub size: u64,
    pub sha1: Option<[u8; 20]>,
    pub mimetype: Option<String>,
}

struct Consumed {
    pub(crate) store_id: Option<i16>,
    pub(crate) sha256: [u8; 32],
    sha1: [u8; 20],
    pub(crate) size: i64,
    pub(crate) mimetype: Option<String>,
    inline: Option<Vec<u8>>,
}

/// What one digest needs from the store or the row.
struct Need {
    size: i64,
    mimetype: Option<String>,
    /// `None`: the bytes are inline.
    store_id: Option<i16>,
    inline: Option<Vec<u8>>,
    /// The staged upload that carries the bytes, with its SHA-1.
    staged: Option<(UploadId, [u8; 20])>,
}

#[derive(Clone)]
pub(crate) struct BlobRow {
    pub(crate) id: i64,
    pub(crate) sha256: [u8; 32],
    pub(crate) deleting: bool,
    pub(crate) swept: bool,
    pub(crate) store_id: Option<i16>,
    pub(crate) size: i64,
    pub(crate) mimetype: Option<String>,
}

fn sha256_of(bytes: Vec<u8>) -> Result<[u8; 32]> {
    <[u8; 32]>::try_from(bytes.as_slice())
        .map_err(|_| Error::db("an ir_blob digest is not 32 bytes"))
}

fn sha1_of(bytes: Vec<u8>) -> Result<[u8; 20]> {
    <[u8; 20]>::try_from(bytes.as_slice())
        .map_err(|_| Error::db("an ir_blob_upload SHA-1 is not 20 bytes"))
}

fn blob_row(row: &erp_sdk::Row) -> Result<BlobRow> {
    let sha: Vec<u8> = col!(row, 1);
    let state: i16 = col!(row, 2);
    Ok(BlobRow {
        id: col!(row, 0),
        sha256: sha256_of(sha)?,
        deleting: state == 1,
        swept: col!(row, 3),
        store_id: col!(row, 4),
        size: col!(row, 5),
        mimetype: col!(row, 6),
    })
}

const BLOB_COLUMNS: &str =
    "id, sha256, state, (sweep_attempted_at IS NOT NULL), store_id, size, detected_mimetype";

/// Link `sources` (at most [`MAX_BATCH`]) and return what each linked to, in order.
///
/// # Errors
///
/// `Validation` (one message) for an upload that is unknown, expired, used or another's; the
/// retryable `Concurrency` error when a blob raced the garbage collector; store errors.
pub(crate) async fn link(
    tx: &mut Transaction,
    env: &Env,
    sources: &[Source],
) -> Result<Vec<Linked>> {
    if sources.len() > MAX_BATCH {
        return Err(Error::validation(format!(
            "At most {MAX_BATCH} attachments are linked in one call."
        )));
    }
    if sources.iter().all(|s| matches!(s, Source::Empty)) {
        return Ok(sources
            .iter()
            .map(|_| Linked {
                blob_id: None,
                size: 0,
                sha1: None,
                mimetype: None,
            })
            .collect());
    }
    let rt = runtime(env)?;

    // 1. consume the uploads
    let mut upload_ids: Vec<String> = Vec::new();
    for source in sources {
        if let Source::Upload(id) = source {
            if upload_ids.iter().any(|u| u == id.as_str()) {
                // one upload carries one attachment
                return Err(upload_not_usable());
            }
            upload_ids.push(id.as_str().to_owned());
        }
    }
    let uid = env.principal().subject().get();
    let actor = actor_text(env);
    let mut consumed: HashMap<String, Consumed> = HashMap::new();
    if !upload_ids.is_empty() {
        let rows = tx
            .query(
                "UPDATE ir_blob_upload u SET state = 2 \
                 FROM (SELECT id FROM ir_blob_upload WHERE id = ANY($1::text[]::uuid[]) \
                       ORDER BY id FOR UPDATE) l \
                 WHERE u.id = l.id AND u.state = 1 AND u.expires_at > now() \
                   AND u.subject_user = $2 AND u.actor = $3 \
                 RETURNING u.id::text, u.store_id, u.sha256, u.sha1, u.size, \
                           u.detected_mimetype, u.inline_data",
                &[&upload_ids, &uid, &actor],
            )
            .await?;
        if rows.len() != upload_ids.len() {
            return Err(upload_not_usable());
        }
        for row in &rows {
            let id: String = col!(row, 0);
            let sha256: Vec<u8> = col!(row, 2);
            let sha1: Vec<u8> = col!(row, 3);
            consumed.insert(
                id,
                Consumed {
                    store_id: col!(row, 1),
                    sha256: sha256_of(sha256)?,
                    sha1: sha1_of(sha1)?,
                    size: col!(row, 4),
                    mimetype: col!(row, 5),
                    inline: col!(row, 6),
                },
            );
        }
    }

    // the digests and the existing blob ids the batch needs
    let mut needs: BTreeMap<[u8; 32], Need> = BTreeMap::new();
    let mut by_id: BTreeSet<i64> = BTreeSet::new();
    for source in sources {
        match source {
            Source::Empty => {}
            Source::Inline {
                bytes,
                sha256,
                mimetype,
                ..
            } => {
                needs.entry(*sha256).or_insert_with(|| Need {
                    size: i64::try_from(bytes.len()).unwrap_or(i64::MAX),
                    mimetype: Some(mimetype.clone()),
                    store_id: None,
                    inline: Some(bytes.clone()),
                    staged: None,
                });
            }
            Source::Upload(id) => {
                let up = consumed.get(id.as_str()).ok_or_else(upload_not_usable)?;
                if up.inline.is_none() && up.store_id.is_none() {
                    return Err(Error::internal(
                        "an upload has neither inline bytes nor a store",
                    ));
                }
                needs.entry(up.sha256).or_insert_with(|| Need {
                    size: up.size,
                    mimetype: up.mimetype.clone(),
                    store_id: if up.inline.is_some() {
                        None
                    } else {
                        up.store_id
                    },
                    inline: up.inline.clone(),
                    staged: if up.inline.is_some() {
                        None
                    } else {
                        Some((id.clone(), up.sha1))
                    },
                });
            }
            Source::Blob(id) => {
                by_id.insert(*id);
            }
        }
    }

    // 2. lock the rows of the digests and of the copied blobs FOR KEY SHARE, in sorted order
    let mut found: HashMap<[u8; 32], BlobRow> = HashMap::new();
    let mut found_by_id: HashMap<i64, BlobRow> = HashMap::new();
    if !needs.is_empty() {
        let digests_param: Vec<Vec<u8>> = needs.keys().map(|d| d.to_vec()).collect();
        let rows = tx
            .query(
                &format!(
                    "SELECT {BLOB_COLUMNS} FROM ir_blob WHERE sha256 = ANY($1::bytea[]) \
                     ORDER BY sha256 FOR KEY SHARE"
                ),
                &[&digests_param],
            )
            .await?;
        for row in &rows {
            let parsed = blob_row(row)?;
            found.insert(parsed.sha256, parsed);
        }
    }
    if !by_id.is_empty() {
        let ids: Vec<i64> = by_id.iter().copied().collect();
        let rows = tx
            .query(
                &format!(
                    "SELECT {BLOB_COLUMNS} FROM ir_blob WHERE id = ANY($1::int8[]) \
                     ORDER BY id FOR KEY SHARE"
                ),
                &[&ids],
            )
            .await?;
        for row in &rows {
            let parsed = blob_row(row)?;
            found_by_id.insert(parsed.id, parsed);
        }
        if found_by_id.len() != ids.len() {
            return Err(retryable("a copied blob row was removed"));
        }
    }

    // 3. insert the digests that have no row
    let missing: Vec<[u8; 32]> = needs
        .keys()
        .filter(|d| !found.contains_key(*d))
        .copied()
        .collect();
    let mut inserted: Vec<BlobRow> = Vec::new();
    if !missing.is_empty() {
        let digests_param: Vec<Vec<u8>> = missing.iter().map(|d| d.to_vec()).collect();
        let mut sizes = Vec::with_capacity(missing.len());
        let mut stores: Vec<Option<i16>> = Vec::with_capacity(missing.len());
        let mut mimes: Vec<Option<String>> = Vec::with_capacity(missing.len());
        for digest in &missing {
            let need = needs
                .get(digest)
                .ok_or_else(|| Error::internal("a digest lost its need"))?;
            sizes.push(need.size);
            stores.push(need.store_id);
            mimes.push(need.mimetype.clone());
        }
        let rows = tx
            .query(
                &format!(
                    "INSERT INTO ir_blob (sha256, size, store_id, detected_mimetype) \
                     SELECT * FROM unnest($1::bytea[], $2::int8[], $3::int2[], $4::text[]) \
                     ORDER BY 1 ON CONFLICT (sha256) DO NOTHING \
                     RETURNING {BLOB_COLUMNS}"
                ),
                &[&digests_param, &sizes, &stores, &mimes],
            )
            .await?;
        for row in &rows {
            inserted.push(blob_row(row)?);
        }
        if inserted.len() != missing.len() {
            // rows this transaction inserted before are found now; a concurrent insert that our
            // snapshot cannot see raised 40001 already
            let seen: BTreeSet<[u8; 32]> = inserted.iter().map(|r| r.sha256).collect();
            let lost: Vec<Vec<u8>> = missing
                .iter()
                .filter(|d| !seen.contains(*d))
                .map(|d| d.to_vec())
                .collect();
            let rows = tx
                .query(
                    &format!(
                        "SELECT {BLOB_COLUMNS} FROM ir_blob WHERE sha256 = ANY($1::bytea[]) \
                         ORDER BY sha256 FOR KEY SHARE"
                    ),
                    &[&lost],
                )
                .await?;
            if rows.len() != lost.len() {
                return Err(retryable(
                    "a blob row appeared and vanished during the insert",
                ));
            }
            for row in &rows {
                let parsed = blob_row(row)?;
                found.insert(parsed.sha256, parsed);
            }
        }
    }

    // 4. bytes for the new rows, proof for the revived ones
    let mut inline_ids: Vec<i64> = Vec::new();
    let mut inline_data: Vec<Vec<u8>> = Vec::new();
    for row in &inserted {
        let need = needs
            .get(&row.sha256)
            .ok_or_else(|| Error::internal("an inserted blob has no need"))?;
        match (&need.inline, row.store_id) {
            (Some(bytes), None) => {
                inline_ids.push(row.id);
                inline_data.push(bytes.clone());
            }
            (_, Some(_)) => promote(tx, &rt, need, row).await?,
            (None, None) => return Err(Error::internal("an inline blob has no bytes")),
        }
    }
    if !inline_ids.is_empty() {
        let mut order: Vec<usize> = (0..inline_ids.len()).collect();
        order.sort_by_key(|i| inline_ids.get(*i).copied());
        let ids: Vec<i64> = order
            .iter()
            .filter_map(|i| inline_ids.get(*i).copied())
            .collect();
        let data: Vec<Vec<u8>> = order
            .iter()
            .filter_map(|i| inline_data.get(*i).cloned())
            .collect();
        tx.execute(
            "INSERT INTO ir_blob_inline (blob_id, data) SELECT * FROM unnest($1::int8[], $2::bytea[])",
            &[&ids, &data],
        )
        .await
        .map_err(super::runtime::map_link_fk)?;
    }
    let mut revive: Vec<i64> = Vec::new();
    let mut revive_rows: Vec<(BlobRow, Option<&Need>)> = Vec::new();
    for row in found.values() {
        if row.deleting {
            revive_rows.push((row.clone(), needs.get(&row.sha256)));
        }
    }
    for row in found_by_id.values() {
        if row.deleting && !found.contains_key(&row.sha256) {
            revive_rows.push((row.clone(), None));
        }
    }
    for (row, need) in &revive_rows {
        match (row.store_id, need) {
            // inline bytes live and die with their row, which this transaction holds
            (None, _) => {}
            (Some(store_id), Some(need))
                if need.staged.is_some() && need.store_id == Some(store_id) =>
            {
                promote(tx, &rt, need, row).await?;
            }
            (Some(_), _) => prove_present(tx, &rt, row).await?,
        }
        revive.push(row.id);
    }
    if !revive.is_empty() {
        revive.sort_unstable();
        revive.dedup();
        let locked = tx
            .query(
                "SELECT id FROM ir_blob WHERE id = ANY($1::int8[]) ORDER BY id FOR NO KEY UPDATE",
                &[&revive],
            )
            .await?;
        if locked.len() != revive.len() {
            return Err(retryable("a blob row was removed while it was revived"));
        }
        tx.execute(
            "UPDATE ir_blob SET state = 0, deleting_since = NULL, sweep_attempted_at = NULL \
             WHERE id = ANY($1::int8[]) AND state = 1",
            &[&revive],
        )
        .await?;
    }

    // the result, in source order
    let row_of = |digest: &[u8; 32]| -> Result<BlobRow> {
        found
            .get(digest)
            .cloned()
            .or_else(|| inserted.iter().find(|r| &r.sha256 == digest).cloned())
            .ok_or_else(|| Error::internal("a linked digest has no blob row"))
    };
    let mut out = Vec::with_capacity(sources.len());
    for source in sources {
        out.push(match source {
            Source::Empty => Linked {
                blob_id: None,
                size: 0,
                sha1: None,
                mimetype: None,
            },
            Source::Inline {
                bytes,
                sha256,
                sha1,
                mimetype,
            } => Linked {
                blob_id: Some(row_of(sha256)?.id),
                size: bytes.len() as u64,
                sha1: Some(*sha1),
                mimetype: Some(mimetype.clone()),
            },
            Source::Upload(id) => {
                let up = consumed.get(id.as_str()).ok_or_else(upload_not_usable)?;
                Linked {
                    blob_id: Some(row_of(&up.sha256)?.id),
                    size: u64::try_from(up.size).unwrap_or(0),
                    sha1: Some(up.sha1),
                    mimetype: up.mimetype.clone(),
                }
            }
            Source::Blob(id) => {
                let row = found_by_id
                    .get(id)
                    .ok_or_else(|| retryable("a copied blob row was removed"))?;
                Linked {
                    blob_id: Some(row.id),
                    size: u64::try_from(row.size).unwrap_or(0),
                    sha1: None,
                    mimetype: row.mimetype.clone(),
                }
            }
        });
    }
    Ok(out)
}

/// Promote the staged bytes of `need` to the content key of `row` (create-only; an existing key
/// is accepted, the bytes under a key are identical by construction).
async fn promote(
    tx: &mut Transaction,
    rt: &erp_sdk::blob::AttachmentRuntime,
    need: &Need,
    row: &BlobRow,
) -> Result<()> {
    let (Some((upload, sha1)), Some(store_id)) = (&need.staged, row.store_id) else {
        return Err(Error::internal(
            "an external blob has no staged upload to promote",
        ));
    };
    let store = store_by_id(tx, rt, store_id).await?;
    let key = StagingKey::new(store.namespace.clone(), upload.as_str()).map_err(blob_error)?;
    let staged = StagedBlob::from_parts(
        key,
        u64::try_from(need.size).unwrap_or(0),
        row.sha256,
        *sha1,
    );
    let content = ContentKey::new(store.namespace.clone(), row.sha256);
    store
        .store
        .promote(&staged, &content)
        .await
        .map_err(blob_error)
}

/// Prove the bytes of a `deleting` external row are present before it is revived: a `head`
/// when no sweep ever started on it, a full read and SHA-256 check when one did.
pub(crate) async fn prove_present(
    tx: &mut Transaction,
    rt: &erp_sdk::blob::AttachmentRuntime,
    row: &BlobRow,
) -> Result<()> {
    let Some(store_id) = row.store_id else {
        return Ok(());
    };
    let store: StoreRef = store_by_id(tx, rt, store_id).await?;
    let key = ContentKey::new(store.namespace.clone(), row.sha256);
    let unproved = |why: &str| {
        retryable_message(format!(
            "the bytes of blob {} could not be proved present ({why})",
            row.id
        ))
    };
    if !row.swept {
        match store.store.head(&key).await {
            Ok(meta) if i64::try_from(meta.size).ok() == Some(row.size) => Ok(()),
            Ok(_) => Err(unproved("size differs")),
            Err(BlobError::NotFound { .. }) => Err(unproved("missing")),
            Err(other) => Err(blob_error(other)),
        }
    } else {
        // a full read: admitted by the node's stream bound, the permit held while the bytes pass
        let _permit = rt.try_stream_permit().map_err(blob_error)?;
        let mut read = match store.store.get(&key, None).await {
            Ok(read) => read,
            Err(BlobError::NotFound { .. }) => return Err(unproved("missing")),
            Err(other) => return Err(blob_error(other)),
        };
        let (sha256, _) = erp_sdk::blob::digest_stream(&mut read.reader)
            .await
            .map_err(|e| Error::internal(format!("A blob could not be read: {e}")))?;
        if sha256 == row.sha256 {
            Ok(())
        } else {
            Err(unproved("digest differs"))
        }
    }
}

fn retryable_message(message: String) -> Error {
    // the text is built per call, the retry marker is the SQLSTATE
    Error::database(
        erp_sdk::ErrorKind::Concurrency,
        Some("40001".to_owned()),
        message.clone(),
        std::io::Error::other(message),
    )
}
