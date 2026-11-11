//! The partition catalog (`erp_sql::partition::PARTITION_CATALOG_QUERY`) and the window
//! arithmetic shared by schema sync, the upkeep calls and the health check.

use erp_fields::{DateTime, Error, ErrorKind, Result};
use erp_registry::{Partition, PartitionInterval, partition_name};
use rustc_hash::FxHashMap;
use tokio_postgres::Row;

use super::MAX_PARTITIONS_PER_CALL;

/// One attached (or pending-detach) partition of a partitioned table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PartitionRow {
    /// The partition's table name.
    pub(crate) name: String,
    /// Lower bound, when the bound is a single timestamp.
    pub(crate) from: Option<DateTime>,
    /// Upper bound, when the bound is a single timestamp.
    pub(crate) to: Option<DateTime>,
    /// An interrupted `DETACH … CONCURRENTLY` left it pending.
    pub(crate) pending: bool,
    /// `relkind` (`r` for a plain table, `p` for a partitioned one).
    pub(crate) relkind: String,
    /// The bound as PostgreSQL writes it (`FOR VALUES FROM (…) TO (…)`, `DEFAULT`).
    pub(crate) bound: String,
}

impl PartitionRow {
    /// True for an attached plain partition with exactly the bounds of `interval`'s partition
    /// named after its start (`<table>_p<suffix>`).
    pub(crate) fn matches(&self, table: &str, interval: PartitionInterval) -> bool {
        let Some(start) = self
            .name
            .strip_prefix(table)
            .and_then(|rest| rest.strip_prefix("_p"))
            .and_then(|suffix| interval.parse_suffix(suffix))
        else {
            return false;
        };
        self.relkind == "r" && self.from == Some(start) && self.to == interval.next(start)
    }
}

/// What the partition catalog says about a set of tables.
#[derive(Debug, Default)]
pub(crate) struct PartitionFacts {
    /// table → `pg_get_partkeydef` (partitioned tables only)
    pub(crate) keys: FxHashMap<String, String>,
    /// parent table → its partitions, by name
    partitions: FxHashMap<String, Vec<PartitionRow>>,
    now: Option<DateTime>,
}

fn unreadable(what: &str) -> Error {
    Error::new(
        ErrorKind::Database,
        format!("could not read the partition catalog: {what}"),
    )
}

fn micros(text: Option<String>, what: &str) -> Result<Option<DateTime>> {
    text.map(|t| {
        t.parse::<i64>()
            .map(DateTime::from_micros)
            .map_err(|_| unreadable(&format!("{what} {t:?} is not a number")))
    })
    .transpose()
}

impl PartitionFacts {
    /// Parse the rows of `PARTITION_CATALOG_QUERY`.
    pub(crate) fn from_rows(rows: &[Row]) -> Result<Self> {
        let mut facts = Self::default();
        for row in rows {
            let text = |i: usize| -> Result<Option<String>> {
                row.try_get::<_, Option<String>>(i).map_err(|e| {
                    Error::new(ErrorKind::Database, "could not read the partition catalog")
                        .with_source(e)
                })
            };
            let kind = text(0)?.ok_or_else(|| unreadable("a row without a kind"))?;
            match kind.as_str() {
                "partkey" => {
                    let table = text(1)?.ok_or_else(|| unreadable("a key without a table"))?;
                    let def = text(3)?.ok_or_else(|| unreadable("a key without a definition"))?;
                    facts.keys.insert(table, def);
                }
                "partition" => {
                    let parent =
                        text(1)?.ok_or_else(|| unreadable("a partition without a parent"))?;
                    let name = text(2)?.ok_or_else(|| unreadable("a partition without a name"))?;
                    let from = micros(text(3)?, "lower bound")?;
                    let to = micros(text(4)?, "upper bound")?;
                    // a range with one unreadable end (`-infinity`, `MINVALUE`) is placed in time
                    // by neither: it is retired and counted by no one
                    let (from, to) = if from.is_some() && to.is_some() {
                        (from, to)
                    } else {
                        (None, None)
                    };
                    let flags = text(5)?.ok_or_else(|| unreadable("a partition without flags"))?;
                    let mut parts = flags.splitn(3, '|');
                    let (Some(pending), Some(relkind), Some(bound)) =
                        (parts.next(), parts.next(), parts.next())
                    else {
                        return Err(unreadable(&format!("partition flags {flags:?}")));
                    };
                    let pending = match pending {
                        "true" => true,
                        "false" => false,
                        other => {
                            return Err(unreadable(&format!("detach-pending flag {other:?}")));
                        }
                    };
                    facts
                        .partitions
                        .entry(parent)
                        .or_default()
                        .push(PartitionRow {
                            name,
                            from,
                            to,
                            pending,
                            relkind: relkind.to_owned(),
                            bound: bound.to_owned(),
                        });
                }
                "now" => facts.now = micros(text(3)?, "now")?,
                other => return Err(unreadable(&format!("unknown row kind {other:?}"))),
            }
        }
        for rows in facts.partitions.values_mut() {
            rows.sort_by(|a, b| a.name.cmp(&b.name));
        }
        Ok(facts)
    }

    /// The database's `now()` when the catalog was read (UTC).
    pub(crate) fn now(&self) -> Result<DateTime> {
        self.now
            .ok_or_else(|| unreadable("the current time is missing"))
    }

    /// The partitions of `table`, by name (none for a table that is not partitioned).
    pub(crate) fn rows(&self, table: &str) -> &[PartitionRow] {
        self.partitions.get(table).map_or(&[], Vec::as_slice)
    }

    /// Attached partitions of `table` whose bounds are not a pair of finite timestamps: the
    /// upkeep cannot place them in time.
    pub(crate) fn unreadable_bounds(&self, table: &str) -> u32 {
        let n = self
            .rows(table)
            .iter()
            .filter(|r| !r.pending && (r.from.is_none() || r.to.is_none()))
            .count();
        u32::try_from(n).unwrap_or(u32::MAX)
    }

    /// `table` of `model` must be a partitioned table in the database.
    ///
    /// # Errors
    ///
    /// `Database` naming the table and the model when it is missing or not partitioned.
    pub(crate) fn require_partitioned(&self, model: &str, table: &str) -> Result<()> {
        if self.keys.contains_key(table) {
            Ok(())
        } else {
            Err(Error::new(
                ErrorKind::Database,
                format!(
                    "table '{table}' of partitioned model '{model}' is not a partitioned table in \
                     the database (missing or a plain table); run the schema synchronization first"
                ),
            ))
        }
    }
}

fn out_of_range(model: &str) -> Error {
    Error::programming(format!(
        "the partition window of '{model}' falls outside the time range partitions can be named \
         for (years 1 to 9999)"
    ))
}

/// The start of the interval holding `t`.
pub(crate) fn start_of(interval: PartitionInterval, t: DateTime, model: &str) -> Result<DateTime> {
    interval.start_of(t).ok_or_else(|| out_of_range(model))
}

/// `start` moved by `n` intervals.
pub(crate) fn shift(
    interval: PartitionInterval,
    start: DateTime,
    n: i64,
    model: &str,
) -> Result<DateTime> {
    interval.shift(start, n).ok_or_else(|| out_of_range(model))
}

/// The end of the declared window of `partition`: the start of the interval `premake` intervals
/// after the one holding `now`.
pub(crate) fn declared_horizon(
    partition: &Partition,
    now: DateTime,
    model: &str,
) -> Result<DateTime> {
    let interval = partition.interval();
    let current = start_of(interval, now, model)?;
    shift(interval, current, i64::from(partition.premake()), model)
}

/// The declared retention cutoff: the start of the interval `retain` intervals before the one
/// holding `now`, or `None` when every partition is kept.
pub(crate) fn declared_cutoff(
    partition: &Partition,
    now: DateTime,
    model: &str,
) -> Result<Option<DateTime>> {
    let Some(retain) = partition.retain().intervals() else {
        return Ok(None);
    };
    let interval = partition.interval();
    let current = start_of(interval, now, model)?;
    shift(interval, current, -i64::from(retain), model).map(Some)
}

/// One partition of a window: its name and range.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Slot {
    pub(crate) name: String,
    pub(crate) from: DateTime,
    pub(crate) to: DateTime,
}

/// The partitions from the interval holding `from` through the one holding `through` (none when
/// `through` is earlier), at most [`MAX_PARTITIONS_PER_CALL`].
///
/// # Errors
///
/// `Programming` when the window holds more partitions than that, or leaves the nameable range.
pub(crate) fn window(
    model: &str,
    table: &str,
    interval: PartitionInterval,
    from: DateTime,
    through: DateTime,
) -> Result<Vec<Slot>> {
    let last = start_of(interval, through, model)?;
    let mut at = start_of(interval, from, model)?;
    let mut out = Vec::new();
    while at <= last {
        if out.len() >= MAX_PARTITIONS_PER_CALL {
            return Err(Error::programming(format!(
                "the partition window of '{model}' through {through} holds more than \
                 {MAX_PARTITIONS_PER_CALL} partitions; create them in several calls"
            )));
        }
        let next = shift(interval, at, 1, model)?;
        let name = partition_name(table, interval, at).ok_or_else(|| out_of_range(model))?;
        out.push(Slot {
            name,
            from: at,
            to: next,
        });
        at = next;
    }
    Ok(out)
}

/// Consecutive intervals with an attached partition of the right bounds, from the one holding
/// `now` on (the health check's measure, design §7.5).
pub(crate) fn covered(
    table: &str,
    interval: PartitionInterval,
    rows: &[PartitionRow],
    now: DateTime,
    model: &str,
) -> Result<u32> {
    let mut at = start_of(interval, now, model)?;
    let mut covered: u32 = 0;
    loop {
        let Some(name) = partition_name(table, interval, at) else {
            return Ok(covered);
        };
        let attached = rows
            .iter()
            .any(|r| r.name == name && !r.pending && r.matches(table, interval));
        if !attached {
            return Ok(covered);
        }
        covered = covered.saturating_add(1);
        at = shift(interval, at, 1, model)?;
    }
}

#[cfg(test)]
mod tests {
    use erp_fields::Date;

    use super::*;

    fn day(d: u32) -> DateTime {
        Date::from_ymd(2026, 10, d).unwrap().at_midnight()
    }

    fn row(name: &str, from: u32, to: u32) -> PartitionRow {
        PartitionRow {
            name: name.to_owned(),
            from: Some(day(from)),
            to: Some(day(to)),
            pending: false,
            relkind: "r".to_owned(),
            bound: String::new(),
        }
    }

    #[test]
    fn windows_and_coverage() {
        let noon =
            DateTime::from_parts(Date::from_ymd(2026, 10, 16).unwrap(), 12, 0, 0, 0).unwrap();
        let slots = window("m", "t", PartitionInterval::Day, noon, day(18)).unwrap();
        let names: Vec<&str> = slots.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["t_p20261016", "t_p20261017", "t_p20261018"]);
        assert_eq!(slots[0].from, day(16));
        assert_eq!(slots[0].to, day(17));
        assert!(
            window("m", "t", PartitionInterval::Day, noon, day(15))
                .unwrap()
                .is_empty()
        );
        let rows = [row("t_p20261016", 16, 17), row("t_p20261017", 17, 18)];
        assert_eq!(
            covered("t", PartitionInterval::Day, &rows, noon, "m").unwrap(),
            2
        );
        // a gap stops the count; wrong bounds do not count
        let rows = [row("t_p20261016", 16, 17), row("t_p20261018", 18, 19)];
        assert_eq!(
            covered("t", PartitionInterval::Day, &rows, noon, "m").unwrap(),
            1
        );
        let rows = [row("t_p20261016", 16, 18)];
        assert_eq!(
            covered("t", PartitionInterval::Day, &rows, noon, "m").unwrap(),
            0
        );
        assert_eq!(
            window("m", "t", PartitionInterval::Hour, noon, day(30))
                .unwrap()
                .len(),
            325
        );
        let new_year = Date::from_ymd(2027, 1, 1).unwrap().at_midnight();
        let error = window("m", "t", PartitionInterval::Hour, noon, new_year).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Programming);
        assert_eq!(
            error.message(),
            format!(
                "the partition window of 'm' through {new_year} holds more than \
                 {MAX_PARTITIONS_PER_CALL} partitions; create them in several calls"
            )
        );
    }

    /// RS-8: a window of exactly `MAX_PARTITIONS_PER_CALL` partitions is fine, one more is refused,
    /// with the message built from the constant.
    #[test]
    fn the_window_limit_is_exact() {
        let from = day(1);
        let last_ok = PartitionInterval::Day
            .shift(from, i64::try_from(MAX_PARTITIONS_PER_CALL).unwrap() - 1)
            .unwrap();
        let slots = window("m", "t", PartitionInterval::Day, from, last_ok).unwrap();
        assert_eq!(slots.len(), MAX_PARTITIONS_PER_CALL);
        let too_far = PartitionInterval::Day.next(last_ok).unwrap();
        let error = window("m", "t", PartitionInterval::Day, from, too_far).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Programming);
        assert_eq!(
            error.message(),
            format!(
                "the partition window of 'm' through {too_far} holds more than \
                 {MAX_PARTITIONS_PER_CALL} partitions; create them in several calls"
            )
        );
    }

    #[test]
    fn rows_match_their_interval() {
        assert!(row("t_p20261016", 16, 17).matches("t", PartitionInterval::Day));
        assert!(!row("t_p20261016", 16, 18).matches("t", PartitionInterval::Day));
        assert!(!row("t_p202610", 1, 2).matches("t", PartitionInterval::Day));
        assert!(!row("other_p20261016", 16, 17).matches("t", PartitionInterval::Day));
        let mut nested = row("t_p20261016", 16, 17);
        nested.relkind = "p".to_owned();
        assert!(!nested.matches("t", PartitionInterval::Day));
    }
}
