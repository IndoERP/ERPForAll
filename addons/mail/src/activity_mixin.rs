//! `mail.activity.mixin`: the abstract mixin that gives a model activities, with the scheduling
//! helpers business code calls, the activities panel data of the chatter and the systray counts.
//!
//! and `mail_activity.py` (`_store_activity_fields`, the systray); design note
//! . Our own implementation. A model gets the activities by inheriting
//! the mixin next to `mail.thread` (`inherit = [MailThread, MailActivityMixin]`): guide
//! "Add activities to a model".
//!
//! The panel ([`ActivityPanel`], method `activities_panel`) and the systray ([`activity_systray`])
//! are plain typed values; the store topics wrap them when merges.

use std::collections::{BTreeMap, BTreeSet};

use erp_sdk::prelude::*;
use erp_sdk::{Fetch, Operation};

use crate::activity::{MailActivity, ScheduleRequest, automation_skipped};
use crate::activity_date::state_of;
use crate::activity_type::{default_type, load_types};

/// The activity mixin: activities of a record and the status derived from them.
#[model(
    name = "mail.activity.mixin",
    description = "Activity Mixin",
    kind = "abstract",
    addon = "mail"
)]
pub struct MailActivityMixin {
    /// The record's active activities, nearest deadline first.
    #[field(
        one2many,
        comodel = "mail.activity",
        inverse_name = "res_id",
        groups = "base.group_user",
        string = "Activities"
    )]
    pub activity_ids: X2many<MailActivity>,
    /// The most urgent state among the active activities: `overdue`, `today` or `planned`; empty
    /// without any. Not stored and not searchable yet.
    #[field(
        selection,
        selection = [("overdue", "Overdue"), ("today", "Today"), ("planned", "Planned")],
        compute = "_compute_activity_state",
        groups = "base.group_user",
        string = "Activity State"
    )]
    pub activity_state: Option<Arc<str>>,
    /// Assignee of the nearest activity.
    #[field(
        many2one,
        comodel = "res.users",
        compute = "_compute_activity_user_id",
        groups = "base.group_user",
        string = "Responsible User"
    )]
    pub activity_user_id: Option<RecId>,
    /// Deadline of the nearest activity.
    #[field(
        date,
        compute = "_compute_activity_date_deadline",
        groups = "base.group_user",
        string = "Next Activity Deadline"
    )]
    pub activity_date_deadline: Option<Date>,
    /// Summary of the nearest activity.
    #[field(
        char,
        compute = "_compute_activity_summary",
        groups = "base.group_user",
        string = "Next Activity Summary"
    )]
    pub activity_summary: Option<Arc<str>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct PanelActivity {
    /// The activity.
    pub id: i64,
    /// Its type.
    pub activity_type_id: Option<i64>,
    /// The type's name.
    pub activity_type_name: Option<String>,
    /// The type's category (`default`, `upload_file`, `phonecall`).
    pub category: Option<String>,
    /// The type's icon.
    pub icon: Option<String>,
    /// What to do.
    pub summary: Option<String>,
    /// The note.
    pub note: Option<String>,
    /// The deadline.
    pub date_deadline: Option<Date>,
    /// `overdue`, `today` or `planned`.
    pub state: String,
    /// The assignee.
    pub user_id: Option<i64>,
    /// The assignee's partner.
    pub user_partner_id: Option<i64>,
    /// The assignee's name.
    pub user_name: Option<String>,
    /// Who created it.
    pub create_uid: Option<i64>,
    /// When it was created.
    pub create_date: Option<DateTime>,
    /// Whether the current user may change it.
    pub can_write: bool,
    /// Created by a business flow.
    pub automated: bool,
}

impl PanelActivity {
    pub fn display_name(&self) -> &str {
        self.summary
            .as_deref()
            .filter(|summary| !summary.is_empty())
            .or(self.activity_type_name.as_deref())
            .unwrap_or("")
    }
}

/// The activities panel of one record: its active activities, nearest deadline first.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct ActivityPanel {
    /// The activities the caller may read.
    pub activities: Vec<PanelActivity>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct SystrayGroup {
    /// Model of the records; `None` for personal to-dos.
    pub res_model: Option<String>,
    /// Activities due before today.
    pub overdue: u64,
    /// Activities due today.
    pub today: u64,
    /// Activities due later.
    pub planned: u64,
}

impl SystrayGroup {
    /// Overdue plus today plus planned.
    pub fn total(&self) -> u64 {
        self.overdue + self.today + self.planned
    }

    /// The count the systray badge shows: due today or before.
    pub fn todo(&self) -> u64 {
        self.overdue + self.today
    }
}

/// Name, category and icon of an activity type, as the panel shows them.
type TypeRow = (Option<String>, Option<String>, Option<String>);

fn int_cell(cell: &Cell, what: &str) -> Result<i64> {
    match cell {
        Cell::Id(id) => Ok(*id),
        other => Err(Error::invariant(format!("{what} is not an id: {other:?}"))),
    }
}

async fn type_ids_of(tx: &mut Transaction, xmlids: &[String]) -> Result<Vec<i64>> {
    let mut modules = Vec::with_capacity(xmlids.len());
    let mut names = Vec::with_capacity(xmlids.len());
    for xmlid in xmlids {
        match xmlid.split_once('.') {
            Some((module, name))
                if !module.is_empty() && !name.is_empty() && !name.contains('.') =>
            {
                modules.push(module.to_owned());
                names.push(name.to_owned());
            }
            _ => {
                return Err(Error::validation(format!(
                    "An activity type external id is 'module.name', not '{xmlid}'."
                )));
            }
        }
    }
    if modules.is_empty() {
        return Ok(Vec::new());
    }
    tx.flush().await?;
    let rows = tx
        .query(
            "SELECT d.res_id::int8 FROM ir_model_data d JOIN unnest($1::text[], $2::text[]) AS w(m, n) \
             ON d.module = w.m AND d.name = w.n WHERE d.model = 'mail.activity.type' ORDER BY d.res_id",
            &[&modules, &names],
        )
        .await?;
    rows.iter()
        .map(|row| {
            row.try_get::<_, i64>(0)
                .map_err(|error| Error::db(format!("unexpected activity type id: {error}")))
        })
        .collect()
}

#[methods(MailActivityMixin, addon = "mail")]
impl MailActivityMixin {
    /// The activities are found
    /// with the caller's access, in one statement for the batch.
    #[compute(
        depends = [
            "activity_ids",
            "activity_ids.active",
            "activity_ids.date_deadline",
            "activity_ids.user_id"
        ],
        depends_context = ["uid"]
    )]
    async fn _compute_activity_state(
        this: &RecordSet<MailActivityMixin>,
        tx: &mut Transaction,
        out: &mut Assign<'_, MailActivityMixin>,
    ) -> Result<()> {
        let records = this.as_records();
        let saved = records.ids();
        if saved.is_empty() {
            return Ok(());
        }
        let domain = Domain::condition("res_model", Operator::Eq, records.model_name())?
            & Domain::condition("res_id", Operator::In, saved)?;
        let found = records
            .env()
            .records("mail.activity")?
            .search(tx, &domain)
            .await?;
        let (owners, states) = (
            found.cells(tx, "res_id").await?,
            found.cells(tx, "state").await?,
        );
        let mut urgent: BTreeMap<i64, u8> = BTreeMap::new();
        for (owner, state) in owners.iter().zip(&states) {
            let rank = match state {
                Cell::Str(state) if state.as_ref() == "overdue" => 0,
                Cell::Str(state) if state.as_ref() == "today" => 1,
                Cell::Str(state) if state.as_ref() == "planned" => 2,
                // archived activities are not found, a missing state has no deadline
                _ => continue,
            };
            let owner = int_cell(owner, "mail.activity.res_id")?;
            let entry = urgent.entry(owner).or_insert(rank);
            *entry = (*entry).min(rank);
        }
        for (index, id) in records.rec_ids().iter().enumerate() {
            let Some(real) = id.as_real().map(|real| real.get()) else {
                continue;
            };
            let state = match urgent.get(&real) {
                Some(0) => Some("overdue"),
                Some(1) => Some("today"),
                Some(_) => Some("planned"),
                None => None,
            };
            out.set(
                index,
                MailActivityMixin::ACTIVITY_STATE,
                state.map(Arc::<str>::from),
            )?;
        }
        Ok(())
    }

    /// The assignee of the nearest activity.
    #[compute(depends = ["activity_ids", "activity_ids.user_id"])]
    fn _compute_activity_user_id(batch: &mut Batch<'_, MailActivityMixin>) -> Result<()> {
        for index in batch.indices() {
            let activities = batch.get(index, MailActivityMixin::ACTIVITY_IDS)?;
            let user = match activities.ids().first() {
                Some(first) => batch
                    .values_of(MailActivity::USER_ID, std::slice::from_ref(first))?
                    .into_iter()
                    .next()
                    .flatten(),
                None => None,
            };
            batch.set(index, MailActivityMixin::ACTIVITY_USER_ID, user)?;
        }
        Ok(())
    }

    /// The deadline of the nearest activity.
    #[compute(depends = ["activity_ids", "activity_ids.date_deadline"])]
    fn _compute_activity_date_deadline(batch: &mut Batch<'_, MailActivityMixin>) -> Result<()> {
        for index in batch.indices() {
            let activities = batch.get(index, MailActivityMixin::ACTIVITY_IDS)?;
            let deadline = match activities.ids().first() {
                Some(first) => batch
                    .values_of(MailActivity::DATE_DEADLINE, std::slice::from_ref(first))?
                    .into_iter()
                    .next()
                    .flatten(),
                None => None,
            };
            batch.set(index, MailActivityMixin::ACTIVITY_DATE_DEADLINE, deadline)?;
        }
        Ok(())
    }

    /// The summary of the nearest activity.
    #[compute(depends = ["activity_ids", "activity_ids.summary"])]
    fn _compute_activity_summary(batch: &mut Batch<'_, MailActivityMixin>) -> Result<()> {
        for index in batch.indices() {
            let activities = batch.get(index, MailActivityMixin::ACTIVITY_IDS)?;
            let summary = match activities.ids().first() {
                Some(first) => batch
                    .values_of(MailActivity::SUMMARY, std::slice::from_ref(first))?
                    .into_iter()
                    .next()
                    .flatten(),
                None => None,
            };
            batch.set(index, MailActivityMixin::ACTIVITY_SUMMARY, summary)?;
        }
        Ok(())
    }

    /// Returns `None` without creating anything when the context
    /// carries `mail_activity_automation_skip`.
    ///
    /// # Errors
    /// As `mail.activity` `create`; `Validation` for an external id that is not `module.name`.
    #[method]
    async fn activity_schedule(
        this: &RecordSet<MailActivityMixin>,
        tx: &mut Transaction,
        request: ScheduleRequest,
    ) -> Result<Option<Records>> {
        let records = this.as_records();
        let env = records.env().clone();
        if automation_skipped(&env) {
            return Ok(None);
        }
        let model = records.model_name();
        let mut type_id = match (&request.act_type_xmlid, request.activity_type_id) {
            (Some(xmlid), _) => type_ids_of(tx, std::slice::from_ref(xmlid))
                .await?
                .first()
                .copied(),
            (None, given) => given,
        };
        if let Some(id) = type_id {
            let info = load_types(tx, &env, &[id]).await?;
            let restricted = info
                .get(&id)
                .and_then(|info| info.res_model.as_ref())
                .is_some_and(|restricted| restricted.as_ref() != model);
            if restricted {
                type_id = None;
            }
        }
        let type_id = match type_id {
            Some(id) => Some(id),
            None => default_type(tx, &env, Some(model)).await?,
        };
        let today = env.context_today()?;
        let mut rows = Vec::with_capacity(records.len());
        for id in records.ids() {
            let mut vals = RawVals::new()
                .set_cell("res_model", Cell::Str(model.into()))
                .set_cell("res_id", Cell::Id(id))
                .set_cell("automated", Cell::Bool(request.automated));
            if let Some(type_id) = type_id {
                vals = vals.set_cell("activity_type_id", Cell::Id(type_id));
            }
            vals = vals.set_cell(
                "date_deadline",
                Cell::Date(request.date_deadline.unwrap_or(today)),
            );
            if let Some(summary) = request.summary.as_deref().filter(|s| !s.is_empty()) {
                vals = vals.set_cell("summary", Cell::Str(summary.into()));
            }
            if let Some(note) = request.note.as_deref().filter(|s| !s.is_empty()) {
                vals = vals.set_cell("note", Cell::Str(note.into()));
            }
            if let Some(user) = request.user_id {
                vals = vals.set_cell("user_id", Cell::Id(user));
            }
            rows.push(vals);
        }
        let created = env.records("mail.activity")?.create(tx, rows).await?;
        Ok(Some(created))
    }

    /// Empty when the context carries `mail_activity_automation_skip` or no type is known.
    ///
    /// # Errors
    /// `Validation` for an external id that is not `module.name`.
    #[method]
    async fn activity_search(
        this: &RecordSet<MailActivityMixin>,
        tx: &mut Transaction,
        act_type_xmlids: Vec<String>,
        user_id: Option<i64>,
        only_automated: bool,
    ) -> Result<Records> {
        let records = this.as_records();
        let env = records.env();
        let activities = env.records("mail.activity")?;
        if automation_skipped(env) {
            return activities.browse(Vec::<i64>::new());
        }
        let types = type_ids_of(tx, &act_type_xmlids).await?;
        if types.is_empty() {
            return activities.browse(Vec::<i64>::new());
        }
        let mut domain = Domain::condition("res_model", Operator::Eq, records.model_name())?
            & Domain::condition("res_id", Operator::In, records.ids())?
            & Domain::condition("activity_type_id", Operator::In, types)?;
        if only_automated {
            domain = domain & Domain::condition("automated", Operator::Eq, true)?;
        }
        if let Some(user) = user_id {
            domain = domain & Domain::condition("user_id", Operator::Eq, user)?;
        }
        activities.search(tx, &domain).await
    }

    /// `None` when the context carries
    /// `mail_activity_automation_skip` or no given type exists.
    ///
    /// # Errors
    /// As `activity_search` and `mail.activity` `write`.
    #[method]
    async fn activity_reschedule(
        this: &RecordSet<MailActivityMixin>,
        tx: &mut Transaction,
        act_type_xmlids: Vec<String>,
        user_id: Option<i64>,
        date_deadline: Option<Date>,
        new_user_id: Option<i64>,
        only_automated: bool,
    ) -> Result<Option<Records>> {
        let env = this.as_records().env();
        if automation_skipped(env) || type_ids_of(tx, &act_type_xmlids).await?.is_empty() {
            return Ok(None);
        }
        let found: Records = this
            .as_records()
            .call(
                tx,
                "activity_search",
                (act_type_xmlids, user_id, only_automated),
            )
            .await?;
        if !found.is_empty() {
            let mut vals = RawVals::new();
            if let Some(deadline) = date_deadline {
                vals = vals.set_cell("date_deadline", Cell::Date(deadline));
            }
            if let Some(user) = new_user_id {
                vals = vals.set_cell("user_id", Cell::Id(user));
            }
            if !vals.is_empty() {
                found.write(tx, vals).await?;
            }
        }
        Ok(Some(found))
    }

    /// # Errors
    /// As `activity_search` and `mail.activity` `action_feedback`.
    #[method]
    async fn activity_feedback(
        this: &RecordSet<MailActivityMixin>,
        tx: &mut Transaction,
        act_type_xmlids: Vec<String>,
        user_id: Option<i64>,
        feedback: Option<String>,
        attachment_ids: Vec<i64>,
        only_automated: bool,
    ) -> Result<bool> {
        let env = this.as_records().env();
        if automation_skipped(env) || type_ids_of(tx, &act_type_xmlids).await?.is_empty() {
            return Ok(false);
        }
        let found: Records = this
            .as_records()
            .call(
                tx,
                "activity_search",
                (act_type_xmlids, user_id, only_automated),
            )
            .await?;
        if !found.is_empty() {
            found
                .call::<_, Option<i64>>(tx, "action_feedback", (feedback, attachment_ids))
                .await?;
        }
        Ok(true)
    }

    /// # Errors
    /// As `activity_search` and `mail.activity` `unlink`.
    #[method]
    async fn activity_unlink(
        this: &RecordSet<MailActivityMixin>,
        tx: &mut Transaction,
        act_type_xmlids: Vec<String>,
        user_id: Option<i64>,
        only_automated: bool,
    ) -> Result<bool> {
        let env = this.as_records().env();
        if automation_skipped(env) || type_ids_of(tx, &act_type_xmlids).await?.is_empty() {
            return Ok(false);
        }
        let found: Records = this
            .as_records()
            .call(
                tx,
                "activity_search",
                (act_type_xmlids, user_id, only_automated),
            )
            .await?;
        found.unlink(tx).await?;
        Ok(true)
    }

    /// Needs read access to the record. A constant number of statements however
    /// many activities there are.
    ///
    /// # Errors
    /// `Validation` for a recordset of more than one record; `Access` without read access.
    #[method]
    async fn activities_panel(
        this: &RecordSet<MailActivityMixin>,
        tx: &mut Transaction,
    ) -> Result<ActivityPanel> {
        let record = this.as_records().ensure_one()?;
        let res_id = match record.ids().as_slice() {
            [id] if *id > 0 => *id,
            _ => {
                return Err(Error::validation(
                    "The activities panel is read for a saved record.",
                ));
            }
        };
        record.check_access(tx, Operation::Read).await?;
        let env = record.env().clone();
        let domain = Domain::condition("res_model", Operator::Eq, record.model_name())?
            & Domain::condition("res_id", Operator::Eq, res_id)?;
        let columns = [
            "activity_type_id",
            "summary",
            "note",
            "date_deadline",
            "user_id",
            "create_uid",
            "create_date",
            "automated",
        ];
        let options = SearchOptions {
            fetch: Fetch::Fields(columns.map(str::to_owned).to_vec()),
            ..SearchOptions::default()
        };
        let found = env
            .records("mail.activity")?
            .search_with(tx, &domain, &options)
            .await?;
        let ids = found.ids();
        if ids.is_empty() {
            return Ok(ActivityPanel {
                activities: Vec::new(),
            });
        }
        let (
            types,
            summaries,
            notes,
            deadlines,
            users,
            creators,
            created,
            automated,
            states,
            can_write,
        ) = (
            found.cells(tx, "activity_type_id").await?,
            found.cells(tx, "summary").await?,
            found.cells(tx, "note").await?,
            found.cells(tx, "date_deadline").await?,
            found.cells(tx, "user_id").await?,
            found.cells(tx, "create_uid").await?,
            found.cells(tx, "create_date").await?,
            found.cells(tx, "automated").await?,
            found.cells(tx, "state").await?,
            found.cells(tx, "can_write").await?,
        );
        let opt_id = |cell: &Cell| match cell {
            Cell::Id(id) => Some(*id),
            _ => None,
        };
        let type_ids: Vec<i64> = types
            .iter()
            .filter_map(opt_id)
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        let user_ids: Vec<i64> = users
            .iter()
            .filter_map(opt_id)
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        let mut type_rows: BTreeMap<i64, TypeRow> = BTreeMap::new();
        if !type_ids.is_empty() {
            let set = env
                .records("mail.activity.type")?
                .sudo(true)
                .browse(type_ids.iter().copied())?;
            let (names, categories, icons) = (
                set.cells(tx, "name").await?,
                set.cells(tx, "category").await?,
                set.cells(tx, "icon").await?,
            );
            let text = |cell: &Cell| match cell {
                Cell::Str(value) if !value.is_empty() => Some(value.to_string()),
                _ => None,
            };
            for (i, id) in set.ids().into_iter().enumerate() {
                type_rows.insert(id, (text(&names[i]), text(&categories[i]), text(&icons[i])));
            }
        }
        let mut user_rows: BTreeMap<i64, (Option<i64>, Option<String>)> = BTreeMap::new();
        if !user_ids.is_empty() {
            let set = env
                .records("res.users")?
                .sudo(true)
                .browse(user_ids.iter().copied())?;
            let (partners, names) = (
                set.cells(tx, "partner_id").await?,
                set.cells(tx, "name").await?,
            );
            for (i, id) in set.ids().into_iter().enumerate() {
                let name = match &names[i] {
                    Cell::Str(value) if !value.is_empty() => Some(value.to_string()),
                    _ => None,
                };
                user_rows.insert(id, (opt_id(&partners[i]), name));
            }
        }
        let text = |cell: &Cell| match cell {
            Cell::Str(value) if !value.is_empty() => Some(value.to_string()),
            _ => None,
        };
        let mut activities = Vec::with_capacity(ids.len());
        for (i, id) in ids.into_iter().enumerate() {
            let type_id = opt_id(&types[i]);
            let (type_name, category, icon) = type_id
                .and_then(|type_id| type_rows.get(&type_id).cloned())
                .unwrap_or((None, None, None));
            let user_id = opt_id(&users[i]);
            let (user_partner_id, user_name) = user_id
                .and_then(|user| user_rows.get(&user).cloned())
                .unwrap_or((None, None));
            let state = match &states[i] {
                Cell::Str(state) => state.to_string(),
                other => {
                    return Err(Error::invariant(format!(
                        "mail.activity {id} has no state ({other:?}); the deadline is required"
                    )));
                }
            };
            activities.push(PanelActivity {
                id,
                activity_type_id: type_id,
                activity_type_name: type_name,
                category,
                icon,
                summary: text(&summaries[i]),
                note: text(&notes[i]),
                date_deadline: match &deadlines[i] {
                    Cell::Date(date) => Some(*date),
                    _ => None,
                },
                state,
                user_id,
                user_partner_id,
                user_name,
                create_uid: opt_id(&creators[i]),
                create_date: match &created[i] {
                    Cell::DateTime(at) => Some(*at),
                    _ => None,
                },
                can_write: matches!(can_write[i], Cell::Bool(true)),
                automated: matches!(automated[i], Cell::Bool(true)),
            });
        }
        Ok(ActivityPanel { activities })
    }
}

/// The systray counts of the current user: their active activities by model, split into overdue,
/// today and planned by the deadline against today in their time zone (UTC without one).
/// An
/// activity assigned to the user counts whether or not they still read its record, as the rule
/// lets them read it.
///
/// # Errors
/// `Validation` when the user's time zone is not a known zone; database and access errors.
pub async fn activity_systray(env: &Env, tx: &mut Transaction) -> Result<Vec<SystrayGroup>> {
    let domain = Domain::condition("user_id", Operator::Eq, env.uid())?;
    let groups = env
        .records("mail.activity")?
        ._read_group(
            tx,
            ReadGroup::new(domain)
                .groupby(["res_model", "date_deadline:day"])
                .aggregates(["__count"]),
        )
        .await?;
    let zones = user_timezones_of(tx, env, env.uid()).await?;
    let today = Date::context_today(zones.as_deref().unwrap_or("UTC"))?;
    let mut per: BTreeMap<Option<String>, SystrayGroup> = BTreeMap::new();
    for row in groups.rows() {
        let [model, day, count] = row.as_slice() else {
            return Err(Error::invariant(
                "the systray statement returned rows of another shape",
            ));
        };
        let model = match model {
            GroupValue::Str(model) => Some(model.to_string()),
            GroupValue::False | GroupValue::Null => None,
            other => {
                return Err(Error::invariant(format!(
                    "unexpected systray model value {other:?}"
                )));
            }
        };
        let GroupValue::Date(day) = day else {
            return Err(Error::invariant(format!(
                "unexpected systray deadline value {day:?}"
            )));
        };
        let GroupValue::Int(count) = count else {
            return Err(Error::invariant(format!(
                "unexpected systray count value {count:?}"
            )));
        };
        let count =
            u64::try_from(*count).map_err(|_| Error::invariant("a negative systray count"))?;
        let group = per.entry(model.clone()).or_insert_with(|| SystrayGroup {
            res_model: model,
            overdue: 0,
            today: 0,
            planned: 0,
        });
        match state_of(true, *day, today) {
            "overdue" => group.overdue += count,
            "today" => group.today += count,
            _ => group.planned += count,
        }
    }
    Ok(per.into_values().collect())
}

/// The time zone of user `uid`, if set.
async fn user_timezones_of(tx: &mut Transaction, env: &Env, uid: i64) -> Result<Option<String>> {
    let user = env.records("res.users")?.sudo(true).browse([uid])?;
    match user.cell(tx, "tz").await? {
        Cell::Str(tz) if !tz.is_empty() => Ok(Some(tz.to_string())),
        Cell::Str(_) | Cell::Null => Ok(None),
        other => Err(Error::invariant(format!(
            "res.users.tz is not text: {other:?}"
        ))),
    }
}
