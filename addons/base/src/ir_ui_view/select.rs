//! Which view a client gets: the default view of a
//! model and type, and the `<type>_view_ref` context override.
//!
//! A `*_view_ref` that cannot be used (not a qualified external id, not found, or a view of
//! another model or type) falls back to the default view, and the reason is returned as a
//! [`ViewNotice`], not only logged.
//!
//! ```ignore
//! let form = ViewType::new(&tx, "form")?;
//! let picked = select_view(&mut tx, &ViewRequest::new("res.partner", &form)).await?;
//! assert!(picked.notices().is_empty());
//! ```

use std::fmt;

use erp_sdk::prelude::*;
use erp_sdk::views::record::{RecordLimit, ViewRecordError};
use erp_sdk::{Context, Error, PrincipalError, QualifiedXmlId};

use super::{MODEL, view_error, view_types};

/// Most bytes a `*_view_ref` value may have.
pub const MAX_VIEW_REF_BYTES: usize = 256;

/// A view type the registry's `ir.ui.view.type` selection has (addons add types with
/// `selection_add`).
///
/// The check is made against the registry of the transaction given to [`ViewType::new`]; the
/// value carries no tie to that registry. Use it within that transaction: after a registry
/// change (the addon that added the type uninstalled), make it again (review T-12).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ViewType(Arc<str>);

impl ViewType {
    /// `value` as a view type of the transaction's registry.
    ///
    /// # Errors
    ///
    /// `Validation` "Unknown view type '<value>'. Allowed types are: ..." when the selection has
    /// no such value.
    pub fn new(tx: &Transaction, value: &str) -> Result<Self> {
        let allowed = view_types(tx)?;
        if !allowed.iter().any(|t| t == value) {
            return Err(view_error(ViewRecordError::UnknownViewType {
                value: value.to_owned(),
                allowed,
            }));
        }
        Ok(Self(Arc::from(value)))
    }

    /// The type's name.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ViewType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// The id of an `ir.ui.view` record (positive).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ViewId(i64);

impl ViewId {
    /// `id` as a view id.
    ///
    /// # Errors
    ///
    /// `Programming` for an id that is not positive.
    pub fn new(id: i64) -> Result<Self> {
        if id <= 0 {
            return Err(Error::programming(format!(
                "{id} is not an ir.ui.view record id"
            )));
        }
        Ok(Self(id))
    }

    /// The record id.
    pub fn get(self) -> i64 {
        self.0
    }
}

/// What a client asks for: a model and a view type, optionally a view, read with a context
/// (the transaction's when none is given).
#[derive(Debug, Clone, Copy)]
#[must_use]
pub struct ViewRequest<'a> {
    model: &'a str,
    view_type: &'a ViewType,
    view_id: Option<ViewId>,
    context: Option<&'a Context>,
}

impl<'a> ViewRequest<'a> {
    /// The view of `view_type` for `model`.
    pub fn new(model: &'a str, view_type: &'a ViewType) -> Self {
        Self {
            model,
            view_type,
            view_id: None,
            context: None,
        }
    }

    /// A given view: no default and no `*_view_ref` lookup; [`select_view`] checks that it exists
    /// and is of the request's model and type.
    pub fn view_id(mut self, id: ViewId) -> Self {
        self.view_id = Some(id);
        self
    }

    /// The context `*_view_ref` is read from.
    pub fn context(mut self, context: &'a Context) -> Self {
        self.context = Some(context);
        self
    }

    /// The model.
    pub fn model(&self) -> &'a str {
        self.model
    }

    /// The view type.
    pub fn view_type(&self) -> &'a ViewType {
        self.view_type
    }
}

/// Why a `*_view_ref` was not used; the default view was taken instead.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ViewNotice {
    RefNotQualified {
        /// The context key (`form_view_ref`, ...).
        key: String,
        /// The value.
        value: String,
        /// What is wrong with it, as a predicate ("has no module part"; review F-6).
        problem: &'static str,
    },
    RefNotFound {
        /// The context key.
        key: String,
        /// The external id.
        xmlid: QualifiedXmlId,
    },
    /// The view is of another model (BUG-073).
    RefOtherModel {
        /// The context key.
        key: String,
        /// The external id.
        xmlid: QualifiedXmlId,
        /// The view's model (`None` for a view without one).
        model: Option<String>,
    },
    /// The view is of another type (BUG-073).
    RefOtherType {
        /// The context key.
        key: String,
        /// The external id.
        xmlid: QualifiedXmlId,
        /// The view's type (`None` when unset).
        view_type: Option<String>,
    },
}

impl fmt::Display for ViewNotice {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::RefNotQualified {
                key,
                value,
                problem,
            } => write!(
                f,
                "'{key}' requires a fully-qualified external id (got: '{value}', which \
                 {problem}); the default view is used"
            ),
            Self::RefNotFound { key, xmlid } => {
                write!(f, "'{key}': no view '{xmlid}'; the default view is used")
            }
            Self::RefOtherModel { key, xmlid, model } => write!(
                f,
                "'{key}': view '{xmlid}' is a view of model {}; the default view is used",
                quoted(model.as_deref())
            ),
            Self::RefOtherType {
                key,
                xmlid,
                view_type,
            } => write!(
                f,
                "'{key}': view '{xmlid}' is a view of type {}; the default view is used",
                quoted(view_type.as_deref())
            ),
        }
    }
}

fn quoted(value: Option<&str>) -> String {
    value.map_or_else(|| "False".to_owned(), |v| format!("'{v}'"))
}

/// The view picked for a [`ViewRequest`], and why a `*_view_ref` was not used. `#[must_use]`:
/// the notices are the caller's to pass on (review F-6).
#[derive(Debug, Clone, PartialEq, Eq)]
#[must_use = "the notices say why a *_view_ref was not used; pass them on"]
pub struct SelectedView {
    id: ViewId,
    notices: Vec<ViewNotice>,
}

impl SelectedView {
    /// The view.
    pub fn id(&self) -> ViewId {
        self.id
    }

    /// Why the context's `*_view_ref` was not used (empty when it was, or when there was none).
    pub fn notices(&self) -> &[ViewNotice] {
        &self.notices
    }
}

/// One statement, served by the `_default_view` partial index (its predicate is
/// `mode = 'primary' AND active IS TRUE`, which the query repeats word for word so the planner
/// can use it). A missing model is matched with `model IS NULL`, not `IS NOT DISTINCT FROM $1`:
/// PostgreSQL cannot use a btree index for `IS NOT DISTINCT FROM`, it can for `IS NULL`.
///
/// # Errors
///
/// A database error.
pub async fn default_view(
    tx: &mut Transaction,
    model: Option<&str>,
    view_type: &ViewType,
) -> Result<Option<ViewId>> {
    super::tree::flush(tx).await?;
    let rows = match model {
        Some(model) => {
            tx.query(
                "SELECT id FROM ir_ui_view \
                  WHERE model = $1 AND type = $2 AND mode = 'primary' AND active IS TRUE \
                  ORDER BY priority, name, id LIMIT 1",
                &[&model, &view_type.as_str()],
            )
            .await?
        }
        None => {
            tx.query(
                "SELECT id FROM ir_ui_view \
                  WHERE model IS NULL AND type = $1 AND mode = 'primary' AND active IS TRUE \
                  ORDER BY priority, name, id LIMIT 1",
                &[&view_type.as_str()],
            )
            .await?
        }
    };
    match rows.first() {
        None => Ok(None),
        Some(row) => {
            let id: i64 = row
                .try_get(0)
                .map_err(|e| Error::db(format!("unexpected value in ir_ui_view.id: {e}")))?;
            ViewId::new(id).map(Some)
        }
    }
}

/// The view a [`ViewRequest`] gets: the given view, once checked (one statement); else the view
/// the context's `<type>_view_ref` names (one statement, joined with the view's model and type);
/// else the default view.
///
/// # Errors
///
/// when there is no
/// view at all; `Validation` for a `*_view_ref` that is not text or longer than
/// [`MAX_VIEW_REF_BYTES`]; a database error.
pub async fn select_view(tx: &mut Transaction, req: &ViewRequest<'_>) -> Result<SelectedView> {
    if let Some(id) = req.view_id {
        given(tx, id, req).await?;
        return Ok(SelectedView {
            id,
            notices: Vec::new(),
        });
    }
    let key = format!("{}_view_ref", req.view_type);
    let value = match req.context {
        Some(context) => context.get(&key),
        None => tx.env().context().get(&key),
    };
    let mut notices = Vec::new();
    match value {
        None | Some(Value::False | Value::Bool(false)) => {}
        Some(Value::Str(text)) if text.is_empty() => {}
        Some(Value::Str(text)) => {
            if text.len() > MAX_VIEW_REF_BYTES {
                return Err(view_error(ViewRecordError::Limit {
                    limit: RecordLimit::ViewRefBytes { key: key.clone() },
                    actual: text.len(),
                    max: MAX_VIEW_REF_BYTES,
                }));
            }
            match QualifiedXmlId::parse(&text) {
                Err(PrincipalError::InvalidXmlId { problem, .. }) => {
                    notices.push(ViewNotice::RefNotQualified {
                        key: key.clone(),
                        value: text.to_string(),
                        problem,
                    });
                }
                Err(other) => {
                    return Err(Error::invariant(format!(
                        "parsing context['{key}'] as an external id failed with {other:?}, \
                         not an InvalidXmlId"
                    )));
                }
                Ok(xmlid) => match by_ref(tx, &key, xmlid, req).await? {
                    Ok(id) => return Ok(SelectedView { id, notices }),
                    Err(notice) => notices.push(notice),
                },
            }
        }
        Some(other) => {
            return Err(Error::validation(format!(
                "context['{key}'] must be the external id of a view, not {other:?}"
            )));
        }
    }
    match default_view(tx, Some(req.model), req.view_type).await? {
        Some(id) => Ok(SelectedView { id, notices }),
        None => Err(Error::user(format!(
            "No default view of type '{}' could be found! (generated default views come with \
             a later release)",
            req.view_type
        ))),
    }
}

/// The view `xmlid` names, when it is a view of the request's model and type.
async fn by_ref(
    tx: &mut Transaction,
    key: &str,
    xmlid: QualifiedXmlId,
    req: &ViewRequest<'_>,
) -> Result<std::result::Result<ViewId, ViewNotice>> {
    if tx
        .registry()
        .model("ir.model.data")
        .is_none_or(|m| !m.has_table())
    {
        // no external id can exist
        return Ok(Err(ViewNotice::RefNotFound {
            key: key.to_owned(),
            xmlid,
        }));
    }
    super::tree::flush(tx).await?;
    // The join reads external-id writes that may still be dirty in this transaction.
    let model_data = tx
        .registry()
        .model("ir.model.data")
        .ok_or_else(|| Error::invariant("the external-id model disappeared during view selection"))?
        .id;
    tx.flush_model(model_data).await?;
    let rows = tx
        .query(
            "SELECT v.id, v.model, v.type FROM ir_model_data md \
               JOIN ir_ui_view v ON v.id = md.res_id \
              WHERE md.model = $1 AND md.module = $2 AND md.name = $3",
            &[&MODEL, &xmlid.module(), &xmlid.name()],
        )
        .await?;
    let Some(row) = rows.first() else {
        return Ok(Err(ViewNotice::RefNotFound {
            key: key.to_owned(),
            xmlid,
        }));
    };
    let id: i64 = row
        .try_get(0)
        .map_err(|e| Error::db(format!("unexpected value in ir_ui_view.id: {e}")))?;
    let model: Option<String> = row
        .try_get(1)
        .map_err(|e| Error::db(format!("unexpected value in ir_ui_view.model: {e}")))?;
    let view_type: Option<String> = row
        .try_get(2)
        .map_err(|e| Error::db(format!("unexpected value in ir_ui_view.type: {e}")))?;
    if model.as_deref() != Some(req.model) {
        return Ok(Err(ViewNotice::RefOtherModel {
            key: key.to_owned(),
            xmlid,
            model,
        }));
    }
    if view_type.as_deref() != Some(req.view_type.as_str()) {
        return Ok(Err(ViewNotice::RefOtherType {
            key: key.to_owned(),
            xmlid,
            view_type,
        }));
    }
    ViewId::new(id).map(Ok)
}

/// Check a given view (review A-T5): it exists, and is a view of the request's model and type.
async fn given(tx: &mut Transaction, id: ViewId, req: &ViewRequest<'_>) -> Result<()> {
    super::tree::flush(tx).await?;
    let rows = tx
        .query(
            "SELECT model, type FROM ir_ui_view WHERE id = $1",
            &[&id.get()],
        )
        .await?;
    let Some(row) = rows.first() else {
        return Err(Error::missing(format!(
            "Record does not exist or has been deleted.\n(Record: ir.ui.view({}))",
            id.get()
        )));
    };
    let model: Option<String> = row
        .try_get(0)
        .map_err(|e| Error::db(format!("unexpected value in ir_ui_view.model: {e}")))?;
    let view_type: Option<String> = row
        .try_get(1)
        .map_err(|e| Error::db(format!("unexpected value in ir_ui_view.type: {e}")))?;
    if model.as_deref() != Some(req.model) {
        return Err(Error::validation(format!(
            "View {} is a view of model {}, not of '{}'",
            id.get(),
            quoted(model.as_deref()),
            req.model
        )));
    }
    if view_type.as_deref() != Some(req.view_type.as_str()) {
        return Err(Error::validation(format!(
            "View {} is a view of type {}, not '{}'",
            id.get(),
            quoted(view_type.as_deref()),
            req.view_type
        )));
    }
    Ok(())
}
