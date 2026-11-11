//! The principal resolver (§2.3,
//! §3.2, §3.4): reads a subject's row, direct groups and companies in **one** statement, at
//! transaction start for the principal the transaction begins with, and again (one statement for
//! all of them, `uid = ANY($1)`) for every subject bound in a transaction after a membership
//! change in it.
//!
//! Membership (`res_groups_users_rel`, `res_company_users_rel`, `res_users.company_id`,
//! `active`) is not in the policy snapshot, so it needs no generation: it is read fresh every
//! time. The groups are closed through the snapshot ([`AccessPolicy::groups`], which refuses a
//! group it does not know); the companies are validated by [`PrincipalBuilder`].
//!
//! An inactive or missing subject is an `Access` error naming the principal kind (R-S15), never a
//! principal without groups.

use std::fmt;
use std::num::NonZeroU64;
use std::sync::Arc;

use erp_fields::{Error, ErrorKind, FieldType, Result};
use erp_orm::{
    AccessPolicy, BoxFuture, CompanyId, GroupId, Principal, PrincipalBuilder, PrincipalHint,
    PrincipalKind, PrincipalResolver, Resolution, ResolverStatement, Transaction, UserId,
};
use erp_registry::Registry;
use erp_sql::param::{Param, Scalar};
use tokio_postgres::Row;

/// The subject a transaction begins with: its row, direct groups and companies (`$1`: its id).
/// This is the statement of [`Resolver::id_order`], which does not know the registry: the companies
/// are in id order and archived companies are not filtered. [`Resolver::for_registry`] builds the
/// statement with the default company order and the archived filter (see there).
pub const SUBJECT_SQL: &str = "\
SELECT u.id::int8, u.active, u.company_id::int8,
       ARRAY(SELECT r.gid::int8 FROM res_groups_users_rel r WHERE r.uid = u.id ORDER BY 1),
       ARRAY(SELECT c.cid::int8 FROM res_company_users_rel c WHERE c.user_id = u.id ORDER BY 1)
  FROM res_users u
 WHERE u.id = $1::int8";

/// The subjects bound in a transaction, re-read after a membership change (`$1`: their ids).
/// Like [`SUBJECT_SQL`], the statement of [`Resolver::id_order`].
pub const SUBJECTS_SQL: &str = "\
SELECT u.id::int8, u.active, u.company_id::int8,
       ARRAY(SELECT r.gid::int8 FROM res_groups_users_rel r WHERE r.uid = u.id ORDER BY 1),
       ARRAY(SELECT c.cid::int8 FROM res_company_users_rel c WHERE c.user_id = u.id ORDER BY 1)
  FROM res_users u
 WHERE u.id = ANY($1::int8[])";

/// `sequence`, `name` and `active` are used only when `res.company` stores them.
fn companies_array(active: bool, sequence: bool, name: bool) -> String {
    let mut order = String::new();
    if sequence {
        order.push_str("rc.sequence, ");
    }
    if name {
        order.push_str("rc.name, ");
    }
    order.push_str("rc.id");
    let filter = if active {
        " AND (rc.active IS TRUE OR rc.id = u.company_id)"
    } else {
        ""
    };
    format!(
        "ARRAY(SELECT c.cid::int8 FROM res_company_users_rel c \
         JOIN res_company rc ON rc.id = c.cid WHERE c.user_id = u.id{filter} ORDER BY {order})"
    )
}

/// The company array of [`SUBJECT_SQL`] (id order, archived companies not filtered).
const ID_ORDER_COMPANIES: &str =
    "ARRAY(SELECT c.cid::int8 FROM res_company_users_rel c WHERE c.user_id = u.id ORDER BY 1)";

/// The five subject columns over the alias `u`.
fn subject_columns(companies: &str) -> String {
    format!(
        "u.id::int8, u.active, u.company_id::int8,\n       \
         ARRAY(SELECT r.gid::int8 FROM res_groups_users_rel r WHERE r.uid = u.id ORDER BY 1),\n       \
         {companies}"
    )
}

/// A subject statement over `where_clause`.
fn subject_statement(companies: &str, where_clause: &str) -> String {
    format!(
        "SELECT {}\n  FROM res_users u\n WHERE {where_clause}",
        subject_columns(companies)
    )
}

/// Why a principal cannot be resolved.
///
/// Converts into [`erp_fields::Error`] with the kind of [`ResolveError::kind`] and the same
/// message, with this error as its source.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ResolveError {
    /// The subject's `res.users` row does not exist.
    MissingSubject {
        /// The principal's kind.
        kind: PrincipalKind,
        /// The subject.
        user: UserId,
    },
    /// The subject is archived (`active` is not true).
    InactiveSubject {
        /// The principal's kind.
        kind: PrincipalKind,
        /// The subject.
        user: UserId,
    },
    /// A principal the core builds itself (`System`), never read from the database.
    NotResolved {
        /// The principal's kind.
        kind: PrincipalKind,
    },
    /// A principal kind a later slice resolves.
    Unsupported {
        /// The principal's kind.
        kind: PrincipalKind,
        /// The board task that adds it.
        task: &'static str,
    },
}

impl ResolveError {
    /// `Access` for a subject that cannot act, `Programming` for a principal that is not resolved
    /// here.
    pub fn kind(&self) -> ErrorKind {
        match self {
            Self::MissingSubject { .. } | Self::InactiveSubject { .. } => ErrorKind::Access,
            Self::NotResolved { .. } | Self::Unsupported { .. } => ErrorKind::Programming,
        }
    }
}

impl fmt::Display for ResolveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingSubject { kind, user } => write!(
                f,
                "The {} principal cannot act: user {} does not exist.",
                kind.as_str(),
                user.get()
            ),
            Self::InactiveSubject { kind, user } => write!(
                f,
                "The {} principal cannot act: user {} is archived.",
                kind.as_str(),
                user.get()
            ),
            Self::NotResolved { kind } => write!(
                f,
                "the {} principal is built by the core and never resolved from the database",
                kind.as_str()
            ),
            Self::Unsupported { kind, task } => write!(
                f,
                "{} principals are not resolved until {task}",
                kind.as_str()
            ),
        }
    }
}

impl std::error::Error for ResolveError {}

impl From<ResolveError> for Error {
    fn from(error: ResolveError) -> Self {
        Error::new(error.kind(), error.to_string()).with_source(error)
    }
}

/// One subject as the resolver's statements read it.
#[derive(Debug)]
struct SubjectRow {
    id: i64,
    active: bool,
    company: Option<CompanyId>,
    groups: Vec<GroupId>,
    companies: Vec<CompanyId>,
}

impl SubjectRow {
    fn read(row: &Row) -> Result<Self> {
        let id: i64 = get(row, 0)?;
        let active = get::<Option<bool>>(row, 1)? == Some(true);
        let company = get::<Option<i64>>(row, 2)?
            .map(CompanyId::new)
            .transpose()?;
        let groups = get::<Vec<i64>>(row, 3)?
            .into_iter()
            .map(GroupId::new)
            .collect::<std::result::Result<Vec<_>, _>>()?;
        let companies = get::<Vec<i64>>(row, 4)?
            .into_iter()
            .map(CompanyId::new)
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(Self {
            id,
            active,
            company,
            groups,
            companies,
        })
    }

    /// The subject's resolution at `epoch`: groups closed under `policy`, all its companies.
    fn resolution(
        &self,
        kind: PrincipalKind,
        user: UserId,
        policy: &dyn AccessPolicy,
        epoch: NonZeroU64,
    ) -> Result<Resolution> {
        if !self.active {
            return Err(ResolveError::InactiveSubject { kind, user }.into());
        }
        let groups = policy.groups(&self.groups)?;
        Ok(
            Resolution::in_id_order(groups, self.companies.iter().copied(), epoch)?
                .with_default_order(self.allowed())?,
        )
    }

    fn allowed(&self) -> Vec<CompanyId> {
        let mut allowed: Vec<CompanyId> = self.company.into_iter().collect();
        allowed.extend(
            self.companies
                .iter()
                .copied()
                .filter(|c| Some(*c) != self.company),
        );
        allowed
    }
}

fn get<'r, T: tokio_postgres::types::FromSql<'r>>(row: &'r Row, idx: usize) -> Result<T> {
    row.try_get(idx).map_err(|e| {
        Error::new(
            ErrorKind::Database,
            format!("could not decode column {idx} of a principal row: {e}"),
        )
        .with_source(e)
    })
}

/// The epoch of the resolution a transaction begins with.
const FIRST_EPOCH: NonZeroU64 = NonZeroU64::MIN;

/// The resolver of user principals (stateless; one per `Orm` through
/// [`erp_orm::OrmBuilder::resolver`]).
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct Resolver {
    /// The statements built for a registry; `None`: [`SUBJECT_SQL`] and [`SUBJECTS_SQL`].
    statements: Option<Arc<Statements>>,
}

/// The subject statements of one registry.
#[derive(Debug)]
struct Statements {
    one: String,
    many: String,
    companies: String,
}

impl Resolver {
    /// The resolver with the fixed statements [`SUBJECT_SQL`] and [`SUBJECTS_SQL`]: companies in
    /// id order, archived companies not filtered. It is **not** the company order
    /// rule 5 (main company first, then `sequence`, `name`, `id`, active ones): it exists for
    /// registries without a `res.company` model and for tests of the statements; real
    /// installations build the resolver with [`Resolver::for_registry`]. There is deliberately no
    /// `Default`.
    pub fn id_order() -> Self {
        Self { statements: None }
    }

    /// A column `res.company` does not store (`active`, `sequence`, `name`)
    /// is left out of the filter and the order; with none of them the result is the id order
    /// of [`Resolver::id_order`]. Still one statement per resolution.
    ///
    /// # Errors
    ///
    /// `Programming` when `res.company` declares `active`, `sequence` or `name` as a type the
    /// statement cannot use (not a stored boolean, integer and char).
    pub fn for_registry(registry: &Registry) -> Result<Self> {
        let Some(company) = registry.model("res.company") else {
            return Ok(Self::id_order());
        };
        let stored = |name: &str, want: fn(&FieldType) -> bool, what: &str| -> Result<bool> {
            match company.field(name) {
                None => Ok(false),
                Some(f) if f.store && want(&f.ty) => Ok(true),
                Some(f) => Err(Error::programming(format!(
                    "res.company.{name} must be a stored {what} for the principal resolver, \
                     found {:?} (store = {})",
                    f.ty, f.store
                ))),
            }
        };
        let active = stored("active", |t| matches!(t, FieldType::Boolean), "boolean")?;
        let sequence = stored("sequence", |t| matches!(t, FieldType::Integer), "integer")?;
        let name = stored("name", |t| matches!(t, FieldType::Char { .. }), "char")?;
        let companies = companies_array(active, sequence, name);
        Ok(Self {
            statements: Some(Arc::new(Statements {
                one: subject_statement(&companies, "u.id = $1::int8"),
                many: subject_statement(&companies, "u.id = ANY($1::int8[])"),
                companies,
            })),
        })
    }

    fn one_sql(&self) -> &str {
        self.statements.as_ref().map_or(SUBJECT_SQL, |s| &s.one)
    }

    fn many_sql(&self) -> &str {
        self.statements.as_ref().map_or(SUBJECTS_SQL, |s| &s.many)
    }

    /// The five subject columns of this resolver's statements over the alias `u` (`res_users`),
    /// for a statement that joins the subject to a credential row (`auth::session`): the
    /// columns come first, so [`Resolver::user_from_row`] reads them at indices 0 to 4.
    pub(crate) fn subject_columns(&self) -> String {
        subject_columns(
            self.statements
                .as_ref()
                .map_or(ID_ORDER_COMPANIES, |s| s.companies.as_str()),
        )
    }

    /// The user principal of the subject columns at the start of `row`, its groups closed under
    /// `policy`; `Ok(None)` for an archived subject (the caller refuses the credential).
    ///
    /// # Errors
    ///
    /// Undecodable columns, an id that is not a user id, a group the policy does not know.
    pub(crate) fn user_from_row(row: &Row, policy: &dyn AccessPolicy) -> Result<Option<Principal>> {
        let subject = SubjectRow::read(row)?;
        if !subject.active {
            return Ok(None);
        }
        let user = UserId::new(subject.id)?;
        let resolution = subject.resolution(PrincipalKind::User, user, policy, FIRST_EPOCH)?;
        Ok(Some(
            PrincipalBuilder::user(user)
                .resolution(resolution)
                .companies(subject.allowed())
                .build()?,
        ))
    }

    /// The kind of `hint` if this resolver reads it.
    fn user_kind(hint: &PrincipalHint) -> Result<PrincipalKind> {
        match hint.kind() {
            PrincipalKind::User => Ok(PrincipalKind::User),
            kind @ PrincipalKind::System => Err(ResolveError::NotResolved { kind }.into()),
            kind @ (PrincipalKind::Agent | PrincipalKind::Service) => {
                Err(ResolveError::Unsupported {
                    kind,
                    task: "a later release",
                }
                .into())
            }
            // public and link principals, and any kind a later slice adds with them
            kind => Err(ResolveError::Unsupported {
                kind,
                task: "a later release",
            }
            .into()),
        }
    }

    /// Re-read `subjects` in `tx` (its own uncommitted view) with one statement, close their
    /// groups under `policy` and stamp the resolutions `epoch`, in the order given.
    ///
    /// # Errors
    ///
    /// `Access` for a subject that no longer exists or is archived ([`ResolveError`]); the
    /// policy's error for a group it does not know; the statement's error.
    pub async fn resolve_subjects(
        &self,
        tx: &mut Transaction,
        subjects: &[UserId],
        policy: &dyn AccessPolicy,
        epoch: NonZeroU64,
    ) -> Result<Vec<Resolution>> {
        let ids: Vec<i64> = subjects.iter().map(|s| s.get()).collect();
        let rows = tx.query(self.many_sql(), &[&ids]).await?;
        let read = rows
            .iter()
            .map(SubjectRow::read)
            .collect::<Result<Vec<SubjectRow>>>()?;
        subjects
            .iter()
            .map(|&user| {
                let row = read.iter().find(|r| r.id == user.get()).ok_or(
                    ResolveError::MissingSubject {
                        kind: PrincipalKind::User,
                        user,
                    },
                )?;
                row.resolution(PrincipalKind::User, user, policy, epoch)
            })
            .collect()
    }
}

impl PrincipalResolver for Resolver {
    fn statement(&self, hint: &PrincipalHint) -> Result<ResolverStatement> {
        Self::user_kind(hint)?;
        Ok(ResolverStatement::new(
            self.one_sql().to_owned(),
            vec![Param::Scalar(Scalar::Int8(hint.subject().get()))],
        ))
    }

    fn finish(
        &self,
        hint: &PrincipalHint,
        rows: &[Row],
        policy: &dyn AccessPolicy,
    ) -> Result<Principal> {
        let kind = Self::user_kind(hint)?;
        let user = hint.subject();
        let row = match rows {
            [] => return Err(ResolveError::MissingSubject { kind, user }.into()),
            [row] => SubjectRow::read(row)?,
            more => {
                return Err(Error::programming(format!(
                    "the principal statement returned {} rows for user {}",
                    more.len(),
                    user.get()
                )));
            }
        };
        if row.id != user.get() {
            return Err(Error::programming(format!(
                "the principal statement for user {} returned user {}",
                user.get(),
                row.id
            )));
        }
        let resolution = row.resolution(kind, user, policy, FIRST_EPOCH)?;
        Ok(PrincipalBuilder::user(user)
            .resolution(resolution)
            .companies(row.allowed())
            .build()?)
    }

    /// Re-read `subjects` in `tx` with one statement and close their groups with the snapshot
    /// `tx` decides with now (its private one after a policy write). The engine numbers the
    /// resolutions it records (`ResolutionEpoch`), so the epoch given here is the first one.
    fn re_resolve<'a>(
        &'a self,
        tx: &'a mut Transaction,
        subjects: &'a [UserId],
    ) -> BoxFuture<'a, Result<Vec<Resolution>>> {
        Box::pin(async move {
            let policy = Arc::clone(tx.policy());
            self.resolve_subjects(tx, subjects, policy.as_ref(), FIRST_EPOCH)
                .await
        })
    }
}
