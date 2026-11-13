//! Window actions: the `views` they open, the `view_mode` rules and the model checks.
//!
//! `views` lists the view of every `view_ids` row first (its view or `false`, its type), then
//! each type of `view_mode` that no row gives: the action's `view_id` first when its type is
//! one of them, the others as `(false, type)`.

use erp_sdk::prelude::*;
use erp_sdk::serde_json::{Value as Json, json};

use super::{IrActionsActWindow, IrActionsActWindowView};
use crate::ir_ui_view::IrUiView;

/// Longest `view_mode` accepted, in bytes (design m2-s3 §12).
pub const MAX_VIEW_MODE_BYTES: usize = 256;

/// Most view types in one `view_mode`.
pub const MAX_VIEW_MODES: usize = 32;

fn py_list(items: &[&str]) -> String {
    let inner = items
        .iter()
        .map(|item| crate::group_graph::py_repr(item))
        .collect::<Vec<_>>()
        .join(", ");
    format!("[{inner}]")
}

/// Check one `view_mode` value.
///
/// ```
/// use erp_addon_base::ir_actions::window::check_view_mode;
/// assert!(check_view_mode("list,form,kanban").is_ok());
/// let error = check_view_mode("list,form,list").unwrap_err();
/// assert_eq!(
///     error.to_string(),
///     "The modes in view_mode must not be duplicated: ['list', 'form', 'list']"
/// );
/// ```
///
/// # Errors
///
/// `Validation`: more than [`MAX_VIEW_MODE_BYTES`] bytes or [`MAX_VIEW_MODES`] types; a type
/// given twice; whitespace anywhere or an empty type (BUG-072).
pub fn check_view_mode(view_mode: &str) -> Result<()> {
    if view_mode.len() > MAX_VIEW_MODE_BYTES {
        return Err(Error::validation(format!(
            "view_mode is {} bytes long; at most {MAX_VIEW_MODE_BYTES} are allowed.",
            view_mode.len()
        )));
    }
    let modes: Vec<&str> = view_mode.split(',').collect();
    if modes.len() > MAX_VIEW_MODES {
        return Err(Error::validation(format!(
            "view_mode lists {} view types; at most {MAX_VIEW_MODES} are allowed.",
            modes.len()
        )));
    }
    let duplicated = modes
        .iter()
        .enumerate()
        .any(|(i, mode)| modes[..i].contains(mode));
    if duplicated {
        return Err(Error::validation(format!(
            "The modes in view_mode must not be duplicated: {}",
            py_list(&modes)
        )));
    }
    if modes
        .iter()
        .any(|mode| mode.is_empty() || mode.chars().any(char::is_whitespace))
    {
        return Err(Error::validation(format!(
            "No spaces allowed in view_mode: “{}”",
            py_list(&modes)
        )));
    }
    Ok(())
}

/// `views` of one action from its parts: `given` are the
/// `(view, type)` pairs of its `view_ids` rows in their order, `view_mode` its types, `view` its
/// `view_id` with that view's type.
pub fn views_of(
    given: &[(Option<i64>, &str)],
    view_mode: &str,
    view: Option<(i64, &str)>,
) -> Vec<(Option<i64>, String)> {
    let mut views: Vec<(Option<i64>, String)> = given
        .iter()
        .map(|(id, mode)| (*id, (*mode).to_owned()))
        .collect();
    let modes = if view_mode.is_empty() {
        None
    } else {
        Some(view_mode.split(','))
    };
    let mut missing: Vec<&str> = modes
        .into_iter()
        .flatten()
        .filter(|mode| !given.iter().any(|(_, m)| m == mode))
        .collect();
    if !missing.is_empty() {
        if let Some((id, kind)) = view
            && let Some(at) = missing.iter().position(|m| *m == kind)
        {
            missing.remove(at);
            views.push((Some(id), kind.to_owned()));
        }
        views.extend(missing.into_iter().map(|mode| (None, mode.to_owned())));
    }
    views
}

/// The JSON value of `views`: `[[id or false, type], …]`.
fn views_json(views: &[(Option<i64>, String)]) -> Json {
    Json::Array(
        views
            .iter()
            .map(|(id, mode)| json!([id.map_or(Json::Bool(false), Json::from), mode]))
            .collect(),
    )
}

fn real(id: RecId) -> Result<i64> {
    crate::group_graph::row_id(id).ok_or_else(|| {
        Error::programming(format!(
            "{id} has no database row: the views of a window action name saved views"
        ))
    })
}

#[methods(IrActionsActWindow)]
impl IrActionsActWindow {
    /// `views`.
    #[compute(depends = [
        "view_ids", "view_ids.view_mode", "view_ids.view_id", "view_ids.sequence",
        "view_mode", "view_id", "view_id.type"
    ])]
    fn _compute_views(batch: &mut Batch<'_, IrActionsActWindow>) -> Result<()> {
        for i in batch.indices() {
            let rows = batch.get(i, IrActionsActWindow::VIEW_IDS)?;
            let mut given: Vec<(Option<i64>, Arc<str>)> = Vec::with_capacity(rows.len());
            for row in &rows {
                let view = batch.value_of(IrActionsActWindowView::VIEW_ID, *row)?;
                let mode = batch
                    .value_of(IrActionsActWindowView::VIEW_MODE, *row)?
                    .ok_or_else(|| {
                        Error::programming(format!(
                            "ir.actions.act_window.view {row} has no view_mode (a required field)"
                        ))
                    })?;
                given.push((view.map(real).transpose()?, mode));
            }
            let view_mode = batch.get(i, IrActionsActWindow::VIEW_MODE)?;
            let view = match batch.get(i, IrActionsActWindow::VIEW_ID)? {
                Some(id) => batch
                    .value_of(IrUiView::TYPE, id)?
                    .map(|kind| real(id).map(|id| (id, kind)))
                    .transpose()?,
                None => None,
            };
            let pairs: Vec<(Option<i64>, &str)> =
                given.iter().map(|(id, mode)| (*id, &**mode)).collect();
            let views = views_of(
                &pairs,
                view_mode.as_deref().unwrap_or_default(),
                view.as_ref().map(|(id, kind)| (*id, &**kind)),
            );
            batch.set(i, IrActionsActWindow::VIEWS, Some(views_json(&views)))?;
        }
        Ok(())
    }

    /// `view_mode` rules (BUG-072).
    #[constrains("view_mode")]
    async fn _check_view_mode(
        this: &RecordSet<IrActionsActWindow>,
        tx: &mut Transaction,
    ) -> Result<()> {
        for cell in this.as_records().cells(tx, "view_mode").await? {
            match cell {
                Cell::Str(view_mode) => {
                    check_view_mode(&view_mode)?;
                    for key in view_mode.split(',') {
                        crate::ir_ui_view::require_loaded_view_type(
                            tx,
                            this.as_records().env(),
                            key,
                        )?;
                    }
                }
                // a required field: the database refuses NULL
                Cell::Null => {}
                other => {
                    return Err(Error::programming(format!(
                        "ir.actions.act_window.view_mode read as {other:?}, not text"
                    )));
                }
            }
        }
        Ok(())
    }

    /// `res_model` and the binding model are models of the registry.
    #[constrains("res_model", "binding_model_id")]
    async fn _check_model(
        this: &RecordSet<IrActionsActWindow>,
        tx: &mut Transaction,
    ) -> Result<()> {
        let registry = std::sync::Arc::clone(this.as_records().env().registry());
        let invalid = |name: &str| {
            Error::validation(format!("Invalid model name “{name}” in action definition."))
        };
        for cell in this.as_records().cells(tx, "res_model").await? {
            match &cell {
                Cell::Str(name) if registry.model(name).is_none() => return Err(invalid(name)),
                Cell::Str(_) => {}
                // required: refused here with a message, before the database's not-null error
                Cell::Null => {
                    return Err(Error::validation(
                        "A window action needs a Destination Model (res_model).",
                    ));
                }
                other => {
                    return Err(Error::programming(format!(
                        "ir.actions.act_window.res_model read as {other:?}, not text"
                    )));
                }
            }
        }
        let mut bound: Vec<i64> = Vec::new();
        for cell in this.as_records().cells(tx, "binding_model_id").await? {
            match cell {
                Cell::Id(id) => bound.push(id),
                Cell::Null => {}
                other => {
                    return Err(Error::programming(format!(
                        "ir.actions.act_window.binding_model_id read as {other:?}, not an id"
                    )));
                }
            }
        }
        if bound.is_empty() {
            return Ok(());
        }
        let rows = tx
            .query(
                "SELECT model FROM ir_model WHERE id = ANY($1) ORDER BY id",
                &[&bound],
            )
            .await?;
        for row in &rows {
            let name: String = row
                .try_get(0)
                .map_err(|e| Error::db(format!("unexpected value in ir_model.model: {e}")))?;
            if registry.model(&name).is_none() {
                return Err(invalid(&name));
            }
        }
        Ok(())
    }

    /// Without a name, the action takes its model's description.
    #[method(override)]
    async fn create(
        this: &RecordSet<IrActionsActWindow>,
        tx: &mut Transaction,
        mut vals_list: Vec<RawVals>,
        sup: Super,
    ) -> Result<Records> {
        let registry = std::sync::Arc::clone(this.as_records().env().registry());
        for vals in &mut vals_list {
            let named = matches!(vals.get("name"), Some(Cell::Str(name)) if !name.is_empty());
            if named {
                continue;
            }
            if let Some(Cell::Str(model)) = vals.get("res_model")
                && let Some(spec) = registry.model(model)
            {
                let description: Arc<str> = Arc::from(spec.description.as_str());
                vals.insert("name".into(), Cell::Str(description));
            }
        }
        sup.call(tx, this.as_records(), (vals_list,)).await
    }
}

#[methods(IrActionsActWindowView)]
impl IrActionsActWindowView {
    /// Inline action-view rows carry the same loader origin as their parent operation.
    #[constrains("view_mode")]
    async fn _check_h18_type(
        this: &RecordSet<IrActionsActWindowView>,
        tx: &mut Transaction,
    ) -> Result<()> {
        for cell in this.as_records().cells(tx, "view_mode").await? {
            match cell {
                Cell::Str(key) => {
                    crate::ir_ui_view::require_loaded_view_type(tx, this.as_records().env(), &key)?
                }
                Cell::Null => {}
                other => {
                    return Err(Error::programming(format!(
                        "ir.actions.act_window.view.view_mode read as {other:?}, not text"
                    )));
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "tests")]

    use super::*;

    /// The pure rules of `views` and `view_mode`; the database test reads `views` back.
    #[test]
    fn views_order_and_mode_rules() {
        assert_eq!(
            views_of(
                &[(Some(7), "kanban")],
                "list,form,kanban",
                Some((3, "form"))
            ),
            vec![
                (Some(7), "kanban".to_owned()),
                (Some(3), "form".to_owned()),
                (None, "list".to_owned())
            ]
        );
        // the action's view only when its type is among the missing ones
        assert_eq!(
            views_of(&[(Some(7), "form")], "list,form", Some((3, "form"))),
            vec![(Some(7), "form".to_owned()), (None, "list".to_owned())]
        );
        // nothing missing: the view is not appended
        assert_eq!(
            views_of(&[(None, "list")], "list", Some((3, "list"))),
            vec![(None, "list".to_owned())]
        );
        assert_eq!(
            views_json(&views_of(&[], "list,form", None)),
            json!([[false, "list"], [false, "form"]])
        );
        // review C-S6: no `view_mode` lists nothing (it gave `[[false, ""]]`)
        assert_eq!(views_json(&views_of(&[], "", None)), json!([]));
        assert_eq!(
            views_of(&[(Some(7), "kanban")], "", Some((3, "form"))),
            vec![(Some(7), "kanban".to_owned())]
        );
        for (bad, message) in [
            (
                "list,form,list",
                "The modes in view_mode must not be duplicated: ['list', 'form', 'list']",
            ),
            (
                "list, form",
                "No spaces allowed in view_mode: “['list', ' form']”",
            ),
            (
                "list,,form",
                "No spaces allowed in view_mode: “['list', '', 'form']”",
            ),
            (
                "list,\tform",
                "No spaces allowed in view_mode: “['list', '\\tform']”",
            ),
            ("", "No spaces allowed in view_mode: “['']”"),
        ] {
            let error = check_view_mode(bad).unwrap_err();
            assert_eq!(error.kind(), erp_sdk::ErrorKind::Validation, "{bad:?}");
            assert_eq!(error.to_string(), message, "{bad:?}");
        }
        assert!(check_view_mode("list,form,kanban,x_custom").is_ok());
        let many = vec!["m"; MAX_VIEW_MODES + 1]
            .iter()
            .enumerate()
            .map(|(i, m)| format!("{m}{i}"))
            .collect::<Vec<_>>()
            .join(",");
        assert_eq!(
            check_view_mode(&many).unwrap_err().to_string(),
            "view_mode lists 33 view types; at most 32 are allowed."
        );
        let long = format!("{},x", "a".repeat(MAX_VIEW_MODE_BYTES));
        assert_eq!(
            check_view_mode(&long).unwrap_err().to_string(),
            "view_mode is 258 bytes long; at most 256 are allowed."
        );
    }
}
