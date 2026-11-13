//! The files of a thread as the store carries them (
//! ): the attachments posted with a page of messages and
//! the attachments panel of a record.
//!
//! Both are one set-based statement however many messages or files there are. The metadata of an
//! `ir.attachment` is carried (name, media type, size, SHA-1 `checksum`, type and URL); the
//! content never is, and `ir.attachment` has no access token or route until.
//!
//! The
//! panel lists the files of a record the caller was just allowed to read (
//! a file follows the read access of its record).

use erp_addon_base::attachments::{self, AttachmentContent, NewAttachment, ParentRef};
use erp_sdk::prelude::*;
use erp_sdk::serde_json::{Value as Json, json};
use erp_sdk::store_wire::FieldValue as Wire;
use erp_sdk::{Operation, ValueMap};

use crate::store::{ATTACHMENT, MESSAGE, StoreFields};

/// Most files the attachments panel of a record lists.
pub const MAX_PANEL_ATTACHMENTS: i64 = 100;

/// The files posted with messages, with the same record as the message, for a page of messages:
/// `(message, file)` in message then file id order.
const OF_MESSAGES: &str = "\
SELECT r.message_id::int8, a.id::int8, a.name::text, a.mimetype::text, a.file_size::int8, \
       a.checksum::text, a.type::text, a.url::text \
FROM message_attachment_rel r \
JOIN mail_message m ON m.id = r.message_id \
JOIN ir_attachment a ON a.id = r.attachment_id \
WHERE r.message_id = ANY($1::int8[]) \
  AND a.res_model = m.model AND a.res_id = m.res_id AND a.res_field IS NULL \
ORDER BY r.message_id, a.id";

/// The newest files of a record, with the number of all of them in every row.
const OF_RECORD: &str = "\
SELECT a.id::int8, a.name::text, a.mimetype::text, a.file_size::int8, a.checksum::text, \
       a.type::text, a.url::text, count(*) OVER ()::int8 \
FROM ir_attachment a \
WHERE a.res_model = $1 AND a.res_id = $2::int8 AND a.res_field IS NULL \
ORDER BY a.id DESC LIMIT $3::int8";

/// One file's metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FileRow {
    id: u64,
    name: String,
    mimetype: String,
    file_size: i64,
    checksum: String,
    kind: String,
    url: String,
}

impl FileRow {
    fn entries(&self) -> Vec<(&'static str, Wire)> {
        vec![
            ("name", Wire::String(self.name.clone())),
            ("mimetype", Wire::String(self.mimetype.clone())),
            ("file_size", Wire::Int(self.file_size)),
            ("checksum", Wire::String(self.checksum.clone())),
            ("type", Wire::Selection(self.kind.clone())),
            ("url", Wire::String(self.url.clone())),
        ]
    }
}

fn bad(what: &str, error: impl std::error::Error + Send + Sync + 'static) -> Error {
    Error::internal(format!("an attachment row has no readable {what}")).with_source(error)
}

fn record_id(id: i64, what: &str) -> Result<u64> {
    u64::try_from(id)
        .ok()
        .filter(|n| *n > 0)
        .ok_or_else(|| Error::invariant(format!("{what} {id} is not a record id")))
}

/// The file at column `at` of `row`: id, name, media type, size, checksum, type, URL.
fn file_of(row: &erp_sdk::Row, at: usize) -> Result<FileRow> {
    let id: i64 = row.try_get(at).map_err(|e| bad("id", e))?;
    let name: String = row.try_get(at + 1).map_err(|e| bad("name", e))?;
    let mimetype: Option<String> = row.try_get(at + 2).map_err(|e| bad("media type", e))?;
    let size: Option<i64> = row.try_get(at + 3).map_err(|e| bad("size", e))?;
    let checksum: Option<String> = row.try_get(at + 4).map_err(|e| bad("checksum", e))?;
    let kind: String = row.try_get(at + 5).map_err(|e| bad("type", e))?;
    let url: Option<String> = row.try_get(at + 6).map_err(|e| bad("URL", e))?;
    let id = record_id(id, "attachment id")?;
    // the service sets both for a stored file: a binary file without a size, or a non-empty one
    // without a checksum, is a damaged row, not a file to show with invented values. An empty
    // file legitimately has no checksum, and a URL file has no stored content.
    if kind == "binary" && size.is_none() {
        return Err(Error::invariant(format!(
            "the binary attachment {id} has no file_size: the attachment service sets it"
        )));
    }
    if kind == "binary" && size.is_some_and(|n| n > 0) && checksum.is_none() {
        return Err(Error::invariant(format!(
            "the non-empty binary attachment {id} has no checksum: the attachment service sets it"
        )));
    }
    Ok(FileRow {
        id,
        name,
        // an unset text column reads as empty text, as `text_of` does for message fields
        mimetype: mimetype.unwrap_or_default(),
        file_size: size.unwrap_or(0),
        // empty only for an empty file or a URL (checked above)
        checksum: checksum.unwrap_or_default(),
        kind,
        url: url.unwrap_or_default(),
    })
}

/// Stage `attachment_ids` of every message in `message_ids` and the files they name into
/// `fields`, in one statement. A message without files gets an empty list, so a client replaces
/// what it held.
///
/// The ids must come from a search under the caller's own `mail.message` rules.
///
/// # Errors
/// `Internal` for a row that does not decode, `Invariant` for a non-positive id, a binary file
/// without a size, or a non-empty binary file without a checksum.
pub(crate) async fn stage_of_messages(
    tx: &mut Transaction,
    message_ids: &[i64],
    fields: &mut StoreFields,
) -> Result<()> {
    if message_ids.is_empty() {
        return Ok(());
    }
    // a post answers after its own writes: the raw statement must see them
    tx.flush().await?;
    let rows = tx.query(OF_MESSAGES, &[&message_ids]).await?;
    let mut per_message: std::collections::BTreeMap<u64, Vec<u64>> = message_ids
        .iter()
        .map(|id| record_id(*id, "message id").map(|id| (id, Vec::new())))
        .collect::<Result<_>>()?;
    for row in &rows {
        let message: i64 = row.try_get(0).map_err(|e| bad("message id", e))?;
        let file = file_of(row, 1)?;
        let message = record_id(message, "message id")?;
        let id = file.id;
        fields.extend(ATTACHMENT, id, file.entries());
        per_message.entry(message).or_default().push(id);
    }
    for (message, ids) in per_message {
        fields.set(MESSAGE, message, "attachment_ids", Wire::X2ManyReplace(ids));
    }
    Ok(())
}

/// Stage an empty `attachment_ids` for a message known to have no file (a message this call has
/// just created from a request that named none), so no statement is spent to learn it.
pub(crate) fn stage_none(message_id: i64, fields: &mut StoreFields) -> Result<()> {
    let id = record_id(message_id, "message id")?;
    fields.set(
        MESSAGE,
        id,
        "attachment_ids",
        Wire::X2ManyReplace(Vec::new()),
    );
    Ok(())
}

/// The attachments panel of a record: its newest `limit` files staged into `fields`, and the
/// panel's document (`attachment_ids` newest first, `has_more`) with the number of all the
/// record's files.
///
/// The caller has been allowed to read the record.
///
/// # Errors
/// `Validation` for a limit outside 1 to [`MAX_PANEL_ATTACHMENTS`]; `Internal` for a row that
/// does not decode.
pub(crate) async fn stage_of_record(
    tx: &mut Transaction,
    model: &str,
    id: i64,
    limit: i64,
    fields: &mut StoreFields,
) -> Result<(Json, i64)> {
    if !(1..=MAX_PANEL_ATTACHMENTS).contains(&limit) {
        return Err(Error::validation(format!(
            "The attachments panel lists 1 to {MAX_PANEL_ATTACHMENTS} files, not {limit}."
        )));
    }
    let rows = tx.query(OF_RECORD, &[&model, &id, &limit]).await?;
    let mut ids = Vec::with_capacity(rows.len());
    let mut count = 0_i64;
    for row in &rows {
        let file = file_of(row, 0)?;
        count = row.try_get(7).map_err(|e| bad("count", e))?;
        ids.push(file.id);
        fields.extend(ATTACHMENT, file.id, file.entries());
    }
    let has_more = usize::try_from(count).is_ok_and(|all| all > ids.len());
    Ok((json!({"attachment_ids": ids, "has_more": has_more}), count))
}

/// Most composer files (uploaded, not yet posted) one user may hold at a time; a fifty-first is
/// refused until some are posted or removed (the unposted ones also expire, see
/// the staging cleaner of the attachments addon, after 24 hours).
pub const MAX_PENDING_PER_USER: i64 = 50;
/// Historical advisory lock class, retained for compatibility. Pending admission now uses a
/// transactional generation row; an advisory lock alone cannot refresh an older snapshot.
/// Previously the first key that serialized uploads of one user
/// (second key: `hashtext` of the user id); registered next to the other classes in
/// `erp_sql::locks` (`MAIL_PENDING_UPLOAD_LOCK_CLASS`; a test pins the two equal).
pub const PENDING_LOCK_CLASS: i32 = 1706;

/// What `thread_upload` and `thread_attach` answer for the file they created.
fn uploaded(row: &erp_sdk::Row) -> Result<Value> {
    let text = |i: usize, what: &str| -> Result<Value> {
        let v: Option<String> = row.try_get(i).map_err(|e| bad(what, e))?;
        Ok(Value::Str(Arc::from(v.unwrap_or_default())))
    };
    let mut out = ValueMap::default();
    out.insert(
        "attachment_id".to_owned(),
        Value::Int(row.try_get::<_, i64>(0).map_err(|e| bad("id", e))?),
    );
    out.insert("name".to_owned(), text(1, "name")?);
    out.insert("mimetype".to_owned(), text(2, "mimetype")?);
    out.insert(
        "file_size".to_owned(),
        Value::Int(row.try_get::<_, i64>(3).map_err(|e| bad("file_size", e))?),
    );
    out.insert("checksum".to_owned(), text(4, "checksum")?);
    Ok(Value::Map(Arc::new(out)))
}

/// Adopt one bounded in-memory source or staged upload for `record`: `composer` keeps it pending (no record,
/// its creator's alone, claimed by `thread_post`), otherwise it is attached to the record at once
/// (the attachments panel). Content above the inline threshold is refused by name until the HTTP
/// upload route exists. Public callers are refused; a pending file needs the access a
/// post needs; an attached one needs write access to the record (`ir.attachment` create).
pub(crate) async fn upload(
    tx: &mut Transaction,
    record: &Records,
    name: String,
    mimetype: Option<String>,
    content: AttachmentContent,
    composer: bool,
) -> Result<Value> {
    let env = record.env().clone();
    let id = record
        .ids()
        .first()
        .copied()
        .ok_or_else(|| Error::invariant("a single-record call has no record id"))?;
    if crate::thread_rpc::standing(tx, &env).await? == crate::thread_rpc::Standing::Public {
        return Err(Error::access(
            "Public users cannot upload files to a thread.",
        ));
    }
    let mut item = NewAttachment::new(name, content);
    if let Some(m) = mimetype {
        item = item.declared(m);
    }
    if composer {
        let operation: Operation = record.call(tx, "_mail_post_access", ()).await?;
        record.check_access(tx, operation).await?;
    } else {
        record.check_access(tx, Operation::Write).await?;
        item = item.on(ParentRef::new(record.model_name().to_owned(), id));
    }
    let created = if composer {
        attachments::create_pending(tx, crate::post::COMPOSER_MODEL, usize::try_from(MAX_PENDING_PER_USER).map_err(|_| Error::invariant("The pending file limit must be positive."))?, vec![item]).await.map_err(|error| {
            if std::error::Error::source(&error).is_some_and(|source| source.is::<attachments::PendingLimitExceeded>()) {
                Error::validation(format!("At most {MAX_PENDING_PER_USER} uploaded files wait for a message; post or remove some first."))
            } else { error }
        })?
    } else {
        attachments::create(tx, vec![item]).await?
    };
    let new_id = created
        .first()
        .map(|r| r.get())
        .ok_or_else(|| Error::invariant("attachments::create returned no id"))?;
    let rows = tx
        .query(
            "SELECT id::int8, name::text, mimetype::text, file_size::int8, checksum::text \
             FROM ir_attachment WHERE id = $1::int8",
            &[&new_id],
        )
        .await?;
    uploaded(
        rows.first()
            .ok_or_else(|| Error::invariant("the uploaded file cannot be read back"))?,
    )
}
