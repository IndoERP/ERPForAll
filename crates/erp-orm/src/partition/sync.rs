//! Schema sync of partitioned leaf models (design §7.3–§7.7, §11).
//!
//! Before phase 1 the sync decides, per model, what the table's partitioning needs:
//! - no table yet: phase 1 creates it, partitioned when declared;
//! - a partitioned table matching the declaration (strategy, key column and type, primary key
//!   `(id, key)`, no default partition, every partition named and bounded for the interval): it is
//!   adopted as it is, whoever created it ('s hand-written DDL uses the same naming);
//! - a regular table where partitioning is declared, a partitioned table where it is not, or a
//!   partitioned table that does not match: the table is locked and, when it was never written
//!   (no heap pages, a measure that does not depend on the transaction's snapshot), dropped and
//!   created again in phase 1; a table with rows is refused (converting live tables is the M5
//!   business-table ADR's job).
//!
//! After phase 1 has run, the detaches an interruption left pending are finished and the missing
//! partitions of each declared window are created. That needs no generation bump (design §7.9):
//! attaching a partition changes no result shape. An index on a partitioned table above the
//! large-table threshold is a post-commit step (`SchemaStepKind::CreatePartitionedIndex`).

use erp_fields::{Error, Result};
use erp_registry::{ModelSpec, PARTITION_ADR, Partition};

use super::catalog::{self, PartitionFacts};
use crate::Transaction;

/// What the catalog says about one table of the sync (built by schema sync from its catalog).
pub(crate) struct TableState<'a> {
    pub(crate) spec: &'a ModelSpec,
    /// The table exists.
    pub(crate) exists: bool,
    /// The table exists and is partitioned (`relkind = 'p'`).
    pub(crate) partitioned: bool,
    /// The type (`udt` name) of the declared partition key's column, when it exists.
    pub(crate) key_udt: Option<String>,
    /// The definition of the table's primary key, when it has one.
    pub(crate) primary_key: Option<String>,
}

/// What schema sync does about partitioning (see the module documentation).
#[derive(Debug, Default)]
pub(crate) struct PartitionPlan {
    /// Tables dropped at the start of phase 1 and created again with the declared partitioning.
    recreate: Vec<String>,
    /// The partition catalog of the tables involved (`None` when no table is partitioned or
    /// declares partitioning).
    facts: Option<PartitionFacts>,
}

/// How an existing table differs from its declaration.
enum Change {
    ToPartitioned,
    ToRegular,
    Mismatch(Vec<String>),
}

impl PartitionPlan {
    /// The tables to drop and create again (their catalog entries are stale).
    pub(crate) fn recreated(&self) -> &[String] {
        &self.recreate
    }

    /// `DROP TABLE` of the tables created again, for the start of phase 1.
    pub(crate) fn drops(&self) -> Vec<String> {
        self.recreate
            .iter()
            .map(|t| erp_sql::partition::drop_table(t))
            .collect()
    }

    /// The statements creating the missing partitions of every partitioned model's declared
    /// window, after phase 1: one idempotent block per missing partition (the partition locks
    /// were taken by [`Transaction::plan_partitions`]). The catalog was read at the sync's
    /// snapshot, so every statement that depends on it checks the current catalog itself (a
    /// detach finalized by an upkeep call since is a no-op), and a pending detach in the window
    /// is refused only after [`Transaction::run_window`] found it still pending.
    pub(crate) fn window_statements(&self, specs: &[&ModelSpec]) -> Result<WindowPlan> {
        let Some(facts) = &self.facts else {
            return Ok(WindowPlan::default());
        };
        let now = facts.now()?;
        let mut out = WindowPlan::default();
        for spec in specs {
            let Some(partition) = spec.partition else {
                continue;
            };
            let table = spec.table.as_str();
            let through = catalog::declared_horizon(&partition, now, spec.name)?;
            let slots = catalog::window(spec.name, table, partition.interval(), now, through)?;
            // a table dropped for recreation has none of its old partitions
            let existing = if self.recreate.iter().any(|t| t == table) {
                &[][..]
            } else {
                facts.rows(table)
            };
            // A detach an interruption left pending is finished first (what the upkeep calls do):
            // otherwise the partition counts as absent and the window block below would find the
            // table in the way. The name of a pending partition that the window needs cannot be
            // had once the table is standalone: refused, naming the table, once the current
            // catalog still shows it pending.
            for row in existing.iter().filter(|r| r.pending) {
                if slots.iter().any(|slot| slot.name == row.name) {
                    out.pending_in_window.push(PendingInWindow {
                        model: spec.name,
                        table: table.to_owned(),
                        partition: row.name.clone(),
                    });
                    continue;
                }
                out.statements
                    .push(erp_sql::partition::detach_finalize_if_pending(
                        table, &row.name,
                    ));
            }
            let missing: Vec<&catalog::Slot> = slots
                .iter()
                .filter(|slot| !existing.iter().any(|r| r.name == slot.name && !r.pending))
                .collect();
            if missing.is_empty() {
                continue;
            }
            for slot in missing {
                out.statements
                    .push(erp_sql::partition::ensure_partition_block(
                        table, &slot.name, slot.from, slot.to,
                    ));
            }
        }
        Ok(out)
    }
}

/// What the window of the declared partitions needs after phase 1 (see
/// [`PartitionPlan::window_statements`]).
#[derive(Debug, Default)]
pub(crate) struct WindowPlan {
    /// finalize the stale pending detaches, create the missing partitions
    pub(crate) statements: Vec<String>,
    /// pending detaches whose name the window needs: refused when still pending
    pending_in_window: Vec<PendingInWindow>,
}

impl WindowPlan {
    /// True when there is nothing to run or check.
    pub(crate) fn is_empty(&self) -> bool {
        self.statements.is_empty() && self.pending_in_window.is_empty()
    }
}

#[derive(Debug)]
struct PendingInWindow {
    model: &'static str,
    table: String,
    partition: String,
}

impl PendingInWindow {
    fn refusal(&self) -> Error {
        Error::programming(format!(
            "partition '{}' of '{}' is being detached (an interrupted retirement) but lies in \
             the window of '{}'; drop or attach the table by hand, then synchronize the schema \
             again",
            self.partition, self.table, self.model
        ))
    }
}

/// The differences between an existing partitioned table and the declared partitioning.
fn differences(
    state: &TableState<'_>,
    partition: &Partition,
    facts: &PartitionFacts,
) -> Vec<String> {
    let table = state.spec.table.as_str();
    let key = partition.key_name();
    let mut out = Vec::new();
    let declared_key = format!("RANGE ({key})");
    match facts.keys.get(table) {
        Some(def) if *def == declared_key => {}
        Some(def) => out.push(format!("partition key is {def} (declared: {declared_key})")),
        None => out.push(format!(
            "partition key is missing (declared: {declared_key})"
        )),
    }
    match state.key_udt.as_deref() {
        Some("timestamp") => {}
        Some(udt) => out.push(format!(
            "partition key column '{key}' has type {udt} (declared: timestamp)"
        )),
        None => out.push(format!("partition key column '{key}' is missing")),
    }
    let declared_pk = format!("PRIMARY KEY (id, {key})");
    match state.primary_key.as_deref() {
        Some(def) if def == declared_pk => {}
        Some(def) => out.push(format!("primary key is {def} (declared: {declared_pk})")),
        None => out.push(format!("primary key is missing (declared: {declared_pk})")),
    }
    let interval = partition.interval();
    for row in facts.rows(table) {
        if row.bound == "DEFAULT" {
            out.push(format!(
                "default partition '{}' is not allowed (it forbids DETACH PARTITION ... \
                 CONCURRENTLY)",
                row.name
            ));
        } else if row.relkind == "p" {
            out.push(format!("partition '{}' is itself partitioned", row.name));
        } else if row.relkind != "r" {
            out.push(format!(
                "partition '{}' is not a plain table (relkind {})",
                row.name, row.relkind
            ));
        } else if !row.matches(table, interval) {
            out.push(format!(
                "partition '{}' does not match interval {}",
                row.name,
                interval.name()
            ));
        }
    }
    out
}

/// The refusal for a table with rows whose partitioning would change.
fn refusal(state: &TableState<'_>, change: &Change, visible: bool) -> Error {
    let rows = if visible {
        "has rows"
    } else {
        "has rows (none visible to this transaction: deleted rows not yet vacuumed, or rows \
         another transaction committed meanwhile)"
    };
    let what = match change {
        Change::Mismatch(differences) => format!(
            " and does not match the partitioning declared for '{}': {}",
            state.spec.name,
            differences.join("; ")
        ),
        Change::ToPartitioned | Change::ToRegular => String::new(),
    };
    Error::programming(format!(
        "table '{}' {rows}{what}; changing its partitioning is not supported ({PARTITION_ADR}; \
         business tables: M5 partitioning ADR)",
        state.spec.table
    ))
}

fn unreadable(error: tokio_postgres::Error) -> Error {
    Error::new(
        erp_fields::ErrorKind::Database,
        "could not read the emptiness probe of a table whose partitioning changes",
    )
    .with_source(error)
}

impl Transaction {
    /// Run the window of the declared partitions: first the recheck of every pending detach in the
    /// window against the current catalog (the plan was read at this transaction's snapshot, so an
    /// upkeep call may have finished the detach since; only a detach still pending is refused),
    /// then the statements.
    ///
    /// # Errors
    ///
    /// `Programming` for a pending detach in the window that is still pending; the statements'
    /// errors otherwise.
    pub(crate) async fn run_window(&mut self, plan: WindowPlan) -> Result<()> {
        for pending in &plan.pending_in_window {
            let rows = self
                .query(
                    erp_sql::partition::STILL_PARTITION_QUERY,
                    &[&erp_sql::quote(&pending.partition)],
                )
                .await?;
            let still: bool = rows
                .first()
                .ok_or_else(|| {
                    Error::new(
                        erp_fields::ErrorKind::Database,
                        format!(
                            "the recheck of partition '{}' returned no row",
                            pending.partition
                        ),
                    )
                })?
                .try_get(0)
                .map_err(unreadable)?;
            if still {
                return Err(pending.refusal());
            }
        }
        if !plan.statements.is_empty() {
            self.batch(&plan.statements.join(";\n")).await?;
        }
        Ok(())
    }

    /// Decide what partitioning needs before phase 1 (see the module documentation): take the
    /// partition locks of the tables involved and read their partition catalog (two statements,
    /// none when no table is partitioned or declares partitioning), and lock and probe the tables
    /// whose partitioning would change (only then).
    ///
    /// # Errors
    ///
    /// `Programming` for a table with rows whose partitioning would change; `Lock` when such a
    /// table cannot be locked within the schema step's `lock_timeout`; `Database` otherwise.
    pub(crate) async fn plan_partitions(
        &mut self,
        states: &[TableState<'_>],
    ) -> Result<PartitionPlan> {
        let involved: Vec<&TableState<'_>> = states
            .iter()
            .filter(|s| s.spec.partition.is_some() || s.partitioned)
            .collect();
        if involved.is_empty() {
            return Ok(PartitionPlan::default());
        }
        let tables: Vec<String> = involved.iter().map(|s| s.spec.table.clone()).collect();
        // before any DDL: an upkeep call in progress finishes first, none starts until commit
        self.query(erp_sql::partition::XACT_LOCKS, &[&tables])
            .await?;
        let rows = self
            .query(erp_sql::partition::PARTITION_CATALOG_QUERY, &[&tables])
            .await?;
        let facts = PartitionFacts::from_rows(&rows)?;
        let mut changes: Vec<(&TableState<'_>, Change)> = Vec::new();
        for state in involved {
            if !state.exists {
                continue;
            }
            match (state.partitioned, &state.spec.partition) {
                (false, Some(_)) => changes.push((state, Change::ToPartitioned)),
                (true, None) => changes.push((state, Change::ToRegular)),
                (true, Some(partition)) => {
                    let found = differences(state, partition, &facts);
                    if !found.is_empty() {
                        changes.push((state, Change::Mismatch(found)));
                    }
                }
                // not involved (filtered above)
                (false, None) => {}
            }
        }
        let mut plan = PartitionPlan {
            recreate: Vec::new(),
            facts: Some(facts),
        };
        if changes.is_empty() {
            return Ok(plan);
        }
        // no row can arrive between the probe and the drop
        let locked: Vec<&str> = changes.iter().map(|(s, _)| s.spec.table.as_str()).collect();
        self.batch(&erp_sql::partition::lock_tables(&locked))
            .await?;
        for (state, change) in &changes {
            let rows = self
                .query(&erp_sql::partition::rows_probe(&state.spec.table), &[])
                .await?;
            let Some(row) = rows.first() else {
                return Err(Error::new(
                    erp_fields::ErrorKind::Database,
                    format!(
                        "the emptiness probe of table '{}' returned no row",
                        state.spec.table
                    ),
                ));
            };
            let bytes: i64 = row.try_get(0).map_err(unreadable)?;
            let visible: bool = row.try_get(1).map_err(unreadable)?;
            if bytes > 0 || visible {
                return Err(refusal(state, change, visible));
            }
            plan.recreate.push(state.spec.table.clone());
        }
        Ok(plan)
    }
}
