//! `mail.activity`: a thing to do on a record (or on its own), with a type, a deadline and an
//! assignee.
//!
//! Our own implementation.
//!
//! An activity is created with the defaults its type gives (summary, note, deadline, assignee),
//! subscribes the assignee to the record, and ends as done: the done path posts a message with the
//! `mail.mt_activities` subtype through [`crate::MailThread`]'s `message_post` and archives the
//! activity (`active = false`, `date_done` = today). `state` is not stored: it is computed from the
//! deadline against today in the assignee's time zone.
//!
//! Not here (design note): the assignment notification, bus events, attachments on feedback
//! (`ir.attachment`), plans, reminders and chaining.

use std::collections::{BTreeMap, BTreeSet};

use erp_sdk::Operation;
use erp_sdk::prelude::*;

use crate::activity_date::{add_days, add_delay, monday_on_or_before, state_of};
use crate::activity_type::{default_type, load_types};
use crate::post::{Body, PostRequest, PostSubtype, Posted};

pub const MAX_ACTIVITY_TEXT_BYTES: usize = 1 << 20;
/// Most activities one `create` call (rows given) or one done call (`action_feedback`, and so
/// `action_done`) handles; `write` and `unlink` are not capped by this constant.
pub const MAX_ACTIVITIES_PER_CALL: usize = 1000;

erp_sdk::rule_operator!(
    /// `activity_thread_access` (Amendment 1), the document rule of activities: in the
    /// rows of `security/ir.access.csv`, `('res_id', 'activity_thread_access', 'read')` (or
    /// `'write'`) stands for "the principal may read (write) the document
    /// `(res_model, res_id)`", for the documents of every concrete model that inherits
    /// `mail.thread`. The twin of `thread_access`, which belongs to `mail.message` only.
    /// `create` is not spelled here: its document access is the model's `_mail_post_access`
    /// operation, checked in [`MailActivity::create`].
    pub ACTIVITY_THREAD_ACCESS = "activity_thread_access" {
        addon: "mail",
        model: "mail.activity",
        field: "res_id",
        inherits: "mail.thread",
        operations: [Read, Write],
    }
);

/// An activity.
#[model(
    name = "mail.activity",
    description = "Activity",
    order = "date_deadline, id",
    rec_name = "summary",
    addon = "mail",
    indexes = [
        ("_res_model_res_id_idx", "(res_model, res_id)"),
        ("_user_id_active_idx", "(user_id) WHERE active")
    ],
    constraints = [
        (
            "_check_res_id_is_set_if_model",
            "CHECK((COALESCE(res_model, '') <> '' AND res_id IS NOT NULL AND res_id <> 0) OR \
             (COALESCE(res_model, '') = '' AND (res_id IS NULL OR res_id = 0)))",
            "Activities have to be linked to records with a not null res_id."
        ),
        (
            "_check_user_id_is_set_if_model",
            "CHECK(COALESCE(res_model, '') <> '' OR user_id IS NOT NULL)",
            "Activities must be assigned if not attached to a document."
        )
    ]
)]
pub struct MailActivity {
    /// Model of the record the activity is on; empty for a personal to-do.
    #[field(char, index = true, string = "Related Document Model")]
    pub res_model: Option<Arc<str>>,
    /// Id of the record the activity is on.
    #[field(
        many2one_reference,
        model_field = "res_model",
        index = true,
        string = "Related Document ID"
    )]
    pub res_id: i64,
    /// Name of the record when the activity was created.
    #[field(char, readonly = true, string = "Document Name")]
    pub res_name: Option<Arc<str>>,
    /// What kind of activity.
    #[field(
        many2one,
        comodel = "mail.activity.type",
        ondelete = "restrict",
        index = "btree_not_null",
        string = "Activity Type"
    )]
    pub activity_type_id: Option<RecId>,
    /// What to do, in a few words.
    #[field(char)]
    pub summary: Option<Arc<str>>,
    /// Plain text note (HTML notes wait for).
    #[field(text)]
    pub note: Option<Arc<str>>,
    /// When it is due.
    #[field(date, required = true, index = true, string = "Due Date")]
    pub date_deadline: Option<Date>,
    #[field(date, string = "Done Date")]
    pub date_done: Option<Date>,
    /// What the user said when marking it done.
    #[field(text)]
    pub feedback: Option<Arc<str>>,
    /// Created by a business flow, not by a user.
    #[field(boolean, readonly = true, string = "Automated activity")]
    pub automated: bool,
    /// Who has to do it.
    #[field(
        many2one,
        comodel = "res.users",
        index = true,
        ondelete = "cascade",
        string = "Assigned to"
    )]
    pub user_id: Option<RecId>,
    /// Done activities are archived.
    #[field(boolean, default = true)]
    pub active: bool,
    /// `overdue`, `today` or `planned` by the deadline against today in the assignee's time zone
    /// (UTC without one); `done` once archived. Not stored.
    #[field(
        selection,
        selection = [
            ("overdue", "Overdue"),
            ("today", "Today"),
            ("planned", "Planned"),
            ("done", "Done")
        ],
        compute = "_compute_state",
        string = "State"
    )]
    pub state: Option<Arc<str>>,
    /// Whether the current user may change the activity (buttons are hidden otherwise). Not
    /// stored; depends on the caller.
    #[field(boolean, compute = "_compute_can_write")]
    pub can_write: bool,
}

/// What `activity_schedule` creates: built with [`ScheduleRequest::new`], then fields set.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct ScheduleRequest {
    /// External id (`module.name`) of the activity type; wins over `activity_type_id`.
    pub act_type_xmlid: Option<String>,
    /// The activity type by id.
    pub activity_type_id: Option<i64>,
    /// The deadline; today in the environment's time zone by default.
    pub date_deadline: Option<Date>,
    /// The summary; the type's by default.
    pub summary: Option<String>,
    /// The note; the type's default note by default.
    pub note: Option<String>,
    /// The assignee; the type's default user, else the current user.
    pub user_id: Option<i64>,
    pub automated: bool,
}

impl ScheduleRequest {
    /// An automated activity of the model's default type, due today.
    pub fn new() -> Self {
        Self {
            act_type_xmlid: None,
            activity_type_id: None,
            date_deadline: None,
            summary: None,
            note: None,
            user_id: None,
            automated: true,
        }
    }
}

impl Default for ScheduleRequest {
    fn default() -> Self {
        Self::new()
    }
}

fn text_cell(value: &str) -> Cell {
    Cell::Str(value.into())
}

fn check_len(what: &str, value: &str) -> Result<()> {
    if value.len() > MAX_ACTIVITY_TEXT_BYTES {
        return Err(Error::validation(format!(
            "An activity {what} holds at most {MAX_ACTIVITY_TEXT_BYTES} bytes, not {}.",
            value.len()
        )));
    }
    Ok(())
}

/// `cells` of one column as ids (`None` for an empty many2one).
async fn id_cells(
    tx: &mut Transaction,
    records: &Records,
    field: &str,
) -> Result<Vec<Option<i64>>> {
    records
        .cells(tx, field)
        .await?
        .into_iter()
        .map(|cell| match cell {
            Cell::Id(id) => Ok(Some(id)),
            Cell::Null => Ok(None),
            other => Err(Error::invariant(format!(
                "{}.{field} is not an id: {other:?}",
                records.model_name()
            ))),
        })
        .collect()
}

/// The text of a cell, `None` for NULL or empty.
fn opt_text(cell: &Cell, what: &str) -> Result<Option<Arc<str>>> {
    match cell {
        Cell::Str(value) if !value.is_empty() => Ok(Some(Arc::clone(value))),
        Cell::Str(_) | Cell::Null => Ok(None),
        other => Err(Error::invariant(format!("{what} is not text: {other:?}"))),
    }
}

/// The time zone name of each user id (those with one), read privileged in one statement.
async fn user_timezones(
    tx: &mut Transaction,
    env: &Env,
    users: &[i64],
) -> Result<BTreeMap<i64, Arc<str>>> {
    let mut out = BTreeMap::new();
    if users.is_empty() {
        return Ok(out);
    }
    let records = env
        .records("res.users")?
        .sudo(true)
        .browse(users.iter().copied())?;
    for (id, cell) in records
        .ids()
        .into_iter()
        .zip(records.cells(tx, "tz").await?)
    {
        if let Some(tz) = opt_text(&cell, "res.users.tz")? {
            out.insert(id, tz);
        }
    }
    Ok(out)
}

async fn documents_by_model(
    tx: &mut Transaction,
    activities: &Records,
    skip_own: Option<i64>,
) -> Result<BTreeMap<String, BTreeSet<i64>>> {
    let (models, ids, users, creators) = (
        activities.cells(tx, "res_model").await?,
        id_cells(tx, activities, "res_id").await?,
        id_cells(tx, activities, "user_id").await?,
        id_cells(tx, activities, "create_uid").await?,
    );
    let mut out: BTreeMap<String, BTreeSet<i64>> = BTreeMap::new();
    for (((model, id), user), creator) in models.iter().zip(ids).zip(users).zip(creators) {
        let (Some(model), Some(id)) = (opt_text(model, "res_model")?, id) else {
            continue;
        };
        if skip_own.is_some() && (user == skip_own || creator == skip_own) {
            continue;
        }
        out.entry(model.to_string()).or_default().insert(id);
    }
    Ok(out)
}

/// The document access an activity needs for `operation` (`Write` for `write` and `unlink`; the
/// model's `_mail_post_access` for `create`), checked on each document once. Refusals are the
/// engine's `Access` errors.
async fn check_documents(
    tx: &mut Transaction,
    env: &Env,
    documents: &BTreeMap<String, BTreeSet<i64>>,
    create: bool,
) -> Result<()> {
    for (model, ids) in documents {
        let docs = env.records(model)?.browse(ids.iter().copied())?;
        let operation = if create {
            docs.call::<_, Operation>(tx, "_mail_post_access", ())
                .await?
        } else {
            Operation::Write
        };
        docs.check_access(tx, operation).await?;
    }
    Ok(())
}

async fn subscribe_assignees(
    tx: &mut Transaction,
    env: &Env,
    wanted: &[(String, i64, i64)],
) -> Result<()> {
    if wanted.is_empty() {
        return Ok(());
    }
    let users: Vec<i64> = wanted
        .iter()
        .map(|(_, _, user)| *user)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let records = env
        .records("res.users")?
        .sudo(true)
        .browse(users.iter().copied())?;
    let partners: BTreeMap<i64, i64> = records
        .ids()
        .into_iter()
        .zip(id_cells(tx, &records, "partner_id").await?)
        .filter_map(|(user, partner)| partner.map(|p| (user, p)))
        .collect();
    let mut per: BTreeMap<(String, i64), BTreeSet<i64>> = BTreeMap::new();
    for (model, res_id, user) in wanted {
        if let Some(partner) = partners.get(user) {
            per.entry((model.clone(), *partner))
                .or_default()
                .insert(*res_id);
        }
    }
    for ((model, partner), res_ids) in per {
        env.records(&model)?
            .sudo(true)
            .browse(res_ids)?
            .call::<_, ()>(tx, "follow", (vec![partner], None::<Vec<i64>>))
            .await?;
    }
    Ok(())
}

fn owner_of(vals: &RawVals) -> Result<Option<(String, i64)>> {
    let model = match vals.get("res_model") {
        None | Some(Cell::Null) => None,
        Some(Cell::Str(model)) if model.is_empty() => None,
        Some(Cell::Str(model)) => Some(model.to_string()),
        Some(other) => {
            return Err(Error::validation(format!(
                "An activity's res_model is a model name, not {other:?}."
            )));
        }
    };
    let id = match vals.get("res_id") {
        None | Some(Cell::Null) => None,
        Some(Cell::Id(id)) if *id != 0 => Some(*id),
        Some(Cell::Id(_)) => None,
        Some(other) => {
            return Err(Error::validation(format!(
                "An activity's res_id is a record id, not {other:?}."
            )));
        }
    };
    match (model, id) {
        (Some(model), Some(id)) if id > 0 => Ok(Some((model, id))),
        (Some(_), _) => Err(Error::validation(
            "Activities have to be linked to records with a not null res_id.",
        )),
        (None, Some(_)) => Err(Error::validation(
            "Activities have to be linked to records with a not null res_id.",
        )),
        (None, None) => Ok(None),
    }
}

/// The models of `owners` must exist and have the activity and thread mixins (no statement).
fn check_owner_models(env: &Env, owners: &BTreeMap<String, BTreeSet<i64>>) -> Result<()> {
    for model in owners.keys() {
        let docs = env
            .records(model)
            .map_err(|_| Error::validation(format!("Unknown model '{model}' for an activity.")))?;
        if !docs.has_field("activity_ids") || !docs.has_field("message_ids") {
            return Err(Error::validation(format!(
                "{model} has no activities: it must inherit mail.activity.mixin and mail.thread."
            )));
        }
    }
    Ok(())
}

/// The records of `owners` must exist (found privileged, so this runs after the caller's access to
/// them was checked and never tells an unauthorized caller whether a record exists).
async fn check_owner_records(
    tx: &mut Transaction,
    env: &Env,
    owners: &BTreeMap<String, BTreeSet<i64>>,
) -> Result<()> {
    for (model, ids) in owners {
        let wanted: Vec<i64> = ids.iter().copied().collect();
        let existing = env
            .records(model)?
            .sudo(true)
            .browse(wanted.iter().copied())?
            .exists(tx)
            .await?
            .ids();
        if let Some(missing) = wanted.iter().find(|id| !existing.contains(id)) {
            return Err(Error::validation(format!(
                "{model} {missing} does not exist: an activity needs its record."
            )));
        }
    }
    Ok(())
}

/// Whether the context asks to skip the automation (`mail_activity_automation_skip`).
pub(crate) fn automation_skipped(env: &Env) -> bool {
    env.context()
        .get("mail_activity_automation_skip")
        .is_some_and(|value| value.is_truthy())
}

#[methods(MailActivity, addon = "mail")]
impl MailActivity {
    /// # Errors
    /// `Validation` when an assignee's time zone is not a known zone.
    #[compute(depends = ["active", "date_deadline", "user_id"])]
    async fn _compute_state(
        this: &RecordSet<MailActivity>,
        tx: &mut Transaction,
        out: &mut Assign<'_, MailActivity>,
    ) -> Result<()> {
        let actives = this.mapped(tx, MailActivity::ACTIVE).await?;
        let deadlines = this.mapped(tx, MailActivity::DATE_DEADLINE).await?;
        let users = this.mapped(tx, MailActivity::USER_ID).await?;
        let wanted: Vec<i64> = users
            .iter()
            .flatten()
            .filter_map(|user| user.as_real().map(|real| real.get()))
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        let zones = user_timezones(tx, this.as_records().env(), &wanted).await?;
        let mut todays: BTreeMap<Arc<str>, Date> = BTreeMap::new();
        let utc: Arc<str> = Arc::from("UTC");
        for index in 0..deadlines.len() {
            let Some(deadline) = deadlines[index] else {
                continue;
            };
            let zone = users[index]
                .and_then(|user| user.as_real().map(|real| real.get()))
                .and_then(|user| zones.get(&user))
                .unwrap_or(&utc);
            let today = match todays.get(zone) {
                Some(today) => *today,
                None => {
                    let today = Date::context_today(zone)?;
                    todays.insert(Arc::clone(zone), today);
                    today
                }
            };
            out.set(
                index,
                MailActivity::STATE,
                Some(Arc::<str>::from(state_of(actives[index], deadline, today))),
            )?;
        }
        Ok(())
    }

    /// Whether the caller may write the activity: the rows OR-ed (assignee, creator, or write
    /// access to the record).
    #[compute(depends = ["res_model", "res_id", "user_id"], depends_context = ["uid"])]
    async fn _compute_can_write(
        this: &RecordSet<MailActivity>,
        tx: &mut Transaction,
        out: &mut Assign<'_, MailActivity>,
    ) -> Result<()> {
        let records = this.as_records();
        let env = records.env().clone();
        let rows = records.filtered_access(tx, Operation::Write).await?;
        let mut allowed: BTreeSet<RecId> = rows.rec_ids().iter().copied().collect();
        if !env.su() {
            let documents = documents_by_model(tx, &rows, Some(env.uid())).await?;
            let mut refused: BTreeSet<(String, i64)> = BTreeSet::new();
            for (model, ids) in &documents {
                let docs = env.records(model)?.browse(ids.iter().copied())?;
                let writable: BTreeSet<i64> = docs
                    .filtered_access(tx, Operation::Write)
                    .await?
                    .ids()
                    .into_iter()
                    .collect();
                refused.extend(
                    ids.iter()
                        .filter(|id| !writable.contains(id))
                        .map(|id| (model.clone(), *id)),
                );
            }
            if !refused.is_empty() {
                let (models, ids, users, creators) = (
                    rows.cells(tx, "res_model").await?,
                    id_cells(tx, &rows, "res_id").await?,
                    id_cells(tx, &rows, "user_id").await?,
                    id_cells(tx, &rows, "create_uid").await?,
                );
                for ((((row, model), id), user), creator) in rows
                    .rec_ids()
                    .iter()
                    .zip(models)
                    .zip(ids)
                    .zip(users)
                    .zip(creators)
                {
                    if let (Some(model), Some(id)) = (opt_text(&model, "res_model")?, id)
                        && user != Some(env.uid())
                        && creator != Some(env.uid())
                        && refused.contains(&(model.to_string(), id))
                    {
                        allowed.remove(row);
                    }
                }
            }
        }
        for (index, id) in records.rec_ids().iter().enumerate() {
            out.set(index, MailActivity::CAN_WRITE, allowed.contains(id))?;
        }
        Ok(())
    }

    /// The values
    /// and the record are checked first.
    ///
    /// # Errors
    /// `Validation` for an owner that is half given, an unknown model or record, a model without the
    /// activity mixins, a type restricted to another model, a text over the limit or more than
    /// [`MAX_ACTIVITIES_PER_CALL`] rows; `Access` without the access on the record, or for a
    /// personal to-do assigned to someone else.
    #[method(override)]
    async fn create(
        this: &RecordSet<MailActivity>,
        tx: &mut Transaction,
        vals_list: Vec<RawVals>,
        sup: Super,
    ) -> Result<Records> {
        let records = this.as_records();
        if vals_list.len() > MAX_ACTIVITIES_PER_CALL {
            return Err(Error::validation(format!(
                "At most {MAX_ACTIVITIES_PER_CALL} activities are created per call, not {}.",
                vals_list.len()
            )));
        }
        let env = records.env().clone();
        let mut owners = Vec::with_capacity(vals_list.len());
        let mut documents: BTreeMap<String, BTreeSet<i64>> = BTreeMap::new();
        for vals in &vals_list {
            for (field, what) in [
                ("summary", "summary"),
                ("note", "note"),
                ("feedback", "feedback"),
            ] {
                if let Some(Cell::Str(value)) = vals.get(field) {
                    check_len(what, value)?;
                }
            }
            let owner = owner_of(vals)?;
            if owner.is_none() && matches!(vals.get("user_id"), Some(Cell::Null)) {
                return Err(Error::validation(
                    "Activities must be assigned if not attached to a document.",
                ));
            }
            if let Some((model, id)) = &owner {
                documents.entry(model.clone()).or_default().insert(*id);
            }
            owners.push(owner);
        }
        check_owner_models(&env, &documents)?;
        if !env.su() {
            check_documents(tx, &env, &documents, true).await?;
        }
        check_owner_records(tx, &env, &documents).await?;
        // types: the given ones and the default of each model without one
        let mut defaults: BTreeMap<String, Option<i64>> = BTreeMap::new();
        for (vals, owner) in vals_list.iter().zip(&owners) {
            if vals.contains("activity_type_id") {
                continue;
            }
            let key = owner
                .as_ref()
                .map(|(model, _)| model.clone())
                .unwrap_or_default();
            if let std::collections::btree_map::Entry::Vacant(slot) = defaults.entry(key) {
                let found =
                    default_type(tx, &env, owner.as_ref().map(|(model, _)| model.as_str())).await?;
                slot.insert(found);
            }
        }
        let mut type_ids = BTreeSet::new();
        for vals in &vals_list {
            match vals.get("activity_type_id") {
                Some(Cell::Id(id)) => {
                    type_ids.insert(*id);
                }
                None | Some(Cell::Null) => {}
                Some(other) => {
                    return Err(Error::validation(format!(
                        "An activity's activity_type_id is a type id, not {other:?}."
                    )));
                }
            }
        }
        type_ids.extend(defaults.values().flatten().copied());
        let types = load_types(tx, &env, &type_ids.iter().copied().collect::<Vec<_>>()).await?;
        let today = env.context_today()?;
        let names = document_names(tx, &env, &documents).await?;
        let mut prepared = Vec::with_capacity(vals_list.len());
        let mut subscribe = Vec::new();
        for (mut vals, owner) in vals_list.into_iter().zip(owners) {
            let type_id = match vals.get("activity_type_id") {
                Some(Cell::Id(id)) => Some(*id),
                Some(_) => None,
                None => defaults
                    .get(owner.as_ref().map_or("", |(model, _)| model.as_str()))
                    .copied()
                    .flatten(),
            };
            let info = type_id.and_then(|id| types.get(&id));
            if let (Some(info), Some((model, _))) = (info, &owner)
                && let Some(restricted) = &info.res_model
                && restricted.as_ref() != model.as_str()
            {
                return Err(Error::validation(format!(
                    "Activity type '{}' is restricted to {restricted}, not to {model}.",
                    info.name
                )));
            }
            if let Some(id) = type_id
                && !vals.contains("activity_type_id")
            {
                vals.insert("activity_type_id".into(), Cell::Id(id));
            }
            if !vals.contains("summary")
                && let Some(info) = info
            {
                let summary = info.summary.clone().unwrap_or_else(|| info.name.clone());
                vals.insert("summary".into(), Cell::Str(summary));
            }
            if !vals.contains("note")
                && let Some(note) = info.and_then(|info| info.default_note.clone())
            {
                vals.insert("note".into(), Cell::Str(note));
            }
            if !vals.contains("date_deadline") {
                let deadline = match info {
                    Some(info) if info.delay_count != 0 => {
                        add_delay(today, &info.delay_unit, info.delay_count)?
                    }
                    _ => today,
                };
                vals.insert("date_deadline".into(), Cell::Date(deadline));
            }
            if !vals.contains("user_id") {
                let user = info.and_then(|info| info.default_user).unwrap_or(env.uid());
                vals.insert("user_id".into(), Cell::Id(user));
            }
            if owner.is_none()
                && !env.su()
                && !matches!(vals.get("user_id"), Some(Cell::Id(user)) if *user == env.uid())
            {
                return Err(Error::access(
                    "You can only create a personal to-do (an activity without a record) for yourself.",
                ));
            }
            if let Some((model, id)) = &owner {
                if let Some(name) = names.get(&(model.clone(), *id)) {
                    vals.insert("res_name".into(), Cell::Str(Arc::clone(name)));
                }
                if let Some(Cell::Id(user)) = vals.get("user_id") {
                    subscribe.push((model.clone(), *id, *user));
                }
            }
            prepared.push(vals);
        }
        let created: Records = sup.call(tx, records, (prepared,)).await?;
        subscribe_assignees(tx, &env, &subscribe).await?;
        Ok(created)
    }

    /// Write activities: an activity stays on its record (`res_model` and `res_id` cannot be
    /// written), archiving stamps `date_done` with today's UTC date and unarchiving clears it, a
    /// new assignee follows the record, and acting on an activity assigned to someone else needs
    /// write access to its record.
    ///
    /// # Errors
    /// `Validation` for `res_model`, `res_id` or a text over the limit; `Access` without the
    /// access on the record.
    #[method(override)]
    async fn write(
        this: &RecordSet<MailActivity>,
        tx: &mut Transaction,
        vals: RawVals,
        sup: Super,
    ) -> Result<()> {
        let records = this.as_records();
        if records.is_empty() {
            return sup.call(tx, records, (vals,)).await;
        }
        if vals.contains("res_model") || vals.contains("res_id") {
            // writing the value an activity already has is not a move
            let (models, ids) = (
                records.cells(tx, "res_model").await?,
                id_cells(tx, records, "res_id").await?,
            );
            let wanted_model = vals
                .get("res_model")
                .map(|cell| opt_text(cell, "res_model"))
                .transpose()?;
            let wanted_id = match vals.get("res_id") {
                None => None,
                Some(Cell::Id(0) | Cell::Null) => Some(None),
                Some(Cell::Id(id)) => Some(Some(*id)),
                Some(other) => {
                    return Err(Error::validation(format!(
                        "An activity's res_id is a record id, not {other:?}."
                    )));
                }
            };
            for (model, id) in models.iter().zip(ids) {
                let moved = match &wanted_model {
                    Some(wanted) => *wanted != opt_text(model, "res_model")?,
                    None => false,
                } || wanted_id.is_some_and(|wanted| wanted != id);
                if moved {
                    return Err(Error::validation(
                        "An activity stays on its record: res_model and res_id cannot be changed.",
                    ));
                }
            }
        }
        for (field, what) in [
            ("summary", "summary"),
            ("note", "note"),
            ("feedback", "feedback"),
        ] {
            if let Some(Cell::Str(value)) = vals.get(field) {
                check_len(what, value)?;
            }
        }
        let env = records.env().clone();
        if !env.su() {
            let documents = documents_by_model(tx, records, Some(env.uid())).await?;
            check_documents(tx, &env, &documents, false).await?;
        }
        let new_user = match vals.get("user_id") {
            Some(Cell::Id(user)) => Some(*user),
            _ => None,
        };
        let mut moved = Vec::new();
        if let Some(user) = new_user {
            let (models, ids, users) = (
                records.cells(tx, "res_model").await?,
                id_cells(tx, records, "res_id").await?,
                id_cells(tx, records, "user_id").await?,
            );
            for ((model, id), previous) in models.iter().zip(ids).zip(users) {
                if let (Some(model), Some(id)) = (opt_text(model, "res_model")?, id)
                    && previous != Some(user)
                {
                    moved.push((model.to_string(), id, user));
                }
            }
        }
        let archive = match vals.get("active") {
            Some(Cell::Bool(active)) if !vals.contains("date_done") => Some(*active),
            _ => None,
        };
        match archive {
            None => sup.call(tx, records, (vals,)).await?,
            Some(true) => {
                let vals = vals.set_cell("date_done", Cell::Null);
                sup.call(tx, records, (vals,)).await?;
            }
            Some(false) => {
                // keep the earliest archive date when archived again
                let done = records.cells(tx, "date_done").await?;
                let (fresh, kept): (Vec<_>, Vec<_>) = records
                    .rec_ids()
                    .iter()
                    .copied()
                    .zip(done)
                    .partition(|(_, done)| matches!(done, Cell::Null));
                let today = Cell::Date(Date::today());
                if !fresh.is_empty() {
                    // through the whole chain again: with `date_done` given it is a plain write
                    let set = records.browse_ids(fresh.into_iter().map(|(id, _)| id));
                    set.write(tx, vals.clone().set_cell("date_done", today))
                        .await?;
                }
                if !kept.is_empty() {
                    let set = records.browse_ids(kept.into_iter().map(|(id, _)| id));
                    sup.call(tx, &set, (vals,)).await?;
                }
            }
        }
        subscribe_assignees(tx, &env, &moved).await
    }

    /// Delete activities; one assigned to someone else needs write access to its record.
    ///
    /// # Errors
    /// `Access` without the access on the record.
    #[method(override)]
    async fn unlink(
        this: &RecordSet<MailActivity>,
        tx: &mut Transaction,
        sup: Super,
    ) -> Result<()> {
        let records = this.as_records();
        if !records.is_empty() && !records.env().su() {
            let env = records.env().clone();
            let documents = documents_by_model(tx, records, Some(env.uid())).await?;
            check_documents(tx, &env, &documents, false).await?;
        }
        sup.call(tx, records, ()).await
    }

    /// # Errors
    /// As `action_feedback`.
    #[method]
    async fn action_done(this: &RecordSet<MailActivity>, tx: &mut Transaction) -> Result<()> {
        let records = this.as_records();
        let actives = records.cells(tx, "active").await?;
        let ongoing: Vec<RecId> = records
            .rec_ids()
            .iter()
            .copied()
            .zip(actives)
            .filter(|(_, active)| matches!(active, Cell::Bool(true)))
            .map(|(id, _)| id)
            .collect();
        if ongoing.is_empty() {
            return Ok(());
        }
        records
            .browse_ids(ongoing)
            .call::<_, Option<i64>>(tx, "action_feedback", (None::<String>, Vec::<i64>::new()))
            .await?;
        Ok(())
    }

    /// Mark the activities done: post a message with the feedback on each record (subtype
    /// `mail.mt_activities`, authored by the current user, posted privileged so an assignee
    /// without access to the record can complete), then archive them and store the feedback
    /// . Returns the first message's id.
    ///
    /// # Errors
    /// `Validation` when `attachment_ids` is not empty (attachments come with) or the
    /// feedback is over the limit; `Access` as `write`.
    #[method]
    async fn action_feedback(
        this: &RecordSet<MailActivity>,
        tx: &mut Transaction,
        feedback: Option<String>,
        attachment_ids: Vec<i64>,
    ) -> Result<Option<i64>> {
        let messages = this
            .as_records()
            .call::<_, Vec<i64>>(tx, "_action_done", (feedback, attachment_ids))
            .await?;
        Ok(messages.first().copied())
    }

    /// The done path of `action_feedback`; returns the posted messages' ids. An activity whose
    /// record is gone is deleted, not archived. Each activity gets its own message with nothing of
    /// another activity's (BUG-101).
    ///
    /// # Errors
    /// As `action_feedback`.
    #[method]
    async fn _action_done(
        this: &RecordSet<MailActivity>,
        tx: &mut Transaction,
        feedback: Option<String>,
        attachment_ids: Vec<i64>,
    ) -> Result<Vec<i64>> {
        let records = this.as_records();
        if !attachment_ids.is_empty() {
            return Err(Error::validation(
                "Attachments on an activity's feedback come with a later release (ir.attachment); mark \
                 the activity done without attachments.",
            ));
        }
        if records.len() > MAX_ACTIVITIES_PER_CALL {
            return Err(Error::validation(format!(
                "At most {MAX_ACTIVITIES_PER_CALL} activities are completed per call, not {}.",
                records.len()
            )));
        }
        let feedback = match feedback {
            Some(text) if text.is_empty() => None,
            Some(text) => {
                check_len("feedback", &text)?;
                Some(text)
            }
            None => None,
        };
        if records.is_empty() {
            return Ok(Vec::new());
        }
        let env = records.env().clone();
        // access before any privileged step (the post and the delete of orphans below): the
        // OR-ed rows, then the record's write access for activities that are not the caller's own
        records.check_access(tx, Operation::Write).await?;
        if !env.su() {
            let documents = documents_by_model(tx, records, Some(env.uid())).await?;
            check_documents(tx, &env, &documents, false).await?;
        }
        let (models, ids, users, types, summaries, dones) = (
            records.cells(tx, "res_model").await?,
            id_cells(tx, records, "res_id").await?,
            id_cells(tx, records, "user_id").await?,
            id_cells(tx, records, "activity_type_id").await?,
            records.cells(tx, "summary").await?,
            records.cells(tx, "date_done").await?,
        );
        // the records that still exist, by model
        let mut owners: BTreeMap<String, BTreeSet<i64>> = BTreeMap::new();
        for (model, id) in models.iter().zip(&ids) {
            if let (Some(model), Some(id)) = (opt_text(model, "res_model")?, id) {
                owners.entry(model.to_string()).or_default().insert(*id);
            }
        }
        let mut existing: BTreeSet<(String, i64)> = BTreeSet::new();
        for (model, wanted) in &owners {
            let found = env
                .records(model)?
                .sudo(true)
                .browse(wanted.iter().copied())?
                .exists(tx)
                .await?;
            existing.extend(found.ids().into_iter().map(|id| (model.clone(), id)));
        }
        let type_ids: Vec<i64> = types
            .iter()
            .flatten()
            .copied()
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        let type_info = load_types(tx, &env, &type_ids).await?;
        let user_ids: Vec<i64> = users
            .iter()
            .flatten()
            .copied()
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        let user_names = user_display_names(tx, &env, &user_ids).await?;
        let mut posted = Vec::new();
        let mut lost = Vec::new();
        let mut kept = Vec::new();
        for (index, id) in records.rec_ids().iter().enumerate() {
            let (model, res_id) = (opt_text(&models[index], "res_model")?, ids[index]);
            let (Some(model), Some(res_id)) = (model, res_id) else {
                kept.push(*id);
                continue;
            };
            if !existing.contains(&(model.to_string(), res_id)) {
                lost.push(*id);
                continue;
            }
            kept.push(*id);
            if !matches!(dones[index], Cell::Null) {
                // already done: archived again below, no second message
                continue;
            }
            let type_name = types[index]
                .and_then(|type_id| type_info.get(&type_id))
                .map_or_else(|| "Activity".to_owned(), |info| info.name.to_string());
            let mut body = format!("{type_name} done");
            if let Some(summary) = opt_text(&summaries[index], "mail.activity.summary")? {
                body.push_str(&format!(" ({summary})"));
            }
            if let Some(assignee) = users[index]
                && assignee != env.uid()
                && let Some(name) = user_names.get(&assignee)
            {
                body.push_str(&format!(" (originally assigned to {name})"));
            }
            if let Some(feedback) = &feedback {
                body.push_str(": ");
                body.push_str(feedback);
            }
            let mut request = PostRequest::new(Body::Text(body));
            request.subtype = PostSubtype::Xmlid("mail.mt_activities".to_owned());
            request.mail_activity_type_id = types[index];
            let doc = env.records(&model)?.sudo(true).browse([res_id])?;
            let sent: Posted = doc.call(tx, "message_post", (request,)).await?;
            posted.push(sent.message_id);
        }
        if !lost.is_empty() {
            records.browse_ids(lost).unlink(tx).await?;
        }
        if !kept.is_empty() {
            let mut vals = RawVals::new().set_cell("active", Cell::Bool(false));
            if let Some(feedback) = &feedback {
                vals = vals.set_cell("feedback", text_cell(feedback));
            }
            records.browse_ids(kept).write(tx, vals).await?;
        }
        Ok(posted)
    }

    /// # Errors
    /// As `write`.
    #[method]
    async fn action_reschedule_today(
        this: &RecordSet<MailActivity>,
        tx: &mut Transaction,
    ) -> Result<()> {
        let today = this.as_records().env().context_today()?;
        reschedule(tx, this.as_records(), today).await
    }

    /// Move the active activities' deadline to tomorrow.
    ///
    /// # Errors
    /// As `write`.
    #[method]
    async fn action_reschedule_tomorrow(
        this: &RecordSet<MailActivity>,
        tx: &mut Transaction,
    ) -> Result<()> {
        let today = this.as_records().env().context_today()?;
        reschedule(tx, this.as_records(), add_days(today, 1)?).await
    }

    /// # Errors
    /// As `write`.
    #[method]
    async fn action_reschedule_nextweek(
        this: &RecordSet<MailActivity>,
        tx: &mut Transaction,
    ) -> Result<()> {
        let today = this.as_records().env().context_today()?;
        let target = monday_on_or_before(add_days(today, 7)?)?;
        reschedule(tx, this.as_records(), target).await
    }

    /// Move the active activities' deadline to `date_deadline`.
    ///
    /// # Errors
    /// As `write`.
    #[method]
    async fn action_reschedule_customdate(
        this: &RecordSet<MailActivity>,
        tx: &mut Transaction,
        date_deadline: Date,
    ) -> Result<()> {
        reschedule(tx, this.as_records(), date_deadline).await
    }
}

/// Write `deadline` on the active activities of `records`.
async fn reschedule(tx: &mut Transaction, records: &Records, deadline: Date) -> Result<()> {
    let actives = records.cells(tx, "active").await?;
    let active: Vec<RecId> = records
        .rec_ids()
        .iter()
        .copied()
        .zip(actives)
        .filter(|(_, active)| matches!(active, Cell::Bool(true)))
        .map(|(id, _)| id)
        .collect();
    if active.is_empty() {
        return Ok(());
    }
    records
        .browse_ids(active)
        .write(
            tx,
            RawVals::new().set_cell("date_deadline", Cell::Date(deadline)),
        )
        .await
}

/// The display name of each record of `documents`, read privileged (one read per model).
async fn document_names(
    tx: &mut Transaction,
    env: &Env,
    documents: &BTreeMap<String, BTreeSet<i64>>,
) -> Result<BTreeMap<(String, i64), Arc<str>>> {
    let mut out = BTreeMap::new();
    for (model, ids) in documents {
        let docs = env.records(model)?.sudo(true).browse(ids.iter().copied())?;
        for (id, cell) in docs
            .ids()
            .into_iter()
            .zip(docs.cells(tx, "display_name").await?)
        {
            if let Some(name) = opt_text(&cell, "display_name")? {
                out.insert((model.clone(), id), name);
            }
        }
    }
    Ok(out)
}

/// The names of users, read privileged.
async fn user_display_names(
    tx: &mut Transaction,
    env: &Env,
    users: &[i64],
) -> Result<BTreeMap<i64, Arc<str>>> {
    let mut out = BTreeMap::new();
    if users.is_empty() {
        return Ok(out);
    }
    let records = env
        .records("res.users")?
        .sudo(true)
        .browse(users.iter().copied())?;
    for (id, cell) in records
        .ids()
        .into_iter()
        .zip(records.cells(tx, "name").await?)
    {
        if let Some(name) = opt_text(&cell, "res.users.name")? {
            out.insert(id, name);
        }
    }
    Ok(out)
}
