//! `res.users` columns the principal resolver reads (design note §2.3):
//! direct group membership (`res_groups_users_rel`), the companies (`company_id`, and
//! `company_ids` in `res_company_users_rel`) and the user type flag `share`. The model
//! itself is the core's (`erp-registry` `USERS_DECL`, `name`, `login`, `active`); it moves here
//! with.
//!
//! Membership is not part of the compiled policy: the resolver reads it at every transaction
//! start, and a change inside a transaction re-resolves the bound subjects (design note §3.4).
//! The tables it reads carry statement triggers that bump the `membership` signal generation
//! (nothing is rebuilt from it): a read replica that has not replayed a membership change is then
//! detected by the routing lag guard.
//!
//! The rules:
//!
//! - **Groups.** `group_ids` defaults to `base.group_user` plus the groups `base.default_user_group`
//!   implies; `all_group_ids` is their transitive closure; no user may hold two of
//!   the user type groups (internal, portal, public); the last active member of
//!   `base.group_system` cannot be removed, archived or deleted, from the user side, the group
//!   side or by deleting the group ([`ensure_an_administrator`]).
//! - **Roles.** `role` reads `group_system`, `light_user`, `regular_user` or false from the
//!   groups ([`Role`]); [`role_domain`] / [`users_with_role`] are its search; the
//!   `_onchange_role` onchange moves the groups for a form.
//! - **`share`.** True unless the user is a direct member of an internal group (`base.group_user`
//!   or a group implying it, so true for everyone while the database has no `base.group_user`);
//!   recomputed with the user's groups, and set-based for every user when the group graph
//!   changes ([`crate::group_graph::recompute_user_share`]). The principal
//!   resolver does not read it; the company restriction of `res.users` does.
//! - **Self-service.** A user who is neither in `base.group_system` nor superuser may
//!   write on their own record only the fields declared `user_writeable`, enforced
//!   by a `#[field_access]` override of the model's field mask.
//! - **Default company.** `company_id` is required by rule,
//!   not by a NOT NULL column (the bootstrap inserts the superuser before any company exists):
//!   `default_get` defaults it, and `company_ids`, to the environment's
//!   company (a `System` principal that names none takes the database's only active company);
//!   a create that gives `company_id` alone allowed it; an active user's default company is one
//!   of its `company_ids` ([`check_user_companies`]; a database without companies, and inactive
//!   users, are exempt); the field is `user_writeable` (the check keeps a self-write inside the
//!   allowed companies; `company_ids` stays admin-only) and `ondelete = "restrict"`. The base
//!   1.10 migration ([`crate::migrations::assign_user_companies`]) fills the existing rows or
//!   refuses the upgrade.
//! - **Commands.** `res.users` does not allow sudo commands (`_allow_sudo_commands = false`):
//!   x2many commands that target it run as the transaction's original user.
//!
//! Missing groups: the rules do not switch themselves off.
//!
//! The bootstrap partner link lives in `res_users_partner`; `res.partner.partner_share` follows
//! the users' `share` and `active`, delegated partner fields remain in the
//! broader partner slice.

use std::borrow::Cow;
use std::collections::BTreeSet;

use erp_sdk::access::{ElevationKind, PrincipalKind};
use erp_sdk::domain::{Operator, Value as DomainValue};
use erp_sdk::prelude::*;
use erp_sdk::{Command, Domain, Env, RecId, ValueMap};

use crate::group_graph::{
    DEFAULT_USER_GROUP, GROUP_SYSTEM, GROUP_USER, GROUP_USER_REGULAR, Graph, direct_groups,
    groups_by_xmlid, internal_group_ids, lock_users_for_type_check, lookup_groups, required_group,
    required_groups, row_id, user_type_groups,
};
use crate::res_groups::exclusive_groups_error;

/// The access-related columns of `res.users`.
#[extend(
    ResUsers,
    company_access = direct(field = "company_ids", branch = "exact", empty = "deny", escape = "share"),
    allow_sudo_commands = false,
    field_parameters = ["user_writeable"],
    signal_triggers = [
        ("_membership_bump", "membership", update_of = ["active", "share", "company_id"]),
        ("_membership_bump_groups", "membership", relation = "group_ids"),
        ("_membership_bump_companies", "membership", relation = "company_ids"),
    ],
)]
pub struct ResUsersAccess {
    /// Name.
    #[field(char, user_writeable)]
    pub name: Option<Arc<str>>,
    /// Email address.
    #[field(char, user_writeable)]
    pub email: Option<Arc<str>>,
    /// A zone the time zone database does not
    /// know is refused on write; the dialog offers the zones through the
    /// `web.preference_choice` widget (a dynamic selection has no human view renderer yet).
    #[field(char, user_writeable)]
    pub tz: Option<Arc<str>>,
    /// Language code: refused on write unless it is an installed language; the
    /// dialog offers the installed languages through the `web.preference_choice` widget.
    #[field(char, user_writeable)]
    pub lang: Option<Arc<str>>,
    /// Phone number, shown in the header block of My Preferences.
    /// `user_writeable`: the user's own
    /// contact detail, the same class as `email`; nothing reads it for access or routing.
    #[field(char, user_writeable)]
    pub phone: Option<Arc<str>>,
    /// Colour scheme the user wants the screen drawn in: `system`, `light` or `dark`.
    /// Default `system`, applied on create. A plain column of the user, not of
    /// `res.users.settings` (keeps that model to one key). Not required at the column: a
    /// required column gets `NOT NULL` and the database default exists only for booleans,
    /// which would refuse every raw `res.users` insert that omits it. So
    /// `_check_color_scheme` refuses an empty value on create and write instead. Schema sync adds
    /// the column with the default, so existing rows read `system` after an upgrade (read from
    /// `schema.rs`, not run); only a row inserted by raw SQL after the column exists holds no value,
    /// and no value means `system`. `user_writeable`: a display choice that affects only the
    /// user's own screen. The client applies it in a later task; until then it is only stored.
    #[field(
        selection,
        selection = [("system", "System"), ("light", "Light"), ("dark", "Dark")],
        default = "system",
        string = "Color Scheme",
        user_writeable
    )]
    pub color_scheme: Option<Arc<str>>,
    /// Groups explicitly assigned to the user.
    #[field(
        many2many,
        comodel = "res.groups",
        relation = "res_groups_users_rel",
        column1 = "uid",
        column2 = "gid"
    )]
    pub group_ids: Vec<RecId>,
    /// The user's groups and every group they imply.
    #[field(
        many2many,
        comodel = "res.groups",
        compute = "_compute_all_group_ids",
        compute_sudo,
        string = "Groups and implied groups"
    )]
    pub all_group_ids: Vec<RecId>,
    /// An external (portal or public) user: data sharing only. The escape of the company
    /// restriction: a user with `share = false` is admitted whatever its
    /// companies. Required with the static default `true`, so a row that was never computed
    /// takes the company test (fail closed); the stored compute replaces the default after the
    /// insert, since a computed field without inverse is not protected by a given value. The
    /// default is also the column's database default (`db_default`), so a row written
    /// outside the ORM without `share` is a share user too.
    #[field(
        boolean,
        compute = "_compute_share",
        store,
        compute_sudo,
        required,
        default = true,
        db_default,
        string = "Share User"
    )]
    pub share: bool,
    /// What the user may do, from the groups: `light_user`, `regular_user` or `group_system`.
    #[field(
        selection,
        selection = [
            ("light_user", "Light"),
            ("regular_user", "User"),
            ("group_system", "Administrator")
        ],
        compute = "_compute_role",
        string = "Role"
    )]
    pub role: Option<Arc<str>>,
    /// The user's default company. Required for an active user by rule, not by a NOT NULL column
    /// : it defaults to the environment's company, must be one of `company_ids` while
    /// the user is active ([`check_user_companies`]) and cannot be deleted while a user has it.
    #[field(
        many2one,
        comodel = "res.company",
        string = "Company",
        ondelete = "restrict",
        user_writeable
    )]
    pub company_id: Option<RecId>,
    /// The companies the user may work in. Administrators only: a self-write could widen access.
    /// Defaults to the environment's company; a create that gives `company_id` and no
    /// `company_ids` allows that company.
    #[field(
        many2many,
        comodel = "res.company",
        relation = "res_company_users_rel",
        column1 = "user_id",
        column2 = "cid",
        string = "Companies"
    )]
    pub company_ids: Vec<RecId>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Role {
    /// An internal user whose groups are all light groups.
    Light,
    /// An internal user with at least one regular (non-light) group, and no administration.
    Regular,
    /// A member of `base.group_system`, directly or through implied groups.
    System,
}

impl Role {
    /// The selection key (`light_user`, `regular_user`, `group_system`).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Light => "light_user",
            Self::Regular => "regular_user",
            Self::System => "group_system",
        }
    }
}

/// The groups of a recordset's users as given (database rows and pending values alike).
async fn direct_of(users: &Records, tx: &mut Transaction) -> Result<Vec<Vec<i64>>> {
    let cells = users.cells(tx, "group_ids").await?;
    cells
        .iter()
        .map(|cell| match cell {
            // a new record without groups
            Cell::Null => Ok(Vec::new()),
            Cell::Ids(ids) => Ok(ids.iter().filter_map(|id| row_id(*id)).collect()),
            other => Err(Error::programming(format!(
                "res.users.group_ids holds {other:?}, expected a list of group ids"
            ))),
        })
        .collect()
}

/// No
/// role at all selects nobody.
///
/// A user holds a group through the closure of its own, so "has a group in S" is
/// `group_ids in down(S)`, computed here from the group graph with four statements whatever the
/// number of users. Archived users are part of the domain's meaning; the search applies
/// `active_test` as for any other domain.
///
/// # Errors
///
/// `Programming` when `base.group_system` or `base.group_user` do not exist (unless the base
/// module is installing, when nobody has a role yet), `Database` for a failed statement.
pub async fn role_domain(tx: &mut Transaction, roles: &[Option<Role>]) -> Result<Domain> {
    if roles.is_empty() {
        return Ok(Domain::FALSE);
    }
    let found = required_groups(tx, &[GROUP_SYSTEM, GROUP_USER]).await?;
    let env = tx.env();
    let light = light_ids(tx, &env).await?;
    let graph = Graph::load(tx).await?;
    let rows = tx.query("SELECT id FROM res_groups", &[]).await?;
    let mut regular: Vec<i64> = Vec::with_capacity(rows.len());
    for row in &rows {
        let id: i64 = row
            .try_get(0)
            .map_err(|e| Error::db(format!("unexpected value in res_groups.id: {e}")))?;
        if !light.contains(&id) {
            regular.push(id);
        }
    }
    // a missing group (while installing) holds nobody
    let held_by = |group: Option<i64>| graph.down(group);
    let in_groups = |groups: BTreeSet<i64>| -> Result<Domain> {
        Ok(Domain::field("group_ids").map_err(Error::from)?.op(
            Operator::In,
            DomainValue::List(groups.into_iter().map(DomainValue::Int).collect()),
        ))
    };
    let is_system = in_groups(held_by(found[0]))?;
    let is_user = in_groups(held_by(found[1]))?;
    let is_regular = in_groups(graph.down(regular))?;
    let mut any_of = Vec::with_capacity(roles.len());
    for role in roles {
        any_of.push(match role {
            Some(Role::Light) => is_user.clone() & !is_regular.clone(),
            Some(Role::Regular) => is_user.clone() & is_regular.clone() & !is_system.clone(),
            Some(Role::System) => is_system.clone(),
            None => !is_user.clone(),
        });
    }
    Ok(Domain::or(any_of))
}

/// The users of `env` whose role is one of `roles` (`None`: users with no role): a search of
/// [`role_domain`] in the caller's environment, so record rules, `active_test` and the
/// caller's company apply as for any search.
///
/// ```
/// # // the layout of a search's future nests deeper than rustc's default query depth allows in
/// # // a crate of its own (PERF.STREAM-WINDOW round 5: the merge of main into the stream slice)
/// # #![recursion_limit = "256"]
/// # async fn demo(tx: &mut erp_sdk::Transaction) -> erp_sdk::Result<()> {
/// use erp_addon_base::res_users::{Role, users_with_role};
/// let env = tx.env();
/// let admins = users_with_role(tx, &env, &[Some(Role::System)]).await?;
/// # let _ = admins;
/// # Ok(())
/// # }
/// ```
///
/// # Errors
///
/// As [`role_domain`], plus the errors of the search.
pub async fn users_with_role(
    tx: &mut Transaction,
    env: &Env,
    roles: &[Option<Role>],
) -> Result<Records> {
    let domain = role_domain(tx, roles).await?;
    env.records("res.users")?.search(tx, &domain).await
}

/// The role of each user of `users`.
async fn roles_of(users: &Records, tx: &mut Transaction) -> Result<Vec<Option<Role>>> {
    let [system, user] = {
        let found = groups_by_xmlid(tx, &[GROUP_SYSTEM, GROUP_USER]).await?;
        [found[0], found[1]]
    };
    let light = light_ids(tx, users.env()).await?;
    let graph = Graph::load(tx).await?;
    let direct = direct_of(users, tx).await?;
    Ok(direct
        .into_iter()
        .map(|groups| {
            let all = graph.up(groups);
            let has = |g: Option<i64>| g.is_some_and(|g| all.contains(&g));
            if has(system) {
                Some(Role::System)
            } else if has(user) && all.iter().all(|g| light.contains(g)) {
                Some(Role::Light)
            } else if has(user) {
                Some(Role::Regular)
            } else {
                None
            }
        })
        .collect())
}

/// The light groups that exist (from the overridable `res.groups._get_light_group_xmlids`), asked
/// in `env`, the caller's environment (its context reaches the override).
async fn light_ids(tx: &mut Transaction, env: &Env) -> Result<BTreeSet<i64>> {
    let groups = env.records("res.groups")?;
    let xmlids: Vec<String> = groups
        .call::<(), Vec<String>>(tx, "_get_light_group_xmlids", ())
        .await?;
    let refs: Vec<&str> = xmlids.iter().map(String::as_str).collect();
    Ok(groups_by_xmlid(tx, &refs)
        .await?
        .into_iter()
        .flatten()
        .collect())
}

/// `base.group_user` must exist: none when
/// the base module is installing and it does not yet.
async fn default_groups(tx: &mut Transaction) -> Result<Vec<i64>> {
    let (required, optional) = lookup_groups(tx, &[GROUP_USER], &[DEFAULT_USER_GROUP]).await?;
    let Some(user) = required[0] else {
        return Ok(Vec::new());
    };
    let mut groups = vec![user];
    if let Some(default_group) = optional[0] {
        let graph = Graph::load(tx).await?;
        for g in graph.direct(default_group) {
            if !groups.contains(g) {
                groups.push(*g);
            }
        }
    }
    Ok(groups)
}

fn rec_ids(ids: impl IntoIterator<Item = i64>) -> Result<Vec<RecId>> {
    ids.into_iter()
        .map(|id| {
            RecId::from_real(id)
                .ok_or_else(|| Error::programming(format!("{id} is not a database record id")))
        })
        .collect()
}

/// A `System` principal that names none takes the database's only active company, if
/// there is exactly one; otherwise there is no default and the membership check refuses the
/// create.
async fn default_company(tx: &mut Transaction, env: &Env) -> Result<Option<i64>> {
    if let Some(company) = tx.companies(env).await?.first() {
        return Ok(Some(company.get()));
    }
    if env.principal().kind() != PrincipalKind::System {
        return Ok(None);
    }
    flush_company_models(tx).await?;
    let rows = tx
        .query(
            "SELECT id::int8 FROM res_company WHERE active IS TRUE ORDER BY id LIMIT 2",
            &[],
        )
        .await?;
    match rows.as_slice() {
        [only] => only
            .try_get(0)
            .map(Some)
            .map_err(|e| Error::db(format!("res.users default company: {e}"))),
        _ => Ok(None),
    }
}

/// Pending writes of users and companies reach the tables before the company rules read them.
async fn flush_company_models(tx: &mut Transaction) -> Result<()> {
    for name in ["res.users", "res.company"] {
        let model = tx
            .env()
            .registry()
            .model(name)
            .map(|m| m.id)
            .ok_or_else(|| Error::programming(format!("the registry has no {name} model")))?;
        tx.flush_model(model).await?;
    }
    Ok(())
}

/// The names of `ids` of `model`, in order, read with sudo. The refusals of `check_user_companies`
/// name only the user being written and that user's allowed companies; the chosen default
/// company is not named, since a user who writes their own `company_id` may not read it.
async fn names_of(tx: &mut Transaction, model: &str, ids: &[i64]) -> Result<Vec<String>> {
    let records = tx
        .env()
        .sudo(true)
        .records(model)?
        .browse(ids.iter().copied())?;
    records
        .values(tx, "name")
        .await?
        .into_iter()
        .map(|name| match name {
            Value::Str(name) => Ok(name.to_string()),
            other => Err(Error::programming(format!(
                "{model}.name holds {other:?}, expected text"
            ))),
        })
        .collect()
}

/// The name of record `id` of `model`, read with sudo.
async fn name_of(tx: &mut Transaction, model: &str, id: i64) -> Result<String> {
    names_of(tx, model, &[id])
        .await?
        .into_iter()
        .next()
        .ok_or_else(|| Error::programming(format!("{model} {id} has no name to show")))
}

/// allowed companies. Inactive users are not checked, and neither is a database without any
/// company (it exists only before the base data loads and in harnesses without it). Archived
/// companies count as allowed, so an archived default company keeps its own refusal
/// (`company_lifecycle::check_users`).
///
/// Concurrency: transactions run at `REPEATABLE READ`, where one transaction that changes the
/// default company and another that removes the allowed company it relies on would both commit
/// (write skew). The check therefore locks the rows it relies on, in a fixed order: the
/// membership row of each default company with `FOR KEY SHARE` (a concurrent removal conflicts)
/// and the user row with `FOR SHARE` (a concurrent change of the default conflicts). The loser
/// gets a serialization failure or deadlock, which surfaces as a `Concurrency` error that
/// `Orm::run` retries; it is never mapped to `Validation`.
///
/// # Errors
///
/// `Validation` naming the first user (in id order) that has no default company, or whose default
/// company is not allowed (the text names that user and the user's allowed companies, never the
/// chosen company: a user writing their own `company_id` may not read it); `Concurrency` as
/// above; `Programming` when a name is not text.
pub async fn check_user_companies(users: &Records, tx: &mut Transaction) -> Result<()> {
    let ids: Vec<i64> = users
        .rec_ids()
        .iter()
        .filter_map(|id| row_id(*id))
        .collect();
    if ids.is_empty() {
        return Ok(());
    }
    // pending changes of the users, and of their relation, must reach the tables first
    flush_company_models(tx).await?;
    // One statement, so a write of inactive users (the superuser, say) costs one query and a
    // database without a company none beyond it. The CTE takes the locks (it is materialized so
    // the planner cannot inline it away): the membership row of each valid user's default company
    // with FOR KEY SHARE and the user row with FOR SHARE, before the outer query judges anyone.
    // A database without any company is not checked (`EXISTS` is false for every row).
    let rows = tx
        .query(
            "WITH locked AS MATERIALIZED ( \
                SELECT u.id FROM res_users u \
                  JOIN res_company_users_rel r ON r.user_id = u.id AND r.cid = u.company_id \
                 WHERE u.id = ANY($1::int8[]) AND u.active IS TRUE \
                 ORDER BY u.id \
                   FOR KEY SHARE OF r FOR SHARE OF u) \
             SELECT u.id::int8, u.company_id::int8 FROM res_users u \
              WHERE u.id = ANY($1::int8[]) AND u.active IS TRUE \
                AND EXISTS (SELECT 1 FROM res_company) \
                AND u.id NOT IN (SELECT id FROM locked) \
              ORDER BY u.id LIMIT 1",
            &[&ids],
        )
        .await?;
    let Some(row) = rows.first() else {
        return Ok(());
    };
    let decode = |e| Error::db(format!("res.users company check: {e}"));
    let user: i64 = row.try_get(0).map_err(decode)?;
    let company: Option<i64> = row.try_get(1).map_err(decode)?;
    let user_name = name_of(tx, "res.users", user).await?;
    if company.is_none() {
        return Err(Error::validation(format!(
            "The user {user_name} has no default company. Choose one of the user's allowed \
             companies."
        )));
    }
    let allowed: Vec<i64> = tx
        .query(
            "SELECT cid::int8 FROM res_company_users_rel WHERE user_id = $1 ORDER BY cid",
            &[&user],
        )
        .await?
        .iter()
        .map(|r| r.try_get(0))
        .collect::<std::result::Result<_, _>>()
        .map_err(decode)?;
    let allowed = if allowed.is_empty() {
        "none".to_owned()
    } else {
        names_of(tx, "res.company", &allowed).await?.join(", ")
    };
    Err(Error::validation(format!(
        "The default company of the user {user_name} is not one of the user's allowed \
         companies ({allowed})."
    )))
}

/// groups.
async fn check_user_types(users: &Records, tx: &mut Transaction) -> Result<()> {
    let types = user_type_groups(tx).await?;
    if types.len() < 2 {
        return Ok(());
    }
    let graph = Graph::load(tx).await?;
    let ids: Vec<i64> = users
        .rec_ids()
        .iter()
        .filter_map(|id| row_id(*id))
        .collect();
    // concurrent membership changes of the same users must conflict before the memberships are
    // read (write skew under REPEATABLE READ, `lock_users_for_type_check`)
    lock_users_for_type_check(tx, &ids).await?;
    let direct = direct_groups(tx, &ids).await?;
    for id in ids {
        // a user without any membership row holds no group
        let all = graph.up(direct.get(&id).cloned().unwrap_or_default());
        if types.iter().filter(|t| all.contains(t)).count() > 1 {
            let name = users.browse([id])?.sudo(true).value(tx, "name").await?;
            let name = match name {
                Value::Str(name) => Some(name.to_string()),
                Value::False => None,
                other => {
                    return Err(Error::programming(format!(
                        "res.users.name holds {other:?}, expected text"
                    )));
                }
            };
            return Err(exclusive_groups_error(tx, id, name.as_deref()).await?);
        }
    }
    Ok(())
}

pub(crate) const NO_ADMINISTRATOR: &str = "You must have at least an administrator user.";

/// user is a direct member of `base.group_system`.
///
/// Concurrency: transactions run at `REPEATABLE READ`, where two transactions that each remove a
/// different administrator would both see the other one and both commit (write skew). The check
/// therefore locks the rows it relies on, in a fixed order and without `LIMIT`: the membership
/// rows of the remaining administrators with `FOR KEY SHARE` (a concurrent removal of one of
/// them conflicts) and their `res_users` rows with `FOR SHARE` (a concurrent archive of one of
/// them conflicts; `FOR KEY SHARE` would not, since archiving is a non-key update). The loser
/// of the conflict gets a serialization failure or deadlock, which surfaces as a `Concurrency`
/// error that `Orm::run` retries; it is never mapped to `Validation`.
///
/// # Errors
pub async fn ensure_an_administrator(tx: &mut Transaction) -> Result<()> {
    let Some(system) = required_group(tx, GROUP_SYSTEM).await? else {
        return Ok(());
    };
    let users = tx
        .env()
        .registry()
        .model("res.users")
        .map(|m| m.id)
        .ok_or_else(|| Error::programming("the registry has no res.users model"))?;
    // a pending `active` write must reach the table before the rows are read and locked
    tx.flush_model(users).await?;
    let rows = tx
        .query(
            "SELECT r.uid FROM res_groups_users_rel r JOIN res_users u ON u.id = r.uid \
              WHERE r.gid = $1 AND u.active \
              ORDER BY r.uid \
                FOR KEY SHARE OF r FOR SHARE OF u",
            &[&system],
        )
        .await?;
    if rows.is_empty() {
        return Err(Error::validation(NO_ADMINISTRATOR));
    }
    Ok(())
}

#[methods(ResUsers)]
impl ResUsersAccess {
    /// The user's groups and every group they imply.
    #[compute(depends = ["group_ids"])]
    async fn _compute_all_group_ids(
        this: &RecordSet<ResUsers>,
        tx: &mut Transaction,
        out: &mut Assign<'_, ResUsers>,
    ) -> Result<()> {
        let graph = Graph::load(tx).await?;
        let direct = direct_of(this.as_records(), tx).await?;
        for (i, groups) in direct.into_iter().enumerate() {
            out.set(i, ResUsersAccess::ALL_GROUP_IDS, rec_ids(graph.up(groups))?)?;
        }
        Ok(())
    }

    /// `share` is false for users that are direct members of an internal group.
    ///
    /// The superuser is never a share user, whatever its groups (it holds none): the company
    /// restriction of `res.users` escapes through `share`, and the superuser row,
    /// which has no company, must stay readable where its partner is (`partner_share` has the same
    /// exception).
    #[compute(depends = ["group_ids"])]
    async fn _compute_share(
        this: &RecordSet<ResUsers>,
        tx: &mut Transaction,
        out: &mut Assign<'_, ResUsers>,
    ) -> Result<()> {
        let internal = internal_group_ids(tx).await?;
        let users = this.as_records();
        let direct = direct_of(users, tx).await?;
        for (i, (id, groups)) in users.rec_ids().iter().zip(direct).enumerate() {
            let superuser = !id.is_new() && id.get() == erp_sdk::SUPERUSER_ID.get();
            let share = !superuser && !groups.iter().any(|g| internal.contains(g));
            out.set(i, ResUsersAccess::SHARE, share)?;
        }
        Ok(())
    }

    /// `group_system` for administrators, otherwise `light_user` or `regular_user` for
    /// internal users (light: every group they hold is a light group), none for other users.
    #[compute(depends = ["group_ids"])]
    async fn _compute_role(
        this: &RecordSet<ResUsers>,
        tx: &mut Transaction,
        out: &mut Assign<'_, ResUsers>,
    ) -> Result<()> {
        let roles = roles_of(this.as_records(), tx).await?;
        for (i, role) in roles.into_iter().enumerate() {
            let role: Option<Arc<str>> = role.map(|r| Arc::from(r.as_str()));
            out.set(i, ResUsersAccess::ROLE, role)?;
        }
        Ok(())
    }

    #[method(override)]
    async fn default_get(
        this: &RecordSet<ResUsers>,
        tx: &mut Transaction,
        names: Vec<String>,
        sup: Super,
    ) -> Result<ValueMap> {
        let asked = names.iter().any(|n| n == "group_ids");
        let ask_company = names.iter().any(|n| n == "company_id");
        let ask_companies = names.iter().any(|n| n == "company_ids");
        let mut defaults: ValueMap = sup.call(tx, this.as_records(), (names,)).await?;
        if asked && !defaults.contains_key("group_ids") {
            let groups = default_groups(tx).await?;
            if !groups.is_empty() {
                defaults.insert(
                    "group_ids".to_owned(),
                    Value::list([Command::Set(rec_ids(groups)?).to_value()]),
                );
            }
        }
        if (ask_company && !defaults.contains_key("company_id"))
            || (ask_companies && !defaults.contains_key("company_ids"))
        {
            // a company the context or `ir.default` chose is the company of both fields
            let chosen = match defaults.get("company_id") {
                Some(Value::Int(id)) => Some(*id),
                _ => None,
            };
            let company = match chosen {
                Some(id) => Some(id),
                None => {
                    let env = this.as_records().env().clone();
                    default_company(tx, &env).await?
                }
            };
            if let Some(company) = company {
                if ask_company && !defaults.contains_key("company_id") {
                    defaults.insert("company_id".to_owned(), Value::Int(company));
                }
                if ask_companies && !defaults.contains_key("company_ids") {
                    defaults.insert(
                        "company_ids".to_owned(),
                        Value::list([Command::Set(rec_ids([company])?).to_value()]),
                    );
                }
            }
        }
        Ok(defaults)
    }

    /// A change of a user's groups changes the users of every group above them: the closure
    /// `all_user_ids` of groups the dependencies do not reach is dropped.
    #[method(override)]
    async fn create(
        this: &RecordSet<ResUsers>,
        tx: &mut Transaction,
        mut vals_list: Vec<RawVals>,
        sup: Super,
    ) -> Result<Records> {
        this.as_records()
            .check_access(tx, erp_sdk::Operation::Create)
            .await?;
        for vals in &vals_list {
            erp_sdk::__private::base_check_write_fields(tx, this.as_records(), vals)?;
        }
        for vals in &vals_list {
            crate::res_users_preferences::check_preference_values(tx, vals).await?;
        }
        // a default company given alone is the allowed company: the
        // environment's company must not become one of the allowed ones instead
        for vals in &mut vals_list {
            let company = match vals.get("company_id") {
                Some(Cell::Id(company)) => Some(*company),
                _ => None,
            };
            if let Some(company) = company
                && !vals.contains("company_ids")
                && vals.commands("company_ids").is_none()
            {
                vals.insert_commands(
                    Cow::Borrowed("company_ids"),
                    vec![Command::Set(rec_ids([company])?)],
                );
            }
        }
        let _lifecycle = crate::company_lifecycle::Guard::enter(tx, &[])?;
        crate::company_lifecycle::fence(tx).await?;
        let groups = vals_list
            .iter()
            .any(|v| v.contains("group_ids") || v.commands("group_ids").is_some());
        // a `lang` or `tz` the caller left out may come from a `default_lang` / `default_tz`
        // context default, which the ORM adds after the check above: check what was stored
        let defaulted = vals_list
            .iter()
            .any(|v| !v.contains("lang") || !v.contains("tz"));
        let created: Records = sup.call(tx, this.as_records(), (vals_list,)).await?;
        crate::company_lifecycle::check_users(tx, &created).await?;
        if defaulted {
            crate::res_users_preferences::check_stored_preferences(tx, &created).await?;
        }
        if groups {
            tx.invalidate_model("res.groups", Some(&["all_user_ids"]))
                .await?;
        }
        Ok(created)
    }

    /// See `create`; also the rules that depend on `active`, and
    /// archiving ends the users' sessions.
    #[method(override)]
    async fn write(
        this: &RecordSet<ResUsers>,
        tx: &mut Transaction,
        vals: RawVals,
        sup: Super,
    ) -> Result<()> {
        // the user-type flag is the escape of the company restriction: only the
        // server's compute and the group recompute set it, never a caller
        if vals.contains("share") && !this.env().is_superuser() {
            // a caller without write access to the field gets the access error first
            this.as_records()
                .check_access(tx, erp_sdk::Operation::Write)
                .await?;
            erp_sdk::__private::base_check_write_fields(tx, this.as_records(), &vals)?;
            return Err(Error::programming(
                "'res.users.share' is set by the server and cannot be written",
            ));
        }
        crate::res_users_auth::check_write(this.as_records(), &vals)?;
        crate::res_users_preferences::check_preference_values(tx, &vals).await?;
        let lifecycle = vals.contains("active") || vals.contains("company_id");
        let _lifecycle = if lifecycle {
            this.as_records()
                .check_access(tx, erp_sdk::Operation::Write)
                .await?;
            erp_sdk::__private::base_check_write_fields(tx, this.as_records(), &vals)?;
            let guard = crate::company_lifecycle::Guard::enter(tx, &[])?;
            crate::company_lifecycle::fence(tx).await?;
            Some(guard)
        } else {
            None
        };
        let groups = vals.contains("group_ids") || vals.commands("group_ids").is_some();
        let active = vals.contains("active");
        let archives = crate::res_users_auth::archives(&vals);
        let renamed = crate::res_users_auth::renamed(this.as_records(), tx, &vals).await?;
        sup.call(tx, this.as_records(), (vals,)).await?;
        if lifecycle {
            crate::company_lifecycle::check_users(tx, this.as_records()).await?;
        }
        if archives {
            crate::res_users_auth::end_sessions_of(this.as_records(), tx).await?;
        } else {
            crate::res_users_auth::end_sessions_of_renamed(tx, &renamed).await?;
        }
        if groups {
            tx.invalidate_model("res.groups", Some(&["all_user_ids"]))
                .await?;
        }
        if active {
            check_user_types(this.as_records(), tx).await?;
            ensure_an_administrator(tx).await?;
        }
        Ok(())
    }

    /// No user may be in two of the user type groups, directly or through implied groups.
    #[constrains("group_ids")]
    async fn _check_disjoint_groups(
        this: &RecordSet<ResUsers>,
        tx: &mut Transaction,
    ) -> Result<()> {
        check_user_types(this.as_records(), tx).await
    }

    /// An active user's default company is one of its allowed companies (
    /// [`check_user_companies`]).
    #[constrains("company_id", "company_ids", "active")]
    async fn _check_user_company(this: &RecordSet<ResUsers>, tx: &mut Transaction) -> Result<()> {
        check_user_companies(this.as_records(), tx).await
    }

    /// A written colour scheme is never empty. A user row that holds no value
    /// (inserted by raw SQL after the column exists) is not touched until it is written.
    #[constrains("color_scheme")]
    async fn _check_color_scheme(this: &RecordSet<ResUsers>, tx: &mut Transaction) -> Result<()> {
        let rows = this.as_records().read(tx, &["color_scheme"]).await?;
        for row in &rows {
            match row.get("color_scheme") {
                Some(Value::Str(key)) if !key.is_empty() => {}
                _ => return Err(Error::validation("The color scheme cannot be empty.")),
            }
        }
        Ok(())
    }

    /// The last active member of `base.group_system` cannot be removed (
    /// [`ensure_an_administrator`]). Archiving and deleting are checked in `write` and `unlink`.
    #[constrains("group_ids")]
    async fn _check_at_least_one_administrator(
        _this: &RecordSet<ResUsers>,
        tx: &mut Transaction,
    ) -> Result<()> {
        ensure_an_administrator(tx).await
    }

    #[method(override)]
    async fn unlink(this: &RecordSet<ResUsers>, tx: &mut Transaction, sup: Super) -> Result<()> {
        let any = !this.as_records().is_empty();
        crate::res_users_auth::end_sessions_of(this.as_records(), tx).await?;
        sup.call(tx, this.as_records(), ()).await?;
        if any {
            tx.invalidate_model("res.groups", Some(&["all_user_ids"]))
                .await?;
            ensure_an_administrator(tx).await?;
        }
        Ok(())
    }

    #[onchange("role")]
    async fn _onchange_role(
        this: &RecordSet<ResUsers>,
        tx: &mut Transaction,
    ) -> Result<OnchangeResult> {
        let role = this.as_records().value(tx, "role").await?;
        let role = match role {
            Value::Str(role) => role.to_string(),
            Value::False | Value::Bool(false) => return Ok(OnchangeResult::none()),
            other => {
                return Err(Error::programming(format!(
                    "res.users.role must be a string or empty, got {other:?}"
                )));
            }
        };
        if ![Role::Light, Role::Regular, Role::System]
            .iter()
            .any(|r| r.as_str() == role)
        {
            return Err(Error::programming(format!(
                "res.users.role '{role}' is not a role: expected 'light_user', 'regular_user' \
                 or 'group_system'"
            )));
        }
        let found = required_groups(tx, &[GROUP_SYSTEM, GROUP_USER, GROUP_USER_REGULAR]).await?;
        let [Some(system), Some(user), Some(regular)] = [found[0], found[1], found[2]] else {
            // the base module is installing: the groups do not exist yet
            return Ok(OnchangeResult::none());
        };
        let graph = Graph::load(tx).await?;
        // the form's single record; an empty set is a form with no groups yet
        let current: Vec<i64> = direct_of(this.as_records(), tx)
            .await?
            .into_iter()
            .next()
            .unwrap_or_default();
        let held = graph.up(current.iter().copied());
        let mut groups: Vec<i64> = current;
        match role.as_str() {
            "regular_user" => {
                if !held.contains(&regular) {
                    groups.push(regular);
                }
                groups.retain(|g| *g != system);
            }
            "group_system" => {
                for g in [system, user] {
                    if !groups.contains(&g) {
                        groups.push(g);
                    }
                }
            }
            "light_user" => {
                // a portal or public user switched to light gets the default groups first
                if !held.contains(&user) {
                    for g in default_groups(tx).await? {
                        if !groups.contains(&g) {
                            groups.push(g);
                        }
                    }
                }
                // and the reduction runs over everything the user now holds
                let origin = this
                    .as_records()
                    .env()
                    .records("res.groups")?
                    .browse(groups.clone())?;
                let reduced = origin
                    .call::<(), Records>(tx, "_reduce_to_light_groups", ())
                    .await?;
                groups = vec![user];
                groups.extend(reduced.ids().into_iter().filter(|g| *g != user));
            }
            other => {
                return Err(Error::programming(format!(
                    "res.users._onchange_role: unhandled role '{other}'"
                )));
            }
        }
        this.as_records()
            .set_value(
                tx,
                "group_ids",
                Value::list([Command::Set(rec_ids(groups)?).to_value()]),
            )
            .await?;
        Ok(OnchangeResult::none())
    }

    /// The exemption therefore follows the ACL: exempting
    /// `group_erp_manager` would let an Access Rights member, who has no user-management row,
    /// write `password`, `login`, `partner_id` or `group_ids` on their own record through the
    /// self row. A scoped elevation counts as no elevation here (it fails closed): a future scope
    /// that should widen the mask has to decide that on purpose.
    #[field_access]
    fn _field_access(
        ctx: &FieldAccessCtx<'_>,
        op: FieldOp,
        mask: FieldMask,
        sup: FieldAccessSuper<'_>,
    ) -> Result<FieldMask> {
        let mut mask = sup.call(ctx, op, mask)?;
        if op == FieldOp::Write
            && ctx.elevation() != ElevationKind::Full
            && !ctx.has_group(GROUP_SYSTEM)?
        {
            for field in ctx.model().fields.iter().filter(|f| !f.user_writeable) {
                mask.deny(field.id, FieldOp::Write)?;
            }
        }
        Ok(mask)
    }
}
