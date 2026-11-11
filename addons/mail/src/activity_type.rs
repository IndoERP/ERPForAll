//! `mail.activity.type`: the kinds of activity (call, meeting, to-do...) with their default
//! summary, assignee and delay.

use std::collections::BTreeMap;

use erp_sdk::Operation;
use erp_sdk::prelude::*;

use crate::activity_date::add_delay;

const MASTERS: [(&str, bool); 5] = [
    ("mail_activity_data_call", false),
    ("mail_activity_data_meeting", false),
    ("mail_activity_data_todo", false),
    ("mail_activity_data_upload_document", true),
    ("mail_activity_data_warning", true),
];

/// External id name of the To-Do master type.
pub(crate) const TODO: &str = "mail_activity_data_todo";

/// A kind of activity.
#[model(
    name = "mail.activity.type",
    description = "Activity Type",
    order = "sequence, id",
    rec_name = "name",
    addon = "mail"
)]
pub struct MailActivityType {
    /// Translated label.
    #[field(char, required = true, translate = true)]
    pub name: Option<Arc<str>>,
    /// Summary a new activity of this type starts with; the name when empty.
    #[field(char, translate = true, string = "Default Summary")]
    pub summary: Option<Arc<str>>,
    /// Order of the types; the first one that fits a model is its default type.
    #[field(integer, default = 10)]
    pub sequence: i32,
    /// Archived types are not offered.
    #[field(boolean, default = true)]
    pub active: bool,
    /// Number of `delay_unit`s between today and a new activity's deadline.
    #[field(integer, default = 0, string = "Schedule")]
    pub delay_count: i32,
    /// Unit of the delay.
    #[field(
        selection,
        selection = [("days", "days"), ("weeks", "weeks"), ("months", "months")],
        required = true,
        default = "days",
        string = "Delay units"
    )]
    pub delay_unit: Option<Arc<str>>,
    /// What the delay counts from. Until chaining both count from today.
    #[field(
        selection,
        selection = [
            ("current_date", "after previous activity completion date"),
            ("previous_activity", "after previous activity deadline")
        ],
        required = true,
        default = "previous_activity",
        string = "Delay Type"
    )]
    pub delay_from: Option<Arc<str>>,
    /// Icon name shown in the chatter.
    #[field(char)]
    pub icon: Option<Arc<str>>,
    /// Highlight of the activities of this type.
    #[field(
        selection,
        selection = [("warning", "Alert"), ("danger", "Error")],
        string = "Decoration Type"
    )]
    pub decoration_type: Option<Arc<str>>,
    /// Model the type is restricted to; empty for every model.
    #[field(char, string = "Model")]
    pub res_model: Option<Arc<str>>,
    /// Type suggested once an activity of this one is done.
    #[field(
        many2one,
        comodel = "mail.activity.type",
        ondelete = "set null",
        string = "Suggest Next Activity"
    )]
    pub suggested_next_type_id: Option<RecId>,
    /// Types this one follows.
    #[field(
        many2many,
        comodel = "mail.activity.type",
        relation = "mail_activity_rel",
        column1 = "recommended_id",
        column2 = "activity_id",
        string = "Preceding Activities"
    )]
    pub previous_type_ids: Vec<RecId>,
    /// Behaviour the clients attach to the type.
    #[field(
        selection,
        selection = [
            ("default", "None"),
            ("upload_file", "Upload Document"),
            ("phonecall", "Phonecall")
        ],
        default = "default",
        string = "Action"
    )]
    pub category: Option<Arc<str>>,
    /// Assignee of new activities of this type.
    #[field(
        many2one,
        comodel = "res.users",
        ondelete = "set null",
        string = "Default User"
    )]
    pub default_user_id: Option<RecId>,
    /// Plain text a new activity's note starts with (HTML notes wait for
    /// ).
    #[field(text, string = "Default Note")]
    pub default_note: Option<Arc<str>>,
}

#[methods(MailActivityType, addon = "mail")]
impl MailActivityType {
    /// # Errors
    /// `Validation` for a recordset of more than one type, an unknown delay unit or a deadline
    /// out of the date range.
    #[method]
    async fn _get_date_deadline(
        this: &RecordSet<MailActivityType>,
        tx: &mut Transaction,
    ) -> Result<Date> {
        let record = this.as_records().ensure_one()?;
        let count = match record.cell(tx, "delay_count").await? {
            Cell::Int(count) => count,
            Cell::Null => 0,
            other => {
                return Err(Error::invariant(format!(
                    "mail.activity.type delay_count is not an integer: {other:?}"
                )));
            }
        };
        let unit = match record.cell(tx, "delay_unit").await? {
            Cell::Str(unit) => unit,
            other => {
                return Err(Error::invariant(format!(
                    "mail.activity.type delay_unit is not text: {other:?}"
                )));
            }
        };
        add_delay(record.env().context_today()?, &unit, count)
    }

    /// # Errors
    /// `User` when a master type is given a model.
    #[method(override)]
    async fn write(
        this: &RecordSet<MailActivityType>,
        tx: &mut Transaction,
        vals: RawVals,
        sup: Super,
    ) -> Result<()> {
        let sets_model = match vals.get("res_model") {
            None | Some(Cell::Null) => false,
            Some(Cell::Str(model)) => !model.is_empty(),
            Some(_) => true,
        };
        if sets_model && !this.as_records().is_empty() {
            let masters = master_ids(tx, this.as_records(), |_| true).await?;
            if !masters.is_empty() {
                let names = type_names(tx, this.as_records(), &masters).await?;
                return Err(Error::user(format!(
                    "You cannot modify {names} target model as they are required in various apps."
                )));
            }
        }
        sup.call(tx, this.as_records(), (vals,)).await
    }

    /// Deleting types moves their activities to To-Do first, archived (done) ones included, then
    /// deletes them. The To-Do, Call and Meeting masters cannot be deleted (
    ///
    /// # Errors
    /// `Access` without delete access on the types; `User` when a protected master type is among the records; `Invariant` when the To-Do type
    /// is missing (the addon's data is not loaded).
    #[method(override)]
    async fn unlink(
        this: &RecordSet<MailActivityType>,
        tx: &mut Transaction,
        sup: Super,
    ) -> Result<()> {
        let records = this.as_records();
        if records.is_empty() {
            return sup.call(tx, records, ()).await;
        }
        // access first: the move below is privileged and must never run for a refused caller
        records.check_access(tx, Operation::Unlink).await?;
        let protected = master_ids(tx, records, |deletable| !deletable).await?;
        if !protected.is_empty() {
            let names = type_names(tx, records, &protected).await?;
            return Err(Error::user(format!(
                "You cannot delete {names} as it is required in various apps."
            )));
        }
        let todo = todo_type_id(tx).await?;
        let activities = records
            .env()
            .records("mail.activity")?
            .sudo(true)
            .with_context([("active_test", Value::Bool(false))]);
        let of_types = Domain::condition("activity_type_id", Operator::In, records.ids())?;
        let moved = activities.search(tx, &of_types).await?;
        if !moved.is_empty() {
            moved
                .write(
                    tx,
                    RawVals::new().set_cell("activity_type_id", Cell::Id(todo)),
                )
                .await?;
        }
        sup.call(tx, records, ()).await
    }

    /// # Errors
    /// `User` when the To-Do type is among the records.
    #[method(override)]
    async fn action_archive(
        this: &RecordSet<MailActivityType>,
        tx: &mut Transaction,
        sup: Super,
    ) -> Result<()> {
        let records = this.as_records();
        if !records.is_empty() {
            let todo = todo_type_id(tx).await?;
            if records.ids().contains(&todo) {
                return Err(Error::user(
                    "The 'To-Do' activity type is used to create reminders from the top bar menu \
                     and the command palette. Consequently, it cannot be archived or deleted.",
                ));
            }
        }
        sup.call(tx, records, ()).await
    }
}

/// The id of the To-Do master type.
///
/// # Errors
/// `Invariant` when the type or `ir.model.data` is missing: the addon's data is not loaded.
pub(crate) async fn todo_type_id(tx: &mut Transaction) -> Result<i64> {
    let rows = tx
        .query(
            "SELECT res_id::int8 FROM ir_model_data WHERE module = 'mail' AND name = $1::text \
             AND model = 'mail.activity.type'",
            &[&TODO],
        )
        .await?;
    let row = rows.first().ok_or_else(|| {
        Error::invariant(
            "the mail activity type mail_activity_data_todo is missing: the mail addon's data is \
             not loaded",
        )
    })?;
    row.try_get::<_, i64>(0)
        .map_err(|error| Error::db(format!("unexpected To-Do type id: {error}")))
}

/// The records of `records` that are master types whose "may be deleted" flag satisfies `keep`,
/// in id order. One statement, after pending writes are flushed.
///
/// # Errors
/// `Programming` when the registry has no `ir.model.data`; database errors.
async fn master_ids(
    tx: &mut Transaction,
    records: &Records,
    keep: impl Fn(bool) -> bool,
) -> Result<Vec<i64>> {
    if records.env().records("ir.model.data").is_err() {
        return Err(Error::programming(
            "mail.activity.type protects its master records by external id and needs the \
             ir.model.data model in the registry",
        ));
    }
    tx.flush().await?;
    let names: Vec<&str> = MASTERS.iter().map(|(name, _)| *name).collect();
    let ids = records.ids();
    let rows = tx
        .query(
            "SELECT name::text, res_id::int8 FROM ir_model_data WHERE module = 'mail' AND model = \
             'mail.activity.type' AND name = ANY($1::text[]) AND res_id = ANY($2::int8[])",
            &[&names, &ids],
        )
        .await?;
    let mut found = Vec::with_capacity(rows.len());
    for row in &rows {
        let name: String = row
            .try_get(0)
            .map_err(|error| Error::db(format!("unexpected master type name: {error}")))?;
        let id: i64 = row
            .try_get(1)
            .map_err(|error| Error::db(format!("unexpected master type id: {error}")))?;
        let deletable = MASTERS
            .iter()
            .find(|(master, _)| *master == name)
            .map(|(_, deletable)| *deletable)
            .ok_or_else(|| Error::invariant(format!("unrequested master type '{name}'")))?;
        if keep(deletable) {
            found.push(id);
        }
    }
    found.sort_unstable();
    Ok(found)
}

/// The names of `ids` (types of `records`' model) in id order, comma separated.
async fn type_names(tx: &mut Transaction, records: &Records, ids: &[i64]) -> Result<String> {
    let subset = records.browse(ids.iter().copied())?;
    let mut names = Vec::with_capacity(ids.len());
    for (id, cell) in ids.iter().zip(subset.cells(tx, "name").await?) {
        match cell {
            Cell::Str(name) => names.push(name.to_string()),
            other => {
                return Err(Error::invariant(format!(
                    "mail.activity.type {id} has no name ({other:?})"
                )));
            }
        }
    }
    Ok(names.join(", "))
}

/// What a new activity takes from its type.
#[derive(Debug, Clone)]
pub(crate) struct TypeInfo {
    pub(crate) name: Arc<str>,
    pub(crate) summary: Option<Arc<str>>,
    pub(crate) delay_count: i32,
    pub(crate) delay_unit: Arc<str>,
    pub(crate) default_user: Option<i64>,
    pub(crate) default_note: Option<Arc<str>>,
    pub(crate) res_model: Option<Arc<str>>,
}

/// The type fields new activities read, by type id; privileged (a type's defaults are applied on
/// behalf of the caller who may create the activity).
///
/// # Errors
/// `Validation` for an unknown id; `Invariant` for a column of another shape.
pub(crate) async fn load_types(
    tx: &mut Transaction,
    env: &Env,
    ids: &[i64],
) -> Result<BTreeMap<i64, TypeInfo>> {
    let mut out = BTreeMap::new();
    if ids.is_empty() {
        return Ok(out);
    }
    let types = env
        .records("mail.activity.type")?
        .sudo(true)
        .browse(ids.iter().copied())?;
    let existing = types.exists(tx).await?.ids();
    if let Some(missing) = ids.iter().find(|id| !existing.contains(id)) {
        return Err(Error::validation(format!(
            "Activity type {missing} does not exist."
        )));
    }
    let (names, summaries, counts, units, users, notes, models) = (
        types.cells(tx, "name").await?,
        types.cells(tx, "summary").await?,
        types.cells(tx, "delay_count").await?,
        types.cells(tx, "delay_unit").await?,
        types.cells(tx, "default_user_id").await?,
        types.cells(tx, "default_note").await?,
        types.cells(tx, "res_model").await?,
    );
    for (i, id) in types.ids().into_iter().enumerate() {
        let text = |cell: &Cell, what: &str| -> Result<Option<Arc<str>>> {
            match cell {
                Cell::Str(value) if !value.is_empty() => Ok(Some(Arc::clone(value))),
                Cell::Str(_) | Cell::Null => Ok(None),
                other => Err(Error::invariant(format!(
                    "mail.activity.type {id} {what} is not text: {other:?}"
                ))),
            }
        };
        let name = text(&names[i], "name")?
            .ok_or_else(|| Error::invariant(format!("mail.activity.type {id} has no name")))?;
        let delay_unit = text(&units[i], "delay_unit")?.ok_or_else(|| {
            Error::invariant(format!("mail.activity.type {id} has no delay_unit"))
        })?;
        let delay_count = match &counts[i] {
            Cell::Int(count) => *count,
            Cell::Null => 0,
            other => {
                return Err(Error::invariant(format!(
                    "mail.activity.type {id} delay_count is not an integer: {other:?}"
                )));
            }
        };
        let default_user = match &users[i] {
            Cell::Id(user) => Some(*user),
            Cell::Null => None,
            other => {
                return Err(Error::invariant(format!(
                    "mail.activity.type {id} default_user_id is not an id: {other:?}"
                )));
            }
        };
        out.insert(
            id,
            TypeInfo {
                name,
                summary: text(&summaries[i], "summary")?,
                delay_count,
                delay_unit,
                default_user,
                default_note: text(&notes[i], "default_note")?,
                res_model: text(&models[i], "res_model")?,
            },
        );
    }
    Ok(out)
}

/// `None` when no type fits.
///
/// # Errors
/// Database errors.
pub(crate) async fn default_type(
    tx: &mut Transaction,
    env: &Env,
    model: Option<&str>,
) -> Result<Option<i64>> {
    let generic = Domain::condition("res_model", Operator::Eq, false)?;
    let domain = match model {
        Some(model) => Domain::condition("res_model", Operator::Eq, model)? | generic,
        None => generic,
    };
    let options = SearchOptions {
        limit: Some(1),
        ..SearchOptions::default()
    };
    let found = env
        .records("mail.activity.type")?
        .sudo(true)
        .search_with(tx, &domain, &options)
        .await?;
    Ok(found.ids().first().copied())
}
