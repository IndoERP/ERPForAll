//! Every write to these tables, to the implied-group relation and to the privileges bumps the
//! `policy` generation through a statement-level trigger: the compiled
//! policy holds the group graph. Membership (`user_ids`, `res_groups_users_rel`) does not: the
//! resolver reads it at every transaction start.
//!
//! What the model adds on top of the columns:
//!
//! - `all_implied_ids`, `all_implied_by_ids`: the reflexive transitive closure of the implication
//!   relation and its inverse; `disjoint_ids`: the other user type groups of a user type group;
//!   `all_user_ids`: users of the group and of every group implying it; `full_name`:
//!   `Privilege / Group`. Closures are computed from the whole relation in memory (one statement
//!   per batch), cycles included.
//! - Constraints: a group may not (transitively) imply two user type groups (portal, internal,
//!   public are exclusive), and no active user may end up in two; a group
//!   name written with a leading `-` is refused.
//! - Methods: `_apply_group` and `_remove_group` (add or remove an implied group on a set of
//!   groups and what their members inherit), `_has_cycle`, `_get_user_type_groups`,
//!   `_get_light_group_xmlids` (overridable by addons), `_is_light_groups`,
//!   `_reduce_to_light_groups`, and the regular-user marker `base.group_user_regular`.
//! - A change of the implication relation recomputes the `share` flag of the users set-based
//!   ([`crate::group_graph::recompute_user_share`]).
//!
//! Not yet here: `category_id` of a privilege (`ir.module.category` arrives with),
//! translated names, `menu_access`/`view_access`, the `all_user_ids` inverse.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use erp_sdk::domain::{Operator, Value as DomainValue};
use erp_sdk::prelude::*;
use erp_sdk::{Command, Domain, RecId};

use crate::group_graph::{
    APPLY_REGULAR_RUNNING, DEFAULT_USER_GROUP, GROUP_SYSTEM, GROUP_USER_REGULAR, Graph, GraphView,
    ImplicationChange, direct_groups, display_names, group_by_xmlid, groups_by_xmlid,
    internal_group_ids, lock_type_groups, lock_users_for_type_check, lookup_groups,
    members_under_changed_implications, py_repr, recompute_user_share, row_id, user_type_groups,
};
use crate::res_users::ensure_an_administrator;

/// `res.groups.privilege`: the "scope" a group belongs to in the user form.
#[model(
    name = "res.groups.privilege",
    description = "Privilege",
    order = "sequence, name, id",
    signal_triggers = [("_policy_bump", "policy")],
)]
pub struct ResGroupsPrivilege {
    /// Name.
    #[field(char, required = true)]
    pub name: Option<Arc<str>>,
    /// Description.
    #[field(text)]
    pub description: Option<Arc<str>>,
    /// Label shown when the user is in none of the privilege's groups.
    #[field(char, default = "No")]
    pub placeholder: Option<Arc<str>>,
    /// Sequence.
    #[field(integer, default = 100)]
    pub sequence: i32,
    /// The groups of this privilege.
    #[field(one2many, comodel = "res.groups", inverse_name = "privilege_id")]
    pub group_ids: Vec<RecId>,
}

/// `res.groups`: an access group; groups imply other groups (`implied_ids`), and a user's
/// effective groups are the closure of its own.
#[model(
    name = "res.groups",
    description = "Access Groups",
    order = "privilege_id, sequence, name, id",
    allow_sudo_commands = false,
    constraints = [(
        "_check_api_key_duration",
        "CHECK(api_key_duration >= 0)",
        "The api key duration cannot be a negative value."
    )],
    signal_triggers = [
        ("_policy_bump", "policy"),
        ("_policy_bump_implied", "policy", relation = "implied_ids"),
    ],
)]
pub struct ResGroups {
    /// Name.
    #[field(char, required = true)]
    pub name: Option<Arc<str>>,
    /// Users explicitly in this group (membership; the resolver reads it, no policy bump).
    #[field(
        many2many,
        comodel = "res.users",
        relation = "res_groups_users_rel",
        column1 = "gid",
        column2 = "uid"
    )]
    pub user_ids: Vec<RecId>,
    /// Users of this group and of every group implying it.
    #[field(
        many2many,
        comodel = "res.users",
        compute = "_compute_all_user_ids",
        string = "Users and implied users"
    )]
    pub all_user_ids: Vec<RecId>,
    /// Its permissions (`ir.access` rows with this group).
    #[field(one2many, comodel = "ir.access", inverse_name = "group_id")]
    pub access_ids: Vec<RecId>,
    /// Notes.
    #[field(text)]
    pub comment: Option<Arc<str>>,
    /// `Privilege / Group`, or the name alone without a privilege.
    #[field(
        char,
        compute = "_compute_full_name",
        compute_sudo,
        depends_context = ["short_display_name"],
        string = "Group Name"
    )]
    pub full_name: Option<Arc<str>>,
    /// A share group (portal and public users: external data sharing).
    #[field(boolean)]
    pub share: bool,
    /// Longest validity of an API key created by a member, in days.
    #[field(float)]
    pub api_key_duration: f64,
    /// Sequence inside its privilege.
    #[field(integer)]
    pub sequence: i32,
    /// The privilege ("scope") it belongs to.
    #[field(many2one, comodel = "res.groups.privilege", index = true)]
    pub privilege_id: Option<RecId>,
    /// Groups its users are implicitly in too.
    #[field(
        many2many,
        comodel = "res.groups",
        relation = "res_groups_implied_rel",
        column1 = "gid",
        column2 = "hid"
    )]
    pub implied_ids: Vec<RecId>,
    /// The group itself with all its implied groups.
    #[field(
        many2many,
        comodel = "res.groups",
        compute = "_compute_all_implied_ids",
        compute_sudo,
        string = "Transitively Implied Groups"
    )]
    pub all_implied_ids: Vec<RecId>,
    /// Groups whose users are implicitly in this one.
    #[field(
        many2many,
        comodel = "res.groups",
        relation = "res_groups_implied_rel",
        column1 = "hid",
        column2 = "gid"
    )]
    pub implied_by_ids: Vec<RecId>,
    /// The group itself with every group that implies it.
    #[field(
        many2many,
        comodel = "res.groups",
        compute = "_compute_all_implied_by_ids",
        compute_sudo,
        string = "Transitively Implying Groups"
    )]
    pub all_implied_by_ids: Vec<RecId>,
    /// Users of a user type group cannot be in the other user type groups.
    #[field(
        many2many,
        comodel = "res.groups",
        compute = "_compute_disjoint_ids",
        string = "Disjoint Groups"
    )]
    pub disjoint_ids: Vec<RecId>,
}

/// Whether a write or create names `field` (a plain value or x2many commands).
fn touches(vals: &RawVals, field: &str) -> bool {
    vals.contains(field) || vals.commands(field).is_some()
}

/// The existing users a write or create names in `user_ids` (links and replacements, as commands
/// or as a plain list): the users whose memberships change from the group side. New users cannot
/// race with anything, so a record without a row is skipped.
///
/// # Errors
///
/// `Programming` for a plain value that is not a list of records: it cannot be read, and
/// locking nobody would hide the race.
fn named_members(vals: &RawVals) -> Result<Vec<i64>> {
    let mut ids = Vec::new();
    match vals.get("user_ids") {
        None | Some(Cell::Null) => {}
        Some(Cell::Ids(set)) => ids.extend(set.iter().filter_map(|id| row_id(*id))),
        Some(other) => {
            return Err(Error::programming(format!(
                "res.groups user_ids was given as {other:?}; it must be a list of records or x2many \
                 commands"
            )));
        }
    }
    for command in vals.commands("user_ids").unwrap_or_default() {
        match command {
            Command::Link(id) => ids.extend(row_id(*id)),
            Command::Set(set) => ids.extend(set.iter().filter_map(|id| row_id(*id))),
            _ => {}
        }
    }
    Ok(ids)
}

fn rec(id: i64) -> Result<RecId> {
    RecId::from_real(id)
        .ok_or_else(|| Error::programming(format!("{id} is not a database record id")))
}

fn rec_ids(ids: impl IntoIterator<Item = i64>) -> Result<Vec<RecId>> {
    ids.into_iter().map(rec).collect()
}

/// The database ids of a recordset (a new record stands for its origin, if any).
///
/// # Errors
///
/// `Programming` for a record with no row (a new record without an origin): dropping it would
/// make the answer about the rest of the set, and an unsaved group is in no graph.
fn row_ids(records: &Records) -> Result<Vec<i64>> {
    records
        .rec_ids()
        .iter()
        .map(|id| {
            row_id(*id).ok_or_else(|| {
                Error::programming(format!(
                    "res.groups record {id} has no database row (a new record without an \
                     origin); save it first"
                ))
            })
        })
        .collect()
}

/// A negative operator negates the positive search; `=`/`!=` are `in`/`not in` of one
/// value.
///
/// ```
/// use erp_addon_base::res_groups::full_name_domain;
/// use erp_sdk::domain::{Operator, Value};
/// let domain = full_name_domain(Operator::ILike, &Value::from("Role / Admin")).unwrap();
/// assert!(!domain.is_true() && !domain.is_false());
/// ```
///
/// # Errors
///
/// `Validation` for an operator that is not a plain comparison (`in`, `=`, `like` and `ilike`
/// families and their negations) or an operand of the wrong shape.
pub fn full_name_domain(operator: Operator, operand: &DomainValue) -> Result<Domain> {
    if operator.is_negative() {
        return Ok(!full_name_domain(operator.positive(), operand)?);
    }
    let operator = match operator {
        Operator::Eq => Operator::In,
        other => other,
    };
    let is_set = matches!(operator, Operator::In);
    let operands: Vec<DomainValue> = match (operator, operand) {
        (_, DomainValue::Str(text)) if !is_set => vec![DomainValue::Str(text.clone())],
        (Operator::In, DomainValue::List(items)) => items.clone(),
        (Operator::In, DomainValue::Set(items)) => items.iter().cloned().collect(),
        (Operator::In, single) => vec![single.clone()],
        (_, other) => {
            return Err(Error::validation(format!(
                "full_name {operator} needs a text, not {other:?}"
            )));
        }
    };
    let wrap = |value: DomainValue| {
        if is_set {
            DomainValue::List(vec![value])
        } else {
            value
        }
    };
    let name = Domain::field("name").map_err(Error::from)?;
    let mut any_of = vec![name.op(operator, operand.clone())];
    for group in operands {
        let text = match &group {
            DomainValue::Str(text) if !text.is_empty() => text.clone(),
            // an empty or non-text member (`False`) matches nothing more than the name does
            _ => continue,
        };
        any_of.push(name.op(operator, wrap(group.clone())));
        let (privilege, group_name) = match text.split_once('/') {
            Some((privilege, group)) => (privilege.trim(), Some(group.trim())),
            None => (text.as_str(), None),
        };
        if privilege.is_empty() {
            continue;
        }
        let privilege_filter = name.op(operator, wrap(DomainValue::Str(privilege.to_owned())));
        let mut domain = Domain::field("privilege_id")
            .map_err(Error::from)?
            .op(Operator::AnyBypass, DomainValue::Domain(privilege_filter));
        if let Some(group_name) = group_name.filter(|g| !g.is_empty()) {
            domain = domain & name.op(operator, wrap(DomainValue::Str(group_name.to_owned())));
        }
        any_of.push(domain);
    }
    Ok(Domain::or(any_of))
}

/// Drop the cached closure fields: the stored dependencies cannot see a change deeper in the
/// graph than the records written.
async fn invalidate_closures(tx: &mut Transaction) -> Result<()> {
    tx.invalidate_model(
        "res.groups",
        Some(&["all_implied_ids", "all_implied_by_ids", "all_user_ids"]),
    )
    .await?;
    tx.invalidate_model("res.users", Some(&["all_group_ids", "role"]))
        .await
}

/// Recompute `share` for every user when the set of internal groups is not the one before.
async fn recompute_share_if_internal_changed(
    tx: &mut Transaction,
    before: &BTreeSet<i64>,
) -> Result<()> {
    if internal_group_ids(tx).await? != *before {
        recompute_user_share(tx, None).await?;
    }
    Ok(())
}

async fn privilege_orders(
    tx: &mut Transaction,
    graph: &Graph,
) -> Result<(BTreeMap<i64, Vec<i64>>, HashMap<i64, Option<i64>>)> {
    let rows = tx
        .query(
            "SELECT id, privilege_id, sequence FROM res_groups ORDER BY id",
            &[],
        )
        .await?;
    let mut privilege_of: HashMap<i64, Option<i64>> = HashMap::with_capacity(rows.len());
    let mut members: BTreeMap<i64, Vec<(i64, i32)>> = BTreeMap::new();
    for row in &rows {
        let id: i64 = row
            .try_get(0)
            .map_err(|e| Error::db(format!("unexpected value in res_groups.id: {e}")))?;
        let privilege: Option<i64> = row
            .try_get(1)
            .map_err(|e| Error::db(format!("unexpected value in res_groups.privilege_id: {e}")))?;
        let sequence: Option<i32> = row
            .try_get(2)
            .map_err(|e| Error::db(format!("unexpected value in res_groups.sequence: {e}")))?;
        privilege_of.insert(id, privilege);
        if let Some(p) = privilege {
            members
                .entry(p)
                .or_default()
                .push((id, sequence.unwrap_or(0)));
        }
    }
    let mut orders = BTreeMap::new();
    for (privilege, groups) in members {
        let ids: BTreeSet<i64> = groups.iter().map(|(g, _)| *g).collect();
        let mut keyed: Vec<(usize, i32, i64)> = groups
            .iter()
            .map(|(g, sequence)| {
                let implied = graph.up([*g]).intersection(&ids).count();
                (implied, *sequence, *g)
            })
            .collect();
        keyed.sort_unstable();
        orders.insert(privilege, keyed.into_iter().map(|(_, _, g)| g).collect());
    }
    Ok((orders, privilege_of))
}

#[methods(ResGroups)]
impl ResGroups {
    /// The group itself with all its implied groups (`g.ids + supersets`).
    #[compute(depends = ["implied_ids"])]
    async fn _compute_all_implied_ids(
        this: &RecordSet<ResGroups>,
        tx: &mut Transaction,
        out: &mut Assign<'_, ResGroups>,
    ) -> Result<()> {
        let graph = Graph::load(tx).await?;
        for (i, id) in this.as_records().rec_ids().iter().enumerate() {
            let closure = row_id(*id).map(|g| graph.up([g])).unwrap_or_default();
            out.set(i, ResGroups::ALL_IMPLIED_IDS, rec_ids(closure)?)?;
        }
        Ok(())
    }

    /// The group itself with every group that implies it.
    #[compute(depends = ["implied_by_ids"])]
    async fn _compute_all_implied_by_ids(
        this: &RecordSet<ResGroups>,
        tx: &mut Transaction,
        out: &mut Assign<'_, ResGroups>,
    ) -> Result<()> {
        let graph = Graph::load(tx).await?;
        for (i, id) in this.as_records().rec_ids().iter().enumerate() {
            // as `_compute_all_implied_ids`: an unsaved group has no closure until it is saved
            let closure = row_id(*id).map(|g| graph.down([g])).unwrap_or_default();
            out.set(i, ResGroups::ALL_IMPLIED_BY_IDS, rec_ids(closure)?)?;
        }
        Ok(())
    }

    /// For a user type group, the other user type groups; none for any other group.
    #[compute(depends = [])]
    async fn _compute_disjoint_ids(
        this: &RecordSet<ResGroups>,
        tx: &mut Transaction,
        out: &mut Assign<'_, ResGroups>,
    ) -> Result<()> {
        let types = user_type_groups(tx).await?;
        for (i, id) in this.as_records().rec_ids().iter().enumerate() {
            let disjoint = match row_id(*id) {
                Some(g) if types.contains(&g) => {
                    rec_ids(types.iter().copied().filter(|t| *t != g))?
                }
                _ => Vec::new(),
            };
            out.set(i, ResGroups::DISJOINT_IDS, disjoint)?;
        }
        Ok(())
    }

    /// Users of the group and of every group implying it.
    #[compute(depends = ["user_ids", "implied_by_ids"])]
    async fn _compute_all_user_ids(
        this: &RecordSet<ResGroups>,
        tx: &mut Transaction,
        out: &mut Assign<'_, ResGroups>,
    ) -> Result<()> {
        let graph = Graph::load(tx).await?;
        let per_group: Vec<BTreeSet<i64>> = this
            .as_records()
            .rec_ids()
            .iter()
            .map(|id| row_id(*id).map(|g| graph.down([g])).unwrap_or_default())
            .collect();
        let wanted: Vec<i64> = per_group
            .iter()
            .flatten()
            .copied()
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        let rows = tx
            .query(
                "SELECT gid, uid FROM res_groups_users_rel WHERE gid = ANY($1) ORDER BY uid",
                &[&wanted],
            )
            .await?;
        let mut members: HashMap<i64, Vec<i64>> = HashMap::new();
        for row in &rows {
            let gid: i64 = row
                .try_get(0)
                .map_err(|e| Error::db(format!("unexpected value in res_groups_users_rel: {e}")))?;
            let uid: i64 = row
                .try_get(1)
                .map_err(|e| Error::db(format!("unexpected value in res_groups_users_rel: {e}")))?;
            members.entry(gid).or_default().push(uid);
        }
        for (i, groups) in per_group.iter().enumerate() {
            let users: BTreeSet<i64> = groups
                .iter()
                .filter_map(|g| members.get(g))
                .flatten()
                .copied()
                .collect();
            out.set(i, ResGroups::ALL_USER_IDS, rec_ids(users)?)?;
        }
        Ok(())
    }

    /// `Privilege / Group`; the group's name alone with `short_display_name` in the context or
    /// without a privilege.
    #[compute(depends = ["privilege_id.name", "name"])]
    fn _compute_full_name(batch: &mut Batch<'_, ResGroups>) -> Result<()> {
        let short = batch
            .env()
            .context()
            .get("short_display_name")
            .is_some_and(|v| v.is_truthy());
        for i in batch.indices() {
            let name = batch.get(i, ResGroups::NAME)?;
            let privilege = batch.get(i, ResGroups::PRIVILEGE_ID)?;
            let full: Option<Arc<str>> = match (privilege, short) {
                (Some(p), false) => {
                    let privilege_name = batch.value_of(ResGroupsPrivilege::NAME, p)?;
                    Some(Arc::from(format!(
                        "{} / {}",
                        privilege_name.as_deref().unwrap_or("False"),
                        name.as_deref().unwrap_or("False")
                    )))
                }
                _ => name,
            };
            batch.set(i, ResGroups::FULL_NAME, full)?;
        }
        Ok(())
    }

    /// The user type groups (internal, portal, public) that exist.
    #[method]
    async fn _get_user_type_groups(
        this: &RecordSet<ResGroups>,
        tx: &mut Transaction,
    ) -> Result<Records> {
        let ids = user_type_groups(tx).await?;
        this.as_records().browse(ids)
    }

    #[method]
    async fn _get_light_group_xmlids(
        _this: &RecordSet<ResGroups>,
        _tx: &mut Transaction,
    ) -> Result<Vec<String>> {
        Ok([
            "base.group_user",
            "base.group_portal",
            "base.group_public",
            "base.group_everyone",
            "base.group_no_one",
            "base.group_multi_currency",
            "base.default_user_group",
        ]
        .map(str::to_owned)
        .to_vec())
    }

    /// Whether every group of the set is a light group. An empty set answers true (nothing in
    /// it is heavy; Python's `all([])`); a new record with no origin is an error.
    #[method]
    async fn _is_light_groups(this: &RecordSet<ResGroups>, tx: &mut Transaction) -> Result<bool> {
        let light = light_group_ids(this, tx).await?;
        Ok(row_ids(this.as_records())?
            .iter()
            .all(|g| light.contains(g)))
    }

    #[method]
    async fn _reduce_to_light_groups(
        this: &RecordSet<ResGroups>,
        tx: &mut Transaction,
    ) -> Result<Records> {
        let light = light_group_ids(this, tx).await?;
        let graph = Graph::load(tx).await?;
        let (orders, privilege_of) = privilege_orders(tx, &graph).await?;
        let mut result: Vec<i64> = Vec::new();
        let ids = row_ids(this.as_records())?;
        for g in ids.iter().filter(|g| light.contains(g)) {
            if !result.contains(g) {
                result.push(*g);
            }
        }
        for g in ids.iter().filter(|g| !light.contains(g)) {
            let Some(Some(privilege)) = privilege_of.get(g) else {
                continue;
            };
            let order = orders.get(privilege).map(Vec::as_slice).unwrap_or_default();
            let implied = graph.up([*g]);
            let best = order
                .iter()
                .find(|candidate| implied.contains(candidate) && light.contains(candidate));
            if let Some(best) = best
                && !result.contains(best)
            {
                result.push(*best);
            }
        }
        this.as_records().browse(result)
    }

    /// Whether some group of the set is in a loop of `field` (`implied_ids` or
    /// `implied_by_ids`).
    #[method]
    async fn _has_cycle(
        this: &RecordSet<ResGroups>,
        tx: &mut Transaction,
        field: String,
    ) -> Result<bool> {
        if field != "implied_ids" && field != "implied_by_ids" {
            return Err(Error::programming(format!(
                "res.groups._has_cycle follows implied_ids or implied_by_ids, not '{field}'"
            )));
        }
        let graph = Graph::load(tx).await?;
        let follow = |g: i64| -> BTreeSet<i64> {
            if field == "implied_ids" {
                graph.up(graph.direct(g).iter().copied())
            } else {
                graph.down(graph.direct_by(g).iter().copied())
            }
        };
        Ok(row_ids(this.as_records())?
            .into_iter()
            .any(|g| follow(g).contains(&g)))
    }

    #[method]
    async fn _apply_group(
        this: &RecordSet<ResGroups>,
        tx: &mut Transaction,
        implied_group: i64,
    ) -> Result<()> {
        let graph = Graph::load(tx).await?;
        let target = rec(implied_group)?;
        let groups: Vec<i64> = row_ids(this.as_records())?
            .into_iter()
            .filter(|g| !graph.up([*g]).contains(&implied_group))
            .collect();
        if groups.is_empty() {
            return Ok(());
        }
        let vals = RawVals::new().set_commands("implied_ids", [Command::link(target)]);
        this.as_records().browse(groups)?.write(tx, vals).await
    }

    /// Members that held the group only through those groups
    /// lose it with the implication.
    #[method]
    async fn _remove_group(
        this: &RecordSet<ResGroups>,
        tx: &mut Transaction,
        implied_group: i64,
    ) -> Result<()> {
        let graph = Graph::load(tx).await?;
        let target = rec(implied_group)?;
        let groups: Vec<i64> = graph
            .up(row_ids(this.as_records())?)
            .into_iter()
            .filter(|g| graph.direct(*g).contains(&implied_group))
            .collect();
        if groups.is_empty() {
            return Ok(());
        }
        let vals = RawVals::new().set_commands("implied_ids", [Command::unlink(target)]);
        this.as_records().browse(groups)?.write(tx, vals).await
    }

    #[method]
    async fn _apply_group_regular(this: &RecordSet<ResGroups>, tx: &mut Transaction) -> Result<()> {
        let (found, _) = lookup_groups(tx, &[GROUP_USER_REGULAR], &[]).await?;
        let Some(regular) = found[0] else {
            return Ok(());
        };
        let light = light_group_ids(this, tx).await?;
        let default_group = group_by_xmlid(tx, DEFAULT_USER_GROUP).await?;
        let graph = Graph::load(tx).await?;
        let (orders, privilege_of) = privilege_orders(tx, &graph).await?;
        let regular_groups: BTreeSet<i64> = privilege_of
            .keys()
            .copied()
            .filter(|g| !light.contains(g) && *g != regular && Some(*g) != default_group)
            .collect();
        // lowest rights first inside a privilege; groups without privilege sort first
        let position = |g: i64| -> usize {
            privilege_of
                .get(&g)
                .copied()
                .flatten()
                .and_then(|p| orders.get(&p))
                .and_then(|order| order.iter().position(|x| *x == g))
                .unwrap_or(0)
        };
        let mut sorted: Vec<i64> = regular_groups.iter().copied().collect();
        sorted.sort_by_key(|g| position(*g));
        let mut seen_privileges: BTreeSet<i64> = BTreeSet::new();
        let mut chosen: BTreeSet<i64> = BTreeSet::new();
        for g in sorted {
            let privilege = privilege_of.get(&g).copied().flatten();
            let implies_regular = graph.direct(g).iter().any(|h| regular_groups.contains(h));
            match privilege {
                None => {
                    chosen.insert(g);
                }
                Some(_) if !implies_regular => {
                    chosen.insert(g);
                }
                Some(p) => {
                    if seen_privileges.insert(p) {
                        chosen.insert(g);
                    }
                }
            }
        }
        let current: BTreeSet<i64> = graph.direct_by(regular).iter().copied().collect();
        if chosen == current {
            return Ok(());
        }
        let regular_group = this
            .as_records()
            .with_context([(APPLY_REGULAR_RUNNING, Value::Bool(true))])
            .browse([regular])?;
        let vals = RawVals::new().set_commands("implied_by_ids", [Command::set(rec_ids(chosen)?)]);
        regular_group.write(tx, vals).await
    }

    /// A user type group may not (transitively) imply another user type group's members: no
    /// group may imply two disjoint groups, and no active user may hold two.
    #[constrains("implied_ids", "implied_by_ids")]
    async fn _check_disjoint_groups(
        this: &RecordSet<ResGroups>,
        tx: &mut Transaction,
    ) -> Result<()> {
        let types = user_type_groups(tx).await?;
        if types.len() < 2 {
            return Ok(());
        }
        let graph = Graph::load(tx).await?;
        for g in row_ids(this.as_records())? {
            let implied = graph.up([g]);
            // the groups above any group that implies `g`: their user types exclude the others
            let reachable = graph.up(graph.down([g]));
            let held: Vec<i64> = types
                .iter()
                .copied()
                .filter(|t| implied.contains(t))
                .collect();
            let excluded_by: Vec<i64> = types
                .iter()
                .copied()
                .filter(|t| reachable.contains(t))
                .collect();
            let clash = held
                .iter()
                .any(|t| excluded_by.iter().any(|other| other != t));
            if clash {
                return Err(Error::validation(
                    "This makes a group imply two disjoint groups.",
                ));
            }
        }
        check_users_disjoint(this, tx, &types, &graph).await
    }

    /// No active user may hold two user type groups through its membership in these groups
    /// .
    #[constrains("user_ids")]
    async fn _check_user_disjoint_groups(
        this: &RecordSet<ResGroups>,
        tx: &mut Transaction,
    ) -> Result<()> {
        let types = user_type_groups(tx).await?;
        if types.len() < 2 {
            return Ok(());
        }
        let graph = Graph::load(tx).await?;
        check_users_disjoint(this, tx, &types, &graph).await
    }

    #[constrains("user_ids")]
    async fn _check_at_least_one_administrator(
        _this: &RecordSet<ResGroups>,
        tx: &mut Transaction,
    ) -> Result<()> {
        ensure_an_administrator(tx).await
    }

    #[method(override)]
    async fn create(
        this: &RecordSet<ResGroups>,
        tx: &mut Transaction,
        vals_list: Vec<RawVals>,
        sup: Super,
    ) -> Result<Records> {
        let graph_change = vals_list
            .iter()
            .any(|v| touches(v, "implied_ids") || touches(v, "implied_by_ids"));
        let vals_touch_members = vals_list.iter().any(|v| touches(v, "user_ids"));
        // the type group probe first (groups, then users: one lock order), then the users
        let view = if graph_change {
            let view = GraphView::load(tx).await?;
            lock_type_groups(tx, &view.types).await?;
            Some(view)
        } else {
            None
        };
        // members added from this side conflict with concurrent changes of their memberships
        let mut members: Vec<i64> = vals_list
            .iter()
            .map(named_members)
            .collect::<Result<Vec<_>>>()?
            .into_iter()
            .flatten()
            .collect();
        // and members of the groups a new group is made to be implied by or to imply
        if let Some(view) = &view {
            let changes: Vec<ImplicationChange> = vals_list
                .iter()
                .map(|v| ImplicationChange::new(Vec::new(), v))
                .collect();
            members.extend(members_under_changed_implications(tx, view, &changes).await?);
        }
        lock_users_for_type_check(tx, &members).await?;
        let before = view.map(|v| v.internal);
        let created: Records = sup.call(tx, this.as_records(), (vals_list,)).await?;
        this._apply_group_regular(tx).await?;
        if graph_change || vals_touch_members {
            invalidate_closures(tx).await?;
        }
        if let Some(before) = before {
            recompute_share_if_internal_changed(tx, &before).await?;
        }
        Ok(created)
    }

    #[method(override)]
    async fn write(
        this: &RecordSet<ResGroups>,
        tx: &mut Transaction,
        vals: RawVals,
        sup: Super,
    ) -> Result<()> {
        if let Some(Cell::Str(name)) = vals.get("name")
            && name.starts_with('-')
        {
            return Err(Error::user(
                "The name of the group can not start with \"-\"",
            ));
        }
        let graph_change = touches(&vals, "implied_ids") || touches(&vals, "implied_by_ids");
        let running = this
            .as_records()
            .env()
            .context()
            .get(APPLY_REGULAR_RUNNING)
            .is_some_and(|v| v.is_truthy());
        let members = touches(&vals, "user_ids");
        let rebalance =
            graph_change || touches(&vals, "privilege_id") || touches(&vals, "sequence");
        // lock order: the type group rows, then the users in one call (named members and the
        // members under a changed implication, sorted there). The nested `_apply_group_regular`
        // write takes the same path: `running` is a context key a client can send, so no lock
        // depends on it.
        let mut locked = named_members(&vals)?;
        let view = if graph_change {
            let view = GraphView::load(tx).await?;
            lock_type_groups(tx, &view.types).await?;
            let change = ImplicationChange::new(row_ids(this.as_records())?, &vals);
            locked.extend(members_under_changed_implications(tx, &view, &[change]).await?);
            Some(view)
        } else {
            None
        };
        lock_users_for_type_check(tx, &locked).await?;
        let before = view.map(|v| v.internal);
        sup.call(tx, this.as_records(), (vals,)).await?;
        if !running && rebalance {
            this._apply_group_regular(tx).await?;
        }
        if graph_change || members {
            invalidate_closures(tx).await?;
        }
        if let Some(before) = before {
            recompute_share_if_internal_changed(tx, &before).await?;
        }
        Ok(())
    }

    #[method(override)]
    async fn unlink(this: &RecordSet<ResGroups>, tx: &mut Transaction, sup: Super) -> Result<()> {
        if let Some(system) = group_by_xmlid(tx, GROUP_SYSTEM).await?
            && row_ids(this.as_records())?.contains(&system)
        {
            return Err(Error::validation(
                "The administrator group (base.group_system) cannot be deleted: you must have \
                 at least an administrator user.",
            ));
        }
        let before = internal_group_ids(tx).await?;
        sup.call(tx, this.as_records(), ()).await?;
        this._apply_group_regular(tx).await?;
        invalidate_closures(tx).await?;
        recompute_share_if_internal_changed(tx, &before).await
    }
}

/// The ids of the light groups that exist, from the overridable `_get_light_group_xmlids`.
async fn light_group_ids(
    this: &RecordSet<ResGroups>,
    tx: &mut Transaction,
) -> Result<BTreeSet<i64>> {
    let xmlids = this._get_light_group_xmlids(tx).await?;
    let refs: Vec<&str> = xmlids.iter().map(String::as_str).collect();
    Ok(groups_by_xmlid(tx, &refs)
        .await?
        .into_iter()
        .flatten()
        .collect())
}

/// One statement for
/// all the groups of the recordset: the (group, members) and (group, others) pairs travel as two
/// pairs of parallel arrays.
async fn check_users_disjoint(
    this: &RecordSet<ResGroups>,
    tx: &mut Transaction,
    types: &[i64],
    graph: &Graph,
) -> Result<()> {
    let mut member_keys: Vec<i32> = Vec::new();
    let mut member_groups: Vec<i64> = Vec::new();
    let mut other_keys: Vec<i32> = Vec::new();
    let mut other_groups: Vec<i64> = Vec::new();
    for (key, g) in row_ids(this.as_records())?.into_iter().enumerate() {
        let key = i32::try_from(key)
            .map_err(|_| Error::programming("too many groups in one disjointness check"))?;
        let up = graph.up([g]);
        let own_types: Vec<i64> = types.iter().copied().filter(|t| up.contains(t)).collect();
        if own_types.is_empty() {
            continue;
        }
        for member in graph.down([g]) {
            member_keys.push(key);
            member_groups.push(member);
        }
        for other in graph.down(types.iter().copied().filter(|t| !own_types.contains(t))) {
            other_keys.push(key);
            other_groups.push(other);
        }
    }
    if member_keys.is_empty() {
        return Ok(());
    }
    // a pending `active` write must reach the table before the rows are read
    let users = tx
        .env()
        .registry()
        .model("res.users")
        .map(|m| m.id)
        .ok_or_else(|| Error::programming("the registry has no res.users model"))?;
    tx.flush_model(users).await?;
    let rows = tx
        .query(
            "WITH m(k, gid) AS (SELECT * FROM unnest($1::int4[], $2::int8[])), \
                  o(k, gid) AS (SELECT * FROM unnest($3::int4[], $4::int8[])) \
             SELECT u.id, u.name \
               FROM m \
               JOIN res_groups_users_rel r1 ON r1.gid = m.gid \
               JOIN o ON o.k = m.k \
               JOIN res_groups_users_rel r2 ON r2.gid = o.gid AND r2.uid = r1.uid \
               JOIN res_users u ON u.id = r1.uid \
              WHERE u.active \
              ORDER BY u.id LIMIT 1",
            &[&member_keys, &member_groups, &other_keys, &other_groups],
        )
        .await?;
    if let Some(row) = rows.first() {
        let uid: i64 = row
            .try_get(0)
            .map_err(|e| Error::db(format!("unexpected value in res_users.id: {e}")))?;
        let name: Option<String> = row
            .try_get(1)
            .map_err(|e| Error::db(format!("unexpected value in res_users.name: {e}")))?;
        return Err(exclusive_groups_error(tx, uid, name.as_deref()).await?);
    }
    Ok(())
}

/// "User 'x' cannot be at the same time in exclusive groups 'A', 'B'." for `uid` (its user type
/// groups through all its groups).
pub(crate) async fn exclusive_groups_error(
    tx: &mut Transaction,
    uid: i64,
    name: Option<&str>,
) -> Result<Error> {
    let types = user_type_groups(tx).await?;
    let graph = Graph::load(tx).await?;
    let direct = direct_groups(tx, &[uid]).await?;
    let all = graph.up(direct.get(&uid).cloned().unwrap_or_default());
    let held: Vec<i64> = types.into_iter().filter(|t| all.contains(t)).collect();
    let names = display_names(tx, &held).await?;
    let listed = names
        .iter()
        .map(|(_, n)| py_repr(n))
        .collect::<Vec<_>>()
        .join(", ");
    let user = name.map_or_else(|| "False".to_owned(), py_repr);
    Ok(Error::validation(format!(
        "User {user} cannot be at the same time in exclusive groups {listed}."
    )))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]

    use std::sync::Arc;

    use erp_sdk::ErrorKind;

    use super::*;

    fn real(id: i64) -> RecId {
        RecId::from_real(id).unwrap()
    }

    /// Commands and a plain list name the users to lock; a record without a row (a new user)
    /// has nothing to lock; removals name nobody.
    #[test]
    fn named_members_reads_commands_and_plain_lists() {
        let none = RawVals::new();
        assert_eq!(named_members(&none).unwrap(), Vec::<i64>::new());
        let vals = RawVals::new().set_commands(
            "user_ids",
            [
                Command::link(real(1)),
                Command::set([real(2), RecId::new_unique(1).unwrap()]),
                Command::unlink(real(3)),
                Command::clear(),
            ],
        );
        assert_eq!(named_members(&vals).unwrap(), vec![1, 2]);
        let ids: Arc<[RecId]> = Arc::from(vec![real(4), real(5)]);
        let vals = RawVals::new().set_cell("user_ids", Cell::Ids(ids));
        assert_eq!(named_members(&vals).unwrap(), vec![4, 5]);
    }

    /// A plain value that is not a list of records cannot be read: an error, not "lock nobody".
    #[test]
    fn named_members_refuses_an_unreadable_plain_value() {
        let vals = RawVals::new().set_cell("user_ids", Cell::Int(3));
        let err = named_members(&vals).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::Programming);
        assert_eq!(
            err.to_string(),
            "res.groups user_ids was given as Int(3); it must be a list of records or x2many \
             commands"
        );
    }
}
