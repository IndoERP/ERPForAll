//! Job kinds: the extension point of the queue.
//!
//! A kind is declared once with [`register_job_kind!`](crate::register_job_kind) into the
//! linkme slice [`JOB_KINDS`]; the ORM builder validates the whole slice (names, ownership,
//! bounds, the principal policy and the kinds a binary requires) before it publishes an
//! [`Orm`](crate::Orm). A kind's work is either a model method run as one transactional chunk
//! ([`HandlerRef::Step`], extended by other addons with super like any method) or a native
//! handler with the full [`JobContext`](super::JobContext) ([`HandlerRef::Native`], core crates
//! and native addons only).

use std::num::NonZeroU32;
use std::ops::RangeInclusive;

use erp_fields::{Error, Result};

use super::{Backoff, JobHandler, Priority};

/// How a kind's work is run.
#[derive(Clone, Copy)]
#[non_exhaustive]
pub enum HandlerRef {
    /// One transactional chunk per call of `model.method`, dispatched through the override
    /// chain (native and WebAssembly overrides alike). The method takes one
    /// [`StepArgs`](super::StepArgs) and returns a [`StepOutcome`](super::StepOutcome).
    Step {
        /// The model that owns the method.
        model: &'static str,
        /// The method name.
        method: &'static str,
    },
    /// Full control: chunks and external steps. For core crates and native addons only; a
    /// handler is compiled code, so a WebAssembly or data-only addon cannot supply one.
    Native(&'static dyn JobHandler),
}

impl std::fmt::Debug for HandlerRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Step { model, method } => f
                .debug_struct("Step")
                .field("model", model)
                .field("method", method)
                .finish(),
            Self::Native(_) => f.write_str("Native(..)"),
        }
    }
}

/// Whether the job's own principal runs it or the core does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum PrincipalPolicy {
    /// The principal stored at enqueue (the default).
    Caller,
    /// The `System` principal: maintenance kinds of core crates and of the `base` addon only
    /// (registry publication refuses it for any other addon).
    System,
}

/// Whether finished jobs leave a row in `erp_job_run`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum HistoryPolicy {
    /// One history row per finished job (done, cancelled, dead-letter expired or discarded).
    Keep,
    /// No history (a delivered mail, say).
    None,
}

/// Which chunk limits a kind's transactions get (`jobs.chunk.<class>.*`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum TimeoutClass {
    /// 5 s lock, 30 s statement, 60 s transaction by default.
    Default,
    /// Long-running upkeep: 5 s, 300 s, 600 s by default.
    Maintenance,
}

/// Transaction isolation of a kind's chunks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum KindIsolation {
    /// The ORM default.
    RepeatableRead,
    /// Allowed for maintenance-class kinds only (attachment migration needs it).
    ReadCommitted,
}

/// How an outbox trigger groups the events it turns into jobs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum TriggerCoalesce {
    /// One job per kind of event.
    PerKind,
    /// One job per record.
    PerRecord,
}

/// An outbox event that enqueues a job of its kind (4). Declaring one is refused at
/// registry publication until the consumer exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct OutboxTrigger {
    /// The model whose events trigger the job.
    pub model: &'static str,
    /// `create`, `write` or `unlink`.
    pub kinds: &'static [&'static str],
    /// Changed fields that trigger it; empty means any change.
    pub fields: &'static [&'static str],
    /// How events are grouped.
    pub coalesce: TriggerCoalesce,
}

impl OutboxTrigger {
    /// A trigger on `model`'s events.
    pub const fn new(
        model: &'static str,
        kinds: &'static [&'static str],
        fields: &'static [&'static str],
        coalesce: TriggerCoalesce,
    ) -> Self {
        Self {
            model,
            kinds,
            fields,
            coalesce,
        }
    }
}

/// The declaration of one job kind. Build it with [`JobKindDecl::new`] and the `const` setters,
/// register it with [`register_job_kind!`](crate::register_job_kind).
///
/// ```
/// use std::time::Duration;
/// use erp_orm::jobs::{Backoff, HandlerRef, JobKindDecl, Priority};
///
/// static NIGHTLY: JobKindDecl = JobKindDecl::new(
///     "stock",
///     "stock.recount",
///     HandlerRef::Step { model: "stock.recount", method: "run_step" },
/// )
/// .version(2)
/// .accepts(1..=2)
/// .backoff(Backoff::Exponential { base: Duration::from_secs(30), cap: Duration::from_secs(1800) })
/// .max_attempts(5)
/// .priority(Priority::DEFAULT);
/// assert_eq!(NIGHTLY.name, "stock.recount");
/// assert!(NIGHTLY.accepts_version(1));
/// assert!(!NIGHTLY.accepts_version(3));
/// ```
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct JobKindDecl {
    /// The declaring addon; `name` starts with `"<addon>."`.
    pub addon: &'static str,
    /// `<addon>.<kind>`, written to `erp_job.kind`.
    pub name: &'static str,
    /// Written on enqueue; at least 1.
    pub version: u16,
    /// Versions this binary can run; `None` means exactly `version`.
    pub accepts: Option<RangeInclusive<u16>>,
    /// How the work runs.
    pub handler: HandlerRef,
    /// Retry delays; `None` uses `jobs.backoff_base_secs` and `jobs.backoff_cap_secs`.
    pub backoff: Option<Backoff>,
    /// Attempts before a transient failure dead-letters the job (1..=100); `None` uses
    /// `jobs.max_attempts`.
    pub max_attempts: Option<u32>,
    /// Priority of jobs enqueued without one.
    pub priority: Priority,
    /// Whether finished jobs leave history.
    pub history: HistoryPolicy,
    /// Who runs the job.
    pub principal: PrincipalPolicy,
    /// Which chunk limits apply.
    pub timeouts: TimeoutClass,
    /// Isolation of the chunks.
    pub isolation: KindIsolation,
    /// Most running jobs of this kind per database, divided among live workers.
    pub max_concurrency: Option<NonZeroU32>,
    /// Outbox events that enqueue this kind (refused until).
    pub triggers: &'static [OutboxTrigger],
    /// An elevated kind runs as an elevated principal against stores or owner state and may run
    /// only on an isolated maintenance runner. A runner built with
    /// [`JobRunnerBuilder::exclude_elevated`](super::JobRunnerBuilder::exclude_elevated) never
    /// claims it. Default `false`.
    pub elevated: bool,
}

impl JobKindDecl {
    /// A kind with the defaults: version 1, the attempts and backoff of the `jobs.*` settings,
    /// priority 5, history kept, the caller's principal, default limits, `REPEATABLE READ`.
    pub const fn new(addon: &'static str, name: &'static str, handler: HandlerRef) -> Self {
        Self {
            addon,
            name,
            version: 1,
            accepts: None,
            handler,
            backoff: None,
            max_attempts: None,
            priority: Priority::DEFAULT,
            history: HistoryPolicy::Keep,
            principal: PrincipalPolicy::Caller,
            timeouts: TimeoutClass::Default,
            isolation: KindIsolation::RepeatableRead,
            max_concurrency: None,
            triggers: &[],
            elevated: false,
        }
    }

    /// The version written on enqueue.
    #[must_use]
    pub const fn version(mut self, version: u16) -> Self {
        self.version = version;
        self
    }

    /// The versions this binary can run.
    #[must_use]
    pub const fn accepts(mut self, versions: RangeInclusive<u16>) -> Self {
        self.accepts = Some(versions);
        self
    }

    /// The retry delays.
    #[must_use]
    pub const fn backoff(mut self, backoff: Backoff) -> Self {
        self.backoff = Some(backoff);
        self
    }

    /// Attempts before a transient failure dead-letters the job.
    #[must_use]
    pub const fn max_attempts(mut self, max_attempts: u32) -> Self {
        self.max_attempts = Some(max_attempts);
        self
    }

    /// The default priority.
    #[must_use]
    pub const fn priority(mut self, priority: Priority) -> Self {
        self.priority = priority;
        self
    }

    /// Whether finished jobs leave history.
    #[must_use]
    pub const fn history(mut self, history: HistoryPolicy) -> Self {
        self.history = history;
        self
    }

    /// Who runs the job.
    #[must_use]
    pub const fn principal(mut self, principal: PrincipalPolicy) -> Self {
        self.principal = principal;
        self
    }

    /// The chunk limits class.
    #[must_use]
    pub const fn timeouts(mut self, timeouts: TimeoutClass) -> Self {
        self.timeouts = timeouts;
        self
    }

    /// The chunk isolation level.
    #[must_use]
    pub const fn isolation(mut self, isolation: KindIsolation) -> Self {
        self.isolation = isolation;
        self
    }

    /// Cap the running jobs of this kind per database.
    #[must_use]
    pub const fn max_concurrency(mut self, limit: NonZeroU32) -> Self {
        self.max_concurrency = Some(limit);
        self
    }

    /// Outbox triggers (refused at publication until).
    #[must_use]
    pub const fn triggers(mut self, triggers: &'static [OutboxTrigger]) -> Self {
        self.triggers = triggers;
        self
    }

    /// Mark the kind elevated: only an isolated maintenance runner may run it.
    #[must_use]
    pub const fn elevated(mut self) -> Self {
        self.elevated = true;
        self
    }

    /// Whether this binary can run a job written at `version`.
    pub fn accepts_version(&self, version: u16) -> bool {
        match &self.accepts {
            Some(range) => range.contains(&version),
            None => version == self.version,
        }
    }
}

/// Every job kind linked into the binary.
#[linkme::distributed_slice]
pub static JOB_KINDS: [JobKindDecl];

/// Register a job kind.
///
/// ```
/// use erp_orm::jobs::{HandlerRef, JobKindDecl};
/// erp_orm::register_job_kind!(
///     DEMO_KIND,
///     JobKindDecl::new(
///         "demo",
///         "demo.cleanup",
///         HandlerRef::Step { model: "demo.cleanup", method: "run_step" },
///     )
/// );
///
/// fn main() {
///     assert!(erp_orm::jobs::JOB_KINDS.iter().any(|k| k.name == "demo.cleanup"));
/// }
/// ```
#[macro_export]
macro_rules! register_job_kind {
    ($ident:ident, $decl:expr) => {
        #[$crate::linkme::distributed_slice($crate::jobs::JOB_KINDS)]
        #[linkme(crate = $crate::linkme)]
        static $ident: $crate::jobs::JobKindDecl = $decl;
    };
}

/// A job kind a binary requires: the expectation that a linked
/// [`register_job_kind!`](crate::register_job_kind) of the crate `owner` registers `kind`. A
/// registration is a static nothing calls, so the linker drops it from a binary that references
/// nothing else of its object file; the expectation lives at the build site, which is always
/// linked ([`OrmBuilder::require_job_kinds`](crate::OrmBuilder::require_job_kinds)).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JobKindRequirement {
    kind: &'static str,
    owner: &'static str,
}

impl JobKindRequirement {
    /// `kind`, registered by the crate `owner`.
    pub const fn new(kind: &'static str, owner: &'static str) -> Self {
        Self { kind, owner }
    }

    /// The kind.
    pub const fn kind(&self) -> &'static str {
        self.kind
    }

    /// The crate that registers it.
    pub const fn owner(&self) -> &'static str {
        self.owner
    }
}

/// Addons whose kinds may declare [`PrincipalPolicy::System`]: the core and the `base` addon
/// (4).
pub const SYSTEM_PRINCIPAL_ADDONS: &[&str] = &["core", "base"];

/// Whether `name` is a valid kind name: `<addon>.<name>` matching the table's `CHECK`
/// (`^[a-z][a-z0-9_]{0,63}\.[a-z][a-z0-9_.]{0,127}$`).
pub fn is_valid_kind_name(name: &str) -> bool {
    let Some((addon, rest)) = name.split_once('.') else {
        return false;
    };
    let head = |s: &str, max: usize, dots: bool| {
        let b = s.as_bytes();
        !b.is_empty()
            && b.len() <= max
            && b[0].is_ascii_lowercase()
            && b.iter().all(|c| {
                c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'_' || (dots && *c == b'.')
            })
    };
    head(addon, 64, false) && head(rest, 128, true)
}

/// Validate `kinds` and the `required` set: the checks of registry publication.
///
/// # Errors
///
/// `Programming` naming the kind and the first violated rule: a name that does not match
/// `<addon>.<name>` or does not start with its addon, a duplicate name, a version or accepted
/// range or attempt count outside its bounds, a backoff outside its bounds, `System` principal
/// for an addon outside [`SYSTEM_PRINCIPAL_ADDONS`], outbox triggers, a
/// `READ COMMITTED` kind outside the maintenance class, and a required kind nobody registered.
pub fn validate(kinds: &[&JobKindDecl], required: &[&JobKindRequirement]) -> Result<()> {
    let mut seen: Vec<&JobKindDecl> = Vec::with_capacity(kinds.len());
    for decl in kinds {
        let name = decl.name;
        if !is_valid_kind_name(name) {
            return Err(Error::programming(format!(
                "job kind '{name}' is not a valid kind name: it must be '<addon>.<name>' \
                 (lowercase letters, digits and underscores; dots allowed after the first)"
            )));
        }
        if !name
            .strip_prefix(decl.addon)
            .is_some_and(|rest| rest.starts_with('.'))
        {
            return Err(Error::programming(format!(
                "job kind '{name}' is declared by addon '{}' but its name does not start with \
                 '{}.': a kind belongs to the addon its name starts with",
                decl.addon, decl.addon
            )));
        }
        if let Some(first) = seen.iter().find(|d| d.name == name) {
            return Err(Error::programming(format!(
                "job kind '{name}' is registered twice: by addon '{}' and by addon '{}'",
                first.addon, decl.addon
            )));
        }
        if decl.version == 0 {
            return Err(Error::programming(format!(
                "job kind '{name}' has version 0; versions start at 1"
            )));
        }
        if let Some(range) = &decl.accepts
            && (*range.start() == 0 || !range.contains(&decl.version))
        {
            return Err(Error::programming(format!(
                "job kind '{name}' accepts versions {}..={} which must start at 1 and include \
                 its own version {}",
                range.start(),
                range.end(),
                decl.version
            )));
        }
        if let Some(attempts) = decl.max_attempts
            && !(1..=100).contains(&attempts)
        {
            return Err(Error::programming(format!(
                "job kind '{name}' has max_attempts {attempts}; it needs 1..=100"
            )));
        }
        if let Some(backoff) = &decl.backoff {
            backoff
                .validate()
                .map_err(|e| Error::programming(format!("job kind '{name}': {}", e.message())))?;
        }
        if decl.principal == PrincipalPolicy::System
            && !SYSTEM_PRINCIPAL_ADDONS.contains(&decl.addon)
        {
            return Err(Error::programming(format!(
                "job kind '{name}' declares the System principal but addon '{}' is not allowed \
                 to: only the core and the base addon run jobs as the system",
                decl.addon
            )));
        }
        if !decl.triggers.is_empty() {
            return Err(Error::programming(format!(
                "job kind '{name}' declares outbox triggers, which need the trigger consumer of \
                 task a later release (not delivered yet)"
            )));
        }
        if decl.isolation == KindIsolation::ReadCommitted
            && decl.timeouts != TimeoutClass::Maintenance
        {
            return Err(Error::programming(format!(
                "job kind '{name}' asks for READ COMMITTED chunks but is not in the maintenance \
                 timeout class: only maintenance kinds may leave the default isolation"
            )));
        }
        if let HandlerRef::Step { model, method } = decl.handler
            && (model.is_empty() || method.is_empty())
        {
            return Err(Error::programming(format!(
                "job kind '{name}' is a step kind without a model or a method name"
            )));
        }
        seen.push(decl);
    }
    for need in required {
        if !seen.iter().any(|d| d.name == need.kind) {
            return Err(Error::programming(format!(
                "job kind '{}' of '{}' is required but not registered in this binary: nothing \
                 linked references the register_job_kind! registration of '{}', so the linker \
                 dropped it; reference the owner's list from the binary",
                need.kind, need.owner, need.owner
            )));
        }
    }
    Ok(())
}

/// The kinds linked into this binary.
pub(crate) fn linked() -> Vec<&'static JobKindDecl> {
    JOB_KINDS.iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::BoxFuture;
    use crate::jobs::{JobContext, RunOutcome};

    struct Nop;
    impl JobHandler for Nop {
        fn run<'a>(
            &'a self,
            _ctx: &'a mut JobContext<'_>,
        ) -> BoxFuture<'a, std::result::Result<RunOutcome, crate::jobs::JobError>> {
            Box::pin(async { Ok(RunOutcome::Done) })
        }
    }
    static NOP: Nop = Nop;

    fn decl(addon: &'static str, name: &'static str) -> JobKindDecl {
        JobKindDecl::new(addon, name, HandlerRef::Native(&NOP))
    }

    fn check(decls: &[JobKindDecl]) -> Result<()> {
        let refs: Vec<&JobKindDecl> = decls.iter().collect();
        validate(&refs, &[])
    }

    #[test]
    fn kind_names_follow_the_table_check_pattern() {
        for good in [
            "a.b",
            "stock.recount",
            "base.cron",
            "mail.send.batch",
            "x9_y.z_1",
        ] {
            assert!(is_valid_kind_name(good), "{good}");
        }
        for bad in [
            "", "nodot", ".b", "a.", "A.b", "a.B", "1a.b", "a.1b", "a b.c", "a.b c", "a..b",
            "a-b.c",
        ] {
            assert!(!is_valid_kind_name(bad), "{bad}");
        }
        assert!(!is_valid_kind_name(&format!("{}.b", "a".repeat(65))));
        assert!(is_valid_kind_name(&format!("{}.b", "a".repeat(64))));
        assert!(!is_valid_kind_name(&format!("a.{}", "b".repeat(129))));
    }

    #[test]
    fn a_valid_declaration_passes() {
        assert!(check(&[decl("stock", "stock.recount")]).is_ok());
    }

    #[test]
    fn a_name_outside_its_addon_prefix_is_refused() {
        let error = check(&[decl("stock", "sales.recount")]).unwrap_err();
        assert_eq!(
            error.message(),
            "job kind 'sales.recount' is declared by addon 'stock' but its name does not start \
             with 'stock.': a kind belongs to the addon its name starts with"
        );
        assert_eq!(error.kind(), erp_fields::ErrorKind::Programming);
    }

    #[test]
    fn a_malformed_name_is_refused() {
        let error = check(&[decl("stock", "Stock")]).unwrap_err();
        assert_eq!(
            error.message(),
            "job kind 'Stock' is not a valid kind name: it must be '<addon>.<name>' (lowercase \
             letters, digits and underscores; dots allowed after the first)"
        );
    }

    #[test]
    fn a_duplicate_name_is_refused_naming_both_addons() {
        let error = check(&[
            decl("stock", "stock.recount"),
            decl("stock", "stock.recount"),
        ])
        .unwrap_err();
        assert_eq!(
            error.message(),
            "job kind 'stock.recount' is registered twice: by addon 'stock' and by addon 'stock'"
        );
    }

    #[test]
    fn the_system_principal_is_for_the_core_and_base_only() {
        let system = |addon, name| decl(addon, name).principal(PrincipalPolicy::System);
        assert!(
            check(&[
                system("base", "base.cron"),
                system("core", "core.retention")
            ])
            .is_ok()
        );
        let error = check(&[system("stock", "stock.recount")]).unwrap_err();
        assert_eq!(
            error.message(),
            "job kind 'stock.recount' declares the System principal but addon 'stock' is not \
             allowed to: only the core and the base addon run jobs as the system"
        );
    }

    #[test]
    fn outbox_triggers_are_refused_until_the_consumer_task() {
        static TRIGGERS: [OutboxTrigger; 1] = [OutboxTrigger::new(
            "res.partner",
            &["write"],
            &[],
            TriggerCoalesce::PerKind,
        )];
        let error = check(&[decl("stock", "stock.recount").triggers(&TRIGGERS)]).unwrap_err();
        assert_eq!(
            error.message(),
            "job kind 'stock.recount' declares outbox triggers, which need the trigger consumer \
             of task a later release (not delivered yet)"
        );
    }

    #[test]
    fn bounds_on_version_attempts_and_isolation_are_checked() {
        assert_eq!(
            check(&[decl("a", "a.b").version(0)]).unwrap_err().message(),
            "job kind 'a.b' has version 0; versions start at 1"
        );
        assert_eq!(
            check(&[decl("a", "a.b").version(3).accepts(1..=2)])
                .unwrap_err()
                .message(),
            "job kind 'a.b' accepts versions 1..=2 which must start at 1 and include its own \
             version 3"
        );
        assert_eq!(
            check(&[decl("a", "a.b").max_attempts(0)])
                .unwrap_err()
                .message(),
            "job kind 'a.b' has max_attempts 0; it needs 1..=100"
        );
        assert_eq!(
            check(&[decl("a", "a.b").max_attempts(101)])
                .unwrap_err()
                .message(),
            "job kind 'a.b' has max_attempts 101; it needs 1..=100"
        );
        assert_eq!(
            check(&[decl("a", "a.b").isolation(KindIsolation::ReadCommitted)])
                .unwrap_err()
                .message(),
            "job kind 'a.b' asks for READ COMMITTED chunks but is not in the maintenance timeout \
             class: only maintenance kinds may leave the default isolation"
        );
        assert!(
            check(&[decl("a", "a.b")
                .isolation(KindIsolation::ReadCommitted)
                .timeouts(TimeoutClass::Maintenance)])
            .is_ok()
        );
        let bad_backoff = decl("a", "a.b").backoff(Backoff::Exponential {
            base: std::time::Duration::ZERO,
            cap: std::time::Duration::from_secs(5),
        });
        assert_eq!(
            check(&[bad_backoff]).unwrap_err().message(),
            "job kind 'a.b': backoff base 0 s is outside 1..=3600 s"
        );
    }

    #[test]
    fn a_required_kind_nobody_registered_names_the_kind_and_its_owner() {
        let kinds = [decl("base", "base.cron")];
        let refs: Vec<&JobKindDecl> = kinds.iter().collect();
        let ok = JobKindRequirement::new("base.cron", "erp-addon-base");
        assert!(validate(&refs, &[&ok]).is_ok());
        let missing = JobKindRequirement::new("core.retention", "erp-orm");
        let error = validate(&refs, &[&missing]).unwrap_err();
        assert_eq!(
            error.message(),
            "job kind 'core.retention' of 'erp-orm' is required but not registered in this \
             binary: nothing linked references the register_job_kind! registration of 'erp-orm', \
             so the linker dropped it; reference the owner's list from the binary"
        );
        assert_eq!(error.kind(), erp_fields::ErrorKind::Programming);
    }

    #[test]
    fn publication_refuses_every_invalid_declaration_and_missing_core_kind() {
        use std::time::Duration;
        static TRIGGERS: [OutboxTrigger; 1] = [OutboxTrigger::new(
            "res.partner",
            &["write"],
            &[],
            TriggerCoalesce::PerKind,
        )];
        let refused: Vec<(JobKindDecl, &str)> = vec![
            (decl("a", "A"), "is not a valid kind name"),
            (decl("a", "b.c"), "does not start with 'a.'"),
            (decl("a", "a.b").version(0), "has version 0"),
            (decl("a", "a.b").max_attempts(0), "max_attempts 0"),
            (
                decl("a", "a.b").backoff(Backoff::Exponential {
                    base: Duration::ZERO,
                    cap: Duration::from_secs(5),
                }),
                "backoff base 0 s",
            ),
            (
                decl("a", "a.b").principal(PrincipalPolicy::System),
                "System principal",
            ),
            (decl("a", "a.b").triggers(&TRIGGERS), "a later release"),
            (
                decl("a", "a.b").isolation(KindIsolation::ReadCommitted),
                "READ COMMITTED",
            ),
        ];
        for (declaration, expected) in refused {
            let error = check(&[declaration]).unwrap_err();
            assert!(error.message().contains(expected), "{}", error.message());
        }
        let duplicate = check(&[decl("a", "a.b"), decl("a", "a.b")]).unwrap_err();
        assert!(duplicate.message().contains("registered twice"));
        let kinds = [decl("base", "base.cron")];
        let refs: Vec<&JobKindDecl> = kinds.iter().collect();
        let missing = JobKindRequirement::new("core.x", "erp-orm");
        let error = validate(&refs, &[&missing]).unwrap_err();
        assert!(error.message().contains(""));
    }
}
