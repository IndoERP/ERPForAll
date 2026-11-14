//! The large-table rule for schema sync: options, the steps a synchronization
//! leaves for after its commit, and the runner of those steps (design note
//! ).
//!
//! A step runs outside any transaction: index builds use `CREATE INDEX CONCURRENTLY` (refused in
//! a transaction block), validations take `SHARE UPDATE EXCLUSIVE` (writes go on), fills and
//! backfills commit every batch. Statement steps run on a connection taken out of the pool for
//! the step and closed afterwards, so the session settings they make never reach another pool
//! user. Every step is idempotent: running it again after it finished, or after it failed
//! half-way, finishes the same work.
//!
//! Lock waits are bounded by what a waiting statement holds up
//! ([`SchemaSyncOptions::step_lock_timeouts`]). The short timeout (default
//! [`SchemaSyncOptions::DEFAULT_SHORT_LOCK_TIMEOUT`]) is for the waits that hold other sessions
//! up: a statement taking `ACCESS EXCLUSIVE` (adding a not-null constraint) queues every reader
//! of the table behind it while it waits, and a fill or backfill batch waiting for a row lock
//! keeps the rows it already changed locked, so their writers wait as well. A batch that gives up
//! rolls back its own rows only: the batches before it stay committed, the step fails with a
//! `Lock` error, and the next synchronization plans the rest again. The long timeout (default
//! [`SchemaSyncOptions::DEFAULT_LONG_LOCK_TIMEOUT`]) is for the statements whose wait holds no
//! reader or writer up: `CREATE`/`DROP INDEX CONCURRENTLY`, `VALIDATE CONSTRAINT` and the
//! comments, each run on its own. `transaction_timeout` is off on the step's connection (a
//! concurrent build is one long implicit transaction).

use std::fmt;
use std::future::Future;
use std::num::NonZeroU32;
use std::time::Duration;

use erp_fields::{Error, ErrorKind, Result};
use erp_sql::conflict;
use erp_sql::ddl;
use rustc_hash::FxHashMap;

use crate::schema::{mark_to_compute, select_null_ids};
use crate::{Orm, SUPERUSER_ID};

/// `statement_timeout` of each statement of a [`SchemaStep`] (an index build or a validation on a
/// large table can take long; the limit is there so a stuck statement still ends).
pub const SCHEMA_STEP_STATEMENT_TIMEOUT: &str = "6h";

/// SQLSTATE of a not-null violation.
const NOT_NULL_VIOLATION: &str = "23502";

/// The longest `lock_timeout` PostgreSQL accepts: `i32::MAX` milliseconds.
const MAX_STEP_LOCK_TIMEOUT: Duration = Duration::from_millis(i32::MAX as u64);

/// How a schema synchronization treats large tables ([`crate::Transaction::init_models_with`]).
///
/// ```
/// use std::num::NonZeroU32;
/// use erp_orm::SchemaSyncOptions;
/// let options = SchemaSyncOptions::new()
///     .large_table_rows(50_000)
///     .backfill_batch(NonZeroU32::new(500).expect("not zero"));
/// assert_eq!(options.large_table_rows_limit(), Some(50_000));
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchemaSyncOptions {
    large_table_rows: Option<u64>,
    backfill_batch: NonZeroU32,
    fill_batch: NonZeroU32,
    lock_timeouts: StepTimeouts,
    lifecycle: bool,
    sql_roles: crate::SqlRoleBindings,
}

/// The timeouts of the statements of a step: two `lock_timeout`s, each within `1 ms..=i32::MAX ms`
/// (PostgreSQL's range; `0` would mean "wait forever"), and the `statement_timeout` of the
/// conflict diagnostic query a failed key build may run. All applied in whole milliseconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct StepTimeouts {
    /// waits that hold other sessions up (module docs)
    short: Duration,
    /// waits that hold no reader or writer up
    long: Duration,
    /// `statement_timeout` of a conflict diagnostic query
    diagnostic: Duration,
}

impl StepTimeouts {
    /// The `lock_timeout` setting of the waits that hold other sessions up.
    fn short_setting(self) -> String {
        format!("{}ms", self.short.as_millis())
    }

    /// The `lock_timeout` setting of the other waits.
    fn long_setting(self) -> String {
        format!("{}ms", self.long.as_millis())
    }

    /// The `statement_timeout` setting of a conflict diagnostic query.
    pub(crate) fn diagnostic_setting(self) -> String {
        erp_sql::conflict::timeout_setting(self.diagnostic)
    }
}

impl SchemaSyncOptions {
    /// Default [`SchemaSyncOptions::large_table_rows`].
    pub const DEFAULT_LARGE_TABLE_ROWS: u64 = 100_000;
    /// Default [`SchemaSyncOptions::backfill_batch`].
    pub const DEFAULT_BACKFILL_BATCH: NonZeroU32 = NonZeroU32::MIN.saturating_add(999);
    /// Default [`SchemaSyncOptions::fill_batch`].
    pub const DEFAULT_FILL_BATCH: NonZeroU32 = NonZeroU32::MIN.saturating_add(9_999);
    /// Default short lock wait of a step statement ([`SchemaSyncOptions::step_lock_timeouts`]):
    /// the schema transaction's own [`crate::SCHEMA_LOCK_TIMEOUT`].
    pub const DEFAULT_SHORT_LOCK_TIMEOUT: Duration = Duration::from_secs(10);
    /// Default long lock wait of a step statement ([`SchemaSyncOptions::step_lock_timeouts`]).
    pub const DEFAULT_LONG_LOCK_TIMEOUT: Duration = Duration::from_secs(3_600);

    /// The defaults: tables above [`Self::DEFAULT_LARGE_TABLE_ROWS`] rows are large.
    pub const fn new() -> Self {
        Self {
            large_table_rows: Some(Self::DEFAULT_LARGE_TABLE_ROWS),
            backfill_batch: Self::DEFAULT_BACKFILL_BATCH,
            fill_batch: Self::DEFAULT_FILL_BATCH,
            lock_timeouts: StepTimeouts {
                short: Self::DEFAULT_SHORT_LOCK_TIMEOUT,
                long: Self::DEFAULT_LONG_LOCK_TIMEOUT,
                diagnostic: erp_sql::conflict::DIAGNOSTIC_TIMEOUT,
            },
            lifecycle: false,
            sql_roles: crate::SqlRoleBindings::new(),
        }
    }

    /// No table is large: all work runs in the transaction ([`crate::Transaction::init_models`]).
    pub const fn inline() -> Self {
        let mut options = Self::new();
        options.large_table_rows = None;
        options
    }

    /// A table that existed before the synchronization is *large* when it has more than `rows`
    /// rows; `0` makes every table with rows large.
    #[must_use]
    pub const fn large_table_rows(mut self, rows: u64) -> Self {
        self.large_table_rows = Some(rows);
        self
    }

    /// Records computed and committed together by a backfill step, and flushed together by an
    /// inline backfill.
    #[must_use]
    pub const fn backfill_batch(mut self, records: NonZeroU32) -> Self {
        self.backfill_batch = records;
        self
    }

    /// Rows a not-null step fills with the field's default per statement.
    #[must_use]
    pub const fn fill_batch(mut self, rows: NonZeroU32) -> Self {
        self.fill_batch = rows;
        self
    }

    /// How long a step statement waits for a lock, in whole milliseconds. `short` bounds the
    /// waits that hold other sessions up: statements taking `ACCESS EXCLUSIVE` (every reader of
    /// the table queues behind them) and the batches of not-null fills and compute backfills
    /// (the writers of the rows a batch already changed wait for it). `long` bounds the others
    /// (`CREATE`/`DROP INDEX CONCURRENTLY`, `VALIDATE CONSTRAINT`, comments), which let reads
    /// and writes go on while they wait. A statement that gives up fails its step with a `Lock`
    /// error; the next synchronization plans the unfinished work again.
    ///
    /// # Errors
    ///
    /// `Programming` when a timeout is under 1 ms (PostgreSQL reads `0` as "wait forever") or
    /// over `i32::MAX` ms.
    pub fn step_lock_timeouts(mut self, short: Duration, long: Duration) -> Result<Self> {
        let accepted = |d: Duration| d >= Duration::from_millis(1) && d <= MAX_STEP_LOCK_TIMEOUT;
        if accepted(short) && accepted(long) {
            self.lock_timeouts = StepTimeouts {
                short,
                long,
                ..self.lock_timeouts
            };
            return Ok(self);
        }
        Err(Error::programming(format!(
            "schema step lock timeouts must be at least 1 ms and at most {} ms, got {} ms \
             (short) and {} ms (long)",
            MAX_STEP_LOCK_TIMEOUT.as_millis(),
            short.as_millis(),
            long.as_millis()
        )))
    }

    /// The `statement_timeout` of the one diagnostic query that lists the id pairs of a "existing
    /// records conflict" error (a business key, an exclusion or a temporal key added over rows
    /// that violate it); default [`erp_sql::conflict::DIAGNOSTIC_TIMEOUT`] (30 s). The
    /// query runs read-only on the failure path only; when it fails or times out the conflict is
    /// still the error, with no pair listed.
    ///
    /// # Errors
    ///
    /// `Programming` when `timeout` is under 1 ms or over `i32::MAX` ms.
    pub fn diagnostic_timeout(mut self, timeout: Duration) -> Result<Self> {
        if timeout < Duration::from_millis(1) || timeout > MAX_STEP_LOCK_TIMEOUT {
            return Err(Error::programming(format!(
                "the diagnostic timeout must be at least 1 ms and at most {} ms, got {} ms",
                MAX_STEP_LOCK_TIMEOUT.as_millis(),
                timeout.as_millis()
            )));
        }
        self.lock_timeouts.diagnostic = timeout;
        Ok(self)
    }

    /// Whether the synchronization runs inside an addon install or upgrade lifecycle transaction
    /// (default false). Only such a synchronization may create or change addon-owned SQL
    /// objects or their provenance (amendment A6); any other one that would fails with
    /// a `Programming` error naming the object, and passes when they are all current. The
    /// module system sets it on its install and upgrade schema step.
    #[must_use]
    pub const fn lifecycle(mut self, on: bool) -> Self {
        self.lifecycle = on;
        self
    }

    /// Whether [`Self::lifecycle`] is set.
    pub const fn is_lifecycle(&self) -> bool {
        self.lifecycle
    }

    /// The database roles of the SQL object role bindings (A2; default: none). A
    /// lifecycle synchronization validates every configured one, gives the `SECURITY DEFINER`
    /// functions to the definer role and reconciles the declared grants; an object that names a
    /// binding not configured here is refused by name.
    #[must_use]
    pub fn sql_roles(mut self, roles: crate::SqlRoleBindings) -> Self {
        self.sql_roles = roles;
        self
    }

    /// The role bindings set by [`Self::sql_roles`].
    pub fn sql_role_bindings(&self) -> crate::SqlRoleBindings {
        self.sql_roles.clone()
    }

    /// The row threshold above which a table is large; `None` for [`Self::inline`].
    pub const fn large_table_rows_limit(&self) -> Option<u64> {
        self.large_table_rows
    }

    pub(crate) const fn large_rows(&self) -> Option<u64> {
        self.large_table_rows
    }

    pub(crate) const fn backfill_records(&self) -> NonZeroU32 {
        self.backfill_batch
    }

    pub(crate) const fn fill_rows(&self) -> NonZeroU32 {
        self.fill_batch
    }

    pub(crate) const fn diagnostic_timeout_value(&self) -> Duration {
        self.lock_timeouts.diagnostic
    }

    pub(crate) const fn lock_timeouts(&self) -> StepTimeouts {
        self.lock_timeouts
    }
}

impl Default for SchemaSyncOptions {
    fn default() -> Self {
        Self::new()
    }
}

/// The outcome of [`crate::Transaction::init_models_with`]: the steps left for after the commit,
/// in the order they should run. Dropping it without running (or recording) the steps leaves
/// their work undone.
#[derive(Debug)]
#[must_use = "the steps must run after the commit (Orm::run_schema_step) or be recorded"]
pub struct SchemaSync {
    pub(crate) steps: Vec<SchemaStep>,
}

impl SchemaSync {
    /// The steps, in the order they should run.
    pub fn steps(&self) -> &[SchemaStep] {
        &self.steps
    }

    /// The steps, by value.
    pub fn into_steps(self) -> Vec<SchemaStep> {
        self.steps
    }

    /// Whether all work ran in the transaction.
    pub fn is_complete(&self) -> bool {
        self.steps.is_empty()
    }
}

/// What a [`SchemaStep`] does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum SchemaStepKind {
    /// `CREATE [UNIQUE] INDEX CONCURRENTLY [IF NOT EXISTS]` for a new or invalid index, or
    /// building under a temporary name and atomically swapping (`ALTER INDEX ... RENAME`) when
    /// replacing a valid index; the index's comment afterwards.
    CreateIndex,
    /// An index of a partitioned table that holds rows: `CREATE INDEX … ON ONLY` the parent
    /// (invalid until complete), `CREATE INDEX CONCURRENTLY` on each partition, `ALTER INDEX …
    /// ATTACH PARTITION`; the index's comment. A run that stopped half-way continues with the
    /// partitions that have no index attached yet.
    CreatePartitionedIndex,
    /// `VALIDATE CONSTRAINT` of a foreign key added `NOT VALID`.
    ValidateForeignKey,
    /// Fill a required column's NULLs with its default (batched), add the not-null constraint
    /// `NOT VALID` when missing, validate it.
    NotNull,
    /// Compute new stored computed columns for the records holding NULL (batched, each batch its
    /// own transaction).
    Backfill,
    /// Step 1 of a business key on a large table: `CREATE UNIQUE INDEX CONCURRENTLY` under the
    /// key's name (`NULLS NOT DISTINCT` unless the key opts out), after dropping an invalid
    /// leftover of an interrupted build. Existing records that conflict fail it with a
    /// `Validation` error naming up to 20 id pairs; the invalid index of the failed build is
    /// dropped at once. Until [`SchemaStepKind::AddKeyConstraint`] has run, an upsert naming the
    /// key fails (`Database`, "not usable yet").
    CreateKeyIndex,
    /// Step 2 of a business key on a large table: `ADD CONSTRAINT … UNIQUE USING INDEX` (no
    /// scan; a short `ACCESS EXCLUSIVE`) and the comment holding the key's canonical definition,
    /// in one statement batch. Does nothing when the constraint is already there.
    AddKeyConstraint,
}

/// Work on a large table left for after the schema transaction commits. Built by
/// [`crate::Transaction::init_models_with`] and [`Orm::plan_schema_step`] only; run it with
/// [`Orm::run_schema_step`]. It describes the work, not how patiently it waits: the lock
/// timeouts come from the [`SchemaSyncOptions`] it runs with, so a step planned again later
/// (possibly under other options) equals the one planned first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchemaStep {
    pub(crate) key: String,
    pub(crate) table: String,
    pub(crate) action: StepAction,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum StepAction {
    Index {
        name: String,
        statement: String,
        comment: Option<String>,
        /// A valid index of that name with another definition exists: built under a temporary name
        /// concurrently, then swapped into place in one short transaction so the table is never
        /// without an index.
        replaces: bool,
    },
    /// An index of a partitioned table, built partition by partition ([`SchemaStepKind::
    /// CreatePartitionedIndex`]); `statement` is the plain `CREATE INDEX` of the declaration.
    PartitionedIndex {
        name: String,
        statement: String,
        comment: Option<String>,
    },
    ValidateForeignKey {
        name: String,
    },
    NotNull {
        column: String,
        constraint: String,
        fill: Option<String>,
    },
    Backfill {
        model: &'static str,
        fields: Vec<&'static str>,
    },
    /// Step 1 of a business key ([`SchemaStepKind::CreateKeyIndex`]).
    KeyIndex(KeyBuild),
    /// Step 2 of a business key ([`SchemaStepKind::AddKeyConstraint`]).
    KeyConstraint(KeyBuild),
}

/// The key of step 1 of the business key whose constraint is `name`.
pub(crate) fn key_index_step_key(name: &str) -> String {
    format!("key_index.{name}")
}

/// The key of step 2 of the business key whose constraint is `name`.
pub(crate) fn key_constraint_step_key(name: &str) -> String {
    format!("key_constraint.{name}")
}

/// A business key built in two steps on a large table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct KeyBuild {
    pub(crate) model: &'static str,
    /// the declared name (`_quant_key`)
    pub(crate) declared: String,
    /// the key's constraint name, also the name of its index
    pub(crate) name: String,
    pub(crate) columns: Vec<String>,
    pub(crate) nulls_distinct: bool,
    /// the canonical definition (comment of the index, then of the constraint)
    pub(crate) definition: String,
}

impl KeyBuild {
    /// The key of step 1 (the index).
    pub(crate) fn index_step_key(&self) -> String {
        key_index_step_key(&self.name)
    }

    /// The key of step 2 (the constraint).
    pub(crate) fn constraint_step_key(&self) -> String {
        key_constraint_step_key(&self.name)
    }
}

impl SchemaStep {
    /// A stable key, unique within one synchronization and the same when a later
    /// synchronization plans the same work again: `index.<index>`, `validate.<constraint>`,
    /// `not_null.<table>.<column>`, `backfill.<table>`, `key_index.<key constraint>` and
    /// `key_constraint.<key constraint>` (the two steps of a business key). ASCII `a-z`, `0-9`, `_`, `.`.
    pub fn key(&self) -> &str {
        &self.key
    }

    /// The table the step works on.
    pub fn table(&self) -> &str {
        &self.table
    }

    /// The stored computed fields a backfill step computes, sorted; empty for other steps.
    pub fn backfill_fields(&self) -> Vec<&'static str> {
        match &self.action {
            StepAction::Backfill { fields, .. } => {
                let mut fields = fields.clone();
                fields.sort_unstable();
                fields
            }
            StepAction::Index { .. }
            | StepAction::PartitionedIndex { .. }
            | StepAction::ValidateForeignKey { .. }
            | StepAction::NotNull { .. }
            | StepAction::KeyIndex(_)
            | StepAction::KeyConstraint(_) => Vec::new(),
        }
    }

    /// What the work is, for telling whether a cursor stored by an earlier run belongs to it:
    /// the kind, the table and the sorted fields, column, index or constraint. A backfill
    /// planned again with more fields (another module added a stored computed field to the
    /// table) has another fingerprint, so a cursor stored for the smaller set is never resumed.
    pub fn fingerprint(&self) -> StepFingerprint {
        let text = match &self.action {
            StepAction::Index { name, .. } => format!("index:{}:{name}", self.table),
            StepAction::PartitionedIndex { name, .. } => {
                format!("partitioned_index:{}:{name}", self.table)
            }
            StepAction::KeyIndex(key) => format!("key_index:{}:{}", self.table, key.name),
            StepAction::KeyConstraint(key) => {
                format!("key_constraint:{}:{}", self.table, key.name)
            }
            StepAction::ValidateForeignKey { name } => {
                format!("validate:{}:{name}", self.table)
            }
            StepAction::NotNull { column, .. } => {
                format!("not_null:{}:{column}", self.table)
            }
            StepAction::Backfill { .. } => {
                format!(
                    "backfill:{}:{}",
                    self.table,
                    self.backfill_fields().join(",")
                )
            }
        };
        StepFingerprint::shortened(text)
    }

    /// What the step does.
    pub fn kind(&self) -> SchemaStepKind {
        match self.action {
            StepAction::Index { .. } => SchemaStepKind::CreateIndex,
            StepAction::PartitionedIndex { .. } => SchemaStepKind::CreatePartitionedIndex,
            StepAction::ValidateForeignKey { .. } => SchemaStepKind::ValidateForeignKey,
            StepAction::NotNull { .. } => SchemaStepKind::NotNull,
            StepAction::Backfill { .. } => SchemaStepKind::Backfill,
            StepAction::KeyIndex(_) => SchemaStepKind::CreateKeyIndex,
            StepAction::KeyConstraint(_) => SchemaStepKind::AddKeyConstraint,
        }
    }

    /// The key of the step of the same synchronization that must have succeeded before this one
    /// can: the index build before the key constraint that adopts it.
    pub(crate) fn depends_on(&self) -> Option<String> {
        match &self.action {
            StepAction::KeyConstraint(key) => Some(key.index_step_key()),
            _ => None,
        }
    }
}

/// What steps left as they found it while doing their work ([`Orm::run_schema_step`] for one
/// step, [`Orm::sync_schema_with`] for all steps of a synchronization): nearly always empty.
/// Show it to the operator or record it.
///
/// ```no_run
/// # async fn example(orm: &erp_orm::Orm) -> erp_fields::Result<()> {
/// let report = orm.sync_schema().await?;
/// for kept in report.kept_comments() {
///     eprintln!(
///         "kept the comment {:?} of column {}.{}",
///         kept.comment(),
///         kept.table(),
///         kept.column()
///     );
/// }
/// # Ok(()) }
/// ```
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[must_use = "the report lists the column comments a backfill kept; show or record it"]
#[non_exhaustive]
pub struct SchemaStepReport {
    kept_comments: Vec<KeptColumnComment>,
}

impl SchemaStepReport {
    /// Column comments a backfill left as they were. When the backfill finished they no longer
    /// carried its pending mark ([`erp_sql::ddl::PENDING_BACKFILL_COMMENT_PREFIX`]) and differed
    /// from the field's label: they were changed outside the ORM while it ran. The step did not
    /// replace them by the labels.
    pub fn kept_comments(&self) -> &[KeptColumnComment] {
        &self.kept_comments
    }

    /// Whether there is nothing to report.
    pub fn is_empty(&self) -> bool {
        self.kept_comments.is_empty()
    }

    fn merge(&mut self, other: Self) {
        self.kept_comments.extend(other.kept_comments);
    }
}

/// Longest [`StepFingerprint`] (bytes).
pub const MAX_STEP_FINGERPRINT_BYTES: usize = 128;

/// Longest [`SchemaStepKey`] (bytes): `not_null.<table>.<column>` with two 63-byte names is 136.
pub const MAX_STEP_KEY_BYTES: usize = 160;

/// What a [`SchemaStep`]'s work is ([`SchemaStep::fingerprint`]): at most
/// [`MAX_STEP_FINGERPRINT_BYTES`] bytes of `a-z`, `0-9`, `_`, `.`, `:`, `,`, `#`; text longer than
/// that is cut and ends in the hash of the whole.
///
/// ```
/// use erp_orm::StepFingerprint;
/// let f = StepFingerprint::parse("backfill:sale_order:name_size").unwrap();
/// assert_eq!(f.as_str(), "backfill:sale_order:name_size");
/// assert!(StepFingerprint::parse("Not Valid").is_err());
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct StepFingerprint(String);

impl StepFingerprint {
    /// Check and wrap a stored fingerprint.
    ///
    /// # Errors
    ///
    /// `Programming` for an empty or longer text or other characters.
    pub fn parse(text: &str) -> Result<Self> {
        let charset = text
            .bytes()
            .all(|b| matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'_' | b'.' | b':' | b',' | b'#'));
        if text.is_empty() || text.len() > MAX_STEP_FINGERPRINT_BYTES || !charset {
            return Err(Error::programming(format!(
                "invalid step fingerprint {text:?}: 1 to {MAX_STEP_FINGERPRINT_BYTES} bytes of \
                 a-z, 0-9, '_', '.', ':', ',', '#'"
            )));
        }
        Ok(Self(text.to_owned()))
    }

    /// The text of a step, cut to the limit with a hash of the whole when it is longer. The text
    /// is built from identifiers of the database, which are ASCII `a-z`, `0-9`, `_`.
    fn shortened(text: String) -> Self {
        if text.len() <= MAX_STEP_FINGERPRINT_BYTES {
            return Self(text);
        }
        let hash = text.bytes().fold(0xcbf2_9ce4_8422_2325_u64, |h, b| {
            (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3)
        });
        let head: String = text.chars().take(MAX_STEP_FINGERPRINT_BYTES - 17).collect();
        Self(format!("{head}#{hash:016x}"))
    }

    /// The text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for StepFingerprint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Where a batched step may start again: the cursor of an earlier run and the fingerprint of the
/// work it belonged to. A resume whose fingerprint is not the step's is not used
/// ([`Orm::run_schema_step_from`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resume {
    fingerprint: StepFingerprint,
    after: StepCursor,
}

impl Resume {
    /// The cursor `after` stored for the work `fingerprint`.
    pub fn new(fingerprint: StepFingerprint, after: StepCursor) -> Self {
        Self { fingerprint, after }
    }

    /// The work the cursor belonged to.
    pub fn fingerprint(&self) -> &StepFingerprint {
        &self.fingerprint
    }

    /// The cursor.
    pub fn after(&self) -> StepCursor {
        self.after
    }
}

/// The key of a [`SchemaStep`], checked: [`MAX_STEP_KEY_BYTES`] bytes of `a-z`, `0-9`, `_`, `.`,
/// shaped by its kind: `index.<name>`, `validate.<name>`, `not_null.<table>.<column>`,
/// `backfill.<table>`, `key_index.<key constraint>`, `key_constraint.<key constraint>`. An
/// `index.` key names the build of an index whatever its form: on a partitioned table the step
/// planned for it is a [`SchemaStepKind::CreatePartitionedIndex`].
///
/// ```
/// use erp_orm::{SchemaStepKey, SchemaStepKind};
/// let key = SchemaStepKey::parse("not_null.sale_order.qty").unwrap();
/// assert_eq!(key.kind(), SchemaStepKind::NotNull);
/// assert_eq!(key.table(), Some("sale_order"));
/// assert!(SchemaStepKey::parse("drop.everything").is_err());
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SchemaStepKey {
    text: String,
    kind: SchemaStepKind,
    table: Option<String>,
}

impl SchemaStepKey {
    /// Check and wrap a key.
    ///
    /// # Errors
    ///
    /// `Programming` for characters or a length outside the rule, a prefix that is no kind, or a
    /// shape that does not fit the kind (`not_null.` needs a table and a column, `backfill.` a
    /// table, the others a name).
    pub fn parse(key: &str) -> Result<Self> {
        let charset = key
            .bytes()
            .all(|b| matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'_' | b'.'));
        if key.is_empty() || key.len() > MAX_STEP_KEY_BYTES || !charset {
            return Err(Error::programming(format!(
                "invalid schema step key {key:?}: 1 to {MAX_STEP_KEY_BYTES} bytes of a-z, 0-9, \
                 '_', '.'"
            )));
        }
        let shape = |what: &str| {
            Error::programming(format!(
                "schema step key {key:?} is not a valid {what} step key"
            ))
        };
        let (kind, table) = if let Some(name) = key.strip_prefix("index.") {
            if name.is_empty() || name.contains('.') {
                return Err(shape("index.<name>"));
            }
            (SchemaStepKind::CreateIndex, None)
        } else if let Some(name) = key.strip_prefix("validate.") {
            if name.is_empty() || name.contains('.') {
                return Err(shape("validate.<name>"));
            }
            (SchemaStepKind::ValidateForeignKey, None)
        } else if let Some(name) = key.strip_prefix("key_index.") {
            if name.is_empty() || name.contains('.') {
                return Err(shape("key_index.<key constraint>"));
            }
            (SchemaStepKind::CreateKeyIndex, None)
        } else if let Some(name) = key.strip_prefix("key_constraint.") {
            if name.is_empty() || name.contains('.') {
                return Err(shape("key_constraint.<key constraint>"));
            }
            (SchemaStepKind::AddKeyConstraint, None)
        } else if let Some(rest) = key.strip_prefix("not_null.") {
            match rest.split_once('.') {
                Some((table, column))
                    if !table.is_empty() && !column.is_empty() && !column.contains('.') =>
                {
                    (SchemaStepKind::NotNull, Some(table.to_owned()))
                }
                _ => return Err(shape("not_null.<table>.<column>")),
            }
        } else if let Some(table) = key.strip_prefix("backfill.") {
            if table.is_empty() || table.contains('.') {
                return Err(shape("backfill.<table>"));
            }
            (SchemaStepKind::Backfill, Some(table.to_owned()))
        } else {
            return Err(Error::programming(format!(
                "schema step key {key:?} starts with no step kind (index., validate., not_null., \
                 backfill., key_index., key_constraint.)"
            )));
        };
        Ok(Self {
            text: key.to_owned(),
            kind,
            table,
        })
    }

    /// The key.
    pub fn as_str(&self) -> &str {
        &self.text
    }

    /// The kind of step the key names ([`SchemaStepKind::CreateIndex`] for every `index.` key).
    pub fn kind(&self) -> SchemaStepKind {
        self.kind
    }

    /// The table a `not_null.` or `backfill.` key names.
    pub fn table(&self) -> Option<&str> {
        self.table.as_deref()
    }

    /// The column of a `not_null.` key, the index of an `index.` key, the constraint of a
    /// `validate.`, `key_index.` or `key_constraint.` key; `None` for `backfill.`.
    pub fn object(&self) -> Option<&str> {
        match self.kind {
            SchemaStepKind::NotNull => self.text.rsplit_once('.').map(|(_, c)| c),
            SchemaStepKind::CreateIndex
            | SchemaStepKind::CreatePartitionedIndex
            | SchemaStepKind::ValidateForeignKey
            | SchemaStepKind::CreateKeyIndex
            | SchemaStepKind::AddKeyConstraint => self.text.split_once('.').map(|(_, n)| n),
            SchemaStepKind::Backfill => None,
        }
    }
}

impl fmt::Display for SchemaStepKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.text)
    }
}

/// The answer of [`Orm::plan_schema_step`].
#[derive(Debug, Clone, PartialEq, Eq)]
#[must_use = "a step that is neither run nor known to be finished is lost work"]
#[non_exhaustive]
pub enum Replanned {
    /// The work is still to do: run this step.
    Pending(SchemaStep),
    /// The registry declares the work and the catalog shows nothing left of it.
    Done,
    /// The registry of this ORM does not declare what the key names (an index, a foreign key, a
    /// required column, a stored computed field): it is not the registry the step was planned
    /// for, and "nothing left" would be a guess.
    NotDeclared {
        /// What is missing, for the message.
        what: String,
    },
}

/// Where a batched step stopped: the id of the last record of the last batch it committed. A
/// run started with it ([`Orm::run_schema_step_from`]) continues after that id instead of
/// scanning the part of the table that is done. Record ids start at 1, so the cursor is positive.
///
/// ```
/// use erp_orm::StepCursor;
/// let cursor = StepCursor::new(1_000).unwrap();
/// assert_eq!(cursor.get(), 1_000);
/// assert!(StepCursor::new(0).is_err());
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct StepCursor(i64);

impl StepCursor {
    /// The cursor after record `last_id`.
    ///
    /// # Errors
    ///
    /// `Programming` unless `last_id` is positive.
    pub fn new(last_id: i64) -> Result<Self> {
        if last_id > 0 {
            Ok(Self(last_id))
        } else {
            Err(Error::programming(format!(
                "a step cursor is the id of a record, which is positive, not {last_id}"
            )))
        }
    }

    /// The id of the last record of the last committed batch.
    pub const fn get(self) -> i64 {
        self.0
    }
}

/// Told after each committed batch of a batched step (a not-null fill or a compute backfill)
/// where it is, so the caller can store the [`StepCursor`] and resume from it
/// ([`Orm::run_schema_step_from`]). A batch's work is committed before this is called: when it
/// fails the step stops with that error (`schema step '<key>' failed`, the error as the source)
/// and the batches done stay done. A cursor that lags behind the work (the process dies between
/// a batch's commit and the store) is safe: rows a batch finished are skipped by their value,
/// so the next run only repeats the scan.
pub trait StepProgress: Send {
    /// A batch ended at `cursor` and the step goes on with the next one.
    fn committed(&mut self, cursor: StepCursor) -> impl Future<Output = Result<()>> + Send;
}

/// [`StepProgress`] that stores nothing ([`Orm::run_schema_step`]).
#[derive(Debug, Clone, Copy, Default)]
pub struct NoProgress;

impl StepProgress for NoProgress {
    async fn committed(&mut self, _cursor: StepCursor) -> Result<()> {
        Ok(())
    }
}

/// A column comment a backfill step kept ([`SchemaStepReport::kept_comments`]).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct KeptColumnComment {
    table: String,
    column: String,
    comment: Option<String>,
}

impl KeptColumnComment {
    /// The table of the column.
    pub fn table(&self) -> &str {
        &self.table
    }

    /// The column.
    pub fn column(&self) -> &str {
        &self.column
    }

    /// The comment found and kept; `None` when the column had none.
    pub fn comment(&self) -> Option<&str> {
        self.comment.as_deref()
    }
}

/// The steps of an [`Orm::sync_schema_with`] that failed after its schema change committed: the
/// source of the error it returns ([`SchemaStepFailures::of`]), so a caller (the module system's
/// post-commit queue, a health check) gets the failed steps as data, not as text.
#[derive(Debug)]
#[non_exhaustive]
pub struct SchemaStepFailures {
    failures: Vec<SchemaStepFailure>,
    total: usize,
    report: SchemaStepReport,
}

/// One failed step and its error.
#[derive(Debug)]
#[non_exhaustive]
pub struct SchemaStepFailure {
    step: SchemaStep,
    error: Error,
}

impl SchemaStepFailure {
    /// The step, to run again ([`Orm::run_schema_step`]) or to record.
    pub fn step(&self) -> &SchemaStep {
        &self.step
    }

    /// Its error ([`Orm::run_schema_step`]'s).
    pub fn error(&self) -> &Error {
        &self.error
    }
}

impl SchemaStepFailures {
    /// The failed steps of a [`Orm::sync_schema_with`] error; `None` for any other error.
    pub fn of(error: &Error) -> Option<&Self> {
        std::error::Error::source(error)?.downcast_ref::<Self>()
    }

    /// The failed steps, in the order they ran (never empty).
    pub fn failures(&self) -> &[SchemaStepFailure] {
        &self.failures
    }

    /// How many steps the synchronization ran, the failed ones included.
    pub fn total(&self) -> usize {
        self.total
    }

    /// What the steps that succeeded reported.
    pub fn report(&self) -> &SchemaStepReport {
        &self.report
    }
}

impl fmt::Display for SchemaStepFailures {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} of {} post-commit schema steps failed: ",
            self.failures.len(),
            self.total
        )?;
        for (i, failure) in self.failures.iter().enumerate() {
            if i > 0 {
                f.write_str("; ")?;
            }
            write!(f, "{}: {}", failure.step.key, failure.error)?;
            if let Some(cause) = std::error::Error::source(&failure.error) {
                write!(f, " ({cause})")?;
            }
        }
        Ok(())
    }
}

impl std::error::Error for SchemaStepFailures {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.failures
            .first()
            .map(|f| &f.error as &(dyn std::error::Error + 'static))
    }
}

/// The catalog's account of a key's unique index ([`StepConnection::key_index_state`]).
struct KeyIndexState {
    valid: bool,
    comment: Option<String>,
    indexdef: String,
    schema: Option<String>,
}

/// Catalog ownership metadata retained by a unique-constraint replacement.
struct IndexSwapConstraint {
    name: String,
    deferrable: bool,
    deferred: bool,
    comment: Option<String>,
}

/// A connection taken out of the pool for one step: closed when dropped, so the session settings
/// of the step stay with it.
struct StepConnection {
    client: deadpool_postgres::ClientWrapper,
}

impl Orm {
    /// Synchronize the schema with [`SchemaSyncOptions::new`], see [`Orm::sync_schema_with`].
    ///
    /// # Errors
    ///
    /// As [`Orm::sync_schema_with`].
    pub async fn sync_schema(&self) -> Result<SchemaStepReport> {
        self.sync_schema_with(&SchemaSyncOptions::new()).await
    }

    /// Create or update the tables of every model in one transaction
    /// ([`crate::Transaction::init_models_with`]), commit, then run the steps it left for large
    /// tables, all of them even when one fails, and return what they reported. The schema step
    /// of [`erp_registry::Databases::set_overlay`]: build the ORM of the new registry, sync, and
    /// the registry is installed only when the sync succeeded. With a [`crate::Signal`], the
    /// schema transaction first creates the generation table
    /// ([`crate::Transaction::install_signal`]), and a schema change in it bumps the registry
    /// generation.
    ///
    /// ```no_run
    /// # async fn example(
    /// #     dbs: &erp_registry::Databases,
    /// #     overlay: erp_registry::Overlay,
    /// #     pool: deadpool_postgres::Pool,
    /// # ) -> erp_fields::Result<()> {
    /// use erp_orm::{Orm, StaticPolicy};
    /// let registry = dbs
    ///     .set_overlay("db", overlay, |registry| async move {
    ///         let orm = Orm::builder(registry, pool).policy(StaticPolicy::AllowAll).build()?;
    ///         let report = orm.sync_schema().await?;
    ///         for kept in report.kept_comments() {
    ///             eprintln!("kept the comment of {}.{}", kept.table(), kept.column());
    ///         }
    ///         Ok(())
    ///     })
    ///     .await?;
    /// // serve the database with an ORM built on `registry` from now on
    /// # Ok(()) }
    /// ```
    ///
    /// # Errors
    ///
    /// `Programming` before any statement when a table required with
    /// [`crate::OrmBuilder::require_core_tables`] has no linked registration (naming the table
    /// and its owner crate). The errors of [`crate::Transaction::init_models`] and of the commit.
    /// A commit that fails
    /// because the connection was lost is indeterminate: the server may have committed the
    /// schema change before the connection dropped, so the schema may or may not match the new
    /// registry. Treat a `Database` error from the commit as "unknown", not as "unchanged":
    /// running the sync again is safe and converges either way.
    ///
    /// When steps fail, the schema change itself is committed: the error has the kind and
    /// SQLSTATE of the first failure, names every failed step, and its source is a
    /// [`SchemaStepFailures`] ([`SchemaStepFailures::of`]) holding each failed step with its
    /// error and the report of the steps that succeeded. Running the sync again plans the
    /// unfinished work again, an interrupted backfill included (its columns stay marked in the
    /// catalog until all of its batches have run).
    pub async fn sync_schema_with(&self, options: &SchemaSyncOptions) -> Result<SchemaStepReport> {
        // checked at build too; again here, before any statement, so the refusal names the
        // missing registration at the schema step that would have skipped it
        crate::core_ddl::validate(&self.shared.registry, self.required_core_tables())?;
        let mut tx = self.begin(SUPERUSER_ID).await?;
        // the generation table may not exist yet: this transaction opens without reading it
        tx.signal_read = false;
        if self.shared.signal.is_some() {
            // The schema lock before the signal table's lock: the upgrade of the table's check
            // takes ACCESS EXCLUSIVE on it, and a concurrent first sync that locked the table
            // first and waits for the schema lock would deadlock with this one. The lock is
            // re-taken for free by `init_models_with`. The schema limits are set first, so the
            // wait is bounded by (and the error names) SCHEMA_LOCK_TIMEOUT, not the ORM's own
            // timeouts; the caller's values go back before `init_models_with` saves them.
            let (caller, lock_timeout) = match tx.set_schema_limits(true).await {
                Ok(saved) => saved,
                Err(error) => return Err(tx.discard_after(error).await),
            };
            if let Err(error) = tx.take_schema_lock(&lock_timeout).await {
                return Err(tx.discard_after(error).await);
            }
            if let Err(error) = tx.install_signal_and_read().await {
                return Err(tx.discard_after(error).await);
            }
            if let Err(error) = tx.set_timeouts(&caller.0, &caller.1).await {
                return Err(tx.discard_after(error).await);
            }
        }
        // core tables: the ORM's own (the node registry), then those of the crates
        // above it (`erp-rpc`'s idempotency keys); after the models, what they refer
        // to in the models' tables (a foreign key to `res_users`)
        if let Err(error) =
            crate::core_ddl::run(&mut tx, crate::core_ddl::CorePhase::BeforeModels).await
        {
            return Err(tx.discard_after(error).await);
        }
        let sync = match tx.init_models_with(&[], options).await {
            Ok(sync) => sync,
            Err(error) => return Err(tx.discard_after(error).await),
        };
        // every model table exists now: nothing may stay deferred
        let after =
            match crate::core_ddl::run(&mut tx, crate::core_ddl::CorePhase::AfterModels).await {
                Ok(deferred) => crate::core_ddl::refuse_deferred(&deferred),
                Err(error) => Err(error),
            };
        if let Err(error) = after {
            return Err(tx.discard_after(error).await);
        }
        if let Err(error) = tx.commit().await {
            // a refused commit leaves the transaction open: roll it back
            return Err(tx.discard_after(error).await);
        }
        drop(tx);
        let steps = sync.into_steps();
        let total = steps.len();
        let mut report = SchemaStepReport::default();
        let mut failures: Vec<SchemaStepFailure> = Vec::new();
        for step in steps {
            // a step that needs an earlier one does not run after its failure: it is listed as
            // failed too, so the failure is never lost. The error is its own: a `Database` error
            // without a SQLSTATE that names the blocking step, not a copy of that step's
            // conflict or violation
            if let Some(needed) = step.depends_on()
                && failures.iter().any(|f| f.step.key == needed)
            {
                let error = Error::new(
                    ErrorKind::Database,
                    format!(
                        "schema step '{}' was not run: it needs step '{needed}', which failed \
                         (see that step's error)",
                        step.key
                    ),
                );
                failures.push(SchemaStepFailure { step, error });
                continue;
            }
            match self.run_schema_step(&step, options).await {
                Ok(step_report) => report.merge(step_report),
                Err(error) => failures.push(SchemaStepFailure { step, error }),
            }
        }
        let Some(first) = failures.first() else {
            return Ok(report);
        };
        let (kind, sqlstate) = (
            first.error.kind(),
            first.error.sqlstate().map(str::to_owned),
        );
        let keys: Vec<&str> = failures.iter().map(|f| f.step.key()).collect();
        let message = format!(
            "the schema change is committed, but {} of its {total} post-commit steps failed: {}",
            failures.len(),
            keys.join(", ")
        );
        Err(Error::new(kind, message)
            .with_sqlstate(sqlstate)
            .with_source(SchemaStepFailures {
                failures,
                total,
                report,
            }))
    }

    /// Run one step left by [`crate::Transaction::init_models_with`], after its transaction
    /// committed, and return what it left as it found it (nearly always nothing). Idempotent: a
    /// step that already ran does nothing more, a step that failed half-way finishes its work.
    /// `options` give the lock timeouts ([`SchemaSyncOptions::step_lock_timeouts`]) and the batch
    /// sizes of the run; the step itself holds only the work. Nothing stores the progress of a
    /// batched step here: [`Orm::run_schema_step_from`] resumes one.
    ///
    /// Statement steps run on a connection used only for the step, with `statement_timeout`
    /// [`SCHEMA_STEP_STATEMENT_TIMEOUT`], `transaction_timeout` off and the lock timeouts of the
    /// options. A not-null fill runs in batches by id, each continuing after the last id of the
    /// one before. A backfill computes as the superuser, one transaction per batch, by id; then
    /// one short transaction replaces the pending mark of each of its columns' comments by the
    /// field's label, unless the comment no longer carries the mark: such a comment (changed
    /// outside the ORM meanwhile) is kept and listed in the report
    /// ([`SchemaStepReport::kept_comments`]).
    ///
    /// The dedicated connection must use the same schema/search_path as the transaction that
    /// planned this step. Index catalog checks and generated DDL resolve in that schema.
    ///
    /// # Errors
    ///
    /// The error of the failing statement or computation, wrapped: its kind (a duplicate value of
    /// a unique index is `Validation`, a lock wait over the timeout `Lock`, …), the message
    /// `schema step '<key>' failed`, its SQLSTATE, the original error as the source. A
    /// concurrent index build that fails leaves an invalid index, which the next run drops and
    /// builds again; an index that is still invalid after the build is a `Database` error.
    /// `Programming` when the model or a field of a backfill is not in this ORM's registry,
    /// `Database` when a column of a backfill does not exist.
    pub async fn run_schema_step(
        &self,
        step: &SchemaStep,
        options: &SchemaSyncOptions,
    ) -> Result<SchemaStepReport> {
        self.run_schema_step_from(step, options, None, &mut NoProgress)
            .await
    }

    /// [`Orm::run_schema_step`] for a batched step that resumes after the cursor of `resume` when
    /// its fingerprint is the step's ([`SchemaStep::fingerprint`]; a cursor stored for other
    /// work is not used and the step starts at the beginning) and tells `progress` the cursor
    /// after each committed batch. A step without batches (an index, a foreign-key validation) ignores
    /// both.
    ///
    /// Row ids are assigned when a row is inserted, not when it commits, and a row can be set
    /// back to NULL, so a NULL can appear behind any cursor. Both batched steps therefore end
    /// with a closing pass from the start of the table over the rows still NULL (it touches no
    /// other row): a not-null step fails, leaving its constraint unadded or `NOT VALID`, when
    /// NULLs remain after it; a backfill fails, keeping the pending mark of its columns, when
    /// a required field still has NULLs. (A field that is not required may legitimately compute
    /// to NULL.) A cursor that lags behind the work only repeats a scan.
    ///
    /// # Errors
    ///
    /// As [`Orm::run_schema_step`]; the error of `progress` after a batch ends the step.
    pub async fn run_schema_step_from(
        &self,
        step: &SchemaStep,
        options: &SchemaSyncOptions,
        resume: Option<&Resume>,
        progress: &mut impl StepProgress,
    ) -> Result<SchemaStepReport> {
        // a cursor stored for other work (another field set, another column) is never resumed
        let usable = resume.filter(|r| r.fingerprint == step.fingerprint());
        let start = match usable {
            Some(resume) => resume.after.get(),
            // no usable cursor: from the beginning of the table
            None => 0,
        };
        let result = match &step.action {
            StepAction::Backfill { .. } => self.backfill(step, options, start, progress).await,
            action => self
                .run_statement_step(&step.table, action, options, start, progress)
                .await
                .map(|()| SchemaStepReport::default()),
        };
        result.map_err(|error| {
            Error::new(error.kind(), format!("schema step '{}' failed", step.key))
                .with_sqlstate(error.sqlstate().map(str::to_owned))
                .with_source(error)
        })
    }

    /// Plan one step again from the registry of this ORM and the catalog, the way
    /// [`crate::Transaction::init_models_with`] would plan it now with `table` large
    /// ([`Replanned::Pending`]), or report that nothing is left ([`Replanned::Done`]) or that
    /// the registry does not declare what the key names ([`Replanned::NotDeclared`]). A queue
    /// that stores a step as a reference (`table`, key, the fields of a backfill) runs this first
    /// and trusts nothing it stored. `recorded_fields` are the stored computed fields the
    /// queued backfill recorded: one that is still stored computed and has NULL rows is pending
    /// even when its column comment no longer carries the pending mark (a comment changed
    /// outside the ORM).
    ///
    /// Only the catalog is read and the table is probed for NULLs where a recorded field needs
    /// it; no DDL runs (not even the `unaccent` wrapper: it is read, not created, so an open
    /// transaction replacing it blocks nothing). The schema lock is not taken: a plan is a read,
    /// and not atomic with the run that follows; that is safe because every step is idempotent
    /// and plans again from the catalog each time it runs.
    ///
    /// Use the ORM of the registry the work belongs to (the one after the operation that
    /// planned it).
    ///
    /// # Errors
    ///
    /// `Programming` for a key that is not a [`SchemaStepKey`], when no model of the registry has
    /// `table`, or the catalog and registry cannot be reconciled (the errors of
    /// [`crate::Transaction::init_models`]'s planning, such as a database default on an ORM
    /// column); `Database` errors.
    pub async fn plan_schema_step(
        &self,
        table: &str,
        key: &str,
        options: &SchemaSyncOptions,
        recorded_fields: &[&str],
    ) -> Result<Replanned> {
        let key = SchemaStepKey::parse(key)?;
        let mut tx = self.begin(SUPERUSER_ID).await?;
        let planned = tx
            .plan_schema_step(table, &key, options, recorded_fields)
            .await;
        match planned {
            Ok(step) => {
                if let Err(error) = tx.commit().await {
                    return Err(tx.discard_after(error).await);
                }
                Ok(step)
            }
            Err(error) => Err(tx.discard_after(error).await),
        }
    }

    /// A connection taken out of the pool, with the step's session settings: the long lock
    /// timeout (statements that hold others up set the short one around themselves), the step
    /// statement timeout, no transaction timeout.
    async fn step_connection(&self, timeouts: StepTimeouts) -> Result<StepConnection> {
        let object = self
            .pool()
            .get()
            .await
            .map_err(crate::transaction::checkout_error)?;
        let step = StepConnection {
            client: deadpool_postgres::Object::take(object),
        };
        step.batch(&format!(
            "SET lock_timeout = {}; SET statement_timeout = {}; SET transaction_timeout = 0",
            erp_sql::literal(&timeouts.long_setting()),
            erp_sql::literal(SCHEMA_STEP_STATEMENT_TIMEOUT)
        ))
        .await
        .map_err(|e| self.map_db_error(e))?;
        Ok(step)
    }

    /// The stepwise build of an index on a partitioned table (design §7.10): `CREATE INDEX IF NOT
    /// EXISTS … ON ONLY` the parent (a catalog entry, invalid until every partition has an index
    /// attached; `SHARE` on the parent, so the short lock wait), then for each partition without
    /// an attached index `CREATE INDEX CONCURRENTLY` and `ALTER INDEX … ATTACH PARTITION`. Every
    /// statement that queues others behind it (the drop of a stale parent index, the `ON ONLY`
    /// creation, the attach) runs under the short lock timeout; the concurrent builds wait the long
    /// one, nobody queues behind them.
    ///
    /// Idempotent: an unfinished parent index of the same declaration keeps the indexes already
    /// attached; one of another declaration is dropped with them and built again. The declaration
    /// is told by the parent index's comment: the declared comment, or for a declaration without
    /// one (a field index) a building marker ([`ddl::partitioned_build_comment`]) removed when the
    /// index is complete; an unfinished parent index without a comment is of unknown origin and
    /// is rebuilt.
    ///
    /// Each partition takes the partition upkeep lock of the table (the lock the retention upkeep
    /// takes) only for its two checks and the attach, never across the concurrent build (see
    /// `index_partition`); during the build the partition's own `SHARE UPDATE EXCLUSIVE` lock keeps
    /// a retirement out, and the upkeep can create the next window meanwhile.
    async fn build_partitioned_index(
        &self,
        conn: &StepConnection,
        table: &str,
        name: &str,
        statement: &str,
        comment: Option<&str>,
        timeouts: StepTimeouts,
    ) -> Result<()> {
        let db = |e| self.map_db_error(e);
        let no_form = || {
            Error::programming(format!(
                "index {name}: no partitioned form of {statement} (an index statement of table \
                 {table} was expected)"
            ))
        };
        let on_only = ddl::create_index_on_only(statement, name, table).ok_or_else(no_form)?;
        // what identifies this declaration on an unfinished parent index
        let marker = comment.map_or_else(
            || ddl::partitioned_build_comment(name, statement),
            str::to_owned,
        );
        // dropping a partitioned index takes ACCESS EXCLUSIVE on the parent and every partition,
        // and so does a wait that queues behind a long reader: short waits for all of it
        conn.set_lock_timeout(&timeouts.short_setting())
            .await
            .map_err(db)?;
        let state = conn.index_state(name).await.map_err(db)?;
        let resumes = matches!(&state, Some((false, Some(have))) if marker
        == format!(
            "COMMENT ON INDEX {} IS {}",
            erp_sql::quote(name),
            erp_sql::literal(have)
        ));
        if !resumes {
            if matches!(state, Some((false, _))) {
                // a build of another declaration, or of unknown origin: its partition indexes are
                // of no use
                conn.batch(&ddl::drop_index(name)).await.map_err(db)?;
            }
            // a crash can leave built but unattached partition indexes of an earlier declaration
            // under the names this build uses; `IF NOT EXISTS` would keep them and every attach
            // would then fail on the definition mismatch
            self.drop_unattached_partition_indexes(conn, table, name)
                .await?;
        }
        conn.batch(&on_only).await.map_err(db)?;
        conn.batch(&marker).await.map_err(db)?;
        conn.set_lock_timeout(&timeouts.long_setting())
            .await
            .map_err(db)?;
        for partition in conn
            .partitions_without_index(table, name)
            .await
            .map_err(db)?
        {
            self.index_partition(conn, table, name, statement, &partition, timeouts)
                .await?;
        }
        if conn.index_valid(name).await.map_err(db)? != Some(true) {
            return Err(Error::new(
                ErrorKind::Database,
                format!("index {name} is missing or invalid after its partitions were attached"),
            ));
        }
        if comment.is_none() {
            conn.batch(&ddl::clear_index_comment(name))
                .await
                .map_err(db)?;
        }
        Ok(())
    }

    /// The index of one partition for the parent index `name` ([`build_partitioned_index`](
    /// Self::build_partitioned_index)): the table's partition upkeep lock guards the checks before
    /// the build and the attach after it, not the build itself.
    async fn index_partition(
        &self,
        conn: &StepConnection,
        table: &str,
        name: &str,
        statement: &str,
        partition: &str,
        timeouts: StepTimeouts,
    ) -> Result<()> {
        let db = |e| self.map_db_error(e);
        let no_form = || {
            Error::programming(format!(
                "index {name}: no partitioned form of {statement} (an index statement of table \
                 {table} was expected)"
            ))
        };
        let held = |e: tokio_postgres::Error| {
            let error = self.map_db_error(e);
            Error::new(
                error.kind(),
                format!("index {name}: could not try the partition upkeep lock of table '{table}'"),
            )
            .with_sqlstate(error.sqlstate().map(str::to_owned))
            .with_source(error)
        };
        // the bounded wait for the lock ran out
        let busy = || {
            Error::new(
                ErrorKind::Lock,
                format!(
                    "index {name}: could not take the partition upkeep lock of table '{table}' \
                     within {} (an upkeep call or a schema synchronization holds it); the step \
                     resumes at the next synchronization",
                    timeouts.long_setting()
                ),
            )
            .with_sqlstate(Some("55P03".to_owned()))
        };
        let not_held = || {
            Error::new(
                ErrorKind::Database,
                format!("the partition upkeep lock of table '{table}' was not held when released"),
            )
        };
        // The lock is held only for the checks and the attach, never across the build: a
        // `CREATE INDEX CONCURRENTLY` waits for every transaction with an older snapshot, and an
        // upkeep call or a sync blocked on this lock holds one, so holding it through the build
        // would be a deadlock. The build's own `SHARE UPDATE EXCLUSIVE` lock on the partition
        // keeps a concurrent detach or drop out meanwhile; one that got in before it is found by
        // the second check.
        if !conn
            .partition_lock(table, timeouts.long)
            .await
            .map_err(held)?
        {
            return Err(busy());
        }
        let open = self.partition_is_open(conn, table, name, partition).await;
        let unlocked = conn.partition_unlock(table).await;
        if !open? {
            // a retirement detached the partition, or another run indexed it
            return if unlocked.map_err(db)? {
                Ok(())
            } else {
                Err(not_held())
            };
        }
        if !unlocked.map_err(db)? {
            return Err(not_held());
        }
        let index = erp_registry::ident::shorten_identifier(&format!("{partition}__{name}"));
        if conn.index_valid(&index).await.map_err(db)? == Some(false) {
            conn.batch(&format!(
                "DROP INDEX CONCURRENTLY IF EXISTS {}",
                erp_sql::quote(&index)
            ))
            .await
            .map_err(db)?;
        }
        let build =
            ddl::create_partition_index_concurrently(statement, name, table, partition, &index)
                .ok_or_else(no_form)?;
        conn.batch(&build).await.map_err(db)?;
        if conn.index_valid(&index).await.map_err(db)? != Some(true) {
            return Err(Error::new(
                ErrorKind::Database,
                format!("index {index} is missing or invalid after its concurrent build"),
            ));
        }
        if !conn
            .partition_lock(table, timeouts.long)
            .await
            .map_err(held)?
        {
            return Err(busy());
        }
        let attached = self
            .attach_built_partition_index(conn, table, name, partition, &index, timeouts)
            .await;
        let unlocked = conn.partition_unlock(table).await;
        attached?;
        if !unlocked.map_err(db)? {
            return Err(not_held());
        }
        Ok(())
    }

    /// Whether `partition` is still attached (not pending a detach) and without an index attached
    /// to the parent index `name`.
    async fn partition_is_open(
        &self,
        conn: &StepConnection,
        table: &str,
        name: &str,
        partition: &str,
    ) -> Result<bool> {
        Ok(conn
            .partitions_without_index(table, name)
            .await
            .map_err(|e| self.map_db_error(e))?
            .iter()
            .any(|p| p == partition))
    }

    /// Attach the built `index` of `partition` to the parent index `name`, under the partition
    /// upkeep lock the caller holds: a partition that changed since the build began is an error
    /// naming it.
    async fn attach_built_partition_index(
        &self,
        conn: &StepConnection,
        table: &str,
        name: &str,
        partition: &str,
        index: &str,
        timeouts: StepTimeouts,
    ) -> Result<()> {
        let db = |e| self.map_db_error(e);
        if !self.partition_is_open(conn, table, name, partition).await? {
            return Err(Error::new(
                ErrorKind::Database,
                format!(
                    "index {name}: partition {partition} of table '{table}' changed while its \
                     index {index} was built (it was detached, dropped or indexed by another \
                     run); the step resumes at the next synchronization"
                ),
            ));
        }
        // the attach locks the partition (queries on it queue behind a wait): short
        conn.set_lock_timeout(&timeouts.short_setting())
            .await
            .map_err(db)?;
        if let Err(e) = conn.batch(&ddl::attach_partition_index(name, index)).await {
            return Err(
                if e.code() == Some(&tokio_postgres::error::SqlState::LOCK_NOT_AVAILABLE) {
                    let error = self.map_db_error(e);
                    Error::new(
                        error.kind(),
                        format!(
                            "index {name}: attaching {index} to it was cut by the short lock \
                             timeout (a query holds {partition}); the index stays built and the \
                             step resumes at the next synchronization"
                        ),
                    )
                    .with_sqlstate(error.sqlstate().map(str::to_owned))
                    .with_source(error)
                } else {
                    db(e)
                },
            );
        }
        conn.set_lock_timeout(&timeouts.long_setting())
            .await
            .map_err(db)
    }

    /// Drop the partition indexes `<partition>__<name>` of `table` that no parent index has
    /// attached (`DROP INDEX CONCURRENTLY`, outside a transaction).
    async fn drop_unattached_partition_indexes(
        &self,
        conn: &StepConnection,
        table: &str,
        name: &str,
    ) -> Result<()> {
        let db = |e| self.map_db_error(e);
        let partitions = conn.partitions_of(table).await.map_err(db)?;
        let candidates: Vec<String> = partitions
            .iter()
            .map(|p| erp_registry::ident::shorten_identifier(&format!("{p}__{name}")))
            .collect();
        for index in conn
            .unattached_indexes(&partitions, &candidates)
            .await
            .map_err(db)?
        {
            conn.batch(&format!(
                "DROP INDEX CONCURRENTLY IF EXISTS {}",
                erp_sql::quote(&index)
            ))
            .await
            .map_err(db)?;
        }
        Ok(())
    }

    fn map_db_error(&self, error: tokio_postgres::Error) -> Error {
        crate::dberror::map_pg_error(self.registry(), error)
    }

    async fn index_swap_constraint(
        &self,
        conn: &StepConnection,
        table: &str,
        name: &str,
    ) -> Result<Option<IndexSwapConstraint>> {
        let db = |error| self.map_db_error(error);
        let row = conn.client.query_opt(
            "SELECT t.relname::text, k.conname::text, k.contype::text, \
                    k.condeferrable, k.condeferred, obj_description(k.oid, 'pg_constraint'), \
                    EXISTS (SELECT 1 FROM pg_constraint f \
                             WHERE f.contype = 'f' AND f.conindid = i.indexrelid) \
               FROM pg_index i JOIN pg_class x ON x.oid = i.indexrelid \
               JOIN pg_namespace n ON n.oid = x.relnamespace \
               JOIN pg_class t ON t.oid = i.indrelid \
               LEFT JOIN pg_constraint k ON k.conindid = i.indexrelid AND k.contype IN ('u','p','x') \
              WHERE n.nspname = current_schema() AND x.relname = $1",
            &[&name],
        ).await.map_err(db)?.ok_or_else(|| Error::programming(format!(
            "index {name}: live index is missing; nothing was built"
        )))?;
        let owner_table: String = row.try_get(0).map_err(db)?;
        if owner_table != table {
            return Err(Error::programming(format!(
                "index {name}: belongs to table {owner_table}, not {table}; nothing was built"
            )));
        }
        let dependent: bool = row.try_get(6).map_err(db)?;
        if dependent {
            return Err(Error::programming(format!(
                "index {name}: dependent foreign keys prevent replacing it; nothing was built; migrate the dependent keys before retrying"
            )));
        }
        let owner: Option<String> = row.try_get(2).map_err(db)?;
        match owner.as_deref() {
            None => Ok(None),
            Some("u") => Ok(Some(IndexSwapConstraint {
                name: row.try_get(1).map_err(db)?,
                deferrable: row.try_get(3).map_err(db)?,
                deferred: row.try_get(4).map_err(db)?,
                comment: row.try_get(5).map_err(db)?,
            })),
            Some(kind) => Err(Error::programming(format!(
                "index {name}: constraint kind {kind} cannot be replaced by a unique-index swap; nothing was built"
            ))),
        }
    }

    async fn run_statement_step(
        &self,
        table: &str,
        action: &StepAction,
        options: &SchemaSyncOptions,
        start: i64,
        progress: &mut impl StepProgress,
    ) -> Result<()> {
        let timeouts = options.lock_timeouts();
        let conn = self.step_connection(timeouts).await?;
        let db = |e| self.map_db_error(e);
        match action {
            StepAction::Index {
                name,
                statement,
                comment,
                replaces,
            } => {
                if !conn.step_lock(name).await.map_err(db)? {
                    return Err(Error::new(
                        ErrorKind::Lock,
                        format!(
                            "index {name}: another runner is building this index (its per-step \
                             lock is held); run the step again when that runner has finished"
                        ),
                    ));
                }

                if !*replaces {
                    if conn.index_valid(name).await.map_err(db)? == Some(false) {
                        conn.batch(&format!(
                            "DROP INDEX CONCURRENTLY IF EXISTS {}",
                            erp_sql::quote(name)
                        ))
                        .await
                        .map_err(db)?;
                    }
                    conn.batch(statement).await.map_err(db)?;
                    if conn.index_valid(name).await.map_err(db)? != Some(true) {
                        return Err(Error::new(
                            ErrorKind::Database,
                            format!(
                                "index {name} is missing or invalid after its concurrent build"
                            ),
                        ));
                    }
                    if let Some(comment) = comment {
                        conn.batch(comment).await.map_err(db)?;
                    }
                    return Ok(());
                }

                // A declaration comment is authoritative. Commentless field indexes keep a
                // completed-step marker; never infer equality by parsing generated SQL.
                let final_marker = match comment {
                    Some(comment) => comment.clone(),
                    None => ddl::comment_on_index(name, &format!("erp:built {statement}")),
                };
                if conn.index_valid(name).await.map_err(db)? == Some(true)
                    && let Some(current) = conn.index_comment(name).await.map_err(db)?
                    && ddl::comment_on_index(name, &current) == final_marker
                {
                    return Ok(());
                }
                // Check ownership and FKs before cleaning up or building any temporary index.
                let constraint = self.index_swap_constraint(&conn, table, name).await?;

                let temp_name = ddl::swap_index_name(name);
                // A declared schema object may legitimately use the derived swap name.
                // Never treat that object's in-flight build as this step's repair artifact.
                let declared = self.registry().models().iter().any(|model| {
                    model
                        .table_objects
                        .iter()
                        .any(|object| object.full_name == temp_name)
                        || model.column_fields().any(|field| {
                            field.index.is_some()
                                && erp_registry::ident::index_name(&model.table, field.name)
                                    == temp_name
                        })
                        || (model.parent_path.is_some()
                            && erp_registry::ident::index_name(&model.table, "parent_path_c")
                                == temp_name)
                });
                if declared {
                    return Err(Error::programming(format!(
                        "swap index {temp_name}: name belongs to a declared schema object; nothing was built; live index {name} is unchanged"
                    )));
                }
                let temp_valid = conn.index_valid(&temp_name).await.map_err(db)?;
                if temp_valid.is_some()
                    && self
                        .index_swap_constraint(&conn, table, &temp_name)
                        .await?
                        .is_some()
                {
                    return Err(Error::programming(format!(
                        "swap index {temp_name}: owned by a constraint; nothing was built; live index {name} is unchanged"
                    )));
                }
                if temp_valid == Some(false) {
                    conn.batch(&format!(
                        "DROP INDEX CONCURRENTLY IF EXISTS {}",
                        erp_sql::quote(&temp_name)
                    ))
                    .await
                    .map_err(db)?;
                }

                let temp_marker = ddl::partitioned_build_comment(&temp_name, statement);

                let need_build = if temp_valid == Some(true) {
                    let current = conn.index_comment(&temp_name).await.map_err(db)?;
                    if current.as_deref().is_some_and(|current| {
                        ddl::comment_on_index(&temp_name, current) == temp_marker
                    }) {
                        false
                    } else if current
                        .as_deref()
                        .is_some_and(|current| current.starts_with("erp:building "))
                    {
                        conn.batch(&format!(
                            "DROP INDEX CONCURRENTLY IF EXISTS {}",
                            erp_sql::quote(&temp_name)
                        ))
                        .await
                        .map_err(db)?;
                        true
                    } else {
                        return Err(Error::programming(format!(
                            "swap index {temp_name}: valid index has no ERP build marker; inspect and rename or remove it before retrying; live index {name} is unchanged"
                        )));
                    }
                } else {
                    true
                };

                if need_build {
                    let build_res = conn.batch(statement).await;
                    let build_err = match build_res {
                        Ok(_) => {
                            if conn.index_valid(&temp_name).await.map_err(db)? != Some(true) {
                                Some(Error::new(
                                    ErrorKind::Database,
                                    format!(
                                        "swap index {temp_name} is missing or invalid after its concurrent \
                                         build"
                                    ),
                                ))
                            } else {
                                None
                            }
                        }
                        Err(e) => Some(self.map_db_error(e)),
                    };

                    if let Some(err) = build_err {
                        let drop_res = conn
                            .batch(&format!(
                                "DROP INDEX CONCURRENTLY IF EXISTS {}",
                                erp_sql::quote(&temp_name)
                            ))
                            .await;
                        match drop_res {
                            Ok(_) => {
                                let msg = format!(
                                    "{err}: failed to build swap index {temp_name}; live index {name} is unchanged and still in use"
                                );
                                return Err(Error::database(
                                    err.kind(),
                                    err.sqlstate().map(str::to_owned),
                                    msg,
                                    err,
                                ));
                            }
                            Err(drop_err) => {
                                let drop_mapped = self.map_db_error(drop_err);
                                let msg = format!(
                                    "{err}: failed to build swap index {temp_name} and cleanup also failed ({drop_mapped}); invalid swap index {temp_name} is left for the next run; live index {name} is unchanged and still in use"
                                );
                                return Err(Error::database(
                                    err.kind(),
                                    err.sqlstate().map(str::to_owned),
                                    msg,
                                    err,
                                ));
                            }
                        }
                    }
                    conn.batch(&temp_marker).await.map_err(db)?;
                }

                conn.set_lock_timeout(&timeouts.short_setting())
                    .await
                    .map_err(db)?;
                let mut swap_sql = String::from("BEGIN;\n");
                if let Some(constraint) = &constraint {
                    swap_sql.push_str(&format!(
                        "ALTER TABLE {} DROP CONSTRAINT {};\n\
                         ALTER TABLE {} ADD CONSTRAINT {} UNIQUE USING INDEX {}{};\n",
                        erp_sql::quote(table),
                        erp_sql::quote(&constraint.name),
                        erp_sql::quote(table),
                        erp_sql::quote(name),
                        erp_sql::quote(&temp_name),
                        if constraint.deferred {
                            " DEFERRABLE INITIALLY DEFERRED"
                        } else if constraint.deferrable {
                            " DEFERRABLE INITIALLY IMMEDIATE"
                        } else {
                            " NOT DEFERRABLE"
                        },
                    ));
                    if let Some(comment) = &constraint.comment {
                        swap_sql.push_str(&format!(
                            "COMMENT ON CONSTRAINT {} ON {} IS {};\n",
                            erp_sql::quote(name),
                            erp_sql::quote(table),
                            erp_sql::literal(comment),
                        ));
                    }
                } else {
                    swap_sql.push_str(&format!(
                        "DROP INDEX {};\n\
                         ALTER INDEX {} RENAME TO {};\n",
                        erp_sql::quote(name),
                        erp_sql::quote(&temp_name),
                        erp_sql::quote(name),
                    ));
                }
                swap_sql.push_str(&final_marker);
                swap_sql.push_str(";\n");
                swap_sql.push_str("COMMIT");
                conn.batch(&swap_sql).await.map_err(db)?;
            }
            StepAction::PartitionedIndex {
                name,
                statement,
                comment,
            } => {
                // one runner per index: a second would see the first's concurrent build in
                // progress as invalid and drop it (design §7.10)
                if !conn.step_lock(name).await.map_err(db)? {
                    return Err(Error::new(
                        ErrorKind::Lock,
                        format!(
                            "index {name}: another runner is building this index (its per-step \
                             lock is held); run the step again when that runner has finished"
                        ),
                    ));
                }
                let built = self
                    .build_partitioned_index(
                        &conn,
                        table,
                        name,
                        statement,
                        comment.as_deref(),
                        timeouts,
                    )
                    .await;
                // on success the release must report the lock held; after a failure the
                // original error stands (the session ends with the step and frees the lock)
                match (built, conn.step_unlock(name).await) {
                    (Ok(()), Ok(true)) => {}
                    (Ok(()), Ok(false)) => {
                        return Err(Error::new(
                            ErrorKind::Database,
                            format!("the per-step lock of index {name} was not held when released"),
                        ));
                    }
                    (Ok(()), Err(e)) => return Err(db(e)),
                    (Err(e), _) => return Err(e),
                }
            }
            StepAction::ValidateForeignKey { name } => {
                conn.batch(&ddl::validate_constraint(table, name))
                    .await
                    .map_err(db)?;
            }
            StepAction::NotNull {
                column,
                constraint,
                fill,
            } => {
                // a fill batch waiting for a row lock holds the rows it already filled, and the
                // ACCESS EXCLUSIVE of `ADD CONSTRAINT` queues every reader behind it: short waits
                conn.set_lock_timeout(&timeouts.short_setting())
                    .await
                    .map_err(db)?;
                if let Some(lit) = fill {
                    let fill_sql = ddl::fill_nulls_batch(table, column, lit);
                    let batch = options.fill_rows();
                    let flat = |e| match e {
                        FillError::Db(e) => db(e),
                        FillError::Other(e) => e,
                    };
                    conn.fill_nulls(&fill_sql, batch, start, progress)
                        .await
                        .map_err(flat)?;
                    // ids are assigned at insert, not at commit, and a row can be set back to
                    // NULL: one closing pass from the start fills what is left behind the cursor
                    conn.fill_nulls(&fill_sql, batch, 0, &mut NoProgress)
                        .await
                        .map_err(flat)?;
                    if conn.has_null(table, column).await.map_err(db)? {
                        // the same error as the validation the step ends with (not-null
                        // violation, SQLSTATE 23502)
                        return Err(Error::new(
                            ErrorKind::Validation,
                            format!(
                                "column {table}.{column} still has rows with NULL after the \
                                 closing pass over the whole table; its not-null constraint is \
                                 not added or validated"
                            ),
                        )
                        .with_sqlstate(Some(NOT_NULL_VIOLATION.to_owned())));
                    }
                }
                let name = match conn.not_null_constraint(table, column).await.map_err(db)? {
                    Some(name) => name,
                    None => {
                        conn.batch(&ddl::add_not_null_not_valid(table, column, constraint))
                            .await
                            .map_err(db)?;
                        constraint.clone()
                    }
                };
                // `VALIDATE` takes SHARE UPDATE EXCLUSIVE: reads and writes go on while it waits
                conn.set_lock_timeout(&timeouts.long_setting())
                    .await
                    .map_err(db)?;
                conn.batch(&ddl::validate_constraint(table, &name))
                    .await
                    .map_err(db)?;
            }
            StepAction::Backfill { .. } => {
                return Err(Error::programming(
                    "a backfill is not a statement step".to_owned(),
                ));
            }
            StepAction::KeyIndex(key) => {
                // one runner per key: a second would see the first's concurrent build in
                // progress as invalid and drop it
                let lock = key.index_step_key();
                self.with_step_lock(
                    &conn,
                    &lock,
                    self.build_key_index(&conn, table, key, timeouts),
                )
                .await?;
            }
            StepAction::KeyConstraint(key) => {
                let lock = key.constraint_step_key();
                self.with_step_lock(
                    &conn,
                    &lock,
                    self.add_key_constraint(&conn, table, key, timeouts),
                )
                .await?;
            }
        }
        Ok(())
    }

    /// Run `work` holding the per-step advisory lock `name` (a second runner of the same step
    /// gets a `Lock` error), and report a lock that was not held when released.
    async fn with_step_lock(
        &self,
        conn: &StepConnection,
        name: &str,
        work: impl std::future::Future<Output = Result<()>>,
    ) -> Result<()> {
        let db = |e| self.map_db_error(e);
        if !conn.step_lock(name).await.map_err(db)? {
            return Err(Error::new(
                ErrorKind::Lock,
                format!(
                    "{name}: another runner is running this step (its per-step lock is held); \
                     run the step again when that runner has finished"
                ),
            ));
        }
        // on success the release must report the lock held; after a failure the original error
        // stands (the session ends with the step and frees the lock)
        match (work.await, conn.step_unlock(name).await) {
            (Ok(()), Ok(true)) => Ok(()),
            (Ok(()), Ok(false)) => Err(Error::new(
                ErrorKind::Database,
                format!("the per-step lock {name} was not held when released"),
            )),
            (Ok(()), Err(e)) => Err(db(e)),
            (Err(e), _) => Err(e),
        }
    }

    /// Step 1 of a business key (design §11.1): the unique index, built concurrently. Nothing to
    /// do when the constraint exists already or the index was built for this definition; an
    /// invalid leftover is dropped first; a valid index of the name with the key's exact shape
    /// but no comment (the build finished, the comment did not) gets its comment; a valid index
    /// of the name that is not this key's unique index is never replaced.
    async fn build_key_index(
        &self,
        conn: &StepConnection,
        table: &str,
        key: &KeyBuild,
        timeouts: StepTimeouts,
    ) -> Result<()> {
        let db = |e| self.map_db_error(e);
        let KeyBuild {
            model,
            declared,
            name,
            definition,
            ..
        } = key;
        if conn
            .constraint_comment(table, name)
            .await
            .map_err(db)?
            .is_some()
        {
            return Ok(());
        }
        match conn.key_index_state(name).await.map_err(db)? {
            Some(state) if state.valid => {
                let shaped = self.key_index_shaped(&state, table, key);
                match state.comment.as_deref() {
                    Some(c) if shaped && c == definition => return Ok(()),
                    None if shaped => {
                        conn.batch(&ddl::comment_on_index(name, definition))
                            .await
                            .map_err(db)?;
                        return Ok(());
                    }
                    _ => {
                        return Err(Error::programming(format!(
                            "cannot build the unique index of key '{declared}' of '{model}': an \
                             index named '{name}' exists that is not this key's unique index \
                             (another shape, columns, comment or uniqueness); it is not \
                             replaced, drop it or rename the key"
                        )));
                    }
                }
            }
            Some(_) => {
                conn.batch(&format!(
                    "DROP INDEX CONCURRENTLY IF EXISTS {}",
                    erp_sql::quote(name)
                ))
                .await
                .map_err(db)?;
            }
            None => {}
        }
        let cols: Vec<&str> = key.columns.iter().map(String::as_str).collect();
        let create = ddl::create_key_index_concurrently(table, name, &cols, key.nulls_distinct);
        if let Err(error) = conn.batch(&create).await {
            if error.code() != Some(&tokio_postgres::error::SqlState::UNIQUE_VIOLATION) {
                return Err(db(error));
            }
            return Err(self.key_conflict(conn, table, key, timeouts, error).await);
        }
        if conn.index_valid(name).await.map_err(db)? != Some(true) {
            return Err(Error::new(
                ErrorKind::Database,
                format!("index {name} is missing or invalid after its concurrent build"),
            ));
        }
        conn.batch(&ddl::comment_on_index(name, definition))
            .await
            .map_err(db)?;
        Ok(())
    }

    /// Whether the valid index `state` is exactly the unique index of `key` on `table`.
    fn key_index_shaped(&self, state: &KeyIndexState, table: &str, key: &KeyBuild) -> bool {
        let cols: Vec<&str> = key.columns.iter().map(String::as_str).collect();
        ddl::key_index_matches(
            &state.indexdef,
            state.schema.as_deref(),
            table,
            &key.name,
            &cols,
            key.nulls_distinct,
        )
    }

    /// The `Validation` error of a key index build that found duplicates: drops the invalid index
    /// the failed build left (an error doing so is added to the message; the next run drops it
    /// again) and names up to 20 id pairs from one bounded read-only aggregate query. When the
    /// diagnostic fails or times out the conflict is still the error, with no pair listed, and the
    /// diagnostic's error is logged.
    async fn key_conflict(
        &self,
        conn: &StepConnection,
        table: &str,
        key: &KeyBuild,
        timeouts: StepTimeouts,
        error: tokio_postgres::Error,
    ) -> Error {
        let cleanup = conn
            .batch(&format!(
                "DROP INDEX CONCURRENTLY IF EXISTS {}",
                erp_sql::quote(&key.name)
            ))
            .await;
        let key_match = if key.nulls_distinct {
            conflict::KeyMatch::Equal
        } else {
            conflict::KeyMatch::NullsAgree
        };
        let cols: Vec<&str> = key.columns.iter().map(String::as_str).collect();
        let query = conflict::key_conflict_query(table, &cols, key_match);
        let mut message = match conn
            .conflict_pairs(&query, &timeouts.diagnostic_setting())
            .await
        {
            Ok(pairs) => conflict::conflict_message(
                conflict::ConflictObject::Key,
                &key.declared,
                key.model,
                &pairs,
            ),
            Err(e) => {
                tracing::warn!(
                    table,
                    key = key.declared.as_str(),
                    error = %e,
                    "the conflict diagnostic query failed; the conflict is reported without pairs"
                );
                conflict::conflict_message_unlisted(
                    conflict::ConflictObject::Key,
                    &key.declared,
                    key.model,
                )
            }
        };
        if let Err(e) = cleanup {
            message.push_str(&format!(
                "; dropping the invalid index '{}' failed too ({e}), the next run drops it",
                key.name
            ));
        }
        Error::validation(message)
            .with_sqlstate(Some("23505".to_owned()))
            .with_source(self.map_db_error(error))
    }

    /// Step 2 of a business key (design §11.1): adopt the index as the key's constraint.
    async fn add_key_constraint(
        &self,
        conn: &StepConnection,
        table: &str,
        key: &KeyBuild,
        timeouts: StepTimeouts,
    ) -> Result<()> {
        let db = |e| self.map_db_error(e);
        let KeyBuild {
            model,
            declared,
            name,
            definition,
            ..
        } = key;
        match conn.constraint_comment(table, name).await.map_err(db)? {
            // an earlier run (or another route) added it
            Some(Some(comment)) if comment == *definition => return Ok(()),
            Some(_) => {
                return Err(Error::programming(format!(
                    "cannot add key '{declared}' to '{model}': table '{table}' already has a \
                     constraint named '{name}' that was not made for this key; it is not \
                     replaced"
                )));
            }
            None => {}
        }
        let ready = match conn.key_index_state(name).await.map_err(db)? {
            Some(state) => {
                state.valid
                    && self.key_index_shaped(&state, table, key)
                    && state.comment.as_deref() == Some(definition.as_str())
            }
            None => false,
        };
        if !ready {
            return Err(Error::new(
                ErrorKind::Database,
                format!(
                    "business key '{declared}' of '{model}' cannot be completed yet: its unique \
                     index '{name}' is missing or not valid, or its build (step '{}') may still \
                     be finishing; run or wait for that step first",
                    key.index_step_key()
                ),
            ));
        }
        // ACCESS EXCLUSIVE, briefly: every reader of the table queues behind a waiting ADD
        conn.set_lock_timeout(&timeouts.short_setting())
            .await
            .map_err(db)?;
        let sql = ddl::add_constraint_using_index(table, name, name, definition).join(";\n");
        conn.batch(&sql).await.map_err(db)?;
        Ok(())
    }

    /// Compute `fields` of `model` for the records of `table` holding a NULL in one of them,
    /// in batches of [`SchemaSyncOptions::backfill_batch`] records, one transaction per batch,
    /// by id after `start`; then a closing pass from the start of the table over the records
    /// still NULL; then replace the pending marks of the columns' comments by the fields' labels
    /// ([`Orm::unmark_backfill`]). A required field that still has NULLs after the closing pass
    /// fails the step with the marks kept.
    async fn backfill(
        &self,
        step: &SchemaStep,
        options: &SchemaSyncOptions,
        start: i64,
        progress: &mut impl StepProgress,
    ) -> Result<SchemaStepReport> {
        let StepAction::Backfill { model, fields } = &step.action else {
            return Err(Error::programming(format!(
                "schema step '{}' is not a backfill",
                step.key
            )));
        };
        let (table, model) = (step.table.as_str(), *model);
        let timeouts = options.lock_timeouts();
        let registry = std::sync::Arc::clone(self.registry());
        let spec = registry.model(model).ok_or_else(|| {
            Error::programming(format!(
                "backfill of model '{model}', which is not in this registry"
            ))
        })?;
        // A previously planned step may now run under an eventful registry.
        // Check before the first batch or any comment/data mutation.
        let mut admission = self.begin(SUPERUSER_ID).await?;
        admission
            .check_backfill_admission(spec, table, fields)
            .await?;
        admission.rollback().await?;
        drop(admission);
        // the labels that replace the pending marks of the comments once every batch has run
        let labels = fields
            .iter()
            .map(|name| {
                spec.field(name)
                    .map(|f| (*name, f.string.clone()))
                    .ok_or_else(|| {
                        Error::programming(format!(
                            "backfill of unknown field '{}.{name}'",
                            spec.name
                        ))
                    })
            })
            .collect::<Result<Vec<_>>>()?;
        let limit = i64::from(options.backfill_records().get());
        let short = timeouts.short_setting();
        self.backfill_pass(spec, table, fields, limit, &short, start, progress)
            .await?;
        // ids are assigned at insert, not at commit, and a row can be set back to NULL: a NULL
        // can sit behind the cursor
        self.backfill_pass(spec, table, fields, limit, &short, 0, &mut NoProgress)
            .await?;
        let required: Vec<&str> = fields
            .iter()
            .copied()
            .filter(|name| spec.field(name).is_some_and(|f| f.required))
            .collect();
        if !required.is_empty() {
            let mut tx = self.begin(SUPERUSER_ID).await?;
            let left = async {
                tx.query(
                    "SELECT set_config('statement_timeout', $1, true)",
                    &[&SCHEMA_STEP_STATEMENT_TIMEOUT],
                )
                .await?;
                tx.note_limits_changed(None);
                select_null_ids(&mut tx, table, &required, Some((0, 1))).await
            }
            .await;
            match left {
                Ok(ids) => match ids.first() {
                    None => tx.rollback().await?,
                    Some(first) => {
                        let error = Error::new(
                            ErrorKind::Validation,
                            format!(
                                "required stored computed field(s) {} of {table} still have \
                                 NULL rows after the closing pass over the whole table (first: \
                                 id {}); the pending marks of their column comments are kept",
                                required.join(", "),
                                first.get()
                            ),
                        )
                        .with_sqlstate(Some(NOT_NULL_VIOLATION.to_owned()));
                        return Err(tx.discard_after(error).await);
                    }
                },
                Err(error) => return Err(tx.discard_after(error).await),
            }
        }
        self.unmark_backfill(table, &labels, timeouts).await
    }

    /// One pass of batches over the records of `table` holding a NULL in `fields`, after id
    /// `after`; `progress` is told the last id of each full batch.
    #[allow(
        clippy::too_many_arguments,
        reason = "one pass over a table, called twice"
    )]
    async fn backfill_pass(
        &self,
        spec: &erp_registry::ModelSpec,
        table: &str,
        fields: &[&'static str],
        limit: i64,
        short: &str,
        mut after: i64,
        progress: &mut impl StepProgress,
    ) -> Result<()> {
        loop {
            let mut tx = self.begin(SUPERUSER_ID).await?;
            let result = async {
                // while its flush waits for a row lock, a batch holds the rows it already wrote:
                // the short timeout bounds how long their writers wait for it
                // and the scan of the batch runs under the step's statement timeout, not the pool's
                tx.query(
                    "SELECT set_config('lock_timeout', $1, true), \
                            set_config('statement_timeout', $2, true)",
                    &[&short, &SCHEMA_STEP_STATEMENT_TIMEOUT],
                )
                .await?;
                tx.note_limits_changed(None);
                let ids = select_null_ids(&mut tx, table, fields, Some((after, limit))).await?;
                mark_to_compute(&mut tx, spec, fields, &ids)?;
                tx.commit().await?;
                let full = i64::try_from(ids.len()).is_ok_and(|n| n >= limit);
                Ok::<_, Error>(ids.last().map(|id| id.get()).filter(|_| full))
            }
            .await;
            match result {
                Ok(Some(last)) => {
                    after = last;
                    // the batch is committed: a failed store ends the step, the work stays done
                    progress.committed(StepCursor::new(last)?).await?;
                }
                Ok(None) => return Ok(()),
                Err(error) => {
                    // a refused commit or a failed statement leaves the transaction open
                    return Err(tx.discard_after(error).await);
                }
            }
        }
    }

    /// Replace the pending backfill mark of the comment of each column in `labels` by its label,
    /// in one short transaction on a step connection. The table's `SHARE UPDATE EXCLUSIVE` lock
    /// (the lock `COMMENT` takes; reads and writes go on, the long timeout bounds the wait) is
    /// held before the comments are read under `READ COMMITTED`, so no comment changes between
    /// the read and its replacement. A comment equal to the label was replaced already (by an
    /// inline synchronization or an earlier run of the step); any other comment without the mark
    /// was changed outside the ORM while the backfill ran: it is kept and reported.
    async fn unmark_backfill(
        &self,
        table: &str,
        labels: &[(&'static str, String)],
        timeouts: StepTimeouts,
    ) -> Result<SchemaStepReport> {
        let conn = self.step_connection(timeouts).await?;
        let db = |e| self.map_db_error(e);
        conn.batch(&format!(
            "BEGIN ISOLATION LEVEL READ COMMITTED; LOCK TABLE {} IN SHARE UPDATE EXCLUSIVE MODE",
            erp_sql::quote(table)
        ))
        .await
        .map_err(db)?;
        let names: Vec<&str> = labels.iter().map(|(name, _)| *name).collect();
        let mut comments = conn.column_comments(table, &names).await.map_err(db)?;
        let mut report = SchemaStepReport::default();
        let mut replace = Vec::new();
        for (name, label) in labels {
            let comment = comments.remove(*name).ok_or_else(|| {
                Error::new(
                    ErrorKind::Database,
                    format!("column {table}.{name} of the backfill does not exist"),
                )
            })?;
            match comment {
                Some(c) if c.starts_with(ddl::PENDING_BACKFILL_COMMENT_PREFIX) => {
                    replace.push(ddl::comment_on_column(table, name, label));
                }
                Some(c) if c == *label => {}
                comment => report.kept_comments.push(KeptColumnComment {
                    table: table.to_owned(),
                    column: (*name).to_owned(),
                    comment,
                }),
            }
        }
        if !replace.is_empty() {
            conn.batch(&replace.join(";\n")).await.map_err(db)?;
        }
        conn.batch("COMMIT").await.map_err(db)?;
        Ok(report)
    }
}

/// A failed fill: a database error, or a result the fill could not read.
enum FillError {
    Db(tokio_postgres::Error),
    Other(Error),
}

impl StepConnection {
    async fn batch(&self, sql: &str) -> std::result::Result<(), tokio_postgres::Error> {
        self.client.batch_execute(sql).await
    }

    /// Set the session's `lock_timeout` (a [`StepTimeouts`] setting) for the statements that
    /// follow.
    async fn set_lock_timeout(
        &self,
        setting: &str,
    ) -> std::result::Result<(), tokio_postgres::Error> {
        self.batch(&format!("SET lock_timeout = {}", erp_sql::literal(setting)))
            .await
    }

    /// Run a [`ddl::fill_nulls_batch`] statement batch after batch, each after the last id of
    /// the one before, until a batch is not full.
    async fn fill_nulls(
        &self,
        sql: &str,
        batch: NonZeroU32,
        start: i64,
        progress: &mut impl StepProgress,
    ) -> std::result::Result<(), FillError> {
        let limit = i64::from(batch.get());
        let mut after = start;
        loop {
            let row = self
                .client
                .query_one(sql, &[&after, &limit])
                .await
                .map_err(FillError::Db)?;
            let rows: i64 = row.try_get(0).map_err(FillError::Db)?;
            let last: Option<i64> = row.try_get(1).map_err(FillError::Db)?;
            match last {
                Some(last) if rows >= limit => {
                    if last <= after {
                        return Err(FillError::Other(Error::new(
                            ErrorKind::Database,
                            format!("fill cursor did not advance past id {after}"),
                        )));
                    }
                    after = last;
                    // the batch is committed (autocommit): a failed store ends the step
                    let cursor = StepCursor::new(last).map_err(FillError::Other)?;
                    progress.committed(cursor).await.map_err(FillError::Other)?;
                }
                // a batch that is not full (or empty) was the last one
                _ => return Ok(()),
            }
        }
    }

    /// Whether a row of `table` holds NULL in `column`.
    async fn has_null(
        &self,
        table: &str,
        column: &str,
    ) -> std::result::Result<bool, tokio_postgres::Error> {
        let sql = format!(
            "SELECT EXISTS (SELECT 1 FROM {} WHERE {} IS NULL)",
            erp_sql::quote(table),
            erp_sql::quote(column)
        );
        self.client.query_one(&sql, &[]).await?.try_get(0)
    }

    /// Take the session advisory lock of the table's partition upkeep, trying without waiting
    /// and retrying with a growing pause until `wait` has passed; whether it was taken. No
    /// statement is in flight between the tries, so no snapshot of this session is held while
    /// the lock is out of reach: an upkeep call holding the lock may be finishing a detach, which
    /// waits for older snapshots, and a blocking `pg_advisory_lock` would be one.
    async fn partition_lock(
        &self,
        table: &str,
        wait: Duration,
    ) -> std::result::Result<bool, tokio_postgres::Error> {
        let deadline = tokio::time::Instant::now() + wait;
        let mut pause = Duration::from_millis(2);
        loop {
            let taken: bool = self
                .client
                .query_one(erp_sql::partition::SESSION_TRY_LOCK, &[&table])
                .await?
                .try_get(0)?;
            let now = tokio::time::Instant::now();
            if taken {
                return Ok(true);
            }
            if now >= deadline {
                return Ok(false);
            }
            tokio::time::sleep(pause.min(deadline - now)).await;
            pause = (pause * 2).min(Duration::from_millis(100));
        }
    }

    /// Release [`Self::partition_lock`]; whether it was held.
    async fn partition_unlock(
        &self,
        table: &str,
    ) -> std::result::Result<bool, tokio_postgres::Error> {
        self.client
            .query_one(erp_sql::partition::SESSION_UNLOCK, &[&table])
            .await?
            .try_get(0)
    }

    /// Try the per-step lock of the index `name` ([`erp_sql::partition::STEP_LOCK_SPACE`]);
    /// whether it was taken.
    async fn step_lock(&self, name: &str) -> std::result::Result<bool, tokio_postgres::Error> {
        self.client
            .query_one(erp_sql::partition::STEP_TRY_LOCK, &[&name])
            .await?
            .try_get(0)
    }

    /// Release [`Self::step_lock`]; whether it was held.
    async fn step_unlock(&self, name: &str) -> std::result::Result<bool, tokio_postgres::Error> {
        self.client
            .query_one(erp_sql::partition::STEP_UNLOCK, &[&name])
            .await?
            .try_get(0)
    }

    /// The comment of constraint `name` of `table` (current schema); `None` when the table has no
    /// such constraint.
    async fn constraint_comment(
        &self,
        table: &str,
        name: &str,
    ) -> std::result::Result<Option<Option<String>>, tokio_postgres::Error> {
        let row = self
            .client
            .query_opt(
                "SELECT obj_description(k.oid, 'pg_constraint') FROM pg_constraint k \
                   JOIN pg_class c ON c.oid = k.conrelid \
                   JOIN pg_namespace n ON n.oid = c.relnamespace \
                  WHERE n.nspname = current_schema() AND c.relname = $1 AND k.conname = $2",
                &[&table, &name],
            )
            .await?;
        row.map(|r| r.try_get(0)).transpose()
    }

    /// The id pairs a [`conflict::conflict_pairs_query`] returns, read in a `READ ONLY`
    /// transaction under `statement_timeout = timeout` (a [`StepTimeouts::diagnostic_setting`]);
    /// the transaction always ends, with a rollback on failure.
    async fn conflict_pairs(
        &self,
        query: &str,
        timeout: &str,
    ) -> std::result::Result<Vec<(i64, i64)>, tokio_postgres::Error> {
        self.client.batch_execute("BEGIN READ ONLY").await?;
        let read = async {
            self.client
                .query_one(
                    "SELECT set_config('statement_timeout', $1, true)",
                    &[&timeout],
                )
                .await?;
            let rows = self.client.query(query, &[]).await?;
            crate::exclusion::read_conflict_pairs(&rows)
        }
        .await;
        let end = self.client.batch_execute("ROLLBACK").await;
        let pairs = read?;
        end?;
        Ok(pairs)
    }

    /// What the catalog says of index `name` (current schema): validity, comment, definition and
    /// the current schema; `None` when it does not exist.
    async fn key_index_state(
        &self,
        name: &str,
    ) -> std::result::Result<Option<KeyIndexState>, tokio_postgres::Error> {
        let row = self
            .client
            .query_opt(
                "SELECT i.indisvalid, obj_description(i.indexrelid, 'pg_class'), \
                        pg_get_indexdef(i.indexrelid), current_schema()::text \
                   FROM pg_index i \
                   JOIN pg_class c ON c.oid = i.indexrelid \
                   JOIN pg_namespace n ON n.oid = c.relnamespace \
                  WHERE n.nspname = current_schema() AND c.relname = $1",
                &[&name],
            )
            .await?;
        row.map(|r| {
            Ok(KeyIndexState {
                valid: r.try_get(0)?,
                comment: r.try_get(1)?,
                indexdef: r.try_get(2)?,
                schema: r.try_get(3)?,
            })
        })
        .transpose()
    }

    /// `(valid, comment)` of index `name` (current schema); `None` when it does not exist.
    async fn index_state(
        &self,
        name: &str,
    ) -> std::result::Result<Option<(bool, Option<String>)>, tokio_postgres::Error> {
        let row = self
            .client
            .query_opt(
                "SELECT i.indisvalid, obj_description(i.indexrelid, 'pg_class') FROM pg_index i \
                   JOIN pg_class c ON c.oid = i.indexrelid \
                   JOIN pg_namespace n ON n.oid = c.relnamespace \
                  WHERE n.nspname = current_schema() AND c.relname = $1",
                &[&name],
            )
            .await?;
        row.map(|r| Ok((r.try_get(0)?, r.try_get(1)?))).transpose()
    }

    /// The partitions of `table` with no index attached to its index `index` yet, by name.
    async fn partitions_without_index(
        &self,
        table: &str,
        index: &str,
    ) -> std::result::Result<Vec<String>, tokio_postgres::Error> {
        let rows = self
            .client
            .query(ddl::PARTITIONS_WITHOUT_INDEX_QUERY, &[&table, &index])
            .await?;
        rows.iter().map(|r| r.try_get(0)).collect()
    }

    /// Every partition of `table` (current schema), pending detaches included, by name.
    async fn partitions_of(
        &self,
        table: &str,
    ) -> std::result::Result<Vec<String>, tokio_postgres::Error> {
        let rows = self.client.query(ddl::PARTITIONS_QUERY, &[&table]).await?;
        rows.iter().map(|r| r.try_get(0)).collect()
    }

    /// Those of the indexes `names` (current schema; `names[i]` an index of `partitions[i]`) that
    /// exist on that partition and are not attached to a parent index.
    async fn unattached_indexes(
        &self,
        partitions: &[String],
        names: &[String],
    ) -> std::result::Result<Vec<String>, tokio_postgres::Error> {
        let rows = self
            .client
            .query(ddl::UNATTACHED_INDEXES_QUERY, &[&partitions, &names])
            .await?;
        rows.iter().map(|r| r.try_get(0)).collect()
    }

    /// Whether index `name` (current schema) is valid; `None` when it does not exist.
    async fn index_valid(
        &self,
        name: &str,
    ) -> std::result::Result<Option<bool>, tokio_postgres::Error> {
        let row = self
            .client
            .query_opt(
                "SELECT i.indisvalid FROM pg_index i \
                   JOIN pg_class c ON c.oid = i.indexrelid \
                   JOIN pg_namespace n ON n.oid = c.relnamespace \
                  WHERE n.nspname = current_schema() AND c.relname = $1",
                &[&name],
            )
            .await?;
        row.map(|r| r.try_get(0)).transpose()
    }

    /// The comment of index `name` (current schema), if it has one.
    async fn index_comment(
        &self,
        name: &str,
    ) -> std::result::Result<Option<String>, tokio_postgres::Error> {
        let row = self
            .client
            .query_opt(
                "SELECT obj_description(c.oid, 'pg_class') FROM pg_class c \
                   JOIN pg_namespace n ON n.oid = c.relnamespace \
                  WHERE n.nspname = current_schema() AND c.relname = $1",
                &[&name],
            )
            .await?;
        row.map(|r| r.try_get(0)).transpose().map(Option::flatten)
    }

    /// The not-null constraint of `table.column` (current schema), if any.
    async fn not_null_constraint(
        &self,
        table: &str,
        column: &str,
    ) -> std::result::Result<Option<String>, tokio_postgres::Error> {
        let row = self
            .client
            .query_opt(
                "SELECT k.conname::text FROM pg_constraint k \
                   JOIN pg_class c ON c.oid = k.conrelid \
                   JOIN pg_namespace n ON n.oid = c.relnamespace \
                   JOIN pg_attribute a ON a.attrelid = k.conrelid AND a.attnum = k.conkey[1] \
                  WHERE n.nspname = current_schema() AND c.relname = $1 AND a.attname = $2 \
                    AND k.contype = 'n'",
                &[&table, &column],
            )
            .await?;
        row.map(|r| r.try_get(0)).transpose()
    }

    /// The comments of `columns` of `table` (current schema) by column name, `None` for a column
    /// without one; a column that does not exist is absent.
    async fn column_comments(
        &self,
        table: &str,
        columns: &[&str],
    ) -> std::result::Result<FxHashMap<String, Option<String>>, tokio_postgres::Error> {
        let rows = self
            .client
            .query(
                "SELECT a.attname::text, col_description(a.attrelid, a.attnum) \
                   FROM pg_attribute a \
                   JOIN pg_class c ON c.oid = a.attrelid \
                   JOIN pg_namespace n ON n.oid = c.relnamespace \
                  WHERE n.nspname = current_schema() AND c.relname = $1 \
                    AND a.attname = ANY($2) AND a.attnum > 0 AND NOT a.attisdropped",
                &[&table, &columns],
            )
            .await?;
        let mut comments = FxHashMap::default();
        for row in &rows {
            let name: String = row.try_get(0)?;
            let comment: Option<String> = row.try_get(1)?;
            comments.insert(name, comment);
        }
        Ok(comments)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The defaults are inside the range `step_lock_timeouts` accepts, and the short one is the
    /// schema transaction's own lock timeout.
    #[test]
    fn default_lock_timeouts_are_accepted_values() {
        let defaults = SchemaSyncOptions::new();
        let explicit = SchemaSyncOptions::new()
            .step_lock_timeouts(
                SchemaSyncOptions::DEFAULT_SHORT_LOCK_TIMEOUT,
                SchemaSyncOptions::DEFAULT_LONG_LOCK_TIMEOUT,
            )
            .unwrap();
        assert_eq!(explicit, defaults);
        assert_eq!(defaults.lock_timeouts().short_setting(), "10000ms");
        assert_eq!(defaults.lock_timeouts().long_setting(), "3600000ms");
        assert_eq!(
            format!(
                "{}s",
                SchemaSyncOptions::DEFAULT_SHORT_LOCK_TIMEOUT.as_secs()
            ),
            crate::SCHEMA_LOCK_TIMEOUT
        );
    }

    /// The accepted range is PostgreSQL's (`1..=i32::MAX` ms); sub-millisecond parts are
    /// dropped when the setting is written.
    #[test]
    fn step_lock_timeouts_bounds() {
        let max = Duration::from_millis(u64::from(i32::MAX.unsigned_abs()));
        let edges = SchemaSyncOptions::new()
            .step_lock_timeouts(Duration::from_millis(1), max)
            .unwrap();
        assert_eq!(edges.lock_timeouts().short_setting(), "1ms");
        assert_eq!(edges.lock_timeouts().long_setting(), "2147483647ms");
        let fraction = SchemaSyncOptions::new()
            .step_lock_timeouts(Duration::from_micros(1_500), Duration::from_secs(1))
            .unwrap();
        assert_eq!(fraction.lock_timeouts().short_setting(), "1ms");
        for (short, long, message) in [
            (
                Duration::from_micros(999),
                Duration::from_secs(1),
                "schema step lock timeouts must be at least 1 ms and at most 2147483647 ms, got \
                 0 ms (short) and 1000 ms (long)",
            ),
            (
                Duration::from_secs(1),
                max + Duration::from_millis(1),
                "schema step lock timeouts must be at least 1 ms and at most 2147483647 ms, got \
                 1000 ms (short) and 2147483648 ms (long)",
            ),
        ] {
            let err = SchemaSyncOptions::new()
                .step_lock_timeouts(short, long)
                .unwrap_err();
            assert_eq!(err.kind(), ErrorKind::Programming);
            assert_eq!(err.message(), message);
        }
    }

    fn key_build() -> KeyBuild {
        KeyBuild {
            model: "sale.order",
            declared: "_ref_key".to_owned(),
            name: "sale_order_ref_key".to_owned(),
            columns: vec!["ref".to_owned()],
            nulls_distinct: false,
            definition: "UNIQUE NULLS NOT DISTINCT (ref)".to_owned(),
        }
    }

    /// The keys of the two steps of a business key are step keys the post-commit
    /// queue can hold: each parses to the kind of its step and names the key's constraint.
    #[test]
    fn the_step_keys_of_a_business_key_parse_to_their_kinds() {
        let build = key_build();
        let steps = [
            SchemaStep {
                key: build.index_step_key(),
                table: "sale_order".to_owned(),
                action: StepAction::KeyIndex(build.clone()),
            },
            SchemaStep {
                key: build.constraint_step_key(),
                table: "sale_order".to_owned(),
                action: StepAction::KeyConstraint(build.clone()),
            },
        ];
        for step in &steps {
            let key = SchemaStepKey::parse(step.key()).unwrap();
            assert_eq!(key.kind(), step.kind(), "{}", step.key());
            assert_eq!(key.object(), Some("sale_order_ref_key"));
            assert_eq!(key.table(), None);
            assert!(step.backfill_fields().is_empty());
        }
        for (bad, shape) in [
            ("key_index.", "key_index.<key constraint>"),
            ("key_index.a.b", "key_index.<key constraint>"),
            ("key_constraint.", "key_constraint.<key constraint>"),
        ] {
            let err = SchemaStepKey::parse(bad).unwrap_err();
            assert_eq!(err.kind(), ErrorKind::Programming);
            assert_eq!(
                err.message(),
                format!("schema step key {bad:?} is not a valid {shape} step key")
            );
        }
        assert_eq!(
            SchemaStepKey::parse("key.x").unwrap_err().message(),
            "schema step key \"key.x\" starts with no step kind (index., validate., not_null., \
             backfill., key_index., key_constraint.)"
        );
    }

    /// Steps of different kinds on the same object never share a fingerprint, so a cursor stored
    /// for one is never resumed by another.
    #[test]
    fn fingerprints_tell_every_kind_apart() {
        let build = key_build();
        let name = build.name.clone();
        let step = |action| SchemaStep {
            key: format!("index.{name}"),
            table: "sale_order".to_owned(),
            action,
        };
        let fingerprints: Vec<String> = [
            step(StepAction::Index {
                name: name.clone(),
                statement: String::new(),
                comment: None,
                replaces: false,
            }),
            step(StepAction::PartitionedIndex {
                name: name.clone(),
                statement: String::new(),
                comment: None,
            }),
            step(StepAction::KeyIndex(build.clone())),
            step(StepAction::KeyConstraint(build)),
        ]
        .iter()
        .map(|s| s.fingerprint().as_str().to_owned())
        .collect();
        assert_eq!(
            fingerprints,
            [
                "index:sale_order:sale_order_ref_key",
                "partitioned_index:sale_order:sale_order_ref_key",
                "key_index:sale_order:sale_order_ref_key",
                "key_constraint:sale_order:sale_order_ref_key",
            ]
        );
        for f in &fingerprints {
            assert_eq!(StepFingerprint::parse(f).unwrap().as_str(), f);
        }
    }
}
