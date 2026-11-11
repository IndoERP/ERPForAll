//! The attachments API for addons: attach, list, open, move, copy and unlink by record, with no
//! SQL on the caller's side.
//!
//! Every call checks access first (see [`super::access`]), then writes the `ir.attachment` rows
//! through the ORM under a private service scope (so the model's overrides accept the server-set
//! columns) and sets `blob_id` in one statement per batch.

use base64::Engine as _;
use erp_sdk::blob::{ContentKey, digests, normalize_declared, resolve, to_hex};
use erp_sdk::prelude::*;
use erp_sdk::{BoxFuture, Cell, Env, Operation, UnlinkHook};

use super::access::{self, DENIED_PARENT_MODELS, Target};
use super::link::{self, Source};
use super::model::check_serving;
use super::runtime::{col_error, map_link_fk, runtime, store_by_id};
use super::types::{
    AttachmentContent, AttachmentMeta, BlobHandle, HandleSource, ListOptions, MAX_BATCH,
    NewAttachment, ParentRef, URL_MAX_BYTES, UploadId,
};
use super::upload::check_name;

/// Set while the service writes through the ORM: the model's overrides then accept the columns
/// only the server sets. A private type, so no other crate can open the scope.
#[derive(Default)]
pub(crate) struct ServiceScope(u32);

pub(crate) fn in_scope(tx: &mut Transaction) -> bool {
    tx.extension::<ServiceScope>().0 > 0
}

pub(crate) fn enter_scope(tx: &mut Transaction) {
    tx.extension::<ServiceScope>().0 += 1;
}

pub(crate) fn leave_scope(tx: &mut Transaction) {
    let scope = tx.extension::<ServiceScope>();
    scope.0 = scope.0.saturating_sub(1);
}

macro_rules! col {
    ($row:expr, $i:expr) => {
        $row.try_get($i).map_err(|e| col_error("attachments", e))?
    };
}

/// A row to insert, before its content is linked.
struct Pending {
    name: String,
    description: Option<String>,
    parent: Option<ParentRef>,
    /// The placeholder model of a file that waits for its record (`res_id` 0).
    placeholder: Option<String>,
    res_field: Option<String>,
    company_id: Option<i64>,
    public: bool,
    kind: &'static str,
    url: Option<String>,
    source: Source,
    /// The type to store when linking yields none (a copy keeps the original's).
    mimetype: Option<String>,
    checksum: Option<String>,
    size: u64,
}

fn check_parent_shape(item: &NewAttachment) -> Result<()> {
    if let Some(model) = &item.pending_model {
        if item.parent.is_some() || item.res_field.is_some() {
            return Err(Error::validation(
                "A pending attachment belongs to a placeholder model, not to a record or a \
                 field: give no parent and no res_field.",
            ));
        }
        if model.is_empty() || model.len() > 128 {
            return Err(Error::validation(
                "An attachment's placeholder model name is 1 to 128 bytes.",
            ));
        }
        if DENIED_PARENT_MODELS.contains(&model.as_str()) {
            return Err(Error::validation(format!(
                "Attachments cannot be attached to {model}: it holds security data."
            )));
        }
    }
    if item.res_field.is_some() && item.parent.is_none() {
        return Err(Error::validation(
            "res_field names a field of the record the attachment belongs to: give a parent too.",
        ));
    }
    if let Some(parent) = &item.parent {
        if parent.id <= 0 {
            return Err(Error::validation(
                "An attachment's parent record id must be positive.",
            ));
        }
        if parent.model.is_empty() || parent.model.len() > 128 {
            return Err(Error::validation(
                "An attachment's parent model name is 1 to 128 bytes.",
            ));
        }
        if DENIED_PARENT_MODELS.contains(&parent.model.as_str()) {
            return Err(Error::validation(format!(
                "Attachments cannot be attached to {}: it holds security data.",
                parent.model
            )));
        }
    }
    Ok(())
}

pub(crate) fn check_url(url: &str) -> Result<()> {
    let lower = url.to_ascii_lowercase();
    if url.len() > URL_MAX_BYTES
        || !(lower.starts_with("http://") || lower.starts_with("https://"))
        || url.bytes().any(|b| b <= b' ' || b == 0x7f)
    {
        return Err(Error::validation(format!(
            "An attachment URL is an http or https address of at most {URL_MAX_BYTES} bytes \
             without spaces or control characters."
        )));
    }
    Ok(())
}

/// Resolve one new attachment's content into a link source and its stored properties.
async fn prepare(tx: &mut Transaction, env: &Env, item: NewAttachment) -> Result<Pending> {
    check_name(&item.name)?;
    check_parent_shape(&item)?;
    let rt = runtime(env)?;
    let limits = rt.limits().clone();
    let declared = match &item.declared_mimetype {
        Some(d) => Some(normalize_declared(d).map_err(|e| Error::validation(e.to_string()))?),
        None => None,
    };
    let mut pending = Pending {
        name: item.name.clone(),
        description: item.description.clone(),
        parent: item.parent.clone(),
        placeholder: item.pending_model.clone(),
        res_field: item.res_field.clone(),
        company_id: item.company_id,
        public: item.public,
        kind: "binary",
        url: None,
        source: Source::Empty,
        mimetype: declared.clone(),
        checksum: None,
        size: 0,
    };
    let bytes = match item.content {
        AttachmentContent::Empty => return Ok(pending),
        AttachmentContent::Url(url) => {
            check_url(&url)?;
            pending.kind = "url";
            pending.url = Some(url);
            pending.mimetype = None;
            return Ok(pending);
        }
        AttachmentContent::Upload(id) => {
            pending.source = Source::Upload(id);
            return Ok(pending);
        }
        AttachmentContent::Bytes(bytes) => bytes,
        AttachmentContent::Base64(text) => {
            let longest = (limits.inline_max_bytes.saturating_add(2) / 3 + 1) * 4;
            if text.len() > longest {
                return Err(upload_required(limits.inline_max_bytes));
            }
            base64::engine::general_purpose::STANDARD
                .decode(text.as_bytes())
                .map_err(|_| Error::user("Attachment is not encoded in base64."))?
        }
    };
    let _ = tx;
    if bytes.is_empty() {
        return Ok(pending);
    }
    if bytes.len() > limits.inline_max_bytes {
        return Err(upload_required(limits.inline_max_bytes));
    }
    let detected = resolve(declared.as_deref(), &bytes, Some(&item.name))
        .map_err(|e| Error::validation(e.to_string()))?;
    let (sha256, sha1) = digests(&bytes);
    pending.mimetype = Some(detected.mimetype.clone());
    pending.checksum = Some(to_hex(&sha1));
    pending.size = bytes.len() as u64;
    pending.source = Source::Inline {
        bytes,
        sha256,
        sha1,
        mimetype: detected.mimetype,
    };
    Ok(pending)
}

fn upload_required(inline_max: usize) -> Error {
    Error::validation(format!(
        "Content above {inline_max} bytes is not passed in memory: stream it with \
         attachments::Uploader::stage and link the upload."
    ))
}

/// The ORM values of a new row.
fn row_vals(env: &Env, p: &Pending, linked: &link::Linked) -> Result<RawVals> {
    let mimetype = linked.mimetype.clone().or_else(|| p.mimetype.clone());
    let checksum = linked
        .sha1
        .map(|s| to_hex(&s))
        .or_else(|| p.checksum.clone());
    let size = if linked.blob_id.is_some() && linked.size > 0 {
        linked.size
    } else {
        p.size
    };
    let size = i32::try_from(size).map_err(|_| {
        Error::validation(
            "An attachment is at most 2147483647 bytes (the reference ERP's file_size).",
        )
    })?;
    let company = match p.company_id {
        Some(c) => Some(c),
        None => env.company_id()?,
    };
    let mut vals = RawVals::new()
        .set_cell("name", Cell::Str(p.name.as_str().into()))
        .set_cell("type", Cell::Str(p.kind.into()))
        .set_cell("public", Cell::Bool(p.public))
        .set_cell("file_size", Cell::Int(size));
    if let Some(d) = &p.description {
        vals = vals.set_cell("description", Cell::Str(d.as_str().into()));
    }
    if let Some(parent) = &p.parent {
        vals = vals
            .set_cell("res_model", Cell::Str(parent.model.as_str().into()))
            .set_cell("res_id", Cell::Id(parent.id));
    }
    if let Some(model) = &p.placeholder {
        vals = vals
            .set_cell("res_model", Cell::Str(model.as_str().into()))
            .set_cell("res_id", Cell::Id(0));
    }
    if let Some(f) = &p.res_field {
        vals = vals.set_cell("res_field", Cell::Str(f.as_str().into()));
    }
    if let Some(c) = company {
        vals = vals.set_cell("company_id", Cell::Id(c));
    }
    if let Some(u) = &p.url {
        vals = vals.set_cell("url", Cell::Str(u.as_str().into()));
    }
    if let Some(m) = mimetype {
        vals = vals.set_cell("mimetype", Cell::Str(m.into()));
    }
    if let Some(c) = checksum {
        vals = vals.set_cell("checksum", Cell::Str(c.into()));
    }
    Ok(vals)
}

/// Check the parents, link the content, create the rows and set their blobs.
async fn insert(tx: &mut Transaction, env: &Env, pending: Vec<Pending>) -> Result<Vec<RecId>> {
    if pending.is_empty() {
        return Ok(Vec::new());
    }
    if pending.len() > MAX_BATCH {
        return Err(Error::validation(format!(
            "At most {MAX_BATCH} attachments are created in one call."
        )));
    }
    let uid = env.uid();
    let targets: Vec<Target> = pending
        .iter()
        .map(|p| Target {
            id: None,
            res_model: p.parent.as_ref().map(|x| x.model.clone()),
            res_id: p.parent.as_ref().map(|x| x.id),
            res_field: p.res_field.clone(),
            create_uid: uid,
        })
        .collect();
    access::check(tx, env, &targets, Operation::Create).await?;
    // The scoped
    // create below skips the model's own check, so it runs here for the caller: a copy keeps a
    // binary attachment's serving URL, and only the serving group may hold one.
    let binary_with_url = pending
        .iter()
        .any(|p| p.kind == "binary" && p.url.as_deref().is_some_and(|u| !u.is_empty()));
    check_serving(tx, env, binary_with_url).await?;
    let sources: Vec<Source> = pending.iter().map(|p| p.source.clone()).collect();
    let linked = link::link(tx, env, &sources).await?;
    let mut vals_list = Vec::with_capacity(pending.len());
    for (p, l) in pending.iter().zip(&linked) {
        vals_list.push(row_vals(env, p, l)?);
    }
    enter_scope(tx);
    let created = env
        .sudo(true)
        .records("ir.attachment")?
        .create(tx, vals_list)
        .await;
    leave_scope(tx);
    let created = created?;
    let ids = created.rec_ids().to_vec();
    let (att, blob): (Vec<i64>, Vec<i64>) = ids
        .iter()
        .zip(&linked)
        .filter_map(|(id, l)| l.blob_id.map(|b| (id.get(), b)))
        .unzip();
    if !att.is_empty() {
        tx.execute(
            "UPDATE ir_attachment a SET blob_id = x.b FROM unnest($1::int8[], $2::int8[]) x(i, b) \
             WHERE a.id = x.i",
            &[&att, &blob],
        )
        .await
        .map_err(map_link_fk)?;
    }
    Ok(ids)
}

/// Create attachments (one batch). The caller needs write access to each parent record (and, for a
/// field-bound attachment, to that field); a standalone attachment belongs to its creator.
///
/// # Errors
///
/// `Access` when a parent is refused; `Validation` for a bad name, URL, parent or declared type,
/// an upload that is not usable, or bytes above the inline threshold (stream them with
/// [`super::Uploader`]); `Programming` when no attachment runtime is attached.
pub async fn create(tx: &mut Transaction, items: Vec<NewAttachment>) -> Result<Vec<RecId>> {
    let env = tx.env();
    let mut pending = Vec::with_capacity(items.len());
    for item in items {
        pending.push(prepare(tx, &env, item).await?);
    }
    insert(tx, &env, pending).await
}

/// Standalone attachments come before field-bound ones, then
/// the oldest.
///
/// # Errors
///
/// As [`create`]; `User` ("Attachment is not encoded in base64.") for malformed base64.
pub async fn create_unique(tx: &mut Transaction, items: Vec<NewAttachment>) -> Result<Vec<RecId>> {
    let env = tx.env();
    let mut out = Vec::with_capacity(items.len());
    for item in items {
        let pending = prepare(tx, &env, item).await?;
        let (checksum, mimetype) = match &pending.source {
            Source::Upload(id) => upload_identity(tx, &env, id).await?,
            _ => (pending.checksum.clone(), pending.mimetype.clone()),
        };
        let mut reuse = None;
        if let (Some(checksum), Some(mimetype)) = (&checksum, &mimetype) {
            let rows = tx
                .query(
                    "SELECT id FROM ir_attachment WHERE checksum = $1 AND mimetype = $2 \
                     ORDER BY (res_field IS NOT NULL), id LIMIT 100",
                    &[checksum, mimetype],
                )
                .await?;
            let ids: Vec<i64> = rows
                .iter()
                .map(|r| r.try_get(0).map_err(|e| col_error("attachments", e)))
                .collect::<Result<_>>()?;
            if !ids.is_empty() {
                let targets = access::load_targets(tx, &ids).await?;
                let verdict = access::allowed(tx, &env, &targets, Operation::Write).await?;
                reuse = ids.iter().find_map(|id| {
                    targets
                        .iter()
                        .zip(&verdict)
                        .find(|(t, ok)| **ok && t.id == Some(*id))
                        .map(|_| *id)
                });
            }
        }
        match reuse {
            Some(id) => out.push(
                RecId::from_real(id)
                    .ok_or_else(|| Error::internal("an attachment id is not positive"))?,
            ),
            None => out.extend(insert(tx, &env, vec![pending]).await?),
        }
    }
    Ok(out)
}

/// The checksum and type of a staged upload the caller owns, without consuming it.
async fn upload_identity(
    tx: &mut Transaction,
    env: &Env,
    id: &UploadId,
) -> Result<(Option<String>, Option<String>)> {
    let rows = tx
        .query(
            "SELECT sha1, detected_mimetype FROM ir_blob_upload WHERE id = $1::text::uuid \
             AND state = 1 AND expires_at > now() AND subject_user = $2 AND actor = $3",
            &[&id.as_str(), &env.uid(), &super::upload::actor_text(env)],
        )
        .await?;
    let row = rows.first().ok_or_else(super::types::upload_not_usable)?;
    let sha1: Option<Vec<u8>> = col!(row, 0);
    Ok((sha1.map(|s| to_hex(&s)), col!(row, 1)))
}

/// Replace the content of existing attachments (the old blob becomes a garbage candidate).
///
/// # Errors
///
/// `Access` unless the caller may write every attachment; as [`create`] for the content.
pub async fn write_content(
    tx: &mut Transaction,
    ids: &[i64],
    content: AttachmentContent,
) -> Result<()> {
    if ids.is_empty() {
        return Ok(());
    }
    let env = tx.env();
    let targets = access::load_targets(tx, ids).await?;
    if targets.len() != ids.len() {
        return Err(access::denied(Operation::Write, ids, env.uid()));
    }
    access::check(tx, &env, &targets, Operation::Write).await?;
    let first = ids.first().copied().unwrap_or(0);
    let name_row = tx
        .query("SELECT name FROM ir_attachment WHERE id = $1", &[&first])
        .await?;
    let name: String = name_row
        .first()
        .map(|r| r.try_get(0))
        .transpose()
        .map_err(|e| col_error("attachments", e))?
        .unwrap_or_default();
    let item = NewAttachment::new(
        if name.is_empty() {
            "content".to_owned()
        } else {
            name
        },
        content,
    );
    let pending = prepare(tx, &env, item).await?;
    let sources = vec![pending.source.clone()];
    let linked = link::link(tx, &env, &sources).await?;
    let l = linked
        .first()
        .ok_or_else(|| Error::internal("no link result"))?;
    let vals = {
        let full = row_vals(&env, &pending, l)?;
        let mut v = RawVals::new();
        for key in ["file_size", "checksum", "mimetype"] {
            v = match full.get(key) {
                Some(cell) => v.set_cell(key, cell.clone()),
                None => v.set_cell(key, Cell::Null),
            };
        }
        v
    };
    enter_scope(tx);
    let written = env
        .sudo(true)
        .records("ir.attachment")?
        .browse(ids.iter().copied())?
        .write(tx, vals)
        .await;
    leave_scope(tx);
    written?;
    tx.flush().await?;
    let blob: Option<i64> = l.blob_id;
    tx.execute(
        "UPDATE ir_attachment SET blob_id = $1 WHERE id = ANY($2::int8[])",
        &[&blob, &ids],
    )
    .await
    .map_err(map_link_fk)?;
    Ok(())
}

/// Copy attachments. The copies reference the same content: no byte is copied (
/// decision 4). `parent` re-parents the copies (`None` keeps each original's parent).
///
/// # Errors
///
/// `Access` unless the caller may read the originals and write the target parents.
pub async fn copy(
    tx: &mut Transaction,
    ids: &[i64],
    parent: Option<ParentRef>,
) -> Result<Vec<RecId>> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let env = tx.env();
    let targets = access::load_targets(tx, ids).await?;
    if targets.len() != ids.len() {
        return Err(access::denied(Operation::Read, ids, env.uid()));
    }
    access::check(tx, &env, &targets, Operation::Read).await?;
    let rows = tx
        .query(
            "SELECT id, name, description, res_model, res_id, res_field, mimetype, checksum, \
             file_size, coalesce(public, false), type, url, company_id, blob_id FROM ir_attachment \
             WHERE id = ANY($1::int8[]) ORDER BY id",
            &[&ids],
        )
        .await?;
    let mut pending = Vec::with_capacity(rows.len());
    for row in &rows {
        let model: Option<String> = col!(row, 3);
        let res_id: Option<i64> = col!(row, 4);
        let kind: String = col!(row, 10);
        let blob: Option<i64> = col!(row, 13);
        let size: Option<i32> = col!(row, 8);
        let own_parent = match (model, res_id) {
            (Some(m), Some(i)) if i > 0 => Some(ParentRef::new(m, i)),
            _ => None,
        };
        pending.push(Pending {
            name: col!(row, 1),
            description: col!(row, 2),
            parent: parent.clone().or(own_parent),
            placeholder: None,
            res_field: if parent.is_some() { None } else { col!(row, 5) },
            company_id: col!(row, 12),
            public: col!(row, 9),
            kind: if kind == "url" { "url" } else { "binary" },
            url: col!(row, 11),
            source: blob.map_or(Source::Empty, Source::Blob),
            mimetype: col!(row, 6),
            checksum: col!(row, 7),
            size: u64::try_from(size.unwrap_or(0)).unwrap_or(0),
        });
    }
    insert(tx, &env, pending).await
}

/// Move attachments to another record (`None`: make them standalone), as an activity's files move
/// to its message when it is done. The caller needs write access to the attachments' current
/// parents and to the new one.
///
/// # Errors
///
/// `Access` when either side is refused; `Validation` for an attachment moved onto itself.
pub async fn reattach(tx: &mut Transaction, ids: &[i64], parent: Option<ParentRef>) -> Result<()> {
    if ids.is_empty() {
        return Ok(());
    }
    let env = tx.env();
    let current = access::load_targets(tx, ids).await?;
    if current.len() != ids.len() {
        return Err(access::denied(Operation::Write, ids, env.uid()));
    }
    access::check(tx, &env, &current, Operation::Write).await?;
    let vals = match &parent {
        Some(p) => {
            let shape = NewAttachment::new("x", AttachmentContent::Empty).on(p.clone());
            check_parent_shape(&shape)?;
            RawVals::new()
                .set_cell("res_model", Cell::Str(p.model.as_str().into()))
                .set_cell("res_id", Cell::Id(p.id))
        }
        None => RawVals::new()
            .set_cell("res_model", Cell::Null)
            .set_cell("res_id", Cell::Null)
            .set_cell("res_field", Cell::Null),
    };
    // the model override checks the new parent and the circular reference
    env.records("ir.attachment")?
        .browse(ids.iter().copied())?
        .write(tx, vals)
        .await?;
    tx.flush().await
}

/// Which files [`adopt`] accepts besides the caller's own composer files.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdoptScope {
    ComposerAndRecord,
    /// A file already on the record is
    /// refused by name.
    ComposerOnly,
}

/// Each id must be one of:
///
/// - a file the caller uploaded for a record that did not exist yet: its `res_model` is one of
///   `placeholders`, its record id is 0, it is not field-bound and its creator is the caller;
///   it is moved onto `parent`;
/// - a file already on `parent` (not field-bound): left as it is, only for
///   [`AdoptScope::ComposerAndRecord`].
///
/// **Not for public or shared principals until tokens exist:** the creator test
/// compares the caller's user id, and a shared or anonymous principal has no stable creator.
///
/// Anything else (another user's pending file, a file of another record, a field-bound file, an
/// id that does not exist) refuses the whole call and names the ids, and nothing is moved; a
/// missing and a foreign id read alike, so the answer tells nothing about other records.
/// This function refuses both cases by name (standards §1) and,
/// for a file already on the record, asks for write access to the record.
///
/// The move runs as the system inside the private service scope, set-based (one statement for the
/// batch): unlike [`reattach`] it does not ask for write access to `parent` for the caller's own
/// composer files, because the right to add one is the caller's right to post, which the calling
/// path checked. **The caller must have checked that access before this call.** Files already on
/// the record are checked here for write access.
///
/// # Errors
///
/// `Validation` for more than [`MAX_BATCH`] ids, a non-positive id, or a `parent` that is not a
/// valid parent; `Access` naming the refused ids, or when the caller may not write a file that is
/// already on the record.
pub async fn adopt(
    tx: &mut Transaction,
    ids: &[i64],
    parent: ParentRef,
    placeholders: &[&str],
    scope: AdoptScope,
) -> Result<()> {
    if ids.is_empty() {
        return Ok(());
    }
    if ids.len() > MAX_BATCH {
        return Err(Error::validation(format!(
            "At most {MAX_BATCH} attachments are claimed in one call."
        )));
    }
    if let Some(bad) = ids.iter().find(|id| **id <= 0) {
        return Err(Error::validation(format!(
            "An attachment id is a positive number, not {bad}."
        )));
    }
    check_parent_shape(&NewAttachment::new("x", AttachmentContent::Empty).on(parent.clone()))?;
    let env = tx.env();
    let uid = env.uid();
    // locked: the decision below and the move must see the same rows
    let targets = access::load_targets_locked(tx, ids).await?;
    let found: std::collections::BTreeSet<i64> = targets.iter().filter_map(|t| t.id).collect();
    let mut refused: Vec<i64> = ids
        .iter()
        .copied()
        .filter(|id| !found.contains(id))
        .collect();
    let mut to_move: Vec<i64> = Vec::new();
    let mut on_record: Vec<access::Target> = Vec::new();
    for target in &targets {
        let Some(id) = target.id else {
            continue;
        };
        let on_parent = target.res_field.is_none()
            && target.res_model.as_deref() == Some(parent.model.as_str())
            && target.res_id == Some(parent.id);
        let pending = target.res_field.is_none()
            && target.create_uid == uid
            && target.res_id.unwrap_or(0) == 0
            && target
                .res_model
                .as_deref()
                .is_some_and(|model| placeholders.contains(&model));
        if on_parent && scope == AdoptScope::ComposerAndRecord {
            on_record.push(target.clone());
            continue;
        }
        if pending {
            to_move.push(id);
        } else {
            refused.push(id);
        }
    }
    if !refused.is_empty() {
        refused.sort_unstable();
        refused.dedup();
        let allowed = match scope {
            AdoptScope::ComposerAndRecord => {
                "Only files you uploaded for this purpose and files already on the record can be used."
            }
            AdoptScope::ComposerOnly => "Only files you uploaded for this purpose can be used.",
        };
        return Err(Error::access(format!(
            "These attachments cannot be linked here: {refused:?}. {allowed}"
        )));
    }
    if !on_record.is_empty() {
        access::check(tx, &env, &on_record, Operation::Write).await?;
    }
    if to_move.is_empty() {
        return Ok(());
    }
    let vals = RawVals::new()
        .set_cell("res_model", Cell::Str(parent.model.as_str().into()))
        .set_cell("res_id", Cell::Id(parent.id));
    enter_scope(tx);
    let moved = env
        .sudo(true)
        .records("ir.attachment")?
        .browse(to_move)?
        .write(tx, vals)
        .await;
    leave_scope(tx);
    moved?;
    tx.flush().await
}

/// List the attachments of records of `model` the caller may read.
///
/// # Errors
///
/// `Validation` for a limit outside 1 to 10,000.
pub async fn for_record(
    tx: &mut Transaction,
    model: &str,
    res_ids: &[i64],
    options: ListOptions,
) -> Result<Vec<AttachmentMeta>> {
    if options.limit == 0 || options.limit > 10_000 {
        return Err(Error::validation(
            "A listing returns 1 to 10000 attachments.",
        ));
    }
    if res_ids.is_empty() {
        return Ok(Vec::new());
    }
    let env = tx.env();
    let mut visible: Vec<i64> = res_ids.to_vec();
    if !env.su() {
        if DENIED_PARENT_MODELS.contains(&model) || env.registry().model(model).is_none() {
            return Ok(Vec::new());
        }
        if let Some(field) = &options.res_field
            && !tx
                .has_field_access(&env, model, field, erp_sdk::FieldOp::Read)
                .await?
        {
            return Ok(Vec::new());
        }
        let records = env.records(model)?.browse(res_ids.iter().copied())?;
        visible = records.filtered_access(tx, Operation::Read).await?.ids();
    }
    if visible.is_empty() {
        return Ok(Vec::new());
    }
    let limit = i64::try_from(options.limit).unwrap_or(1000);
    let rows = tx
        .query(
            "SELECT id, name, res_model, res_id, res_field, mimetype, file_size, checksum, type, \
             url, coalesce(public, false), create_uid FROM ir_attachment \
             WHERE res_model = $1 AND res_id = ANY($2::int8[]) \
             AND res_field IS NOT DISTINCT FROM $3::text ORDER BY id DESC LIMIT $4",
            &[&model, &visible, &options.res_field, &limit],
        )
        .await?;
    let mut out = Vec::with_capacity(rows.len());
    for row in &rows {
        let size: Option<i32> = col!(row, 6);
        out.push(AttachmentMeta {
            id: col!(row, 0),
            name: col!(row, 1),
            res_model: col!(row, 2),
            res_id: col!(row, 3),
            res_field: col!(row, 4),
            mimetype: col!(row, 5),
            file_size: i64::from(size.unwrap_or(0)),
            checksum: col!(row, 7),
            kind: col!(row, 8),
            url: col!(row, 9),
            public: col!(row, 10),
            create_uid: col!(row, 11),
        });
    }
    Ok(out)
}

/// Open the content of attachment `id` for the caller.
///
/// # Errors
///
/// `Access` (one message for a missing and a refused attachment); `Validation` for a `url`
/// attachment; `Internal` (`AttachmentContentMissing`) when a blob row exists whose bytes do not.
pub async fn open(tx: &mut Transaction, id: i64) -> Result<BlobHandle> {
    let env = tx.env();
    let rows = tx
        .query(
            "SELECT type, blob_id, file_size, mimetype, checksum FROM ir_attachment WHERE id = $1",
            &[&id],
        )
        .await?;
    let Some(row) = rows.first() else {
        return Err(access::denied(Operation::Read, &[id], env.uid()));
    };
    let targets = access::load_targets(tx, &[id]).await?;
    access::check(tx, &env, &targets, Operation::Read).await?;
    let kind: String = col!(row, 0);
    if kind == "url" {
        return Err(Error::validation(
            "This attachment is a link to an external URL: it has no stored content.",
        ));
    }
    let blob: Option<i64> = col!(row, 1);
    let size: Option<i32> = col!(row, 2);
    let mimetype: Option<String> = col!(row, 3);
    let checksum: Option<String> = col!(row, 4);
    let mimetype = mimetype.unwrap_or_else(|| "application/octet-stream".to_owned());
    let checksum = checksum.unwrap_or_default();
    let Some(blob) = blob else {
        return Ok(BlobHandle::new(
            id,
            0,
            mimetype,
            checksum,
            HandleSource::Empty,
        ));
    };
    let rows = tx
        .query(
            "SELECT b.sha256, b.store_id, b.size, i.data FROM ir_blob b \
             LEFT JOIN ir_blob_inline i ON i.blob_id = b.id WHERE b.id = $1",
            &[&blob],
        )
        .await?;
    let missing = || {
        Error::internal(format!(
            "AttachmentContentMissing: the content of attachment {id} has no blob row"
        ))
    };
    let row = rows.first().ok_or_else(missing)?;
    let sha: Vec<u8> = col!(row, 0);
    let store_id: Option<i16> = col!(row, 1);
    let blob_size: i64 = col!(row, 2);
    let inline: Option<Vec<u8>> = col!(row, 3);
    let sha256 = <[u8; 32]>::try_from(sha.as_slice())
        .map_err(|_| Error::db("an ir_blob digest is not 32 bytes"))?;
    let source = match (inline, store_id) {
        (Some(bytes), _) => HandleSource::Inline(std::sync::Arc::new(bytes)),
        (None, Some(store_id)) => {
            let rt = runtime(&env)?;
            let store = store_by_id(tx, &rt, store_id).await?;
            HandleSource::Store {
                key: ContentKey::new(store.namespace.clone(), sha256),
                store: store.store.clone(),
            }
        }
        (None, None) => return Err(missing()),
    };
    let _ = size;
    Ok(BlobHandle::new(
        id,
        u64::try_from(blob_size).unwrap_or(0),
        mimetype,
        checksum,
        source,
    ))
}

/// The caller already deleted the records, which they were allowed to; no further
/// access check applies to the files that followed.
///
/// # Errors
///
/// The database's.
pub async fn unlink_for_record(tx: &mut Transaction, model: &str, res_ids: &[i64]) -> Result<u64> {
    if res_ids.is_empty() {
        return Ok(0);
    }
    tx.execute(
        "DELETE FROM ir_attachment WHERE res_model = $1 AND res_id = ANY($2::int8[])",
        &[&model, &res_ids],
    )
    .await
}

fn on_unlink<'a>(
    tx: &'a mut Transaction,
    model: &'a str,
    ids: &'a [RecId],
) -> BoxFuture<'a, Result<()>> {
    Box::pin(async move {
        // a registry without the attachment model has nothing to clean
        if tx.registry().model("ir.attachment").is_none() {
            return Ok(());
        }
        let raw: Vec<i64> = ids.iter().map(|i| i.get()).collect();
        unlink_for_record(tx, model, &raw).await.map(|_| ())
    })
}

/// The unlink hook the composition root registers with `OrmBuilder::unlink_hook`: deleting a
/// record deletes its attachments, in one statement per `unlink` call.
pub const UNLINK_HOOK: UnlinkHook = UnlinkHook::new(on_unlink);
