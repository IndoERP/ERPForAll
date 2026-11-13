// SPDX-License-Identifier: MIT OR Apache-2.0
//! The activity menu: the current user's open activities, counted per model.
//!
//! Our own
//! implementation.
//!
//! [`activity_groups`] scans the caller's own open activities (newest first, bounded by the system
//! parameter `mail.activity.systray.limit`), keeps one activity per record (the most urgent) and
//! counts it by state in the group of its model. Activities that are not on a document the caller
//! can read (personal to-dos, hidden or deleted documents, models without a read right, models that
//! are not registered) are counted together in the group "Other Activities", which never names the
//! document or its model. The RPC `mail.activity.erp_activity_groups` returns
//! [`ActivityGroups::to_value`].

use std::collections::{BTreeMap, BTreeSet};

use erp_sdk::Operation;
use erp_sdk::ValueMap;
use erp_sdk::prelude::*;

/// The system parameter that bounds the scan.
pub const LIMIT_PARAMETER: &str = "mail.activity.systray.limit";
/// The scan bound without the parameter, or with the value 0.
pub const DEFAULT_LIMIT: u32 = 1000;
/// The largest value the parameter may have.
pub const MAX_LIMIT: u32 = 10_000;
/// The model name that stands for the group of activities without a readable document.
pub const OTHER_MODEL: &str = "mail.activity";
/// The label of that group.
pub const OTHER_NAME: &str = "Other Activities";

/// The groups of the activity menu and the badge counter.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct ActivityGroups {
    counter: u32,
    groups: Vec<ActivityGroup>,
}

impl ActivityGroups {
    /// The sum of every group's [`ActivityGroup::total_count`] (the badge).
    pub fn counter(&self) -> u32 {
        self.counter
    }

    /// The groups, ordered by name; "Other Activities" last.
    pub fn groups(&self) -> &[ActivityGroup] {
        &self.groups
    }

    /// The answer of the RPC `mail.activity.erp_activity_groups`: a map with `activity_counter`
    /// and `groups` (a list of maps with `model`, `name`, `addon`, `domain`, `overdue_count`,
    /// `today_count`, `planned_count`, `total_count` and `activity_ids`).
    pub fn to_value(&self) -> Value {
        let groups = self.groups.iter().map(ActivityGroup::to_value);
        map([
            ("activity_counter", Value::Int(i64::from(self.counter))),
            ("groups", Value::list(groups)),
        ])
    }
}

/// The counts of one model (or of the "Other Activities" group).
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct ActivityGroup {
    model: String,
    name: String,
    addon: Option<String>,
    domain: Value,
    overdue_count: u32,
    today_count: u32,
    planned_count: u32,
    activity_ids: Vec<i64>,
    // the ids of the group's model per state (record ids; activity ids for the other group),
    // filled by `count`, so each length is the matching counter
    overdue_ids: Vec<i64>,
    today_ids: Vec<i64>,
    planned_ids: Vec<i64>,
}

impl ActivityGroup {
    /// The model of the records, or [`OTHER_MODEL`] for the other group.
    pub fn model(&self) -> &str {
        &self.model
    }

    /// The model's label, or [`OTHER_NAME`].
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The addon that first declared the model (`mail` for the other group).
    pub fn addon(&self) -> Option<&str> {
        self.addon.as_deref()
    }

    /// The domain a client adds when it opens the model's records: archived records included when
    /// the model has an `active` field, empty otherwise and for the other group.
    pub fn domain(&self) -> &Value {
        &self.domain
    }

    /// Records whose most urgent activity is due before today.
    pub fn overdue_count(&self) -> u32 {
        self.overdue_count
    }

    /// Records whose most urgent activity is due today.
    pub fn today_count(&self) -> u32 {
        self.today_count
    }

    /// Records whose most urgent activity is due later.
    pub fn planned_count(&self) -> u32 {
        self.planned_count
    }

    /// Overdue plus today (planned activities are not to do yet).
    pub fn total_count(&self) -> u32 {
        // bounded by the scan limit (at most `MAX_LIMIT`), so the sum cannot overflow
        self.overdue_count.saturating_add(self.today_count)
    }

    /// The counted activities (the most urgent one of each record, every state), by id.
    pub fn activity_ids(&self) -> &[i64] {
        &self.activity_ids
    }

    /// The ids a list opens for `state`, ascending: the records of the group's model whose most
    /// urgent activity is in that state (activity ids for the other group). Its length is the
    /// matching count; [`OpenState::All`] is overdue plus today, as [`Self::total_count`].
    pub fn open_ids(&self, state: OpenState) -> Vec<i64> {
        let mut ids: Vec<i64> = match state {
            OpenState::Overdue => self.overdue_ids.clone(),
            OpenState::Today => self.today_ids.clone(),
            OpenState::Planned => self.planned_ids.clone(),
            OpenState::All => self
                .overdue_ids
                .iter()
                .chain(self.today_ids.iter())
                .copied()
                .collect(),
        };
        ids.sort_unstable();
        ids
    }

    fn to_value(&self) -> Value {
        let addon = match &self.addon {
            Some(addon) => Value::Str(addon.as_str().into()),
            None => Value::Bool(false),
        };
        map([
            ("model", Value::Str(self.model.as_str().into())),
            ("name", Value::Str(self.name.as_str().into())),
            ("addon", addon),
            ("domain", self.domain.clone()),
            ("overdue_count", Value::Int(i64::from(self.overdue_count))),
            ("today_count", Value::Int(i64::from(self.today_count))),
            ("planned_count", Value::Int(i64::from(self.planned_count))),
            ("total_count", Value::Int(i64::from(self.total_count()))),
            (
                "activity_ids",
                Value::list(self.activity_ids.iter().map(|id| Value::Int(*id))),
            ),
        ])
    }

    fn new(model: &str, name: &str, addon: Option<&str>, domain: Value) -> Self {
        Self {
            model: model.to_owned(),
            name: name.to_owned(),
            addon: addon.map(str::to_owned),
            domain,
            overdue_count: 0,
            today_count: 0,
            planned_count: 0,
            activity_ids: Vec::new(),
            overdue_ids: Vec::new(),
            today_ids: Vec::new(),
            planned_ids: Vec::new(),
        }
    }

    fn count(&mut self, entry: &Entry) -> Result<()> {
        let (slot, ids) = match entry.state {
            Urgency::Overdue => (&mut self.overdue_count, &mut self.overdue_ids),
            Urgency::Today => (&mut self.today_count, &mut self.today_ids),
            Urgency::Planned => (&mut self.planned_count, &mut self.planned_ids),
        };
        *slot = slot
            .checked_add(1)
            .ok_or_else(|| Error::invariant("an activity count overflowed"))?;
        ids.push(entry.target);
        self.activity_ids.push(entry.id);
        Ok(())
    }
}

fn map<const N: usize>(entries: [(&'static str, Value); N]) -> Value {
    let mut out = ValueMap::default();
    for (key, value) in entries {
        out.insert(key.to_owned(), value);
    }
    Value::Map(Arc::new(out))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Urgency {
    Overdue,
    Today,
    Planned,
}

/// One scanned activity.
#[derive(Debug, Clone)]
struct Entry {
    id: i64,
    /// The id of the group's model this entry stands for: the record (`res_id`) of a document
    /// group, the activity itself for the other group.
    target: i64,
    deadline_days: i32,
    state: Urgency,
}

impl Entry {
    /// The sort key of "most urgent": the earliest deadline, then the lowest id.
    fn key(&self) -> (i32, i64) {
        (self.deadline_days, self.id)
    }
}

/// The scan bound: the parameter read in superuser mode (a setting the caller may not read
/// itself); unset or 0 is [`DEFAULT_LIMIT`].
async fn scan_limit(env: &Env, tx: &mut Transaction) -> Result<u32> {
    let value = env
        .records("ir.config_parameter")?
        .sudo(true)
        .call::<_, i64>(tx, "get_int", (LIMIT_PARAMETER.to_owned(), None::<i64>))
        .await?;
    if value == 0 {
        return Ok(DEFAULT_LIMIT);
    }
    match u32::try_from(value) {
        Ok(limit) if limit <= MAX_LIMIT => Ok(limit),
        _ => Err(Error::validation(format!(
            "The system parameter {LIMIT_PARAMETER} must be between 1 and {MAX_LIMIT} (or 0 for \
             the default {DEFAULT_LIMIT}), not {value}."
        ))),
    }
}

fn state_of_cell(cell: &Cell) -> Result<Urgency> {
    match cell {
        Cell::Str(state) if state.as_ref() == "overdue" => Ok(Urgency::Overdue),
        Cell::Str(state) if state.as_ref() == "today" => Ok(Urgency::Today),
        Cell::Str(state) if state.as_ref() == "planned" => Ok(Urgency::Planned),
        other => Err(Error::invariant(format!(
            "mail.activity.state of an open activity is not overdue, today or planned: {other:?}"
        ))),
    }
}

/// The current user's activity groups (to 390).
///
/// Runs as the caller: the model-level read right on `mail.activity` is checked first (public and
/// portal users are refused), the scan is `user_id = caller` through the caller's own read rule,
/// and each model's records are filtered by the caller's read rules. Only the limit parameter is
/// read in superuser mode.
///
/// When the caller's selected companies are fewer than their own companies, a record that exists
/// and is readable with all their companies selected is left out of every group (the caller
/// switches company to see it); other unreadable records count under "Other Activities".
///
/// ```no_run
/// # use erp_sdk::prelude::*;
/// # async fn demo(env: &Env, tx: &mut Transaction) -> Result<()> {
/// let menu = erp_addon_mail::activity_groups(env, tx).await?;
/// for group in menu.groups() {
///     println!("{}: {}", group.name(), group.total_count());
/// }
/// # Ok(()) }
/// ```
///
/// # Errors
/// `Access` when the caller may not read activities; `Validation` when the limit parameter is
/// negative, above 10000 or not a number; `Invariant` when a stored value has an unexpected type;
/// database errors.
pub async fn activity_groups(env: &Env, tx: &mut Transaction) -> Result<ActivityGroups> {
    let activities = env.records("mail.activity")?;
    activities.check_access(tx, Operation::Read).await?;
    let limit = scan_limit(env, tx).await?;

    let domain = Domain::condition("user_id", Operator::Eq, env.uid())?;
    let found = activities
        .search_with(
            tx,
            &domain,
            &SearchOptions::new()
                .order("id desc")
                .limit(u64::from(limit)),
        )
        .await?;
    let ids = found.ids();
    let (models, res_ids, deadlines, states) = (
        found.cells(tx, "res_model").await?,
        found.cells(tx, "res_id").await?,
        found.cells(tx, "date_deadline").await?,
        found.cells(tx, "state").await?,
    );

    // personal to-dos count on their own; documents keep their most urgent activity
    let mut personal: Vec<Entry> = Vec::new();
    let mut documents: BTreeMap<String, BTreeMap<i64, Entry>> = BTreeMap::new();
    for (index, id) in ids.iter().enumerate() {
        let (Some(model), Some(deadline), Some(state), Some(res_id)) = (
            models.get(index),
            deadlines.get(index),
            states.get(index),
            res_ids.get(index),
        ) else {
            return Err(Error::invariant(
                "mail.activity columns of the menu scan have different lengths",
            ));
        };
        let Cell::Date(deadline) = deadline else {
            return Err(Error::invariant(format!(
                "mail.activity.date_deadline is not a date: {deadline:?}"
            )));
        };
        let mut entry = Entry {
            id: *id,
            target: *id,
            deadline_days: deadline.days(),
            state: state_of_cell(state)?,
        };
        let model = match model {
            Cell::Str(model) if !model.is_empty() => Some(model.to_string()),
            Cell::Str(_) | Cell::Null => None,
            other => {
                return Err(Error::invariant(format!(
                    "mail.activity.res_model is not text: {other:?}"
                )));
            }
        };
        let Some(model) = model else {
            personal.push(entry);
            continue;
        };
        let res_id = match res_id {
            Cell::Id(res_id) => *res_id,
            other => {
                return Err(Error::invariant(format!(
                    "mail.activity.res_id of a document activity is not an id: {other:?}"
                )));
            }
        };
        entry.target = res_id;
        let per_model = documents.entry(model).or_default();
        match per_model.get(&res_id) {
            Some(kept) if kept.key() <= entry.key() => {}
            _ => {
                per_model.insert(res_id, entry);
            }
        }
    }

    let mut other = ActivityGroup::new(OTHER_MODEL, OTHER_NAME, Some("mail"), Value::list([]));
    for entry in &personal {
        other.count(entry)?;
    }
    let mut groups: Vec<ActivityGroup> = Vec::new();
    for (model, per_record) in &documents {
        let (readable, hidden) = split_readable(env, tx, model, per_record).await?;
        for res_id in hidden {
            // a hidden document never gets its id out: the other group opens the activity
            let mut entry = entry_of(per_record, model, res_id)?.clone();
            entry.target = entry.id;
            other.count(&entry)?;
        }
        if readable.is_empty() {
            continue;
        }
        let Some(spec) = env.registry().model(model) else {
            return Err(Error::invariant(format!(
                "model {model} was registered a moment ago and is not any more"
            )));
        };
        // a model without a label shows its technical name
        let name = if spec.description.is_empty() {
            model.as_str()
        } else {
            spec.description.as_str()
        };
        // the oldest declaration of this very model: the last mixin of the MRO is another model's
        let addon = spec
            .mro
            .iter()
            .rev()
            .find(|declaration| declaration.name == model.as_str())
            .map(|declaration| declaration.addon);
        let domain = match spec.active_name {
            Some(active) => active_domain(spec.field_by_id(active).name),
            None => Value::list([]),
        };
        let mut group = ActivityGroup::new(model, name, addon, domain);
        for res_id in readable {
            group.count(entry_of(per_record, model, res_id)?)?;
        }
        groups.push(group);
    }
    groups.sort_by(|a, b| a.name.cmp(&b.name).then_with(|| a.model.cmp(&b.model)));
    if !other.activity_ids.is_empty() {
        groups.push(other);
    }
    let mut counter: u32 = 0;
    for group in &mut groups {
        group.activity_ids.sort_unstable();
        group.overdue_ids.sort_unstable();
        group.today_ids.sort_unstable();
        group.planned_ids.sort_unstable();
        counter = counter
            .checked_add(group.total_count())
            .ok_or_else(|| Error::invariant("the activity counter overflowed"))?;
    }
    Ok(ActivityGroups { counter, groups })
}

/// The scanned entry of `model`'s record `res_id`; `split_readable` only returns ids it was given,
/// so a missing entry is a bug, not a case to skip.
fn entry_of<'a>(
    per_record: &'a BTreeMap<i64, Entry>,
    model: &str,
    res_id: i64,
) -> Result<&'a Entry> {
    per_record.get(&res_id).ok_or_else(|| {
        Error::invariant(format!(
            "the activity menu has no scanned activity for {model} {res_id}"
        ))
    })
}

/// `[["active", "in", [true, false]]]` for the active field `field`.
fn active_domain(field: &str) -> Value {
    Value::list([Value::list([
        Value::Str(field.into()),
        Value::Str("in".into()),
        Value::list([Value::Bool(true), Value::Bool(false)]),
    ])])
}

/// The records of `model` the caller may read, and those that count under "Other Activities".
/// Records that are only out of the selected companies are in neither list.
async fn split_readable(
    env: &Env,
    tx: &mut Transaction,
    model: &str,
    per_record: &BTreeMap<i64, Entry>,
) -> Result<(Vec<i64>, Vec<i64>)> {
    let wanted: Vec<i64> = per_record.keys().copied().collect();
    // a model that is not registered, or has no model read right: nothing is readable; an activity
    // on `mail.activity` itself counts here too, so the Other group is the only one keyed by it
    if model == OTHER_MODEL || env.registry().model(model).is_none() {
        return Ok((Vec::new(), wanted));
    }
    let records = env.records(model)?;
    if !records.has_access(tx, Operation::Read).await? {
        return Ok((Vec::new(), wanted));
    }
    // existence takes no rule, so a hidden record and a deleted one are told apart only to choose
    // between "dropped" and "other", both of which hide the model
    let existing = records
        .browse(wanted.iter().copied())?
        .exists(tx)
        .await?
        .ids();
    let readable: BTreeSet<i64> = records
        .browse(existing.iter().copied())?
        .filtered_access(tx, Operation::Read)
        .await?
        .ids()
        .into_iter()
        .collect();
    let mut unreadable: Vec<i64> = wanted
        .iter()
        .copied()
        .filter(|id| !readable.contains(id))
        .collect();
    let own = env.principal().companies().all_of_subject();
    let selected = env.companies()?;
    let same = own.len() == selected.len() && own.iter().all(|c| selected.contains(c));
    if !same && !unreadable.is_empty() {
        // the selected companies first (the current company stays), then the others of the user
        let mut widened: Vec<i64> = selected.iter().map(|c| c.get()).collect();
        let rest: Vec<i64> = own
            .iter()
            .map(|c| c.get())
            .filter(|id| !widened.contains(id))
            .collect();
        widened.extend(rest);
        let everywhere = env.with_context([(
            "allowed_company_ids",
            Value::list(widened.into_iter().map(Value::Int)),
        )]);
        let candidates: Vec<i64> = unreadable
            .iter()
            .copied()
            .filter(|id| existing.contains(id))
            .collect();
        let elsewhere: BTreeSet<i64> = everywhere
            .records(model)?
            .browse(candidates)?
            .filtered_access(tx, Operation::Read)
            .await?
            .ids()
            .into_iter()
            .collect();
        // the records readable with every company are in no list: the user switches company
        unreadable.retain(|id| !elsewhere.contains(id));
    }
    Ok((readable.into_iter().collect(), unreadable))
}

/// The most records one opened list may carry: the result of an open request is a literal id list
/// and the web action result is bounded (`addons/web/src/action_result.rs`).
pub const MAX_OPEN_RECORDS: usize = 1000;

/// Which records of a group a list opens.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum OpenState {
    /// Overdue and today, the menu's total.
    All,
    /// Most urgent activity due before today.
    Overdue,
    /// Most urgent activity due today.
    Today,
    /// Most urgent activity due later.
    Planned,
}

impl OpenState {
    /// The state of the wire value `all`, `overdue`, `today` or `planned`.
    ///
    /// # Errors
    /// `Validation` naming the given text and the four allowed values.
    pub fn parse(text: &str) -> Result<Self> {
        match text {
            "all" => Ok(Self::All),
            "overdue" => Ok(Self::Overdue),
            "today" => Ok(Self::Today),
            "planned" => Ok(Self::Planned),
            other => Err(Error::validation(format!(
                "The activity state {other:?} is not known; use all, overdue, today or planned."
            ))),
        }
    }
}

/// The window action that opens one group of the activity menu in one state (and
/// 393): the records of `model` counted by the same scan as [`activity_groups`], or, for
/// [`OTHER_MODEL`], the caller's own activities. The domain is a literal id list followed by the
/// group's own domain (archived records stay visible); the ids are a snapshot of the scan, the
/// list that opens re-applies the caller's rules.
///
/// # Errors
/// As [`activity_groups`]; `Validation` when the caller has no group for `model` (one message,
/// whether or not the model exists or is readable, so the answer reveals nothing about it) or when
/// the state holds more than [`MAX_OPEN_RECORDS`] records (never a shortened list).
pub async fn activity_group_action(
    env: &Env,
    tx: &mut Transaction,
    model: &str,
    state: OpenState,
) -> Result<Value> {
    let menu = activity_groups(env, tx).await?;
    let Some(group) = menu.groups().iter().find(|group| group.model() == model) else {
        return Err(Error::validation(format!(
            "You have no open activities on {model:?} any more; open the activity menu again."
        )));
    };
    let ids = group.open_ids(state);
    if ids.len() > MAX_OPEN_RECORDS {
        return Err(Error::validation(format!(
            "{} records match; a list opens at most {MAX_OPEN_RECORDS}. Open them from the \
             records' own list instead.",
            ids.len()
        )));
    }
    let mut terms = vec![Value::list([
        Value::Str("id".into()),
        Value::Str("in".into()),
        Value::list(ids.into_iter().map(Value::Int)),
    ])];
    let Value::List(extra) = group.domain() else {
        return Err(Error::invariant(format!(
            "the domain of the activity group {} is not a list",
            group.model()
        )));
    };
    terms.extend(extra.iter().cloned());
    Ok(map([
        ("type", Value::Str("ir.actions.act_window".into())),
        ("name", Value::Str(group.name().into())),
        ("res_model", Value::Str(group.model().into())),
        ("target", Value::Str("current".into())),
        (
            "views",
            Value::list([
                Value::list([Value::Bool(false), Value::Str("list".into())]),
                Value::list([Value::Bool(false), Value::Str("form".into())]),
            ]),
        ),
        ("context", Value::Map(Arc::new(ValueMap::default()))),
        ("domain", Value::list(terms)),
    ]))
}
