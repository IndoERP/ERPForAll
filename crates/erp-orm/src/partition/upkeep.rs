//! The upkeep calls: [`Orm::ensure_partitions`], [`Orm::retire_partitions`],
//! [`Orm::maintain_partitions`] and the health check [`Orm::partition_health`] (design §7.5).
//!
//! Each upkeep call takes one connection out of the owner pool (it is closed afterwards, never
//! returned with session state), sets `lock_timeout` ([`crate::OrmConfig::partition_lock_timeout`])
//! and a statement timeout, and per model takes the session advisory lock of
//! `erp_sql::partition::PARTITION_LOCK_SPACE` before it reads the catalog: two processes running
//! the upkeep of one model take turns, and the second finds what the first created. Reads run
//! outside a transaction (`READ COMMITTED`), so they see everything committed before them.
//!
//! The statements that take a lock on the tables a partition's foreign keys point to (`res_users`
//! and the like: attach, detach, finalize, drop) run under the short
//! [`PARTITION_SHORT_LOCK_TIMEOUT`], then the configured timeout is restored (design §7.10).

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use deadpool_postgres::{ClientWrapper, Object, Pool};
use erp_fields::{DateTime, Error, ErrorKind, Result};
use erp_registry::{ModelSpec, Partition};
use tokio_postgres::Client;
use tokio_postgres::error::SqlState;

use super::catalog::{self, PartitionFacts};
use super::{
    Horizon, KeepAfter, PARTITION_LOCK_TIMEOUT_BOUNDS, PARTITION_SHORT_LOCK_TIMEOUT,
    PartitionAction, PartitionReport, PartitionStep, PartitionWindow, partitioned,
    partitioned_models,
};
use crate::Orm;

/// `statement_timeout` of the upkeep connection: bounds each statement (a concurrent detach waits
/// for older transactions within `lock_timeout`, then gives up and leaves the partition pending).
pub(crate) const PARTITION_STATEMENT_TIMEOUT: &str = "5min";

/// A failed statement as a report reason: the server's message and SQLSTATE, or the client error.
fn reason(error: &tokio_postgres::Error) -> String {
    match error.as_db_error() {
        Some(db) => format!("{} (SQLSTATE {})", db.message(), db.code().code()),
        None => error.to_string(),
    }
}

/// The kind of a failed statement: `Lock` for a lock not granted within `lock_timeout`.
fn kind_of(error: &tokio_postgres::Error) -> ErrorKind {
    if error.code() == Some(&SqlState::LOCK_NOT_AVAILABLE) {
        ErrorKind::Lock
    } else {
        ErrorKind::Database
    }
}

/// The report entry of a failed statement of `step`: reason, SQLSTATE and kind kept.
fn failure(step: PartitionStep, error: &tokio_postgres::Error) -> PartitionAction {
    PartitionAction::failed(
        step,
        reason(error),
        error.code().map(|c| c.code().to_owned()),
        Some(kind_of(error)),
    )
}

/// The report entry of an upkeep error (the start of a model's upkeep): message, SQLSTATE and kind.
fn failure_of(step: PartitionStep, error: &Error) -> PartitionAction {
    PartitionAction::failed(
        step,
        error.message().to_owned(),
        error.sqlstate().map(str::to_owned),
        Some(error.kind()),
    )
}

/// A failed statement of the upkeep before any partition was touched, as an error: `Lock` for a
/// lock not granted within `lock_timeout`, `Database` otherwise.
fn upkeep_error(context: String, error: tokio_postgres::Error) -> Error {
    let sqlstate = error.code().map(|c| c.code().to_owned());
    Error::new(kind_of(&error), format!("{context}: {}", reason(&error)))
        .with_sqlstate(sqlstate)
        .with_source(error)
}

/// An upkeep connection: out of the pool for good, with the upkeep's timeouts set.
struct Upkeep {
    client: ClientWrapper,
    /// the session's `lock_timeout` ([`crate::OrmConfig::partition_lock_timeout`]), restored after
    /// a statement ran with the shorter [`PARTITION_SHORT_LOCK_TIMEOUT`]
    lock_timeout: Duration,
    /// set when the session's `lock_timeout` could not be restored: it is the short one now, so
    /// the call does no more work on this connection
    stopped: AtomicBool,
}

fn set_lock_timeout(timeout: Duration) -> String {
    format!("SET lock_timeout = '{}ms'", timeout.as_millis())
}

impl Upkeep {
    async fn open(orm: &Orm, pool: &Pool) -> Result<Self> {
        let timeout = orm.shared.config.partition_lock_timeout;
        let (floor, ceiling) = PARTITION_LOCK_TIMEOUT_BOUNDS;
        if !(floor..=ceiling).contains(&timeout) {
            return Err(Error::programming(format!(
                "OrmConfig::partition_lock_timeout is {timeout:?}; it must be from {floor:?} to \
                 {ceiling:?} (0 would wait forever)"
            )));
        }
        let object = pool.get().await.map_err(|e| {
            Error::new(
                ErrorKind::Database,
                "could not get a database connection for partition upkeep",
            )
            .with_source(e)
        })?;
        // closed when dropped: its session settings and advisory locks never go back to the pool
        let client = Object::take(object);
        client
            .batch_execute(&format!(
                "{}; SET statement_timeout = '{PARTITION_STATEMENT_TIMEOUT}'",
                set_lock_timeout(timeout)
            ))
            .await
            .map_err(|e| {
                upkeep_error("could not set the partition upkeep timeouts".to_owned(), e)
            })?;
        Ok(Self {
            client,
            lock_timeout: timeout,
            stopped: AtomicBool::new(false),
        })
    }

    fn client(&self) -> &Client {
        &self.client
    }

    /// True once the session's `lock_timeout` could not be restored: no more work is done.
    fn is_stopped(&self) -> bool {
        self.stopped.load(Ordering::Relaxed)
    }

    /// Take the partition lock of `spec`'s table (session scope, within `lock_timeout`).
    async fn acquire(&self, spec: &ModelSpec) -> Result<()> {
        let table = spec.table.as_str();
        self.client()
            .query(erp_sql::partition::SESSION_LOCK, &[&table])
            .await
            .map_err(|e| {
                upkeep_error(
                    format!(
                        "partition upkeep of '{}' could not take the upkeep lock of table \
                         '{table}' (another upkeep or a schema synchronization holds it)",
                        spec.name
                    ),
                    e,
                )
            })?;
        Ok(())
    }

    /// Read the partition catalog of `spec`'s table, which must be partitioned in the database.
    async fn read_facts(&self, spec: &ModelSpec) -> Result<PartitionFacts> {
        let table = spec.table.as_str();
        let tables = vec![table.to_owned()];
        let rows = self
            .client()
            .query(erp_sql::partition::PARTITION_CATALOG_QUERY, &[&tables])
            .await
            .map_err(|e| {
                upkeep_error(
                    format!("could not read the partitions of table '{table}'"),
                    e,
                )
            })?;
        let facts = PartitionFacts::from_rows(&rows)?;
        facts.require_partitioned(spec.name, table)?;
        Ok(facts)
    }

    /// Take the partition lock of `spec`'s table and read its partition catalog.
    async fn lock(&self, spec: &ModelSpec) -> Result<PartitionFacts> {
        self.acquire(spec).await?;
        self.read_facts(spec).await
    }

    /// Release the partition lock of `spec`'s table after its work. A release that fails, or finds
    /// the lock not held, is a `Failed { Unlock }` entry: the work is done and stays in the report
    /// (closing the connection releases the lock anyway). Only called when the lock was taken.
    async fn unlock(&self, report: &mut PartitionReport, spec: &ModelSpec) {
        let table = spec.table.as_str();
        let entry = match self
            .client()
            .query_one(erp_sql::partition::SESSION_UNLOCK, &[&table])
            .await
        {
            Ok(row) => match row.try_get::<_, bool>(0) {
                Ok(true) => return,
                Ok(false) => PartitionAction::failed_text(
                    PartitionStep::Unlock,
                    format!("the upkeep lock of table '{table}' was not held when it was released"),
                ),
                Err(e) => PartitionAction::failed(
                    PartitionStep::Unlock,
                    format!("could not read the result of releasing the upkeep lock: {e}"),
                    None,
                    Some(ErrorKind::Database),
                ),
            },
            Err(e) => failure(PartitionStep::Unlock, &e),
        };
        report.push(spec.name, table, entry);
    }

    /// Run one statement; a failure becomes the report entry `Failed { step, .. }`.
    async fn step(
        &self,
        report: &mut PartitionReport,
        spec: &ModelSpec,
        partition: &str,
        step: PartitionStep,
        sql: &str,
    ) -> bool {
        match self.client().batch_execute(sql).await {
            Ok(()) => true,
            Err(e) => {
                report.push(spec.name, partition, failure(step, &e));
                false
            }
        }
    }

    /// Run `sql` under the short [`PARTITION_SHORT_LOCK_TIMEOUT`] and restore the session's
    /// `lock_timeout` right after, whatever the outcome (design §7.10). `None`: the short timeout
    /// could not be set, nothing ran (the entry is in `report`). `Some(result)`: the statement's
    /// outcome; when the restore failed an entry says so and the call stops.
    async fn short(
        &self,
        report: &mut PartitionReport,
        spec: &ModelSpec,
        partition: &str,
        step: PartitionStep,
        sql: &str,
    ) -> Option<std::result::Result<(), tokio_postgres::Error>> {
        if !self
            .step(
                report,
                spec,
                partition,
                step,
                &set_lock_timeout(PARTITION_SHORT_LOCK_TIMEOUT),
            )
            .await
        {
            self.stopped.store(true, Ordering::Relaxed);
            return None;
        }
        let outcome = self.client().batch_execute(sql).await;
        // restored before anything else runs on this connection, the statement's outcome aside
        if let Err(e) = self
            .client()
            .batch_execute(&set_lock_timeout(self.lock_timeout))
            .await
        {
            self.stopped.store(true, Ordering::Relaxed);
            report.push(
                spec.name,
                partition,
                PartitionAction::failed(
                    step,
                    format!(
                        "could not restore the upkeep lock_timeout, the call stops here: {}",
                        reason(&e)
                    ),
                    e.code().map(|c| c.code().to_owned()),
                    Some(kind_of(&e)),
                ),
            );
        }
        Some(outcome)
    }

    /// Drop the detached table `name` under the short lock timeout (design §7.10): a lock wait cut
    /// by it is the entry `DropDeferred` (the table stays detached and marked; a later retirement
    /// drops it), any other failure the entry `Failed`.
    async fn drop_detached(&self, report: &mut PartitionReport, spec: &ModelSpec, name: &str) {
        let sql = erp_sql::partition::drop_table(name);
        match self
            .short(report, spec, name, PartitionStep::Drop, &sql)
            .await
        {
            None => {}
            Some(Ok(())) => report.push(spec.name, name, PartitionAction::Dropped),
            Some(Err(e)) if e.code() == Some(&SqlState::LOCK_NOT_AVAILABLE) => {
                let why = reason(&e);
                tracing::warn!(
                    target: "erp::orm",
                    model = spec.name,
                    partition = name,
                    reason = %why,
                    "partition drop deferred: the retired table stays detached until the next upkeep"
                );
                report.push(spec.name, name, PartitionAction::drop_deferred(why));
            }
            Some(Err(e)) => report.push(spec.name, name, failure(PartitionStep::Drop, &e)),
        }
    }

    /// Finish every detach an interruption left pending (under the short lock timeout: the
    /// finalize needs locks on the tables the partition's foreign keys point to; a cut wait leaves
    /// the partition pending for the next call).
    async fn finalize(
        &self,
        report: &mut PartitionReport,
        spec: &ModelSpec,
        facts: &PartitionFacts,
    ) {
        for row in facts.rows(&spec.table).iter().filter(|r| r.pending) {
            if self.is_stopped() {
                return;
            }
            let sql = erp_sql::partition::detach_finalize(&spec.table, &row.name);
            match self
                .short(report, spec, &row.name, PartitionStep::Finalize, &sql)
                .await
            {
                None => {}
                Some(Ok(())) => report.push(spec.name, &row.name, PartitionAction::Finalized),
                Some(Err(e)) => {
                    report.push(spec.name, &row.name, failure(PartitionStep::Finalize, &e));
                }
            }
        }
    }

    /// Create every missing partition of the window through `through` into `report`. `facts`
    /// were read before [`Upkeep::finalize`], which the caller ran first: a partition they show
    /// pending is a standalone table now when its finalize succeeded.
    async fn create_window(
        &self,
        report: &mut PartitionReport,
        spec: &ModelSpec,
        slots: Vec<catalog::Slot>,
        facts: &PartitionFacts,
    ) {
        let rows = facts.rows(&spec.table);
        for slot in slots {
            if self.is_stopped() {
                return;
            }
            match rows.iter().find(|r| r.name == slot.name) {
                // a standalone table now, which cannot be attached by name
                Some(row) if row.pending => {
                    let finalized = report.finalized().contains(&slot.name.as_str());
                    let why = if finalized {
                        "the partition was being detached (finalized now); a standalone table of \
                         that name is left and must be removed or attached by hand"
                    } else {
                        "the partition is being detached and its finalize did not complete (see \
                         its entry); once it has, a standalone table of that name is left and \
                         must be removed or attached by hand"
                    };
                    report.push(
                        spec.name,
                        &slot.name,
                        PartitionAction::failed_text(PartitionStep::Create, why.to_owned()),
                    );
                }
                Some(row) if row.from == Some(slot.from) && row.to == Some(slot.to) => {
                    report.push(spec.name, &slot.name, PartitionAction::Existed);
                }
                Some(row) => report.push(
                    spec.name,
                    &slot.name,
                    PartitionAction::failed_text(
                        PartitionStep::Create,
                        format!(
                            "a partition of that name exists with the bounds {} (expected {} to \
                             {})",
                            row.bound, slot.from, slot.to
                        ),
                    ),
                ),
                None => {
                    // one simple query: an implicit transaction, so a failed attach leaves no
                    // table; the attach locks the tables the foreign keys point to, so it runs
                    // under the short lock timeout
                    let sql = format!(
                        "{}; {}",
                        erp_sql::partition::create_partition(&spec.table, &slot.name),
                        erp_sql::partition::attach_partition(
                            &spec.table,
                            &slot.name,
                            slot.from,
                            slot.to
                        )
                    );
                    match self
                        .short(report, spec, &slot.name, PartitionStep::Create, &sql)
                        .await
                    {
                        None => {}
                        Some(Ok(())) => {
                            report.push(spec.name, &slot.name, PartitionAction::Created)
                        }
                        Some(Err(e)) => {
                            report.push(spec.name, &slot.name, failure(PartitionStep::Create, &e));
                        }
                    }
                }
            }
        }
    }

    /// After a failed detach: when the partition is still attached and not pending, the retired
    /// marker set before the detach is removed, so a later manual detach is never taken for an
    /// interrupted retirement and dropped. A partition pending detach keeps it (the next call
    /// finalizes it and drops the leftover).
    async fn unmark_if_attached(&self, report: &mut PartitionReport, spec: &ModelSpec, name: &str) {
        let state = self
            .client()
            .query_opt(erp_sql::partition::DETACH_STATE_QUERY, &[&name])
            .await;
        let attached = match state {
            Ok(Some(row)) => row.try_get::<_, bool>(0).map(|pending| !pending),
            Ok(None) => return,
            Err(e) => {
                report.push(
                    spec.name,
                    name,
                    PartitionAction::failed(
                        PartitionStep::Detach,
                        format!("could not tell whether the failed detach left the partition attached: {}", reason(&e)),
                        e.code().map(|c| c.code().to_owned()),
                        Some(kind_of(&e)),
                    ),
                );
                return;
            }
        };
        match attached {
            Ok(true) => {
                if let Err(e) = self
                    .client()
                    .batch_execute(&erp_sql::partition::unmark_retired(name))
                    .await
                {
                    report.push(
                        spec.name,
                        name,
                        PartitionAction::failed(
                            PartitionStep::Detach,
                            format!(
                                "could not remove the retired marker of the attached partition: {}",
                                reason(&e)
                            ),
                            e.code().map(|c| c.code().to_owned()),
                            Some(kind_of(&e)),
                        ),
                    );
                }
            }
            Ok(false) => {}
            Err(e) => report.push(
                spec.name,
                name,
                PartitionAction::failed(
                    PartitionStep::Detach,
                    format!("could not read the detach state of the partition: {e}"),
                    None,
                    Some(ErrorKind::Database),
                ),
            ),
        }
    }

    /// Drop what an interrupted retirement left detached (the caller ran [`Upkeep::finalize`]
    /// first), then mark, detach (concurrently) and drop every partition that ends at or before
    /// `cutoff`, into `report`. Every failure is an entry of the report.
    async fn retire_old(
        &self,
        report: &mut PartitionReport,
        spec: &ModelSpec,
        facts: &PartitionFacts,
        cutoff: DateTime,
    ) {
        let table = spec.table.as_str();
        let marker = erp_sql::partition::retired_marker(table);
        let prefix = format!("{table}_p");
        // a crash between the marker and the detach left live partitions marked: unmark them
        let parent = erp_sql::quote(table);
        match self
            .client()
            .query(
                erp_sql::partition::ATTACHED_RETIRED_QUERY,
                &[&marker, &prefix, &parent],
            )
            .await
        {
            Ok(marked) => {
                for row in &marked {
                    let name = match row.try_get::<_, String>(0) {
                        Ok(name) => name,
                        Err(e) => {
                            report.push(
                                spec.name,
                                table,
                                PartitionAction::failed(
                                    PartitionStep::Detach,
                                    format!(
                                        "could not read the name of an attached partition marked \
                                         as retired: {}",
                                        reason(&e)
                                    ),
                                    None,
                                    Some(kind_of(&e)),
                                ),
                            );
                            continue;
                        }
                    };
                    if let Err(e) = self
                        .client()
                        .batch_execute(&erp_sql::partition::unmark_retired(&name))
                        .await
                    {
                        report.push(
                            spec.name,
                            &name,
                            PartitionAction::failed(
                                PartitionStep::Detach,
                                format!(
                                    "could not remove the retired marker of the attached \
                                     partition: {}",
                                    reason(&e)
                                ),
                                e.code().map(|c| c.code().to_owned()),
                                Some(kind_of(&e)),
                            ),
                        );
                    }
                }
            }
            Err(e) => report.push(
                spec.name,
                table,
                PartitionAction::failed(
                    PartitionStep::Detach,
                    format!(
                        "could not list the attached partitions marked as retired: {}",
                        reason(&e)
                    ),
                    e.code().map(|c| c.code().to_owned()),
                    Some(kind_of(&e)),
                ),
            ),
        }
        // tables an interrupted retirement left detached (marked before their detach)
        match self
            .client()
            .query(
                erp_sql::partition::RETIRED_TABLES_QUERY,
                &[&marker, &prefix],
            )
            .await
        {
            Ok(leftovers) => {
                for row in &leftovers {
                    if self.is_stopped() {
                        return;
                    }
                    let name: String = match row.try_get(0) {
                        Ok(name) => name,
                        Err(e) => {
                            report.push(
                                spec.name,
                                table,
                                PartitionAction::failed(
                                    PartitionStep::Drop,
                                    format!("could not read a retired partition name: {e}"),
                                    None,
                                    Some(ErrorKind::Database),
                                ),
                            );
                            continue;
                        }
                    };
                    self.drop_detached(report, spec, &name).await;
                }
            }
            Err(e) => report.push(
                spec.name,
                table,
                PartitionAction::failed(
                    PartitionStep::Drop,
                    format!("could not list the retired partitions: {}", reason(&e)),
                    e.code().map(|c| c.code().to_owned()),
                    Some(kind_of(&e)),
                ),
            ),
        }
        // a partition whose bounds are not a pair of timestamps cannot be compared with the
        // cutoff: listed as failed on every call until someone detaches or fixes it by hand
        for row in facts
            .rows(table)
            .iter()
            .filter(|r| !r.pending && r.to.is_none())
        {
            report.push(
                spec.name,
                &row.name,
                PartitionAction::failed_text(
                    PartitionStep::Detach,
                    format!(
                        "the partition's bounds ({}) are not a range of timestamps, so the upkeep \
                         cannot tell whether it is old enough to retire; detach it by hand",
                        row.bound
                    ),
                ),
            );
        }
        let mut old: Vec<&catalog::PartitionRow> = facts
            .rows(table)
            .iter()
            .filter(|r| !r.pending && r.to.is_some_and(|to| to <= cutoff))
            .collect();
        old.sort_by_key(|r| r.from);
        for row in old {
            if self.is_stopped() {
                return;
            }
            let name = row.name.as_str();
            let mark = erp_sql::partition::mark_retired(table, name);
            if !self
                .step(report, spec, name, PartitionStep::Detach, &mark)
                .await
            {
                continue;
            }
            // under the short lock timeout: the detach locks the tables the foreign keys point
            // to; a cut wait leaves the partition attached or pending (design §7.10)
            let detach = erp_sql::partition::detach_concurrently(table, name);
            match self
                .short(report, spec, name, PartitionStep::Detach, &detach)
                .await
            {
                None => continue,
                Some(Ok(())) => {}
                Some(Err(e)) => {
                    report.push(spec.name, name, failure(PartitionStep::Detach, &e));
                    if !self.is_stopped() {
                        self.unmark_if_attached(report, spec, name).await;
                    }
                    continue;
                }
            }
            report.push(spec.name, name, PartitionAction::Detached);
            self.drop_detached(report, spec, name).await;
        }
    }

    /// One model of [`Orm::maintain_partitions`]: everything that can fail before an action is
    /// checked first, so an error means nothing was done for the model. The model's lock is held.
    async fn maintain_locked(
        &self,
        spec: &ModelSpec,
        partition: &Partition,
        now: DateTime,
    ) -> Result<PartitionReport> {
        let facts = self.read_facts(spec).await?;
        // One clock: the caller's `now` places the window and the retention cutoff alike. The
        // database's clock only guards (the cutoff may not lie after it, so a clock running
        // ahead cannot retire live partitions); it computes nothing.
        let through = catalog::declared_horizon(partition, now, spec.name)?;
        let slots = catalog::window(spec.name, &spec.table, partition.interval(), now, through)?;
        let cutoff = match catalog::declared_cutoff(partition, now, spec.name)? {
            Some(declared) => Some(cutoff(
                spec,
                partition,
                KeepAfter::Time(declared),
                facts.now()?,
            )?),
            None => None,
        };
        let mut report = PartitionReport::default();
        self.finalize(&mut report, spec, &facts).await;
        self.create_window(&mut report, spec, slots, &facts).await;
        if let Some(cutoff) = cutoff {
            self.retire_old(&mut report, spec, &facts, cutoff).await;
        }
        Ok(report)
    }

    /// One model of [`Orm::maintain_partitions`] into `report`: its lock, its work, its unlock. A
    /// model that cannot start is a `Failed { Prepare }` entry; the lock is released only when it
    /// was taken.
    async fn maintain(
        &self,
        report: &mut PartitionReport,
        spec: &ModelSpec,
        partition: &Partition,
        now: DateTime,
    ) {
        if let Err(error) = self.acquire(spec).await {
            report.push(
                spec.name,
                &spec.table,
                failure_of(PartitionStep::Prepare, &error),
            );
            return;
        }
        match self.maintain_locked(spec, partition, now).await {
            Ok(done) => report.merge(done),
            Err(error) => report.push(
                spec.name,
                &spec.table,
                failure_of(PartitionStep::Prepare, &error),
            ),
        }
        // a connection that could not restore its lock_timeout is not used any more; closing it
        // releases the lock
        if !self.is_stopped() {
            self.unlock(report, spec).await;
        }
    }
}

/// The cutoff of `keep_after`, checked to lie in the past.
fn cutoff(
    spec: &ModelSpec,
    partition: &Partition,
    keep_after: KeepAfter,
    now: DateTime,
) -> Result<DateTime> {
    let cutoff = match keep_after {
        KeepAfter::Declared => {
            catalog::declared_cutoff(partition, now, spec.name)?.ok_or_else(|| {
                Error::programming(format!(
                    "retire_partitions on '{}': the model keeps every partition (retain = \"all\")",
                    spec.name
                ))
            })?
        }
        KeepAfter::Time(t) => {
            if partition.retain().intervals().is_none() {
                return Err(Error::programming(format!(
                    "retire_partitions on '{}': the model keeps every partition (retain = \"all\")",
                    spec.name
                )));
            }
            t
        }
    };
    if cutoff > now {
        return Err(Error::programming(format!(
            "retire_partitions on '{}': keep_after {cutoff} is later than the current time {now}; \
             only partitions that ended in the past are retired",
            spec.name
        )));
    }
    Ok(cutoff)
}

impl Orm {
    /// Create every missing partition of `model` from the current interval through `horizon`,
    /// after finishing any detach an interruption left pending (the primitive of design §7.5).
    /// Idempotent and safe to call from several processes at once: a partition that exists is
    /// reported `existed`, one created by another call meanwhile too. Runs on `owner` (a pool
    /// connecting as the owner role: the request role has no DDL right), outside any request
    /// transaction; bumps no generation.
    ///
    /// ```no_run
    /// # async fn example(orm: &erp_orm::Orm, owner: &deadpool_postgres::Pool) -> erp_fields::Result<()> {
    /// use erp_orm::Horizon;
    /// let report = orm.ensure_partitions(owner, "ir.event.outbox", Horizon::Declared).await?;
    /// report.into_result()?; // a failed step becomes an error listing it
    /// # Ok(()) }
    /// ```
    ///
    /// # Errors
    ///
    /// `Programming` for an unknown or unpartitioned model, a horizon before the interval holding
    /// the current time, a horizon more than
    /// [`MAX_PARTITIONS_PER_CALL`](super::MAX_PARTITIONS_PER_CALL) partitions ahead or an
    /// out-of-bounds `OrmConfig::partition_lock_timeout`; `Lock` when another upkeep or a schema
    /// synchronization holds the model's upkeep lock longer than that timeout; `Database` when the
    /// table is not partitioned in the database or the catalog cannot be read. A statement that
    /// fails on one partition is a `failed` entry of the report, not an error (a wait for a lock
    /// cut by the short lock timeout is a `failed` entry with SQLSTATE `55P03`
    /// ([`PartitionEntry::is_lock_timeout`](super::PartitionEntry::is_lock_timeout)): the next run
    /// repeats it).
    pub async fn ensure_partitions(
        &self,
        owner: &Pool,
        model: &str,
        horizon: Horizon,
    ) -> Result<PartitionReport> {
        let registry = std::sync::Arc::clone(self.registry());
        let (spec, partition) = partitioned(&registry, model, "ensure_partitions")?;
        let upkeep = Upkeep::open(self, owner).await?;
        let facts = upkeep.lock(spec).await?;
        let now = facts.now()?;
        let through = match horizon {
            Horizon::Declared => catalog::declared_horizon(&partition, now, spec.name)?,
            Horizon::Through(t) => t,
        };
        let interval = partition.interval();
        let current = catalog::start_of(interval, now, spec.name)?;
        if catalog::start_of(interval, through, spec.name)? < current {
            return Err(Error::programming(format!(
                "ensure_partitions on '{model}': the horizon {through} lies before the {} holding \
                 the current time {now} (it starts {current}); a horizon is at or after the \
                 current interval",
                interval.name()
            )));
        }
        let slots = catalog::window(spec.name, &spec.table, interval, now, through)?;
        let mut report = PartitionReport::default();
        upkeep.finalize(&mut report, spec, &facts).await;
        upkeep.create_window(&mut report, spec, slots, &facts).await;
        if !upkeep.is_stopped() {
            upkeep.unlock(&mut report, spec).await;
        }
        Ok(report)
    }

    /// Retire the partitions of `model` that end at or before `keep_after` (the retention half of
    /// design §7.5): finish pending detaches, drop the tables an interrupted retirement left
    /// detached, then mark, detach (`DETACH PARTITION … CONCURRENTLY`, writers keep inserting)
    /// and drop each old partition. Runs on the owner pool, outside any request transaction;
    /// bumps no generation.
    ///
    /// # Errors
    ///
    /// `Programming` for an unknown or unpartitioned model, a model that keeps every partition
    /// (`retain = "all"`), a cutoff later than the database's current time or an out-of-bounds
    /// `OrmConfig::partition_lock_timeout`; `Lock` and `Database` as for
    /// [`Orm::ensure_partitions`]. A statement that fails on one partition is a `failed` entry of
    /// the report (a detach stopped by `lock_timeout` leaves the partition pending: the next call
    /// finalizes it).
    pub async fn retire_partitions(
        &self,
        owner: &Pool,
        model: &str,
        keep_after: KeepAfter,
    ) -> Result<PartitionReport> {
        let registry = std::sync::Arc::clone(self.registry());
        let (spec, partition) = partitioned(&registry, model, "retire_partitions")?;
        if partition.retain().intervals().is_none() {
            return Err(Error::programming(format!(
                "retire_partitions on '{model}': the model keeps every partition (retain = \"all\")"
            )));
        }
        let upkeep = Upkeep::open(self, owner).await?;
        let facts = upkeep.lock(spec).await?;
        let cutoff = cutoff(spec, &partition, keep_after, facts.now()?)?;
        let mut report = PartitionReport::default();
        upkeep.finalize(&mut report, spec, &facts).await;
        upkeep.retire_old(&mut report, spec, &facts, cutoff).await;
        if !upkeep.is_stopped() {
            upkeep.unlock(&mut report, spec).await;
        }
        Ok(report)
    }

    /// The upkeep of every partitioned model of the registry, relative to `now` (the one clock
    /// for the window and the retention cutoff; the database's clock only refuses a cutoff that
    /// lies after it): create the
    /// declared window (`premake` intervals after the one holding `now`) and, unless the model
    /// keeps every partition, retire what ends `retain` intervals before it. The report is the
    /// union of both reports for every model, in registry order. A model whose upkeep cannot start
    /// (its lock is held past the timeout, `now` lies ahead of the database's clock by more than
    /// the retention) is a `failed` entry with step [`PartitionStep::Prepare`] naming its table,
    /// and the other models are maintained all the same. Called at server start and, once
    /// lands, by its maintenance job.
    ///
    /// # Errors
    ///
    /// `Programming` for an out-of-bounds `OrmConfig::partition_lock_timeout`, `Database` when no
    /// upkeep connection can be opened; everything else is in the report.
    pub async fn maintain_partitions(
        &self,
        owner: &Pool,
        now: DateTime,
    ) -> Result<PartitionReport> {
        let registry = std::sync::Arc::clone(self.registry());
        let mut report = PartitionReport::default();
        let models = partitioned_models(&registry);
        if models.is_empty() {
            return Ok(report);
        }
        let upkeep = Upkeep::open(self, owner).await?;
        for (spec, partition) in models {
            // one model that cannot start does not stop the others: its failure is an entry
            upkeep.maintain(&mut report, spec, &partition, now).await;
            // a connection that lost its lock_timeout setting does no more work
            if upkeep.is_stopped() {
                break;
            }
        }
        Ok(report)
    }

    /// The partition window of every partitioned model: how many consecutive intervals from the
    /// current one have an attached partition (design §7.5's health metric; short when fewer than
    /// 2). One catalog read on a pooled connection of `pool` (any role that can read the catalog).
    ///
    /// # Errors
    ///
    /// `Database` when the catalog cannot be read, or when the table of a partitioned model is
    /// missing or not partitioned in the database (run the schema synchronization first).
    pub async fn partition_health(&self, pool: &Pool) -> Result<Vec<PartitionWindow>> {
        let registry = std::sync::Arc::clone(self.registry());
        let models = partitioned_models(&registry);
        if models.is_empty() {
            return Ok(Vec::new());
        }
        let client = pool.get().await.map_err(|e| {
            Error::new(
                ErrorKind::Database,
                "could not get a database connection for the partition health check",
            )
            .with_source(e)
        })?;
        let tables: Vec<String> = models.iter().map(|(m, _)| m.table.clone()).collect();
        let rows = client
            .query(erp_sql::partition::PARTITION_CATALOG_QUERY, &[&tables])
            .await
            .map_err(|e| upkeep_error("could not read the partition catalog".to_owned(), e))?;
        let facts = PartitionFacts::from_rows(&rows)?;
        let now = facts.now()?;
        let mut out = Vec::with_capacity(models.len());
        for (spec, partition) in models {
            facts.require_partitioned(spec.name, &spec.table)?;
            let covered = catalog::covered(
                &spec.table,
                partition.interval(),
                facts.rows(&spec.table),
                now,
                spec.name,
            )?;
            let pending: Vec<&catalog::PartitionRow> = facts
                .rows(&spec.table)
                .iter()
                .filter(|r| r.pending)
                .collect();
            out.push(PartitionWindow {
                model: spec.name,
                covered,
                premake: partition.premake(),
                interval: partition.interval(),
                unreadable: facts.unreadable_bounds(&spec.table),
                pending_detaches: u32::try_from(pending.len()).unwrap_or(u32::MAX),
                oldest_pending_end: pending.iter().filter_map(|r| r.to).min(),
            });
        }
        Ok(out)
    }
}
