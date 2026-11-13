// SPDX-License-Identifier: MIT OR Apache-2.0
//! Approval policy revisions and their resolution (D9 as amended by B7 and B8; slice
//! T2).
//!
//! `ir.approval.policy` holds one row per **revision**: a selector (company or all companies,
//! principal kind, effect, exact model or wildcard, optional exact method), an outcome (allow,
//! approve, deny) and, for an Approve revision, the eligible set of named users and groups. A
//! revision never changes after creation except for `active`: a change is a new revision and the
//! old one is archived; deletion is refused. Only `base.group_system` may write revisions
//! (`security/ir.access.csv`).
//!
//! [`resolve`] selects the one most specific active revision of a call (exact company before all
//! companies, then exact model and method, exact model, wildcard) and, for Approve, the humans who
//! may decide it now. Policies control RPC entry calls, not records: writes a method
//! makes on other models, jobs and `System` calls never consult them. Nothing consults them before
//! the approval dispatch exists.

use std::collections::BTreeMap;

use erp_sdk::prelude::*;
use erp_sdk::rpc::{Effect, Exposure};
use erp_sdk::{Actor, CompanyId, Operation, PrincipalKind, Row, SUPERUSER_ID, UserId};

use crate::group_graph::Graph;

/// Most humans an Approve revision may name or resolve to (users plus groups when written, users
/// when resolved).
pub const MAX_APPROVERS: usize = 64;
/// Most active companies a call may resolve.
pub const MAX_COMPANIES: usize = 256;
/// Longest model or method name of an [`ApprovalCall`] (bytes).
pub const MAX_NAME_BYTES: usize = 256;

/// `ir.approval.policy`: one approval policy revision.
#[model(
    name = "ir.approval.policy",
    description = "Approval Policy Revision",
    order = "id",
    company_access = direct(field = "company_id", branch = "exact", empty = "allow"),
    constraints = [(
        "_method_needs_model",
        "CHECK (method IS NULL OR model IS NOT NULL)",
        "An approval policy that names a method also names its model."
    )],
    unique_indexes = [(
        "_active_selector_uniq",
        "(COALESCE(company_id, 0), principal_kind, effect, COALESCE(model, ''), COALESCE(method, '')) WHERE active",
        "Two active approval policy revisions cannot have the same selector."
    )],
)]
pub struct IrApprovalPolicy {
    /// What the revision is for, for administrators.
    #[field(char, required = true)]
    pub name: Option<Arc<str>>,
    /// Archived revisions select nothing; archiving is the only change a revision accepts.
    #[field(boolean, default = true)]
    pub active: bool,
    /// The company the revision applies to; empty for all companies.
    #[field(many2one, comodel = "res.company", ondelete = "restrict")]
    pub company_id: Option<RecId>,
    /// The kind of caller it applies to. Only `user` revisions are resolved (status).
    #[field(
        selection,
        selection = [("user", "User"), ("agent", "Agent"), ("service", "Service")],
        required = true
    )]
    pub principal_kind: Option<Arc<str>>,
    /// The effect class of the call.
    #[field(
        selection,
        selection = [
            ("read", "Read"),
            ("write", "Write"),
            ("destructive", "Destructive"),
            ("external", "External")
        ],
        required = true
    )]
    pub effect: Option<Arc<str>>,
    /// The exact model name; empty for every model.
    #[field(char)]
    pub model: Option<Arc<str>>,
    /// The exact method name; empty for every method of the model.
    #[field(char)]
    pub method: Option<Arc<str>>,
    /// What a selected call gets.
    #[field(
        selection,
        selection = [("allow", "Allow"), ("approve", "Approve"), ("deny", "Deny")],
        required = true
    )]
    pub outcome: Option<Arc<str>>,
    /// Named users who may decide; a referenced user cannot be deleted.
    #[field(
        many2many,
        comodel = "res.users",
        relation = "ir_approval_policy_users_rel",
        column1 = "policy_id",
        column2 = "user_id",
        ondelete = "restrict",
        string = "Approvers"
    )]
    pub approver_user_ids: Vec<RecId>,
    /// Groups whose current members may decide; a referenced group cannot be deleted.
    #[field(
        many2many,
        comodel = "res.groups",
        relation = "ir_approval_policy_groups_rel",
        column1 = "policy_id",
        column2 = "group_id",
        ondelete = "restrict",
        string = "Approver Groups"
    )]
    pub approver_group_ids: Vec<RecId>,
}

const IMMUTABLE: &str =
    "Approval policy revisions cannot be changed; archive this revision and create a new one.";
const UNDELETABLE: &str = "Approval policy revisions cannot be deleted; archive them instead.";
const AGENT_UNSUPPORTED: &str = "Approval policies for agent and service principals are not \
     supported until accepts their parts (a later release).";

/// The identity of a policy revision (its `ir.approval.policy` row id).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PolicyRevisionId(i64);

impl PolicyRevisionId {
    /// The revision with row id `id`.
    ///
    /// # Errors
    ///
    /// `Validation` for an id that is not positive.
    pub fn new(id: i64) -> Result<Self> {
        if id > 0 {
            Ok(Self(id))
        } else {
            Err(Error::validation(format!(
                "Approval policy revision id {id} is not a positive id."
            )))
        }
    }

    /// The row id.
    #[must_use]
    pub const fn get(self) -> i64 {
        self.0
    }
}

/// The humans who may decide a request under a revision, in id order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EligibleApprovers(Box<[UserId]>);

impl EligibleApprovers {
    /// The users, in id order.
    #[must_use]
    pub fn ids(&self) -> &[UserId] {
        &self.0
    }

    /// Whether `user` may decide.
    #[must_use]
    pub fn contains(&self, user: UserId) -> bool {
        self.0.binary_search(&user).is_ok()
    }

    /// Whether nobody may decide.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// What [`resolve`] decided for a call. Deliberately not `#[non_exhaustive]`: the approval phase
/// matches it exhaustively, and a new outcome must break it (engineering standards §2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PolicyOutcome {
    /// No active revision selects the call: ordinary authorization only.
    Unmanaged,
    /// The selected revision allows the call.
    Allow {
        /// The selected revision.
        revision: PolicyRevisionId,
    },
    /// The call needs a decision by one of `approvers` (never empty).
    Approve {
        /// The selected revision.
        revision: PolicyRevisionId,
        /// Who may decide now.
        approvers: EligibleApprovers,
    },
    /// The selected revision refuses the call.
    Deny {
        /// The selected revision.
        revision: PolicyRevisionId,
    },
}

/// The call a policy is resolved for: the model, the method and the facts about its target
/// records that decide which companies [`resolve`] reads revisions for. The effect is never the
/// caller's: [`resolve`] reads the method's current effect from the registry's RPC exposure.
///
/// The constructors take facts, not a scope flag (amendment 2026-11-13): a call whose
/// records all carry a company is resolved for the caller's active companies plus those
/// companies; every other call (a create, a record without a company, no record at all) is
/// resolved for every company the caller is allowed in. A dispatch cannot narrow a call to the
/// active companies by leaving the targets out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApprovalCall {
    model: Box<str>,
    method: Box<str>,
    scope: CompanyScope,
    target_companies: Box<[CompanyId]>,
}

/// Which companies besides the targets a call is resolved for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CompanyScope {
    /// The caller's validated active companies.
    Active,
    /// Every company the caller is allowed in, the active ones included.
    Allowed,
}

impl ApprovalCall {
    /// A call of `model.method` on existing records: `record_companies` holds one entry per
    /// target record, its `company_id`, or `None` when the model has no company field or the
    /// record's is empty.
    ///
    /// The approval dispatch must supply every target record. When there is
    /// at least one and every one has a company, [`resolve`] reads the revisions of the caller's
    /// active companies and of those companies, so dropping a company from the active set cannot
    /// turn a call on that company's records into `Unmanaged`. A record without a company, or no
    /// record at all, gives no company to add: the call is then resolved for every company the
    /// caller is allowed in, so choosing the active company cannot pass over another company's
    /// revision.
    ///
    /// # Errors
    ///
    /// `Validation` for an empty name, a name longer than [`MAX_NAME_BYTES`] or one holding a
    /// NUL, and more than [`MAX_COMPANIES`] distinct target companies.
    pub fn on_records(
        model: &str,
        method: &str,
        record_companies: &[Option<CompanyId>],
    ) -> Result<Self> {
        let companied =
            !record_companies.is_empty() && record_companies.iter().all(Option::is_some);
        let scope = if companied {
            CompanyScope::Active
        } else {
            CompanyScope::Allowed
        };
        // a `None` adds no company; it has already widened the scope above
        Self::build(
            model,
            method,
            scope,
            record_companies.iter().flatten().copied().collect(),
        )
    }

    /// A call of `model.method` that creates records; `known_companies` are the companies the
    /// new records get when they are known before the write (from the values or a default),
    /// empty when they are not.
    ///
    /// The approval dispatch must supply the companies it knows. A create is always resolved for
    /// every company the caller is allowed in plus `known_companies`: the new records' company is
    /// the caller's choice, so the active companies alone cannot decide which revision applies.
    ///
    /// # Errors
    ///
    /// As [`ApprovalCall::on_records`].
    pub fn create(model: &str, method: &str, known_companies: &[CompanyId]) -> Result<Self> {
        Self::build(
            model,
            method,
            CompanyScope::Allowed,
            known_companies.to_vec(),
        )
    }

    /// A call of the model method `model.method`, which has no target records.
    ///
    /// Resolved for every company the caller is allowed in (fail closed): nothing ties the call
    /// to one company, so the active company cannot decide which revision applies.
    ///
    /// # Errors
    ///
    /// `Validation` for an empty name, a name longer than [`MAX_NAME_BYTES`] or one holding a
    /// NUL.
    pub fn model_level(model: &str, method: &str) -> Result<Self> {
        Self::build(model, method, CompanyScope::Allowed, Vec::new())
    }

    fn build(
        model: &str,
        method: &str,
        scope: CompanyScope,
        mut targets: Vec<CompanyId>,
    ) -> Result<Self> {
        for name in [model, method] {
            if name.is_empty() || name.len() > MAX_NAME_BYTES || name.contains('\0') {
                return Err(Error::validation(format!(
                    "An approval call names a model or method that is empty, longer than \
                     {MAX_NAME_BYTES} bytes or holds a NUL."
                )));
            }
        }
        targets.sort_unstable();
        targets.dedup();
        if targets.len() > MAX_COMPANIES {
            return Err(Error::validation(format!(
                "An approval policy is resolved for at most {MAX_COMPANIES} companies."
            )));
        }
        Ok(Self {
            model: model.into(),
            method: method.into(),
            scope,
            target_companies: targets.into_boxed_slice(),
        })
    }

    /// The model name.
    #[must_use]
    pub fn model(&self) -> &str {
        &self.model
    }

    /// The method name.
    #[must_use]
    pub fn method(&self) -> &str {
        &self.method
    }

    /// The companies of the target records (or, for a create, the known companies of the new
    /// records), in id order without duplicates.
    #[must_use]
    pub fn target_companies(&self) -> &[CompanyId] {
        &self.target_companies
    }

    /// Whether the call is resolved for every company the caller is allowed in, not only the
    /// active ones (a create, a record without a company, no target record).
    #[must_use]
    pub fn widened(&self) -> bool {
        self.scope == CompanyScope::Allowed
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Outcome {
    Allow,
    Approve,
    Deny,
}

/// One stored revision as read back for checks and resolution.
#[derive(Debug)]
struct Stored {
    id: i64,
    name: String,
    active: bool,
    company: Option<i64>,
    kind: PrincipalKind,
    effect: Effect,
    model: Option<String>,
    method: Option<String>,
    outcome: Outcome,
}

impl Stored {
    fn label(&self) -> String {
        format!("'{}' (id {})", self.name, self.id)
    }

    fn refuse(&self, reason: &str) -> Error {
        Error::validation(format!(
            "Approval policy revision {}: {reason}",
            self.label()
        ))
    }

    /// The specificity of the selector after the company: method 2, model 1, wildcard 0 (B8).
    fn level(&self) -> u8 {
        match (&self.model, &self.method) {
            (Some(_), Some(_)) => 2,
            (Some(_), None) => 1,
            (None, _) => 0,
        }
    }
}

/// The columns [`stored`] reads, of `ir_approval_policy p`.
const COLUMNS: &str = "p.id::int8, p.name, p.active, p.company_id::int8, p.principal_kind, p.effect, \
     p.model, p.method, p.outcome";

fn unreadable(column: &str) -> Error {
    Error::internal(format!(
        "ir_approval_policy.{column} holds a value of an unexpected type."
    ))
}

fn stored(row: &Row) -> Result<Stored> {
    let text = |i: usize, column: &str| -> Result<String> {
        row.try_get::<_, String>(i).map_err(|_| unreadable(column))
    };
    let optional = |i: usize, column: &str| -> Result<Option<String>> {
        row.try_get::<_, Option<String>>(i)
            .map_err(|_| unreadable(column))
    };
    let id: i64 = row.try_get(0).map_err(|_| unreadable("id"))?;
    let kind_text = text(4, "principal_kind")?;
    let effect_text = text(5, "effect")?;
    let outcome_text = text(8, "outcome")?;
    let corrupt = |what: &str, value: &str| {
        Error::validation(format!(
            "Approval policy revision {id} holds the unknown {what} '{value}'."
        ))
    };
    let kind = match kind_text.as_str() {
        "user" => PrincipalKind::User,
        "agent" => PrincipalKind::Agent,
        "service" => PrincipalKind::Service,
        other => return Err(corrupt("principal kind", other)),
    };
    let effect = Effect::parse(&effect_text).ok_or_else(|| corrupt("effect", &effect_text))?;
    let outcome = match outcome_text.as_str() {
        "allow" => Outcome::Allow,
        "approve" => Outcome::Approve,
        "deny" => Outcome::Deny,
        other => return Err(corrupt("outcome", other)),
    };
    let revision = Stored {
        id,
        name: text(1, "name")?,
        active: row.try_get(2).map_err(|_| unreadable("active"))?,
        company: row.try_get(3).map_err(|_| unreadable("company_id"))?,
        kind,
        effect,
        model: optional(6, "model")?,
        method: optional(7, "method")?,
        outcome,
    };
    check_combination(&revision)?;
    Ok(revision)
}

/// The combinations the minimum delivery refuses, whatever wrote the row.
fn check_combination(revision: &Stored) -> Result<()> {
    if revision.method.is_some() && revision.model.is_none() {
        return Err(revision.refuse("a method selector needs a model."));
    }
    match (revision.outcome, revision.effect, revision.kind) {
        (Outcome::Approve, Effect::Read, _) => {
            Err(revision.refuse("requiring approval for read calls is not supported."))
        }
        (Outcome::Approve, Effect::External, PrincipalKind::User) => Err(revision.refuse(
            "requiring approval for external calls of users is not supported until external-effect \
             admission is accepted.",
        )),
        (Outcome::Allow, _, PrincipalKind::Agent | PrincipalKind::Service) => Err(revision.refuse(
            "an Allow exception for agent and service principals is not supported until \
             accepts it (D2, a later release).",
        )),
        _ => Ok(()),
    }
}

/// Every write-time rule of the revisions `ids` (after the ORM wrote them).
async fn check_revisions(env: &Env, tx: &mut Transaction, ids: &[i64]) -> Result<()> {
    tx.flush().await?;
    let sql = format!(
        "SELECT {COLUMNS}, (SELECT count(*) FROM ir_approval_policy_users_rel r WHERE r.policy_id = p.id) \
         + (SELECT count(*) FROM ir_approval_policy_groups_rel g WHERE g.policy_id = p.id) \
         FROM ir_approval_policy p WHERE p.id = ANY($1::int8[]) ORDER BY p.id"
    );
    let rows = tx.query(&sql, &[&ids]).await?;
    let mut labels = BTreeMap::new();
    for row in &rows {
        let revision = stored(row)?;
        let named: i64 = row.try_get(9).map_err(|_| unreadable("approvers"))?;
        check_selector(env, tx, &revision)?;
        if revision.outcome != Outcome::Approve && named > 0 {
            return Err(revision.refuse("only an Approve revision names approvers."));
        }
        if usize::try_from(named).map_err(|_| unreadable("approvers"))? > MAX_APPROVERS {
            return Err(revision.refuse(&format!(
                "names more than {MAX_APPROVERS} approvers and groups."
            )));
        }
        if revision.active
            && revision.outcome == Outcome::Approve
            && revision.kind == PrincipalKind::User
            && named == 0
        {
            return Err(
                revision.refuse("requires approval but names no approver or approver group.")
            );
        }
        labels.insert(revision.id, revision.label());
    }
    let inactive = tx
        .query(
            "SELECT r.policy_id::int8, r.user_id::int8 FROM ir_approval_policy_users_rel r \
             JOIN res_users u ON u.id = r.user_id \
             WHERE r.policy_id = ANY($1::int8[]) AND (NOT u.active OR u.share OR u.id = $2::int8) \
             ORDER BY 1, 2 LIMIT 1",
            &[&ids, &SUPERUSER_ID.get()],
        )
        .await?;
    if let Some(row) = inactive.first() {
        let policy: i64 = row.try_get(0).map_err(|_| unreadable("policy_id"))?;
        let user: i64 = row.try_get(1).map_err(|_| unreadable("user_id"))?;
        return Err(Error::validation(format!(
            "Approval policy revision {}: user {user} is not an active internal user and cannot \
             approve.",
            labels.get(&policy).ok_or_else(|| unreadable("policy_id"))?
        )));
    }
    Ok(())
}

/// Refuse `ids` when one of them, taken as active, has the selector of another active revision
/// (before the update, so the message names both instead of the index's generic refusal).
async fn same_selector(tx: &mut Transaction, ids: &[i64]) -> Result<()> {
    let overlap = tx
        .query(
            "SELECT a.id::int8, a.name, b.id::int8, b.name FROM ir_approval_policy a \
             JOIN ir_approval_policy b ON b.id <> ALL($1::int8[]) AND b.active \
             AND b.principal_kind = a.principal_kind AND b.effect = a.effect \
             AND b.company_id IS NOT DISTINCT FROM a.company_id \
             AND b.model IS NOT DISTINCT FROM a.model AND b.method IS NOT DISTINCT FROM a.method \
             WHERE a.id = ANY($1::int8[]) ORDER BY a.id, b.id LIMIT 1",
            &[&ids],
        )
        .await?;
    let Some(row) = overlap.first() else {
        return Ok(());
    };
    let id: i64 = row.try_get(0).map_err(|_| unreadable("id"))?;
    let name: String = row.try_get(1).map_err(|_| unreadable("name"))?;
    let other: i64 = row.try_get(2).map_err(|_| unreadable("id"))?;
    let other_name: String = row.try_get(3).map_err(|_| unreadable("name"))?;
    Err(Error::validation(format!(
        "Approval policy revision '{name}' (id {id}) has the same selector as the active \
         revision '{other_name}' (id {other}); archive one of them first."
    )))
}

/// The selector of a new active revision as `vals` give it, `None` for an archived one.
#[derive(Debug, PartialEq, Eq)]
struct NewSelector {
    name: String,
    company: Option<i64>,
    /// principal kind, effect, model, method
    texts: [Option<String>; 4],
}

/// shortcut: a value of another cell form (or a default other than `active = true`) skips this
/// check; the partial unique index still refuses the tie, with its generic message.
fn new_selector(vals: &RawVals) -> Option<NewSelector> {
    let text = |name: &str| match vals.get(name) {
        None | Some(Cell::Null | Cell::Bool(false)) => Some(None),
        Some(Cell::Str(value)) if !value.is_empty() => Some(Some(value.to_string())),
        Some(_) => None,
    };
    if !matches!(vals.get("active"), None | Some(Cell::Bool(true))) {
        return None;
    }
    let company = match vals.get("company_id") {
        None | Some(Cell::Null | Cell::Bool(false)) => None,
        Some(Cell::Id(id)) => Some(*id),
        Some(Cell::Int(id)) => Some(i64::from(*id)),
        Some(_) => return None,
    };
    Some(NewSelector {
        name: text("name")??,
        company,
        texts: [
            text("principal_kind")?,
            text("effect")?,
            text("model")?,
            text("method")?,
        ],
    })
}

/// Refuse new revisions whose selector equals an active one's or another new one's.
async fn new_same_selector(tx: &mut Transaction, vals_list: &[RawVals]) -> Result<()> {
    let mut seen: Vec<NewSelector> = Vec::new();
    for vals in vals_list {
        let Some(selector) = new_selector(vals) else {
            continue;
        };
        let new = &selector.name;
        if let Some(earlier) = seen
            .iter()
            .find(|s| s.company == selector.company && s.texts == selector.texts)
        {
            return Err(Error::validation(format!(
                "The new approval policy revisions '{}' and '{new}' have the same selector.",
                earlier.name
            )));
        }
        let [kind, effect, model, method] = &selector.texts;
        let rows = tx
            .query(
                "SELECT id::int8, name FROM ir_approval_policy WHERE active \
                 AND company_id::int8 IS NOT DISTINCT FROM $1::int8 \
                 AND principal_kind IS NOT DISTINCT FROM $2 AND effect IS NOT DISTINCT FROM $3 \
                 AND model IS NOT DISTINCT FROM $4 AND method IS NOT DISTINCT FROM $5 \
                 ORDER BY id LIMIT 1",
                &[&selector.company, kind, effect, model, method],
            )
            .await?;
        if let Some(row) = rows.first() {
            let other: i64 = row.try_get(0).map_err(|_| unreadable("id"))?;
            let other_name: String = row.try_get(1).map_err(|_| unreadable("name"))?;
            return Err(Error::validation(format!(
                "The new approval policy revision '{new}' has the same selector as the active \
                 revision '{other_name}' (id {other}); archive one of them first."
            )));
        }
        seen.push(selector);
    }
    Ok(())
}

/// A revision of company X is created only by a user active in X (review r1, D-a); an
/// all-companies revision is global administration, like `ir.access` rows.
async fn check_companies(env: &Env, tx: &mut Transaction, vals_list: &[RawVals]) -> Result<()> {
    if env.principal().kind() == PrincipalKind::System {
        return Ok(());
    }
    let active = tx.companies(env).await?;
    for vals in vals_list {
        let company = match vals.get("company_id") {
            None | Some(Cell::Null | Cell::Bool(false)) => continue,
            Some(Cell::Id(id)) => *id,
            Some(Cell::Int(id)) => i64::from(*id),
            Some(_) => {
                return Err(Error::validation(
                    "The company of an approval policy revision is a company id.",
                ));
            }
        };
        if !active.iter().any(|c| c.get() == company) {
            return Err(Error::access(format!(
                "An approval policy revision of company {company} is created by a user active in \
                 that company."
            )));
        }
    }
    Ok(())
}

/// The model must exist and a method must be exposed over RPC with the revision's effect, so a
/// typo cannot leave a call to fall through to a less specific revision.
fn check_selector(env: &Env, tx: &Transaction, revision: &Stored) -> Result<()> {
    let Some(model) = revision.model.as_deref() else {
        return Ok(());
    };
    let Some(model_id) = env.registry().model_id(model) else {
        return Err(revision.refuse(&format!("names the unknown model '{model}'.")));
    };
    let Some(method) = revision.method.as_deref() else {
        return Ok(());
    };
    match tx.exposure().get(model_id, method) {
        Some(Exposure::Exposed(exposed)) if exposed.effect() == revision.effect => Ok(()),
        _ => Err(revision.refuse(&format!(
            "'{model}.{method}' is not an RPC method of effect {}.",
            revision.effect.as_str()
        ))),
    }
}

#[methods(IrApprovalPolicy, addon = "base")]
impl IrApprovalPolicy {
    /// Check the new revisions. A refusal after the insert poisons the transaction (F1),
    /// so no caller can commit a refused revision.
    #[method(override)]
    async fn create(
        this: &RecordSet<IrApprovalPolicy>,
        tx: &mut Transaction,
        vals_list: Vec<RawVals>,
        sup: Super,
    ) -> Result<Records> {
        // access first: the selector check reads other revisions
        this.as_records()
            .check_access(tx, Operation::Create)
            .await?;
        check_companies(this.env(), tx, &vals_list).await?;
        new_same_selector(tx, &vals_list).await?;
        let created: Records = sup.call(tx, this.as_records(), (vals_list,)).await?;
        created.check_access(tx, Operation::Create).await?;
        check_revisions(this.env(), tx, &created.ids()).await?;
        Ok(created)
    }

    /// Only `active` changes; reactivation checks the revision again.
    #[method(override)]
    async fn write(
        this: &RecordSet<IrApprovalPolicy>,
        tx: &mut Transaction,
        vals: RawVals,
        sup: Super,
    ) -> Result<()> {
        if this.is_empty() {
            return sup.call(tx, this.as_records(), (vals,)).await;
        }
        let records = this.as_records();
        records.check_access(tx, Operation::Write).await?;
        if vals.iter().any(|(name, _)| name != "active") || vals.iter_commands().next().is_some() {
            return Err(Error::validation(IMMUTABLE));
        }
        let activates = match vals.get("active") {
            None | Some(Cell::Null) => false,
            Some(Cell::Bool(active)) => *active,
            // the ORM takes no other form for a boolean (it refuses Int(1) as a programming error)
            Some(_) => {
                return Err(Error::validation(
                    "The archive state of an approval policy revision is true or false.",
                ));
            }
        };
        if activates {
            same_selector(tx, &records.ids()).await?;
        }
        sup.call(tx, records, (vals,)).await?;
        if activates {
            check_revisions(this.env(), tx, &records.ids()).await?;
        }
        Ok(())
    }

    /// Revisions are evidence of what was configured: never deleted.
    #[method(override)]
    async fn unlink(
        this: &RecordSet<IrApprovalPolicy>,
        tx: &mut Transaction,
        sup: Super,
    ) -> Result<()> {
        if this.is_empty() {
            return sup.call(tx, this.as_records(), ()).await;
        }
        this.as_records()
            .check_access(tx, Operation::Unlink)
            .await?;
        Err(Error::validation(UNDELETABLE))
    }
}

/// The users a decision must never come from: the subject and the original principal of an
/// impersonation (B7: by `res.users` identity).
fn requesters(env: &Env) -> Result<Vec<UserId>> {
    let principal = env.principal();
    let mut ids = vec![principal.subject()];
    match principal.actor() {
        None | Some(Actor::ApiKey { .. }) => {}
        Some(Actor::Impersonator(original)) => ids.push(original.subject()),
        Some(_) => {
            return Err(Error::programming(
                "An approval policy cannot be resolved for a user principal with this actor kind.",
            ));
        }
    }
    Ok(ids)
}

/// The active revision the company `company` selects among `candidates`, by B8's order: exact
/// company first, then method, model, wildcard. Two revisions at the best rank refuse.
fn select(candidates: &[Stored], company: i64) -> Result<Option<&Stored>> {
    let rank = |r: &Stored| (r.company.is_some(), r.level());
    let applicable: Vec<&Stored> = candidates
        .iter()
        .filter(|r| r.company.is_none_or(|c| c == company))
        .collect();
    let Some(top) = applicable.iter().map(|r| rank(r)).max() else {
        return Ok(None);
    };
    let mut best = applicable.into_iter().filter(|r| rank(r) == top);
    let chosen = best.next();
    if let (Some(first), Some(second)) = (chosen, best.next()) {
        return Err(Error::validation(format!(
            "Approval policy revisions {} and {} select the same call with equal specificity.",
            first.label(),
            second.label()
        )));
    }
    Ok(chosen)
}

fn named(choice: Option<&Stored>) -> String {
    choice.map_or_else(
        || "no revision".to_owned(),
        |r| format!("revision {}", r.id),
    )
}

/// The refusal a requester who may not read the revisions involved gets. It carries no error
/// source: the detail stays in the server log only (`erp::approvals`), so no debug rendering of
/// the error can show it (review r1, D-c; amendment 2026-11-13).
pub const UNAPPROVABLE: &str =
    "This action needs an approval that is not set up correctly; ask an administrator.";

/// A refusal of [`resolve`]: one caused by the configured revisions (shown in full only to who
/// may read them) or any other error.
enum Refusal {
    Config { revisions: Vec<i64>, detail: Error },
    Other(Error),
}

impl From<Error> for Refusal {
    fn from(error: Error) -> Self {
        Self::Other(error)
    }
}

fn config(revisions: Vec<i64>, detail: Error) -> Refusal {
    Refusal::Config { revisions, detail }
}

/// The kinds of principal whose calls pass the approval phase (and acceptance).
fn admit(kind: PrincipalKind) -> Result<()> {
    match kind {
        PrincipalKind::User => Ok(()),
        PrincipalKind::Agent | PrincipalKind::Service => Err(Error::validation(AGENT_UNSUPPORTED)),
        kind => Err(Error::programming(format!(
            "Approval policies apply to user principals only; a {} call does not pass the \
             approval phase.",
            kind.as_str()
        ))),
    }
}

/// The method's current effect, from the registry's RPC exposure (never the caller's).
fn current_effect(env: &Env, tx: &Transaction, call: &ApprovalCall) -> Result<Effect> {
    let exposed = env
        .registry()
        .model_id(call.model())
        .and_then(|model| tx.exposure().get(model, call.method()));
    match exposed {
        Some(Exposure::Exposed(exposed)) => Ok(exposed.effect()),
        _ => Err(Error::programming(format!(
            "'{}.{}' is not an RPC method; approval policies are resolved for RPC entry calls \
             only.",
            call.model(),
            call.method()
        ))),
    }
}

/// The policy outcome of `call` made by `env`'s principal, in `tx` (D2, B7, B8 and
/// the amendment of 2026-11-13).
///
/// The effect is the method's current effect in the registry. The companies are the call's
/// validated active companies ([`Transaction::companies`]), plus, for a widened call
/// ([`ApprovalCall::widened`]: a create, a record without a company, no target record), every
/// company the subject is allowed in, plus the companies the call names
/// ([`ApprovalCall::target_companies`], supplied by the dispatch). Every one must select the same
/// revision, so neither choosing the current company nor dropping one from the active set selects
/// a weaker revision. A method revision written for another effect than the method has now (an
/// override raised it) refuses the call: it is never passed over. Revisions are read with plain
/// SQL, whatever the caller may see; a refusal caused by them names them only to a caller who may
/// read them in its active companies ([`UNAPPROVABLE`] otherwise, without the detail). For
/// Approve, the eligible set is computed now ([`eligible_approvers`]) over every company resolved,
/// without the subject and the impersonating user: for a widened call an approver must be allowed
/// in every company the requester is allowed in.
///
/// ```no_run
/// # async fn demo(env: &erp_sdk::Env, tx: &mut erp_sdk::Transaction) -> erp_sdk::Result<()> {
/// use erp_addon_base::ir_approval_policy::{ApprovalCall, PolicyOutcome, resolve};
/// // one partner of company 1 and one without a company: the call is widened
/// let record_companies = [Some(erp_sdk::CompanyId::new(1)?), None];
/// let call = ApprovalCall::on_records("res.partner", "write", &record_companies)?;
/// match resolve(env, tx, &call).await? {
///     PolicyOutcome::Unmanaged | PolicyOutcome::Allow { .. } => {}
///     PolicyOutcome::Approve { revision, approvers } => {
///         let _ = (revision, approvers);
///     }
///     PolicyOutcome::Deny { .. } => {}
/// }
/// # Ok(())
/// # }
/// ```
///
/// # Errors
///
/// `Validation` for an agent or service principal (not supported until accepts their
/// parts), a call without companies or with more than [`MAX_COMPANIES`] (a widened call counts
/// every company the subject is allowed in); for companies that
/// select different revisions, two revisions of equal specificity, a stale method revision, a
/// stored revision with an unknown or unsupported value, and an Approve revision without any
/// eligible approver or with more than [`MAX_APPROVERS`] (the detail, or [`UNAPPROVABLE`] for a
/// caller who may not read the revisions); `Programming` for `system`, `public` and `link`
/// principals, which never pass the approval phase, and for a method that is not
/// an RPC method; the errors of [`Transaction::companies`] and of the statements.
pub async fn resolve(
    env: &Env,
    tx: &mut Transaction,
    call: &ApprovalCall,
) -> Result<PolicyOutcome> {
    match decide(env, tx, call).await {
        Ok(outcome) => Ok(outcome),
        Err(Refusal::Other(error)) => Err(error),
        Err(Refusal::Config { revisions, detail }) => {
            // read access as the record rules see it in the call's validated active companies
            let active = tx.companies(env).await?;
            let scoped = env.with_context([(
                "allowed_company_ids",
                erp_sdk::Value::list(active.iter().map(|c| erp_sdk::Value::Int(c.get()))),
            )]);
            let readable = scoped
                .records("ir.approval.policy")?
                .browse(revisions.iter().copied())?
                .has_access(tx, Operation::Read)
                .await?;
            if readable {
                return Err(detail);
            }
            tracing::warn!(target: "erp::approvals", detail = %detail, "an approval policy refused a call");
            // no source: the RPC debug renderer walks it (amendment 2026-11-13)
            Err(Error::validation(UNAPPROVABLE))
        }
    }
}

async fn decide(
    env: &Env,
    tx: &mut Transaction,
    call: &ApprovalCall,
) -> std::result::Result<PolicyOutcome, Refusal> {
    admit(env.principal().kind())?;
    let excluded = requesters(env)?;
    let effect = current_effect(env, tx, call)?;
    let mut companies = tx.companies(env).await?;
    if call.widened() {
        companies.extend(allowed_companies(env, tx).await?);
    }
    companies.extend_from_slice(call.target_companies());
    companies.sort_unstable();
    companies.dedup();
    let company_ids = bounded(&companies)?;
    // method revisions of any effect: one of another effect than the method's is stale
    let sql = format!(
        "SELECT {COLUMNS} FROM ir_approval_policy p WHERE p.active AND p.principal_kind = 'user' \
         AND (p.effect = $1 OR (p.model = $3 AND p.method = $4)) \
         AND (p.company_id IS NULL OR p.company_id = ANY($2::int8[])) \
         AND (p.model IS NULL OR p.model = $3) AND (p.method IS NULL OR p.method = $4) ORDER BY p.id"
    );
    let rows = tx
        .query(
            &sql,
            &[
                &effect.as_str(),
                &company_ids,
                &call.model(),
                &call.method(),
            ],
        )
        .await?;
    let all: Vec<i64> = rows
        .iter()
        .map(|row| row.try_get(0).map_err(|_| unreadable("id")))
        .collect::<Result<_>>()?;
    let candidates = rows
        .iter()
        .map(stored)
        .collect::<Result<Vec<_>>>()
        .map_err(|e| config(all.clone(), e))?;
    if let Some(stale) = candidates.iter().find(|r| r.effect != effect) {
        return Err(config(
            vec![stale.id],
            stale.refuse(&format!(
                "it was written for effect {}, but '{}.{}' now has effect {}; archive it and \
                 create a revision for the current effect.",
                stale.effect.as_str(),
                call.model(),
                call.method(),
                effect.as_str()
            )),
        ));
    }
    let (first, rest) = company_ids
        .split_first()
        .ok_or_else(|| Error::validation(NO_COMPANIES))?;
    let choice = select(&candidates, *first).map_err(|e| config(all.clone(), e))?;
    for company in rest {
        let other = select(&candidates, *company).map_err(|e| config(all.clone(), e))?;
        if other.map(|r| r.id) != choice.map(|r| r.id) {
            let why = if call.widened() {
                "this call is resolved for every company you are allowed in, so these revisions \
                 must agree"
            } else {
                "call with one active company"
            };
            return Err(config(
                choice.iter().chain(other.iter()).map(|r| r.id).collect(),
                Error::validation(format!(
                    "The companies of this call select different approval policy revisions \
                     (company {first}: {}, company {company}: {}); {why}.",
                    named(choice),
                    named(other)
                )),
            ));
        }
    }
    let Some(chosen) = choice else {
        return Ok(PolicyOutcome::Unmanaged);
    };
    let revision = PolicyRevisionId::new(chosen.id)?;
    Ok(match chosen.outcome {
        Outcome::Allow => PolicyOutcome::Allow { revision },
        Outcome::Deny => PolicyOutcome::Deny { revision },
        Outcome::Approve => {
            let approvers = eligible_approvers(tx, revision, &excluded, &companies)
                .await
                .map_err(|e| config(vec![chosen.id], e))?;
            if approvers.is_empty() {
                return Err(config(
                    vec![chosen.id],
                    chosen.refuse(
                        "no eligible approver other than the requester is configured for this \
                         call.",
                    ),
                ));
            }
            PolicyOutcome::Approve {
                revision,
                approvers,
            }
        }
    })
}

/// Every company the subject of `env` is allowed in (`res_company_users_rel`), read with plain SQL
/// in `tx`. The same source as the principal resolver's company list
/// (`erp_security::resolver::SUBJECT_SQL`), without its filter on archived companies: a superset,
/// which can only add refusals, never turn one into `Unmanaged`. The user's default company
/// (`res_users.company_id`) is read too: base keeps it among the rows
/// (`res_users::check_user_companies`), but a row written outside the ORM cannot drop it from the
/// set (review RVFIX r1, finding 2). The subject is the user whose
/// authority the call uses (the impersonated user under impersonation). At most
/// [`MAX_COMPANIES`] + 1 rows are read: one more than the bound is enough for [`bounded`] to refuse.
async fn allowed_companies(env: &Env, tx: &mut Transaction) -> Result<Vec<CompanyId>> {
    let limit = i64::try_from(MAX_COMPANIES + 1)
        .map_err(|_| Error::internal("The company bound does not fit a statement limit."))?;
    let rows = tx
        .query(
            "SELECT c.cid FROM (SELECT r.cid::int8 AS cid FROM res_company_users_rel r \
             WHERE r.user_id = $1::int8 UNION SELECT u.company_id::int8 FROM res_users u \
             WHERE u.id = $1::int8 AND u.company_id IS NOT NULL) c ORDER BY 1 LIMIT $2::int8",
            &[&env.principal().subject().get(), &limit],
        )
        .await?;
    rows.iter()
        .map(|row| {
            let id: i64 = row
                .try_get(0)
                .map_err(|_| unreadable_rel("res_company_users_rel.cid"))?;
            Ok(CompanyId::new(id)?)
        })
        .collect()
}

fn unreadable_rel(column: &str) -> Error {
    Error::internal(format!("{column} holds a value of an unexpected type."))
}

const NO_COMPANIES: &str =
    "An approval policy is resolved for a call with at least one active company.";

fn bounded(companies: &[CompanyId]) -> Result<Vec<i64>> {
    if companies.is_empty() {
        return Err(Error::validation(NO_COMPANIES));
    }
    if companies.len() > MAX_COMPANIES {
        return Err(Error::validation(format!(
            "An approval policy is resolved for at most {MAX_COMPANIES} companies."
        )));
    }
    Ok(companies.iter().map(|c| c.get()).collect())
}

/// The humans who may decide a request under the Approve revision `revision` now: its named users
/// and the current members of its groups (through implied groups), without `excluded`, archived,
/// share and superuser accounts, and users not allowed in every one of `companies`. Group
/// membership is read when this runs, not when the revision was written (the
/// decision checks current eligibility).
///
/// # Errors
///
/// `Validation` for an unknown revision, one that is not an Approve revision, an empty or too long
/// company list, and more than [`MAX_APPROVERS`] eligible users; the statements' errors.
pub async fn eligible_approvers(
    tx: &mut Transaction,
    revision: PolicyRevisionId,
    excluded: &[UserId],
    companies: &[CompanyId],
) -> Result<EligibleApprovers> {
    let company_ids = bounded(companies)?;
    let rows = tx
        .query(
            "SELECT p.outcome, ARRAY(SELECT g.group_id::int8 FROM ir_approval_policy_groups_rel g \
             WHERE g.policy_id = p.id) FROM ir_approval_policy p WHERE p.id = $1::int8",
            &[&revision.get()],
        )
        .await?;
    let Some(row) = rows.first() else {
        return Err(Error::validation(format!(
            "Approval policy revision {} does not exist.",
            revision.get()
        )));
    };
    let outcome: String = row.try_get(0).map_err(|_| unreadable("outcome"))?;
    if outcome != "approve" {
        return Err(Error::validation(format!(
            "Approval policy revision {} is not an Approve revision.",
            revision.get()
        )));
    }
    let groups: Vec<i64> = row.try_get(1).map_err(|_| unreadable("approver groups"))?;
    let holding: Vec<i64> = Graph::load(tx).await?.down(groups).into_iter().collect();
    let excluded: Vec<i64> = excluded.iter().map(|u| u.get()).collect();
    let limit = i64::try_from(MAX_APPROVERS + 1)
        .map_err(|_| Error::internal("The approver bound does not fit a statement limit."))?;
    let rows = tx
        .query(
            "SELECT u.id::int8 FROM res_users u \
             WHERE u.active AND NOT u.share AND u.id <> $5::int8 AND u.id <> ALL($2::int8[]) \
             AND (EXISTS (SELECT 1 FROM ir_approval_policy_users_rel r \
                          WHERE r.policy_id = $1::int8 AND r.user_id = u.id) \
                  OR EXISTS (SELECT 1 FROM res_groups_users_rel m \
                             WHERE m.uid = u.id AND m.gid = ANY($3::int8[]))) \
             AND NOT EXISTS (SELECT 1 FROM unnest($4::int8[]) AS c(cid) WHERE NOT EXISTS \
                 (SELECT 1 FROM res_company_users_rel a WHERE a.user_id = u.id AND a.cid = c.cid)) \
             ORDER BY u.id LIMIT $6::int8",
            &[
                &revision.get(),
                &excluded,
                &holding,
                &company_ids,
                &SUPERUSER_ID.get(),
                &limit,
            ],
        )
        .await?;
    if rows.len() > MAX_APPROVERS {
        return Err(Error::validation(format!(
            "Approval policy revision {} has more than {MAX_APPROVERS} eligible approvers.",
            revision.get()
        )));
    }
    let mut ids = Vec::with_capacity(rows.len());
    for row in &rows {
        let id: i64 = row.try_get(0).map_err(|_| unreadable("res_users.id"))?;
        ids.push(UserId::new(id)?);
    }
    Ok(EligibleApprovers(ids.into_boxed_slice()))
}

#[cfg(test)]
mod tests {
    use erp_sdk::ErrorKind;

    use super::*;

    /// builder cannot build agent, service, public or link principals before and
    /// )
    #[test]
    fn only_user_principals_pass_the_approval_phase() {
        assert!(admit(PrincipalKind::User).is_ok());
        for kind in [PrincipalKind::Agent, PrincipalKind::Service] {
            let refused = admit(kind).err();
            assert_eq!(
                refused.as_ref().map(|e| (e.kind(), e.message())),
                Some((ErrorKind::Validation, AGENT_UNSUPPORTED)),
                "{kind:?}"
            );
        }
        for kind in [
            PrincipalKind::System,
            PrincipalKind::Public,
            PrincipalKind::Link,
        ] {
            let refused = admit(kind).err();
            let message = format!(
                "Approval policies apply to user principals only; a {} call does not pass the \
                 approval phase.",
                kind.as_str()
            );
            assert_eq!(
                refused.as_ref().map(|e| (e.kind(), e.message())),
                Some((ErrorKind::Programming, message.as_str())),
                "{kind:?}"
            );
        }
    }
}
