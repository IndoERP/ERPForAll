//! There is no `raw`, `datas`, `db_datas`, `store_fname` or `index_content` field: bytes never
//! live in this table. `blob_id` is a column the ORM does not know (it would show which records
//! share content); only the attachments service sets it. `file_size`, `checksum` and `mimetype`
//! are set by the server from the stored content and are refused from callers.

use erp_sdk::prelude::*;
use erp_sdk::{Cell, Operation, ValueMap};

use super::access::{self, Target};
use super::service::{self, in_scope};
use super::types::{ParentRef, URL_MAX_BYTES};
use super::upload::check_name;

/// `ir.attachment`.
#[model(
    name = "ir.attachment",
    description = "Attachment",
    order = "id desc",
    company_access = unrestricted(reason = "an attachment follows the access of the record it is attached to, not the company of the user"),
    indexes = [
        ("_res_idx", "(res_model, res_id)"),
        ("_checksum_idx", "(checksum)")
    ]
)]
pub struct IrAttachment {
    /// The file name.
    #[field(char, required)]
    pub name: Option<Arc<str>>,
    /// Free text.
    #[field(text)]
    pub description: Option<Arc<str>>,
    /// The model of the record the attachment belongs to.
    #[field(char, string = "Resource Model")]
    pub res_model: Option<Arc<str>>,
    /// The binary field of that record this attachment stores.
    #[field(char, string = "Resource Field")]
    pub res_field: Option<Arc<str>>,
    /// The record the attachment belongs to.
    #[field(many2one_reference, model_field = "res_model", string = "Resource ID")]
    pub res_id: i64,
    /// The company.
    #[field(many2one, comodel = "res.company", string = "Company")]
    pub company_id: Option<RecId>,
    /// `binary` (content in the store) or `url` (an external address).
    #[field(
        selection,
        selection = [("binary", "File"), ("url", "URL")],
        default = "binary",
        required
    )]
    pub r#type: Option<Arc<str>>,
    /// The address of a `url` attachment. The size is [`super::URL_MAX_BYTES`] (a macro
    /// attribute takes a literal; a test compares the column with the constant).
    #[field(char, size = 1024)]
    pub url: Option<Arc<str>>,
    /// Whether the content route may serve it without a session.
    #[field(boolean)]
    pub public: bool,
    /// Refused until link tokens exist.
    #[field(char, groups = ".", copy = false)]
    pub access_token: Option<Arc<str>>,
    /// The content size in bytes, set by the server.
    #[field(integer, readonly)]
    pub file_size: i32,
    #[field(char, size = 40, readonly)]
    pub checksum: Option<Arc<str>>,
    /// The detected media type, set by the server.
    #[field(char, readonly)]
    pub mimetype: Option<Arc<str>>,
}

fn text(vals: &RawVals, key: &str) -> Result<Option<Option<String>>> {
    match vals.get(key) {
        None => Ok(None),
        Some(Cell::Str(s)) => Ok(Some(Some(s.to_string()))),
        Some(Cell::Null | Cell::Bool(false)) => Ok(Some(None)),
        Some(other) => Err(Error::validation(format!(
            "ir.attachment {key} must be text, not {other:?}"
        ))),
    }
}

fn id_of(vals: &RawVals, key: &str) -> Result<Option<Option<i64>>> {
    match vals.get(key) {
        None => Ok(None),
        Some(Cell::Id(i)) => Ok(Some(Some(*i))),
        Some(Cell::Int(i)) => Ok(Some(Some(i64::from(*i)))),
        Some(Cell::Null | Cell::Bool(false)) => Ok(Some(None)),
        Some(other) => Err(Error::validation(format!(
            "ir.attachment {key} must be a record id, not {other:?}"
        ))),
    }
}

/// The checks of the values a caller writes, before any statement.
fn guard_vals(vals: &RawVals, service: bool) -> Result<()> {
    if !service {
        for key in ["file_size", "checksum", "mimetype"] {
            if vals.contains(key) {
                return Err(Error::validation(format!(
                    "ir.attachment {key} is set by the server from the stored content and cannot \
                     be written."
                )));
            }
        }
    }
    if vals.contains("access_token") {
        return Err(Error::validation(
            "Attachment access tokens are not available yet: they come with a later release.",
        ));
    }
    if let Some(Some(name)) = text(vals, "name")? {
        check_name(&name)?;
    }
    Ok(())
}

/// A `url` attachment points at an `http` or `https` address (T-UPL.4); a binary attachment's
/// `url` is the serving path of a static file and is not held to this.
async fn check_urls(tx: &mut Transaction, ids: &[i64]) -> Result<()> {
    if ids.is_empty() {
        return Ok(());
    }
    let sql = format!(
        "SELECT 1 FROM ir_attachment WHERE id = ANY($1::int8[]) AND type = 'url' \
         AND (url IS NULL OR octet_length(url) > {URL_MAX_BYTES} \
         OR url !~* '^https?://[^[:space:][:cntrl:]]+$') LIMIT 1"
    );
    let rows = tx.query(&sql, &[&ids]).await?;
    if rows.is_empty() {
        Ok(())
    } else {
        // the message of the API's own check, on a value that fails it
        service::check_url("")
    }
}

/// The attachments service runs it too,
/// for the rows it creates under its scope (a copy keeps the original's URL).
pub(crate) async fn check_serving(
    tx: &mut Transaction,
    env: &erp_sdk::Env,
    binary_with_url: bool,
) -> Result<()> {
    if !binary_with_url || env.su() {
        return Ok(());
    }
    if tx.has_group(env, "base.group_system").await? {
        return Ok(());
    }
    Err(Error::validation(
        "Sorry, you are not allowed to write on this document",
    ))
}

async fn check_circular(tx: &mut Transaction, ids: &[i64]) -> Result<()> {
    let rows = tx
        .query(
            "SELECT id FROM ir_attachment WHERE id = ANY($1::int8[]) \
             AND res_model = 'ir.attachment' AND res_id = id ORDER BY id LIMIT 1",
            &[&ids],
        )
        .await?;
    if let Some(row) = rows.first() {
        let id: i64 = row
            .try_get(0)
            .map_err(|e| Error::db(format!("an attachment id could not be read: {e}")))?;
        let record = format!("ir.attachment({id},)");
        return Err(Error::validation(format!(
            "You cannot attach an attachment to itself.\nAttachment {record} cannot have res_id: {record}"
        )));
    }
    Ok(())
}

#[methods(IrAttachment, addon = "base")]
impl IrAttachment {
    /// Check the values, then the parents the new attachments belong to (create needs write on
    /// the parent), then create.
    #[method(override)]
    async fn create(
        this: &RecordSet<IrAttachment>,
        tx: &mut Transaction,
        vals_list: Vec<RawVals>,
        sup: Super,
    ) -> Result<Records> {
        let service = in_scope(tx);
        let env = this.env().clone();
        let mut targets = Vec::with_capacity(vals_list.len());
        let mut binary_with_url = false;
        for vals in &vals_list {
            guard_vals(vals, service)?;
            let model = text(vals, "res_model")?.flatten();
            let res_id = id_of(vals, "res_id")?.flatten();
            targets.push(Target {
                id: None,
                res_model: model,
                res_id,
                res_field: text(vals, "res_field")?.flatten(),
                create_uid: env.uid(),
            });
            let url = text(vals, "url")?.flatten().is_some_and(|u| !u.is_empty());
            let kind = text(vals, "type")?.flatten();
            binary_with_url |= url && kind.as_deref().unwrap_or("binary") == "binary";
        }
        if !service {
            access::check(tx, &env, &targets, Operation::Create).await?;
            check_serving(tx, &env, binary_with_url).await?;
        }
        let created: Records = sup.call(tx, this.as_records(), (vals_list,)).await?;
        check_circular(tx, &created.ids()).await?;
        check_urls(tx, &created.ids()).await?;
        Ok(created)
    }

    /// Check the values, the attachments' current parents and the new parent, then write.
    #[method(override)]
    async fn write(
        this: &RecordSet<IrAttachment>,
        tx: &mut Transaction,
        vals: RawVals,
        sup: Super,
    ) -> Result<()> {
        let service = in_scope(tx);
        let env = this.env().clone();
        guard_vals(&vals, service)?;
        let ids = this.as_records().ids();
        let moves =
            vals.contains("res_model") || vals.contains("res_id") || vals.contains("res_field");
        if !service && !ids.is_empty() {
            let current = access::load_targets(tx, &ids).await?;
            access::check(tx, &env, &current, Operation::Write).await?;
            if moves {
                let new_model = text(&vals, "res_model")?;
                let new_id = id_of(&vals, "res_id")?;
                let new_field = text(&vals, "res_field")?;
                let after: Vec<Target> = current
                    .iter()
                    .map(|t| Target {
                        id: t.id,
                        res_model: new_model.clone().unwrap_or_else(|| t.res_model.clone()),
                        res_id: new_id.unwrap_or(t.res_id),
                        res_field: new_field.clone().unwrap_or_else(|| t.res_field.clone()),
                        create_uid: t.create_uid,
                    })
                    .collect();
                access::check(tx, &env, &after, Operation::Write).await?;
            }
        }
        sup.call(tx, this.as_records(), (vals.clone(),)).await?;
        if moves || vals.contains("url") || vals.contains("type") {
            tx.flush().await?;
        }
        if moves {
            check_circular(tx, &ids).await?;
        }
        if vals.contains("url") || vals.contains("type") {
            check_urls(tx, &ids).await?;
        }
        if !service && (vals.contains("url") || vals.contains("type")) && !ids.is_empty() {
            let rows = tx
                .query(
                    "SELECT 1 FROM ir_attachment WHERE id = ANY($1::int8[]) AND type = 'binary' \
                     AND coalesce(url, '') <> '' LIMIT 1",
                    &[&ids],
                )
                .await?;
            check_serving(tx, &env, !rows.is_empty()).await?;
        }
        Ok(())
    }

    #[method(override)]
    async fn unlink(
        this: &RecordSet<IrAttachment>,
        tx: &mut Transaction,
        sup: Super,
    ) -> Result<()> {
        let env = this.env().clone();
        let ids = this.as_records().ids();
        if !in_scope(tx) && !ids.is_empty() {
            let current = access::load_targets(tx, &ids).await?;
            access::check(tx, &env, &current, Operation::Unlink).await?;
        }
        sup.call(tx, this.as_records(), ()).await
    }

    /// A copy references the same content (no byte is copied); `default` may name a new
    /// `res_model` and `res_id`.
    #[method(override)]
    async fn copy(
        this: &RecordSet<IrAttachment>,
        tx: &mut Transaction,
        default: Option<ValueMap>,
        _sup: Super,
    ) -> Result<Records> {
        let mut model = None;
        let mut res_id = None;
        for (key, value) in default.iter().flatten() {
            match (key.as_str(), value) {
                ("res_model", Value::Str(m)) => model = Some(m.clone()),
                ("res_id", Value::Int(i)) => res_id = Some(*i),
                _ => {
                    return Err(Error::validation(format!(
                        "ir.attachment copy accepts only res_model and res_id as defaults, not \
                         {key}: the content is shared, not rewritten."
                    )));
                }
            }
        }
        let parent = match (model, res_id) {
            (Some(m), Some(i)) => Some(ParentRef::new(m.to_string(), i)),
            (None, None) => None,
            _ => {
                return Err(Error::validation(
                    "ir.attachment copy needs res_model and res_id together.",
                ));
            }
        };
        let ids = this.as_records().ids();
        let created = service::copy(tx, &ids, parent).await?;
        this.env()
            .records("ir.attachment")?
            .browse(created.iter().map(|i| i.get()))
    }
}
