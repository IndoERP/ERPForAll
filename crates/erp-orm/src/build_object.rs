//! `Orm::build_object`: build one exclusion constraint or `WITHOUT OVERLAPS` key on a table that
//! is too large for the install path (design §11 and §11.2; sub-slice).
//!
//! Schema sync refuses to add these objects to a table above `large_table_rows`: the `ADD
//! CONSTRAINT` builds a GiST index under `ACCESS EXCLUSIVE` and has no online form. The operator
//! runs this call (`erp db build-object`) in a maintenance window instead. It takes the schema
//! advisory lock, bounds its lock waits, and in one transaction creates `btree_gist` when needed,
//! adds the period `CHECK` and the constraint, and stores the canonical definition in the
//! constraint's comment, exactly as the install path would. The next install or upgrade finds the
//! object present and issues no DDL for it.

use std::fmt;
use std::time::{Duration, Instant};

use erp_fields::{Error, ErrorKind, Result};
use erp_registry::ModelSpec;

use crate::exclusion::{Compiled, ObjectKind, not_available, not_installable, sync_statements};
use crate::{Orm, SUPERUSER_ID, Transaction};

#[cfg(feature = "test-faults")]
static DIAGNOSTIC_OVERRIDE: std::sync::Mutex<Option<(&'static str, &'static str)>> =
    std::sync::Mutex::new(None);

/// Test hook (feature `test-faults`, debug builds only): replace the diagnostic query's
/// `(statement_timeout, lock_timeout)` for the rest of the process, to exercise the fallback.
#[cfg(feature = "test-faults")]
#[doc(hidden)]
pub fn override_diagnostic_timeouts(statement: &'static str, lock: &'static str) {
    *DIAGNOSTIC_OVERRIDE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some((statement, lock));
}

#[cfg(feature = "test-faults")]
static BUILD_OVERRIDE: std::sync::Mutex<Option<Duration>> = std::sync::Mutex::new(None);

/// Test hook (feature `test-faults`, debug builds only): replace the `statement_timeout` sent for
/// the build for the rest of the process, so a test can stop a build without waiting the minimum
/// build timeout.
///
/// # Errors
///
/// `Programming` when `statement` is zero or not a whole number of milliseconds: it is sent in
/// milliseconds, and `0ms` would turn the server's limit off.
#[cfg(feature = "test-faults")]
#[doc(hidden)]
pub fn override_build_statement_timeout(statement: Duration) -> Result<()> {
    if statement.is_zero() || !statement.subsec_nanos().is_multiple_of(1_000_000) {
        return Err(Error::programming(format!(
            "the build statement timeout override must be a positive whole number of \
             milliseconds, got {statement:?}"
        )));
    }
    *BUILD_OVERRIDE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(statement);
    Ok(())
}

/// The `statement_timeout` sent for the build: the text sent and the limit it sets (told to the
/// transaction, so a statement it stops is reported as the build's own timeout).
fn build_statement_timeout(timeout: BuildTimeout) -> (String, Duration) {
    #[cfg(feature = "test-faults")]
    if let Some(over) = *BUILD_OVERRIDE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
    {
        return (format!("{}ms", over.as_millis()), over);
    }
    (timeout.to_string(), Duration::from_secs(timeout.as_secs()))
}

/// `(statement_timeout, lock_timeout)` of the diagnostic query that lists conflicting rows after
/// a failed build: it runs once, read-only, and must never hold the operator up. Both are
/// [`erp_sql::conflict::DIAGNOSTIC_TIMEOUT`], the value schema sync's diagnostic uses too.
fn diagnostic_timeouts() -> (String, String) {
    #[cfg(feature = "test-faults")]
    if let Some(over) = *DIAGNOSTIC_OVERRIDE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
    {
        return (over.0.to_owned(), over.1.to_owned());
    }
    let setting = erp_sql::conflict::timeout_setting(erp_sql::conflict::DIAGNOSTIC_TIMEOUT);
    (setting.clone(), setting)
}

/// The whole-second value of `secs` when it is from `min` to `max`, else the `UserError` naming
/// `what` and the bounds (the one place the bounds message lives).
fn bounded(what: &str, secs: u64, min: u64, max: u64, hint: &str) -> Result<u64> {
    if (min..=max).contains(&secs) {
        Ok(secs)
    } else {
        Err(Error::user(format!(
            "{what} of {secs} seconds: it must be from {min} to {max} seconds{hint}"
        )))
    }
}

/// `secs` parsed as a whole number of seconds, with or without the `s` that `Display` prints
/// (`30` and `30s`), else the `UserError` naming `what` and the bounds.
fn parse_secs(what: &str, text: &str, min: u64, max: u64) -> Result<u64> {
    let trimmed = text.trim();
    let digits = trimmed.strip_suffix('s').unwrap_or(trimmed);
    digits.parse::<u64>().map_err(|_| {
        Error::user(format!(
            "'{text}' is not a whole number of seconds; the {what} must be from {min} to {max} \
             seconds"
        ))
    })
}

/// How long `Orm::build_object` waits for a lock (the schema lock, then the table lock) before it
/// gives up: from [`LockTimeout::MIN_SECS`] to [`LockTimeout::MAX_SECS`] whole seconds, 30 by
/// default. Zero is refused, never read as "wait forever".
///
/// While the build waits for its `ACCESS EXCLUSIVE` lock, every new reader and writer of the
/// table queues behind it, so choose a short timeout on a busy table.
///
/// ```
/// use erp_orm::LockTimeout;
/// assert_eq!(LockTimeout::default().as_secs(), 30);
/// assert_eq!(LockTimeout::try_from(5).unwrap().as_secs(), 5);
/// assert!(LockTimeout::try_from(0).is_err());
/// assert!("3601".parse::<LockTimeout>().is_err());
/// // `Display` prints `30s`, and `FromStr` reads it back, with or without the `s`
/// let default = LockTimeout::default();
/// assert_eq!(default.to_string(), "30s");
/// assert_eq!(default.to_string().parse::<LockTimeout>().unwrap(), default);
/// assert_eq!("30".parse::<LockTimeout>().unwrap(), default);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct LockTimeout(u64);

impl LockTimeout {
    /// The shortest accepted timeout, in seconds.
    pub const MIN_SECS: u64 = 1;
    /// The longest accepted timeout, in seconds (one hour).
    pub const MAX_SECS: u64 = 3600;
    /// The timeout used when none is given, in seconds.
    pub const DEFAULT_SECS: u64 = 30;

    /// The timeout in whole seconds.
    pub const fn as_secs(self) -> u64 {
        self.0
    }
}

impl TryFrom<u64> for LockTimeout {
    type Error = Error;

    /// # Errors
    ///
    /// `UserError` when `secs` is below [`MIN_SECS`](Self::MIN_SECS) or above
    /// [`MAX_SECS`](Self::MAX_SECS).
    fn try_from(secs: u64) -> Result<Self> {
        bounded(
            "lock timeout",
            secs,
            Self::MIN_SECS,
            Self::MAX_SECS,
            " (0 would wait forever)",
        )
        .map(Self)
    }
}

impl std::str::FromStr for LockTimeout {
    type Err = Error;

    /// # Errors
    ///
    /// `UserError` for text that is not a whole number or is out of range.
    fn from_str(text: &str) -> Result<Self> {
        Self::try_from(parse_secs(
            "lock timeout",
            text,
            Self::MIN_SECS,
            Self::MAX_SECS,
        )?)
    }
}

impl Default for LockTimeout {
    fn default() -> Self {
        Self(Self::DEFAULT_SECS)
    }
}

impl fmt::Display for LockTimeout {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}s", self.0)
    }
}

/// How long each statement of the build may run (`statement_timeout`): from
/// [`BuildTimeout::MIN_SECS`] to [`BuildTimeout::MAX_SECS`] whole seconds, 6 hours by default. It
/// must be longer than the [`LockTimeout`] of the same call.
///
/// ```
/// use erp_orm::BuildTimeout;
/// assert_eq!(BuildTimeout::default().as_secs(), 6 * 3600);
/// assert!(BuildTimeout::try_from(59).is_err());
/// assert!(BuildTimeout::try_from(24 * 3600 + 1).is_err());
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct BuildTimeout(u64);

impl BuildTimeout {
    /// The shortest accepted timeout, in seconds (one minute).
    pub const MIN_SECS: u64 = 60;
    /// The longest accepted timeout, in seconds (24 hours).
    pub const MAX_SECS: u64 = 24 * 3600;
    /// The timeout used when none is given, in seconds (6 hours).
    pub const DEFAULT_SECS: u64 = 6 * 3600;

    /// The timeout in whole seconds.
    pub const fn as_secs(self) -> u64 {
        self.0
    }
}

impl TryFrom<u64> for BuildTimeout {
    type Error = Error;

    /// # Errors
    ///
    /// `UserError` when `secs` is below [`MIN_SECS`](Self::MIN_SECS) or above
    /// [`MAX_SECS`](Self::MAX_SECS).
    fn try_from(secs: u64) -> Result<Self> {
        bounded("build timeout", secs, Self::MIN_SECS, Self::MAX_SECS, "").map(Self)
    }
}

impl std::str::FromStr for BuildTimeout {
    type Err = Error;

    /// # Errors
    ///
    /// `UserError` for text that is not a whole number or is out of range.
    fn from_str(text: &str) -> Result<Self> {
        Self::try_from(parse_secs(
            "build timeout",
            text,
            Self::MIN_SECS,
            Self::MAX_SECS,
        )?)
    }
}

impl Default for BuildTimeout {
    fn default() -> Self {
        Self(Self::DEFAULT_SECS)
    }
}

impl fmt::Display for BuildTimeout {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}s", self.0)
    }
}

/// The options of [`Orm::build_object`].
#[derive(Debug, Clone, Copy, Default)]
pub struct BuildObjectOptions {
    lock_timeout: LockTimeout,
    build_timeout: BuildTimeout,
}

impl BuildObjectOptions {
    /// The defaults: [`LockTimeout::DEFAULT_SECS`] and [`BuildTimeout::DEFAULT_SECS`].
    pub fn new() -> Self {
        Self::default()
    }

    /// Set how long the build waits for a lock.
    ///
    /// # Errors
    ///
    /// `UserError` when the build timeout already set is not longer than `timeout`.
    pub fn with_lock_timeout(mut self, timeout: LockTimeout) -> Result<Self> {
        check_build_outlasts_lock(timeout, self.build_timeout)?;
        self.lock_timeout = timeout;
        Ok(self)
    }

    /// Set how long each statement of the build may run.
    ///
    /// # Errors
    ///
    /// `UserError` when `timeout` is not longer than the lock timeout already set.
    pub fn with_build_timeout(mut self, timeout: BuildTimeout) -> Result<Self> {
        check_build_outlasts_lock(self.lock_timeout, timeout)?;
        self.build_timeout = timeout;
        Ok(self)
    }

    /// The lock timeout.
    pub fn lock_timeout(&self) -> LockTimeout {
        self.lock_timeout
    }

    /// The build timeout.
    pub fn build_timeout(&self) -> BuildTimeout {
        self.build_timeout
    }
}

/// The build timeout must outlast the lock timeout: checked when the options are built, so
/// options that exist are valid.
fn check_build_outlasts_lock(lock: LockTimeout, build: BuildTimeout) -> Result<()> {
    if build.0 > lock.0 {
        Ok(())
    } else {
        Err(Error::user(format!(
            "build timeout of {} seconds must be longer than the lock timeout of {} seconds",
            build.0, lock.0
        )))
    }
}

/// What [`Orm::build_object`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum BuildOutcome {
    /// The object was missing or out of date and was built.
    Built,
    /// The table already had the object with the canonical definition: nothing was changed.
    AlreadyPresent,
}

/// The result of [`Orm::build_object`].
#[derive(Debug, Clone)]
pub struct BuildObjectReport {
    model: &'static str,
    object: &'static str,
    table: String,
    what: String,
    kind: ObjectKind,
    statements: Vec<String>,
    lock_timeout: LockTimeout,
    build_timeout: BuildTimeout,
    duration: Duration,
}

impl BuildObjectReport {
    /// The one constructor: the outcome follows from the statements that ran (none: the object
    /// was there already).
    fn new(
        spec: &ModelSpec,
        compiled: &Compiled,
        options: &BuildObjectOptions,
        statements: Vec<String>,
        duration: Duration,
    ) -> Self {
        Self {
            model: spec.name,
            object: compiled.name,
            table: spec.table.clone(),
            what: compiled.what.clone(),
            kind: compiled.kind,
            statements,
            lock_timeout: options.lock_timeout,
            build_timeout: options.build_timeout,
            duration,
        }
    }

    /// The model name.
    pub fn model(&self) -> &str {
        self.model
    }

    /// The declared name of the object (`_no_double_booking`).
    pub fn object(&self) -> &str {
        self.object
    }

    /// The table the object is on.
    pub fn table(&self) -> &str {
        &self.table
    }

    /// Whether the object is an exclusion or a `WITHOUT OVERLAPS` key.
    pub fn kind(&self) -> ObjectKind {
        self.kind
    }

    /// Whether the object was built or was there already.
    pub fn outcome(&self) -> BuildOutcome {
        if self.statements.is_empty() {
            BuildOutcome::AlreadyPresent
        } else {
            BuildOutcome::Built
        }
    }

    /// The DDL statements that ran, in order (empty for [`BuildOutcome::AlreadyPresent`]).
    pub fn statements(&self) -> &[String] {
        &self.statements
    }

    /// The `lock_timeout` the build ran under (the value sent to the database).
    pub fn lock_timeout(&self) -> LockTimeout {
        self.lock_timeout
    }

    /// The `statement_timeout` the build ran under (the value sent to the database).
    pub fn build_timeout(&self) -> BuildTimeout {
        self.build_timeout
    }

    /// How long the call took, the lock waits included.
    pub fn duration(&self) -> Duration {
        self.duration
    }
}

impl fmt::Display for BuildObjectReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.outcome() {
            BuildOutcome::AlreadyPresent => write!(
                f,
                "{} on '{}' (table '{}') is already present with the canonical definition; \
                 nothing to do ({:.3}s)",
                self.what,
                self.model,
                self.table,
                self.duration.as_secs_f64()
            ),
            BuildOutcome::Built => {
                write!(
                    f,
                    "built {} on '{}' (table '{}') in {:.3}s; statements run:",
                    self.what,
                    self.model,
                    self.table,
                    self.duration.as_secs_f64()
                )?;
                for statement in &self.statements {
                    write!(f, "\n  {statement}")?;
                }
                Ok(())
            }
        }
    }
}

impl Orm {
    /// Build the exclusion constraint or `WITHOUT OVERLAPS` key `object` of `model` on a table
    /// that schema sync refuses to build it on (above `large_table_rows`).
    ///
    /// **This is an operator tool.** The build holds `ACCESS EXCLUSIVE` on the table for the whole
    /// GiST build, and while it waits for that lock every new reader and writer of the table
    /// queues behind it. Run it in a maintenance window, through `erp db build-object`; call it
    /// from code only for tests and for tooling that owns the window.
    ///
    /// Takes the schema advisory lock (the one schema sync takes; it does not take the
    /// lifecycle lock of a server, so a running server keeps serving: reads and writes of the
    /// table wait behind the build's `ACCESS EXCLUSIVE` lock), bounds every lock wait by
    /// `options`, and runs in one transaction: `CREATE EXTENSION IF NOT EXISTS btree_gist` when
    /// the object compares columns with `=`, the period `CHECK` of a typed exclusion, `ADD
    /// CONSTRAINT`, and the canonical definition as the constraint's comment. The next schema
    /// sync finds the object present and issues no DDL for it. An object that is present with
    /// the canonical comment is left alone ([`BuildOutcome::AlreadyPresent`]).
    ///
    /// ```no_run
    /// # async fn example(orm: &erp_orm::Orm) -> erp_fields::Result<()> {
    /// use erp_orm::{BuildObjectOptions, LockTimeout};
    /// let options = BuildObjectOptions::new().with_lock_timeout(LockTimeout::try_from(5)?)?;
    /// let report = orm.build_object("scale.booking", "_no_double_booking", &options).await?;
    /// println!("{report}");
    /// # Ok(()) }
    /// ```
    ///
    /// # Errors
    ///
    /// - `UserError` for options whose build timeout is not longer than the lock timeout, a model
    ///   the registry does not have, an object the model does not
    ///   declare or that is not an exclusion or `WITHOUT OVERLAPS` key (the message lists what
    ///   the model declares), and a table that does not exist yet (the install creates it).
    /// - `Validation` `cannot add <object> to '<model>': existing records conflict, e.g. (<id>,
    ///   <id>)` (up to 20 pairs, ids only) when existing rows violate the object; nothing is left
    ///   behind. Rows that violate the period `CHECK` fail with its field-named message.
    /// - `Lock` naming the table and the timeout when a lock is not granted in time.
    /// - `Database` (SQLSTATE `57014`) naming the object, the table and the limit when a
    ///   statement runs past the build timeout.
    /// - `Database` for an extension the server lacks or the role may not create, and any other
    ///   database error.
    pub async fn build_object(
        &self,
        model: &str,
        object: &str,
        options: &BuildObjectOptions,
    ) -> Result<BuildObjectReport> {
        let started = Instant::now();
        let registry = &self.shared.registry;
        let spec = registry
            .model(model)
            .ok_or_else(|| Error::user(format!("unknown model '{model}'")))?;
        let compiled = self.find_object(spec, object)?;
        let mut tx = self.begin(SUPERUSER_ID).await?;
        let built = self.build_in(&mut tx, spec, compiled, options).await;
        match built {
            Ok(statements) => {
                tx.commit().await?;
                Ok(BuildObjectReport::new(
                    spec,
                    compiled,
                    options,
                    statements,
                    started.elapsed(),
                ))
            }
            Err(error) => {
                // A failed ROLLBACK releases the connection, which rolls back all the same, and
                // `discard_after` adds its message to `error`. The explicit drop returns the
                // connection to the pool before the diagnostic transaction asks for one: the
                // CLI's pool has one.
                let error = tx.discard_after(error).await;
                drop(tx);
                if error.sqlstate() == Some("57014") {
                    return Err(build_timed_out(
                        error,
                        spec,
                        compiled,
                        options.build_timeout,
                    ));
                }
                self.explain_failure(spec, compiled, error).await
            }
        }
    }

    /// The compiled object `object` of `spec`, or the error listing what the model declares.
    fn find_object<'a>(&'a self, spec: &ModelSpec, object: &str) -> Result<&'a Compiled> {
        let compiled = self.shared.exclusions.compiled(spec.id);
        let declared = |c: &Compiled| spec.table_object(&c.full_name).map(|o| o.name);
        if let Some(found) = compiled.iter().find(|c| declared(c) == Some(object)) {
            return Ok(found);
        }
        if spec.table_objects.iter().any(|o| o.name == object) {
            return Err(Error::user(format!(
                "object '{object}' of '{}' is not an exclusion or a WITHOUT OVERLAPS key; \
                 `erp db build-object` builds those only{}",
                spec.name,
                listing(spec, compiled)
            )));
        }
        Err(Error::user(format!(
            "model '{}' declares no object named '{object}'{}",
            spec.name,
            listing(spec, compiled)
        )))
    }

    async fn build_in(
        &self,
        tx: &mut Transaction,
        spec: &ModelSpec,
        compiled: &Compiled,
        options: &BuildObjectOptions,
    ) -> Result<Vec<String>> {
        let timeout = options.lock_timeout;
        let table = &spec.table;
        let waited = |error: Error, what: String| lock_error(error, &what, timeout);
        // bound the waits before waiting for anything, the schema lock included; these are the
        // values the report shows
        let (statement_text, statement_limit) = build_statement_timeout(options.build_timeout);
        tx.query(
            "SELECT set_config('lock_timeout', $1, true), set_config('statement_timeout', $2, true)",
            &[&timeout.to_string(), &statement_text],
        )
        .await?;
        // a statement stopped from here on is stopped by the build's limit, not by the one the
        // transaction began with
        tx.note_limits_changed(Some(statement_limit));
        tx.query(erp_sql::ddl::SCHEMA_LOCK, &[])
            .await
            .map_err(|e| {
                waited(
                    e,
                    "another schema synchronization of this database is running".to_owned(),
                )
            })?;
        let Some(constraints) = tx.catalog_constraints(table).await? else {
            return Err(Error::user(format!(
                "cannot build {} on '{}': table '{table}' does not exist in this database; \
                 install or upgrade the module first (a new table gets the object in the install \
                 itself)",
                compiled.what, spec.name
            )));
        };
        let table_object = spec.table_object(&compiled.full_name).ok_or_else(|| {
            Error::programming(format!(
                "table object '{}' of '{}' vanished from the registry",
                compiled.full_name, spec.name
            ))
        })?;
        let Some(sync) = sync_statements(
            &self.shared.exclusions,
            spec,
            table_object,
            Some(&constraints),
        )?
        else {
            // present with the canonical definition: nothing ran
            return Ok(Vec::new());
        };
        let mut statements = Vec::new();
        if compiled.needs_btree_gist {
            let ext = erp_registry::PgExtension::BtreeGist;
            let present = tx
                .query(
                    "SELECT 1 FROM pg_extension WHERE extname = $1",
                    &[&ext.name()],
                )
                .await?;
            if present.is_empty() {
                let available = tx
                    .query(
                        "SELECT 1 FROM pg_available_extensions WHERE name = $1",
                        &[&ext.name()],
                    )
                    .await?;
                if available.is_empty() {
                    return Err(not_available(spec.name, ext, &compiled.what));
                }
                let create = erp_sql::exclusion::create_extension(ext);
                tx.batch(&create)
                    .await
                    .map_err(|e| not_installable(spec.name, ext, &compiled.what, e))?;
                statements.push(create);
            }
        }
        for statement in sync.statements {
            tx.batch(&statement).await.map_err(|e| {
                waited(
                    e,
                    format!("table '{table}' is in use (building {})", compiled.what),
                )
            })?;
            statements.push(statement);
        }
        Ok(statements)
    }

    /// The error to return for a failed build: the conflict message (with the diagnostic query
    /// run on a fresh, read-only, time-limited transaction, since the failed one is rolled back)
    /// for a violation by existing rows, a message naming the cause for rows that cannot be
    /// indexed or break the period check, the error itself otherwise.
    ///
    /// A diagnostic that cannot run or finish never hides the conflict: the message then says
    /// no pair could be listed, the original error stays the source and the diagnostic's own
    /// error is logged.
    async fn explain_failure(
        &self,
        spec: &ModelSpec,
        compiled: &Compiled,
        error: Error,
    ) -> Result<BuildObjectReport> {
        let prefix = format!("cannot add {} to '{}'", compiled.what, spec.name);
        let message = match error.sqlstate() {
            // 23P01 for an exclusion and a temporal key, 23505 if the server reports the key so
            Some("23P01" | "23505") => match self.conflict_pairs(&compiled.pairs_query).await {
                Ok(pairs) => erp_sql::conflict::conflict_message(
                    compiled.kind.conflict_object(),
                    compiled.name,
                    spec.name,
                    &pairs,
                ),
                Err(diagnostic) => {
                    tracing::warn!(
                        target: "erp::orm",
                        "listing the conflicting rows of {} on '{}' failed: {diagnostic}",
                        compiled.what,
                        spec.name
                    );
                    erp_sql::conflict::conflict_message_unlisted(
                        compiled.kind.conflict_object(),
                        compiled.name,
                        spec.name,
                    )
                }
            },
            // a period that breaks the CHECK of an exclusion, or a row a key cannot index
            Some("23514") if error.kind() == ErrorKind::Validation => {
                let detail = error
                    .message()
                    .strip_prefix(crate::dberror::PREFIX)
                    .unwrap_or_else(|| error.message());
                match compiled.kind {
                    ObjectKind::Exclusion => {
                        format!("{prefix}: existing records break the period check: {detail}")
                    }
                    ObjectKind::TemporalKey => {
                        format!("{prefix}: existing records cannot be indexed: {detail}")
                    }
                }
            }
            _ => return Err(error),
        };
        let sqlstate = error.sqlstate().map(str::to_owned);
        Err(Error::new(ErrorKind::Validation, message)
            .with_sqlstate(sqlstate)
            .with_source(error))
    }

    /// The conflicting pairs `sql` finds, sorted, in a read-only transaction with its own short
    /// timeouts. The transaction is rolled back and its connection released before this returns.
    async fn conflict_pairs(&self, sql: &str) -> Result<Vec<(i64, i64)>> {
        let (statement, lock) = diagnostic_timeouts();
        let mut tx = self.begin(SUPERUSER_ID).await?;
        let result = async {
            // READ ONLY first (SET TRANSACTION must precede any query); constants only
            tx.batch(&format!(
                "SET TRANSACTION READ ONLY; SET LOCAL lock_timeout = '{lock}'; \
                 SET LOCAL statement_timeout = '{statement}'"
            ))
            .await?;
            let rows = tx.query(sql, &[]).await?;
            crate::exclusion::read_conflict_pairs(&rows).map_err(unreadable)
        }
        .await;
        match result {
            Ok(pairs) => {
                tx.rollback().await?;
                Ok(pairs)
            }
            Err(error) => Err(tx.discard_after(error).await),
        }
    }
}

/// `; it declares: exclusion '_a', key '_b'` or `; it declares none`.
fn listing(spec: &ModelSpec, compiled: &[Compiled]) -> String {
    let names: Vec<String> = compiled
        .iter()
        .map(|c| match spec.table_object(&c.full_name) {
            Some(o) => format!("'{}'", o.name),
            None => format!("'{}'", c.full_name),
        })
        .collect();
    if names.is_empty() {
        format!(
            "; '{}' declares no exclusions or WITHOUT OVERLAPS keys",
            spec.name
        )
    } else {
        format!("; '{}' declares: {}", spec.name, names.join(", "))
    }
}

/// A lock wait cut by `lock_timeout` as a `Lock` error naming what waited and the timeout; any
/// other error unchanged.
fn lock_error(error: Error, what: &str, timeout: LockTimeout) -> Error {
    if error.kind() != ErrorKind::Lock {
        return error;
    }
    let sqlstate = error.sqlstate().map(str::to_owned);
    Error::new(
        ErrorKind::Lock,
        format!(
            "{what}; gave up waiting for a lock after {timeout} (lock_timeout), try again when \
             it has finished or raise --lock-timeout"
        ),
    )
    .with_sqlstate(sqlstate)
    .with_source(error)
}

/// A statement of the build stopped by `statement_timeout` (`57014`), as an error naming the
/// object, the table and the limit.
fn build_timed_out(
    error: Error,
    spec: &ModelSpec,
    compiled: &Compiled,
    limit: BuildTimeout,
) -> Error {
    Error::new(
        ErrorKind::Database,
        format!(
            "building {} on '{}' (table '{}') ran past the build timeout of {limit} and was \
             stopped; nothing was changed. Raise it with --build-timeout (up to {} seconds)",
            compiled.what,
            spec.name,
            spec.table,
            BuildTimeout::MAX_SECS
        ),
    )
    .with_sqlstate(Some("57014".to_owned()))
    .with_source(error)
}

fn unreadable(error: tokio_postgres::Error) -> Error {
    Error::new(ErrorKind::Database, "could not read the conflicting ids").with_source(error)
}
