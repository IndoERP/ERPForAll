//! "My Preferences" of `res.users`:
//! the dialog a user opens from the user menu to change their own name, e-mail, language and time
//! zone.
//!
//!   The client adds the current user as the record to open.
//!   It writes nothing; it answers that the dialog
//!   is done.
//! - **Choices.** The human client has no renderer for a dynamic selection field
//!   (`res.partner.lang` has the same limit), so `res.users.lang` and `res.users.tz` stay text
//!   fields and the dialog's `web.preference_choice` widget offers the allowed values.
//!   No method of this module lists them.
//! - **Validation.** A text field is not checked against those choices by the field layer, so
//!   [`check_preference_values`] refuses a language that is not installed and a zone the database
//!   does not know, on `create` and `write`. An empty value
//!   clears the field, as before.
//!
//! Self-service writes are not decided here: a user outside `base.group_system` writes only
//! the `user_writeable` fields of their own record, and the `ir.access` rows of
//! `addons/base/security/ir.access.csv` give internal users read and update on their own row.

use erp_sdk::prelude::*;
use erp_sdk::serde_json::json;
use erp_sdk::{Cell, Load, ValueMap};

/// External id of the preferences window action (module, name).
pub const PREFERENCES_ACTION_XMLID: (&str, &str) = ("base", "action_res_users_my");

/// The fields of the preferences action that `action_get` returns, besides `id` and `type`.
const ACTION_FIELDS: [&str; 5] = ["name", "res_model", "target", "view_mode", "views"];

/// Whether the zone database knows `name` (one of the zones `res.partner`'s `_tz_get` offers).
fn known_time_zone(name: &str) -> bool {
    jiff::tz::db().available().any(|zone| zone.as_str() == name)
}

/// The kind of a cell, for a message (never the value itself).
fn cell_kind(cell: &Cell) -> &'static str {
    match cell {
        Cell::Null => "no value",
        Cell::Bool(_) => "a boolean",
        Cell::Int(_) | Cell::Id(_) => "an integer",
        Cell::Float(_) | Cell::Decimal(_) => "a number",
        Cell::Str(_) => "text",
        Cell::Date(_) => "a date",
        Cell::DateTime(_) => "a date and time",
        Cell::Uuid(_) => "a UUID",
        Cell::Xid8(_) => "a transaction id",
        Cell::TimestampTz(_) => "a timezone-aware date and time",
        Cell::Json(_) => "a JSON document",
        Cell::Ids(_) => "a list of record ids",
        Cell::DateRange(_) | Cell::DatetimeRange(_) => "a range",
    }
}

/// # Errors
///
/// `Validation` for a value of another kind than text (the field layer already converted it, so
/// this is a defensive refusal and not a substitution).
fn code_of<'a>(model: &str, field: &str, vals: &'a RawVals) -> Result<Option<&'a str>> {
    match vals.get(field) {
        None | Some(Cell::Null) => Ok(None),
        Some(Cell::Str(code)) if code.is_empty() => Ok(None),
        Some(Cell::Str(code)) => Ok(Some(code)),
        Some(other) => Err(Error::validation(format!(
            "Wrong value for {model}.{field}: expected text, got {}",
            cell_kind(other)
        ))),
    }
}

/// Refuse a `lang` that is not an installed language and a `tz` that is not a known zone in
/// `vals` (a `create` or `write` of `res.users`).
///
/// # Errors
///
/// `Validation` `Wrong value for res.users.lang: 'xx_XX'` (resp.
pub(crate) async fn check_preference_values(tx: &mut Transaction, vals: &RawVals) -> Result<()> {
    let lang = code_of("res.users", "lang", vals)?;
    let tz = code_of("res.users", "tz", vals)?;
    check_codes(tx, lang, tz).await
}

/// [`check_preference_values`] for the values a `create` stored, after the ORM completed them
/// with the context defaults (`default_lang`, `default_tz`).
///
/// # Errors
///
/// As [`check_preference_values`]; `Database` for a failed read.
pub(crate) async fn check_stored_preferences(
    tx: &mut Transaction,
    created: &Records,
) -> Result<()> {
    let rows = created.sudo(true).read(tx, &["lang", "tz"]).await?;
    for row in &rows {
        let text = |field: &str| match row.get(field) {
            Some(Value::Str(code)) if !code.is_empty() => Some(code.to_string()),
            _ => None,
        };
        check_codes(tx, text("lang").as_deref(), text("tz").as_deref()).await?;
    }
    Ok(())
}

async fn check_codes(tx: &mut Transaction, lang: Option<&str>, tz: Option<&str>) -> Result<()> {
    if let Some(code) = lang {
        let installed = tx
            .env()
            .records("res.lang")?
            .sudo(true)
            .search_read(
                tx,
                &(Domain::condition("code", Operator::Eq, code)?
                    & Domain::condition("active", Operator::Eq, true)?),
                &["code"],
                &SearchOptions::new().limit(1),
            )
            .await?;
        if installed.is_empty() {
            return Err(Error::validation(format!(
                "Wrong value for res.users.lang: {}",
                Value::str(code)
            )));
        }
    }
    if let Some(zone) = tz
        && !known_time_zone(zone)
    {
        return Err(Error::validation(format!(
            "Wrong value for res.users.tz: {}",
            Value::str(zone)
        )));
    }
    Ok(())
}

/// The id the external id `module.name` names as a window action.
///
/// # Errors
///
/// `Programming` when the registry has no `ir.model.data` model; `Validation` when the action was
/// not loaded (the base data files did not run); `Database` for a failed statement.
async fn window_action_id(tx: &mut Transaction, module: &str, name: &str) -> Result<i64> {
    let Some(data) = tx.env().registry().model("ir.model.data").map(|m| m.id) else {
        return Err(Error::programming(
            "res.users.action_get looks the preferences action up by external id, but this \
             registry has no ir.model.data model: compose the data addon \
             (erp_data::model_data::ADDON with erp_data::ir_model_data_decl()) with base",
        ));
    };
    // an external id created earlier in this transaction may still be a pending ORM write
    tx.flush_model(data).await?;
    let rows = tx
        .query(
            "SELECT res_id FROM ir_model_data \
             WHERE module = $1 AND name = $2 AND model = 'ir.actions.act_window'",
            &[&module, &name],
        )
        .await?;
    let row = rows.first().ok_or_else(|| {
        Error::validation(format!(
            "The preferences action {module}.{name} does not exist: the base data files were not \
             loaded."
        ))
    })?;
    let id: Option<i64> = row.try_get(0).map_err(|e| {
        Error::db(format!(
            "unexpected value in ir_model_data.res_id of {module}.{name}: {e}"
        ))
    })?;
    id.ok_or_else(|| Error::validation(format!("The external id {module}.{name} names no record.")))
}

/// Methods of the preferences dialog (no columns of its own).
#[extend(ResUsers)]
pub struct ResUsersPreferences {}

#[methods(ResUsers)]
impl ResUsersPreferences {
    /// The window action of the preferences dialog: `type`, `id`, `name`,
    /// `res_model`, `target` (`new`), `view_mode` (`form`) and the computed `views`. Open it on
    /// the current user's record.
    ///
    /// # Errors
    ///
    /// `Validation` when the base data files did not load the action.
    #[method]
    #[rpc(effect = read, model)]
    async fn action_get(this: &RecordSet<ResUsers>, tx: &mut Transaction) -> Result<Value> {
        let (module, name) = PREFERENCES_ACTION_XMLID;
        let id = window_action_id(tx, module, name).await?;
        let action = this
            .as_records()
            .env()
            .records("ir.actions.act_window")?
            .browse(vec![id])?
            .sudo(true);
        let rows = action.read_with(tx, &ACTION_FIELDS, Load::Raw).await?;
        let row = rows.first().ok_or_else(|| {
            Error::invariant(format!(
                "the preferences action {module}.{name} disappeared"
            ))
        })?;
        let mut out = ValueMap::default();
        out.insert("type".to_owned(), Value::str("ir.actions.act_window"));
        out.insert("id".to_owned(), Value::Int(id));
        for field in ACTION_FIELDS {
            let value = row.get(field).ok_or_else(|| {
                Error::invariant(format!(
                    "the preferences action read lacks the field '{field}'"
                ))
            })?;
            out.insert(field.to_owned(), value.clone());
        }
        Ok(Value::Map(Arc::new(out)))
    }

    /// The *Update Preferences* button, after the dialog's form was saved: it
    /// writes nothing and answers `act_window_close`; the client reloads the page so the new
    /// language and time zone apply. Only the caller's own record is accepted.
    ///
    /// # Errors
    ///
    /// `Validation` for anything but exactly the caller's own record.
    #[method]
    #[rpc(effect = write)]
    async fn preference_save(this: &RecordSet<ResUsers>, _tx: &mut Transaction) -> Result<Value> {
        let ids = this.as_records().ids();
        if ids != [this.env().uid()] {
            return Err(Error::validation(
                "Preferences can only be saved for the current user's own record.",
            ));
        }
        Ok(Value::Json(Arc::new(
            json!({"type": "ir.actions.act_window_close"}),
        )))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "tests")]
mod native_kind_tests {
    use super::*;

    #[test]
    fn native_preference_cells_are_refused_with_kind_only_diagnostics() {
        for (cell, kind) in [
            (
                Cell::Uuid(Uuid::parse("00000000-0000-0000-0000-000000000001").unwrap()),
                "a UUID",
            ),
            (
                Cell::Xid8(Xid8::parse("18446744073709551615").unwrap()),
                "a transaction id",
            ),
            (
                Cell::TimestampTz(TimestampTz::parse("2026-11-05T00:00:00Z").unwrap()),
                "a timezone-aware date and time",
            ),
        ] {
            let values = RawVals::new().set_cell("tz", cell);
            let error = code_of("res.users", "tz", &values).unwrap_err();
            assert_eq!(error.kind(), erp_sdk::ErrorKind::Validation);
            assert_eq!(
                error.message(),
                format!("Wrong value for res.users.tz: expected text, got {kind}")
            );
        }
    }
}
