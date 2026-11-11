//! `ir.sequence`: sequential numbering engine (BASE §4.2, H21).
//!
//! Document numbering primitives:
//! - Standard: gap-allowed PostgreSQL sequence (`SELECT nextval(...)`), lock-free.
//! - Period partitioning: subsequences partitioned by date ranges (`ir.sequence.date_range`).
//! - Template interpolation: Python `%(var)s` directives supporting standard and ISO calendars.
//!   These
//!   are pure: no transaction, no database access.

use erp_fields::{Condition, Date, Error, ErrorKind, Result, Value};
use rustc_hash::FxHashMap;

use crate::Transaction;
use crate::env::Env;
use crate::records::Records;
use crate::vals::RawVals;

pub mod pattern;

pub use pattern::{
    FiscalYear, MAX_NAME_BYTES, MAX_PATTERN_PADDING, MAX_PATTERN_TEXT_BYTES, PeriodKey,
    SequencePattern, SequencePatternBuilder, SequenceReset, YearDigits, render_sequence_format,
    resolve_period_key,
};

/// How a draw on a gapless (`no_gap`) sequence takes its row lock (1, D2.8), stored in
/// `ir.sequence.wait_mode`. Standard sequences take no row lock, so the mode does not apply to them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum SequenceWaitMode {
    /// The stored default. A relation-lock timeout (concurrent DDL)
    /// trips the transaction's `lock_timeout` instead of an immediate row-lock refusal, returning
    /// the tagged [`Condition::LockTimeout`].
    #[default]
    NoWait,
    /// `FOR UPDATE`: the draw waits for the holder, at most the transaction's `lock_timeout` (the
    /// unit of work's [`Timeouts::lock`](crate::Timeouts::lock), 10 s by default; the slice sets no
    /// limit of its own). Under `REPEATABLE READ` a holder that updated the row and committed makes
    /// the waiting statement fail with `Concurrency`/`40001`, which [`Orm::run`](crate::Orm::run)
    /// retries as a new attempt that reads the new counter; a bare transaction gets
    /// the `40001`. When the limit runs out first the draw fails with `Lock`/`55P03` carrying
    /// [`Condition::LockTimeout`] and naming the sequence.
    LockTimeout,
}

impl SequenceWaitMode {
    /// Parse the stored value (`"nowait"` or `"lock_timeout"`).
    ///
    /// # Errors
    ///
    /// `Validation` naming the value for anything else.
    pub fn parse(text: &str) -> Result<Self> {
        match text {
            "nowait" => Ok(Self::NoWait),
            "lock_timeout" => Ok(Self::LockTimeout),
            other => Err(Error::validation(format!(
                "unknown sequence wait_mode {other:?}; expected 'nowait' or 'lock_timeout'"
            ))),
        }
    }

    /// The stored value.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NoWait => "nowait",
            Self::LockTimeout => "lock_timeout",
        }
    }
}

/// A typed identifier for PostgreSQL backing sequences.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SequenceName {
    Standard(i64),
    DateRange(i64, i64),
}

impl SequenceName {
    pub(crate) fn to_identifier(self) -> String {
        match self {
            Self::Standard(seq_id) => format!("ir_sequence_{seq_id:03}"),
            Self::DateRange(seq_id, range_id) => format!("ir_sequence_{seq_id:03}_{range_id:03}"),
        }
    }

    /// The quoted SQL identifier for DDL statements (e.g. `"ir_sequence_001"`).
    pub(crate) fn quoted(self) -> String {
        format!("\"{}\"", self.to_identifier())
    }

    /// The regclass string argument for `nextval($1::regclass)`.
    pub(crate) fn regclass_arg(self) -> String {
        format!("\"{}\"", self.to_identifier())
    }
}

/// Interpolate a template string containing Python-style `%(var)s` placeholders.
pub fn interpolate_template(
    template: &str,
    vars: &FxHashMap<String, String>,
    seq_name: &str,
) -> Result<String> {
    let mut out = String::with_capacity(template.len());
    let mut chars = template.char_indices().peekable();

    while let Some((_, c)) = chars.next() {
        if c == '%' {
            match chars.peek() {
                Some((_, '%')) => {
                    chars.next();
                    out.push('%');
                }
                Some((_, '(')) => {
                    chars.next(); // consume '('
                    let mut var_name = String::new();
                    let mut closed = false;
                    for (_, vc) in chars.by_ref() {
                        if vc == ')' {
                            closed = true;
                            break;
                        }
                        var_name.push(vc);
                    }
                    if !closed || chars.next().map(|(_, sc)| sc) != Some('s') {
                        return Err(Error::user(format!(
                            "Invalid prefix or suffix for sequence “{seq_name}”"
                        )));
                    }
                    if let Some(val) = vars.get(var_name.as_str()) {
                        out.push_str(val);
                    } else {
                        return Err(Error::user(format!(
                            "Invalid prefix or suffix for sequence “{seq_name}”"
                        )));
                    }
                }
                _ => {
                    return Err(Error::user(format!(
                        "Invalid prefix or suffix for sequence “{seq_name}”"
                    )));
                }
            }
        } else {
            out.push(c);
        }
    }

    Ok(out)
}

/// Uses `jiff::civil::Date` for proven calendar arithmetic.
pub fn date_interpolation_parts(date: Date) -> Result<(u32, u32, i32, u32, u32)> {
    let (year, month, day) = date.ymd();
    let cdate = jiff::civil::Date::new(year as i16, month as i8, day as i8)
        .map_err(|e| Error::validation(format!("Invalid date for sequence interpolation: {e}")))?;

    let doy = cdate.day_of_year() as u32;
    let iso = cdate.iso_week_date();
    let isoyear = iso.year() as i32;
    let isoweek = iso.week() as u32;

    // Week of year (%W in Python): Monday as first day of week. Days before first Monday are week 0.
    let jan1 = jiff::civil::Date::new(year as i16, 1, 1)
        .map_err(|e| Error::validation(format!("Invalid date for sequence interpolation: {e}")))?;
    let jan1_wd = jan1.weekday().to_monday_one_offset();
    let first_monday_doy = if jan1_wd == 1 { 1 } else { (8 - jan1_wd) + 1 };
    let woy = if doy < first_monday_doy as u32 {
        0
    } else {
        (doy - first_monday_doy as u32) / 7 + 1
    };

    // Weekday in Python %w: 0 = Sunday, 1 = Monday, ..., 6 = Saturday
    let odoo_wd = cdate.weekday().to_sunday_zero_offset() as u32;

    Ok((doy, woy, isoyear, isoweek, odoo_wd))
}

/// Maximum supported padding width for sequence numbers.
pub const MAX_PADDING: i32 = 64;

fn date_directives(date: Date) -> Result<[(&'static str, String); 14]> {
    let (year, month, day) = date.ymd();
    let (doy, woy, isoyear, isoweek, odoo_wd) = date_interpolation_parts(date)?;
    Ok([
        ("year", format!("{year:04}")),
        ("month", format!("{month:02}")),
        ("day", format!("{day:02}")),
        ("y", format!("{:02}", year.rem_euclid(100))),
        ("doy", doy.to_string()),
        ("woy", format!("{woy:02}")),
        ("weekday", odoo_wd.to_string()),
        ("h24", "00".to_string()),
        ("h12", "12".to_string()),
        ("min", "00".to_string()),
        ("sec", "00".to_string()),
        ("isoyear", format!("{isoyear:04}")),
        ("isoy", format!("{:02}", isoyear.rem_euclid(100))),
        ("isoweek", format!("{isoweek:02}")),
    ])
}

/// Build dictionary of date and ISO calendar interpolation directives.
///
/// Supports effective date directives, `range_*` directives (from date range),
/// and `current_*` directives (from current clock).
pub fn build_interpolation_vars(
    target_date: Date,
    range_date: Date,
    current_date: Date,
) -> Result<FxHashMap<String, String>> {
    let mut vars = FxHashMap::default();
    for (k, v) in date_directives(target_date)? {
        vars.insert(k.to_string(), v);
    }
    for (k, v) in date_directives(range_date)? {
        vars.insert(format!("range_{k}"), v);
    }
    for (k, v) in date_directives(current_date)? {
        vars.insert(format!("current_{k}"), v);
    }
    Ok(vars)
}

/// Build dictionary of date and ISO calendar interpolation directives for a single date.
pub fn build_date_vars(date: Date) -> Result<FxHashMap<String, String>> {
    build_interpolation_vars(date, date, date)
}

/// Create a backing PostgreSQL sequence for standard implementation.
pub(crate) async fn create_backing_sequence(
    tx: &mut Transaction,
    name: SequenceName,
    increment: i32,
    start: i32,
) -> Result<()> {
    if increment == 0 {
        return Err(Error::user("Step must not be zero."));
    }
    let quoted = name.quoted();
    let sql = format!(
        "CREATE SEQUENCE IF NOT EXISTS {quoted} INCREMENT BY {increment} START WITH {start}"
    );
    tx.execute(&sql, &[]).await?;
    Ok(())
}

/// Drop a backing PostgreSQL sequence.
pub(crate) async fn drop_backing_sequence(tx: &mut Transaction, name: SequenceName) -> Result<()> {
    let quoted = name.quoted();
    let sql = format!("DROP SEQUENCE IF EXISTS {quoted} RESTRICT");
    tx.execute(&sql, &[]).await?;
    Ok(())
}

/// Alter a backing PostgreSQL sequence restart value and increment.
pub(crate) async fn alter_backing_sequence(
    tx: &mut Transaction,
    name: SequenceName,
    restart: Option<i32>,
    increment: Option<i32>,
) -> Result<()> {
    if increment == Some(0) {
        return Err(Error::user("Step must not be zero."));
    }
    let quoted = name.quoted();
    let mut parts = Vec::new();
    if let Some(restart_val) = restart {
        parts.push(format!("RESTART WITH {restart_val}"));
    }
    if let Some(inc_val) = increment {
        parts.push(format!("INCREMENT BY {inc_val}"));
    }
    if parts.is_empty() {
        return Ok(());
    }
    let sql = format!("ALTER SEQUENCE {quoted} {}", parts.join(" "));
    tx.execute(&sql, &[]).await?;
    Ok(())
}

/// Hook called when `ir.sequence` or `ir.sequence.date_range` records are created.
pub(crate) async fn on_records_created(tx: &mut Transaction, records: &Records) -> Result<()> {
    let model = records.model_name();
    if model == "ir.sequence" {
        for id in records.rec_ids() {
            let rows = tx
                .query(
                    "SELECT implementation, number_increment, number_next FROM ir_sequence WHERE id = $1",
                    &[&id.get()],
                )
                .await?;
            let row = rows
                .first()
                .ok_or_else(|| Error::db(format!("ir.sequence {id} missing after insert")))?;
            let impl_type: String = row.try_get(0).map_err(|e| Error::db(e.to_string()))?;
            let inc: i32 = row.try_get(1).map_err(|e| Error::db(e.to_string()))?;
            let next: i32 = row.try_get(2).map_err(|e| Error::db(e.to_string()))?;
            if impl_type == "standard" {
                create_backing_sequence(tx, SequenceName::Standard(id.get()), inc, next).await?;
            }
        }
    } else if model == "ir.sequence.date_range" {
        for id in records.rec_ids() {
            let rows = tx
                .query(
                    r#"
                    SELECT r.sequence_id, r.number_next, s.implementation, s.number_increment
                    FROM ir_sequence_date_range r
                    JOIN ir_sequence s ON r.sequence_id = s.id
                    WHERE r.id = $1
                    "#,
                    &[&id.get()],
                )
                .await?;
            let row = rows.first().ok_or_else(|| {
                Error::db(format!("ir.sequence.date_range {id} missing after insert"))
            })?;
            let seq_id: i64 = row.try_get(0).map_err(|e| Error::db(e.to_string()))?;
            let next: i32 = row.try_get(1).map_err(|e| Error::db(e.to_string()))?;
            let impl_type: String = row.try_get(2).map_err(|e| Error::db(e.to_string()))?;
            let inc: i32 = row.try_get(3).map_err(|e| Error::db(e.to_string()))?;
            if impl_type == "standard" {
                create_backing_sequence(tx, SequenceName::DateRange(seq_id, id.get()), inc, next)
                    .await?;
            }
        }
    }
    Ok(())
}

/// Hook called when `ir.sequence` or `ir.sequence.date_range` records are written.
pub(crate) async fn on_records_written(
    tx: &mut Transaction,
    records: &Records,
    vals: &RawVals,
) -> Result<()> {
    let model = records.model_name();
    if model == "ir.sequence" {
        let has_impl = vals.contains("implementation");
        let has_next = vals.contains("number_next");
        let has_inc = vals.contains("number_increment");
        if !has_impl && !has_next && !has_inc {
            return Ok(());
        }
        tx.flush_model(records.model_id()).await?;
        for id in records.rec_ids() {
            let rows = tx
                .query(
                    "SELECT implementation, number_increment, number_next FROM ir_sequence WHERE id = $1",
                    &[&id.get()],
                )
                .await?;
            let row = rows
                .first()
                .ok_or_else(|| Error::db(format!("ir.sequence {id} missing during write hook")))?;
            let impl_type: String = row.try_get(0).map_err(|e| Error::db(e.to_string()))?;
            let inc: i32 = row.try_get(1).map_err(|e| Error::db(e.to_string()))?;
            let next: i32 = row.try_get(2).map_err(|e| Error::db(e.to_string()))?;

            if impl_type == "standard" {
                // Ensure sequence exists (e.g. if switched from no_gap)
                create_backing_sequence(tx, SequenceName::Standard(id.get()), inc, next).await?;
                alter_backing_sequence(
                    tx,
                    SequenceName::Standard(id.get()),
                    if has_next { Some(next) } else { None },
                    if has_inc { Some(inc) } else { None },
                )
                .await?;

                if has_inc {
                    let range_rows = tx
                        .query(
                            "SELECT id FROM ir_sequence_date_range WHERE sequence_id = $1",
                            &[&id.get()],
                        )
                        .await?;
                    for rrow in range_rows {
                        let rid: i64 = rrow.try_get(0).map_err(|e| Error::db(e.to_string()))?;
                        alter_backing_sequence(
                            tx,
                            SequenceName::DateRange(id.get(), rid),
                            None,
                            Some(inc),
                        )
                        .await?;
                    }
                }
            } else {
                // implementation is no_gap: drop standard sequence if it existed
                drop_backing_sequence(tx, SequenceName::Standard(id.get())).await?;
                let range_rows = tx
                    .query(
                        "SELECT id FROM ir_sequence_date_range WHERE sequence_id = $1",
                        &[&id.get()],
                    )
                    .await?;
                for rrow in range_rows {
                    let rid: i64 = rrow.try_get(0).map_err(|e| Error::db(e.to_string()))?;
                    drop_backing_sequence(tx, SequenceName::DateRange(id.get(), rid)).await?;
                }
            }
        }
    } else if model == "ir.sequence.date_range" {
        let has_next = vals.contains("number_next");
        if !has_next {
            return Ok(());
        }
        tx.flush_model(records.model_id()).await?;
        for id in records.rec_ids() {
            let rows = tx
                .query(
                    r#"
                    SELECT r.sequence_id, r.number_next, s.implementation
                    FROM ir_sequence_date_range r
                    JOIN ir_sequence s ON r.sequence_id = s.id
                    WHERE r.id = $1
                    "#,
                    &[&id.get()],
                )
                .await?;
            let row = rows.first().ok_or_else(|| {
                Error::db(format!(
                    "ir.sequence.date_range {id} missing during write hook"
                ))
            })?;
            let seq_id: i64 = row.try_get(0).map_err(|e| Error::db(e.to_string()))?;
            let next: i32 = row.try_get(1).map_err(|e| Error::db(e.to_string()))?;
            let impl_type: String = row.try_get(2).map_err(|e| Error::db(e.to_string()))?;
            if impl_type == "standard" {
                alter_backing_sequence(
                    tx,
                    SequenceName::DateRange(seq_id, id.get()),
                    Some(next),
                    None,
                )
                .await?;
            }
        }
    }
    Ok(())
}

/// Hook called when `ir.sequence` or `ir.sequence.date_range` records are unlinked.
pub(crate) async fn on_records_unlinked(
    tx: &mut Transaction,
    records: &Records,
    ids: &[erp_fields::RecId],
) -> Result<()> {
    let model = records.model_name();
    if model == "ir.sequence" {
        for id in ids {
            drop_backing_sequence(tx, SequenceName::Standard(id.get())).await?;
            let range_rows = tx
                .query(
                    "SELECT id FROM ir_sequence_date_range WHERE sequence_id = $1",
                    &[&id.get()],
                )
                .await?;
            for rrow in range_rows {
                let rid: i64 = rrow.try_get(0).map_err(|e| Error::db(e.to_string()))?;
                drop_backing_sequence(tx, SequenceName::DateRange(id.get(), rid)).await?;
            }
        }
    } else if model == "ir.sequence.date_range" {
        for id in ids {
            let rows = tx
                .query(
                    "SELECT sequence_id FROM ir_sequence_date_range WHERE id = $1",
                    &[&id.get()],
                )
                .await?;
            let row = rows.first().ok_or_else(|| {
                Error::db(format!(
                    "ir.sequence.date_range {id} missing during unlink hook"
                ))
            })?;
            let seq_id: i64 = row.try_get(0).map_err(|e| Error::db(e.to_string()))?;
            drop_backing_sequence(tx, SequenceName::DateRange(seq_id, id.get())).await?;
        }
    }
    Ok(())
}

struct SequenceRecordData {
    seq_id: i64,
    implementation: String,
    prefix_tpl: Option<String>,
    suffix_tpl: Option<String>,
    number_next: i32,
    number_increment: i32,
    padding: i32,
    use_date_range: bool,
    seq_name: String,
    wait_mode: SequenceWaitMode,
}

impl SequenceRecordData {
    fn from_row(row: &tokio_postgres::Row) -> Result<Self> {
        let seq_id: i64 = row.try_get(0).map_err(|e| Error::db(e.to_string()))?;
        let implementation: String = row.try_get(1).map_err(|e| Error::db(e.to_string()))?;
        let prefix_tpl: Option<String> = row.try_get(2).map_err(|e| Error::db(e.to_string()))?;
        let suffix_tpl: Option<String> = row.try_get(3).map_err(|e| Error::db(e.to_string()))?;
        let number_next: i32 = row.try_get(4).map_err(|e| Error::db(e.to_string()))?;
        let number_increment: i32 = row.try_get(5).map_err(|e| Error::db(e.to_string()))?;
        let padding: i32 = row.try_get(6).map_err(|e| Error::db(e.to_string()))?;
        let use_date_range: bool = row.try_get(7).map_err(|e| Error::db(e.to_string()))?;
        let seq_name: String = row.try_get(8).map_err(|e| Error::db(e.to_string()))?;
        // NULL was mapped to the field's default 'nowait' by the query's COALESCE (an absent
        // value); any other value must be one of the two modes
        let wait_mode_raw: String = row.try_get(9).map_err(|e| Error::db(e.to_string()))?;
        let wait_mode = SequenceWaitMode::parse(&wait_mode_raw).map_err(|_| {
            Error::validation(format!(
                "ir.sequence {seq_id} (“{seq_name}”) has wait_mode {wait_mode_raw:?}; expected \
                 'nowait' or 'lock_timeout'"
            ))
        })?;

        if !(0..=MAX_PADDING).contains(&padding) {
            return Err(Error::validation(format!(
                "Sequence padding must be between 0 and {MAX_PADDING}, got {padding}"
            )));
        }

        Ok(Self {
            seq_id,
            implementation,
            prefix_tpl,
            suffix_tpl,
            number_next,
            number_increment,
            padding,
            use_date_range,
            seq_name,
            wait_mode,
        })
    }
}

/// The row a gapless draw locks.
#[derive(Debug, Clone, Copy)]
enum LockTarget {
    /// The `ir_sequence` row itself.
    Sequence,
    /// An `ir_sequence_date_range` row of the sequence.
    DateRange(i64),
}

/// Lock the counter row of a gapless draw in `mode` and read `number_next` (/036,
///
/// `LockTimeout`: `FOR UPDATE`, bounded by the transaction's `lock_timeout`. The transaction
///   layer's lock-timeout error keeps its kind, SQLSTATE, [`Condition::LockTimeout`] and source;
///   only the sequence is named in front of its message. A `40001` after the wait (the holder
///   updated the row and committed, `REPEATABLE READ`) and every other error pass through
///   unchanged.
async fn lock_number_next(
    tx: &mut Transaction,
    target: LockTarget,
    mode: SequenceWaitMode,
    record: &SequenceRecordData,
) -> Result<i32> {
    let (sql, id, what) = match (target, mode) {
        (LockTarget::Sequence, SequenceWaitMode::NoWait) => (
            "SELECT number_next FROM ir_sequence WHERE id = $1 FOR UPDATE NOWAIT",
            record.seq_id,
            "no_gap sequence",
        ),
        (LockTarget::Sequence, SequenceWaitMode::LockTimeout) => (
            "SELECT number_next FROM ir_sequence WHERE id = $1 FOR UPDATE",
            record.seq_id,
            "no_gap sequence",
        ),
        (LockTarget::DateRange(range_id), SequenceWaitMode::NoWait) => (
            "SELECT number_next FROM ir_sequence_date_range WHERE id = $1 FOR UPDATE NOWAIT",
            range_id,
            "no_gap sequence date range",
        ),
        (LockTarget::DateRange(range_id), SequenceWaitMode::LockTimeout) => (
            "SELECT number_next FROM ir_sequence_date_range WHERE id = $1 FOR UPDATE",
            range_id,
            "no_gap sequence date range",
        ),
    };
    let rows = match tx.query(sql, &[&id]).await {
        Ok(rows) => rows,
        Err(e) => {
            return Err(match mode {
                SequenceWaitMode::NoWait
                    if (e.sqlstate() == Some("55P03") || e.kind() == ErrorKind::Lock)
                        && e.condition().is_none() =>
                {
                    Error::lock(format!("could not obtain lock on {what}"))
                        .with_sqlstate(Some("55P03".to_string()))
                }
                // the condition was set by the transaction layer, which detected the timeout;
                // it is carried over, never derived here from the SQLSTATE
                _ if e.condition() == Some(Condition::LockTimeout) => {
                    let target = match target {
                        LockTarget::Sequence => String::new(),
                        LockTarget::DateRange(range_id) => format!(", date range {range_id}"),
                    };
                    let message = format!(
                        "could not obtain the lock on {what} “{}” (id {}{target}) within \
                         lock_timeout: {}",
                        record.seq_name,
                        record.seq_id,
                        e.message()
                    );
                    let (kind, sqlstate) = (e.kind(), e.sqlstate().map(str::to_owned));
                    Error::database(kind, sqlstate, message, e)
                        .with_condition(Condition::LockTimeout)
                }
                _ => e,
            });
        }
    };
    let row = rows
        .first()
        .ok_or_else(|| Error::db("SELECT number_next returned no rows"))?;
    row.try_get(0).map_err(|e| Error::db(e.to_string()))
}

fn default_clock() -> Date {
    erp_fields::DateTime::now().date()
}

async fn draw_sequence_record<F>(
    tx: &mut Transaction,
    env: &Env,
    record: SequenceRecordData,
    sequence_date: Option<Date>,
    clock: F,
    override_wait_mode: Option<SequenceWaitMode>,
) -> Result<String>
where
    F: Fn() -> Date,
{
    // a mode passed by the caller wins over the stored one
    let effective_wait_mode = override_wait_mode.unwrap_or(record.wait_mode);

    let target_date = match sequence_date {
        Some(d) => d,
        None => match env.context().get("ir_sequence_date") {
            Some(Value::Str(s)) => Date::parse_odoo(s.as_ref()).ok_or_else(|| {
                Error::validation(format!("Invalid ir_sequence_date in context: {s}"))
            })?,
            Some(Value::Date(d)) => d,
            Some(other) => {
                let s = other.to_string();
                Date::parse_odoo(&s).ok_or_else(|| {
                    Error::validation(format!("Invalid ir_sequence_date in context: {s}"))
                })?
            }
            None => clock(),
        },
    };

    let (num, range_date) = if record.use_date_range {
        let range_sql = r#"
            SELECT id, number_next, date_from
            FROM ir_sequence_date_range
            WHERE sequence_id = $1 AND date_from <= $2 AND date_to >= $2
            LIMIT 1
        "#;
        let range_rows = tx.query(range_sql, &[&record.seq_id, &target_date]).await?;
        let (r_id, r_date_from) = if let Some(r_row) = range_rows.first() {
            let r_id: i64 = r_row.try_get(0).map_err(|e| Error::db(e.to_string()))?;
            let r_date: Date = r_row.try_get(2).map_err(|e| Error::db(e.to_string()))?;
            (r_id, r_date)
        } else {
            // Lock parent sequence row to serialize concurrent date range creations.
            // The lock makes concurrent creators take turns. Under REPEATABLE READ,
            // a concurrent creator that committed after our snapshot is not visible to
            // the re-check, and the speculative INSERT ... ON CONFLICT below turns the
            // race into a 40001 serialization error that Orm::run retries.
            tx.execute(
                "SELECT id FROM ir_sequence WHERE id = $1 FOR UPDATE",
                &[&record.seq_id],
            )
            .await?;

            // Re-check after obtaining parent row lock
            let range_rows_recheck = tx.query(range_sql, &[&record.seq_id, &target_date]).await?;
            if let Some(r_row) = range_rows_recheck.first() {
                let r_id: i64 = r_row.try_get(0).map_err(|e| Error::db(e.to_string()))?;
                let r_date: Date = r_row.try_get(2).map_err(|e| Error::db(e.to_string()))?;
                (r_id, r_date)
            } else {
                let (year, _, _) = target_date.ymd();
                let mut d_from =
                    Date::from_ymd(year, 1, 1).ok_or_else(|| Error::validation("invalid date"))?;
                let mut d_to = Date::from_ymd(year, 12, 31)
                    .ok_or_else(|| Error::validation("invalid date"))?;

                let prev_sql = r#"
                    SELECT date_to FROM ir_sequence_date_range
                    WHERE sequence_id = $1 AND date_to >= $2 AND date_to <= $3
                    ORDER BY date_to DESC LIMIT 1
                "#;
                let prev_rows = tx
                    .query(prev_sql, &[&record.seq_id, &d_from, &target_date])
                    .await?;
                if let Some(prow) = prev_rows.first() {
                    let prev_to: Date = prow.try_get(0).map_err(|e| Error::db(e.to_string()))?;
                    let days = prev_to
                        .days()
                        .checked_add(1)
                        .ok_or_else(|| Error::validation("date overflow"))?;
                    d_from = Date::from_days(days);
                }

                let next_sql = r#"
                    SELECT date_from FROM ir_sequence_date_range
                    WHERE sequence_id = $1 AND date_from >= $2 AND date_from <= $3
                    ORDER BY date_from DESC LIMIT 1
                "#;
                let next_rows = tx
                    .query(next_sql, &[&record.seq_id, &target_date, &d_to])
                    .await?;
                if let Some(nrow) = next_rows.first() {
                    let next_from: Date = nrow.try_get(0).map_err(|e| Error::db(e.to_string()))?;
                    let days = next_from
                        .days()
                        .checked_sub(1)
                        .ok_or_else(|| Error::validation("date overflow"))?;
                    d_to = Date::from_days(days);
                }

                let ins_rows = tx
                    .query(
                        r#"
                        INSERT INTO ir_sequence_date_range (sequence_id, date_from, date_to, number_next)
                        VALUES ($1, $2, $3, $4)
                        ON CONFLICT (sequence_id, date_from, date_to) DO NOTHING
                        RETURNING id
                        "#,
                        &[&record.seq_id, &d_from, &d_to, &record.number_next],
                    )
                    .await?;
                let ins_row = ins_rows.first().ok_or_else(|| {
                    let seq_id = record.seq_id;
                    Error::db(format!(
                        "INSERT INTO ir_sequence_date_range conflict for sequence {seq_id} on ({d_from}, {d_to}) but range does not contain target date {target_date}"
                    ))
                })?;
                let r_id: i64 = ins_row.try_get(0).map_err(|e| Error::db(e.to_string()))?;
                if record.implementation == "standard" {
                    let name = SequenceName::DateRange(record.seq_id, r_id);
                    create_backing_sequence(tx, name, record.number_increment, record.number_next)
                        .await?;
                }
                (r_id, d_from)
            }
        };

        let drawn = if record.implementation == "standard" {
            let name = SequenceName::DateRange(record.seq_id, r_id);
            let res = tx
                .query(
                    "SELECT nextval($1::text::regclass)",
                    &[&name.regclass_arg()],
                )
                .await?;
            let res_row = res
                .first()
                .ok_or_else(|| Error::db("nextval returned no rows"))?;
            let n: i64 = res_row.try_get(0).map_err(|e| Error::db(e.to_string()))?;
            i32::try_from(n).map_err(|_| {
                Error::validation(format!("Sequence number overflow: {n} exceeds i32 bounds"))
            })?
        } else {
            let current = lock_number_next(
                tx,
                LockTarget::DateRange(r_id),
                effective_wait_mode,
                &record,
            )
            .await?;
            let next = current
                .checked_add(record.number_increment)
                .ok_or_else(|| Error::validation("Sequence number overflow"))?;
            tx.execute(
                "UPDATE ir_sequence_date_range SET number_next = $1 WHERE id = $2",
                &[&next, &r_id],
            )
            .await?;
            current
        };
        (drawn, r_date_from)
    } else {
        let drawn = if record.implementation == "standard" {
            let name = SequenceName::Standard(record.seq_id);
            let res = tx
                .query(
                    "SELECT nextval($1::text::regclass)",
                    &[&name.regclass_arg()],
                )
                .await?;
            let res_row = res
                .first()
                .ok_or_else(|| Error::db("nextval returned no rows"))?;
            let n: i64 = res_row.try_get(0).map_err(|e| Error::db(e.to_string()))?;
            i32::try_from(n).map_err(|_| {
                Error::validation(format!("Sequence number overflow: {n} exceeds i32 bounds"))
            })?
        } else {
            let current =
                lock_number_next(tx, LockTarget::Sequence, effective_wait_mode, &record).await?;
            let next = current
                .checked_add(record.number_increment)
                .ok_or_else(|| Error::validation("Sequence number overflow"))?;
            tx.execute(
                "UPDATE ir_sequence SET number_next = $1 WHERE id = $2",
                &[&next, &record.seq_id],
            )
            .await?;
            current
        };
        (drawn, target_date)
    };

    // Format number with padding
    let padded = format!("{:0width$}", num, width = record.padding as usize);

    // Format prefix and suffix directives
    let current_clock_date = clock();
    let vars = build_interpolation_vars(target_date, range_date, current_clock_date)?;
    let prefix = match record.prefix_tpl {
        Some(ref tpl) => interpolate_template(tpl, &vars, &record.seq_name)?,
        None => String::new(),
    };
    let suffix = match record.suffix_tpl {
        Some(ref tpl) => interpolate_template(tpl, &vars, &record.seq_name)?,
        None => String::new(),
    };

    Ok(format!("{prefix}{padded}{suffix}"))
}

async fn next_by_code_with_clock_and_wait_mode<F>(
    tx: &mut Transaction,
    env: &Env,
    code: &str,
    sequence_date: Option<Date>,
    clock: F,
    override_wait_mode: Option<SequenceWaitMode>,
) -> Result<Option<String>>
where
    F: Fn() -> Date,
{
    // validated against the subject's newest resolution, also under sudo (review S1)
    let company_id = tx.companies(env).await?.first().map(|c| c.get());

    let sql = r#"
        SELECT id, implementation, prefix, suffix, number_next, number_increment, padding, use_date_range, name, COALESCE(wait_mode, 'nowait')
        FROM ir_sequence
        WHERE code = $1 AND (company_id = $2 OR company_id IS NULL)
        ORDER BY company_id ASC, id ASC
        LIMIT 1
    "#;

    let rows = tx.query(sql, &[&code, &company_id]).await?;
    let Some(row) = rows.first() else {
        return Ok(None);
    };

    let record = SequenceRecordData::from_row(row)?;
    let drawn =
        draw_sequence_record(tx, env, record, sequence_date, clock, override_wait_mode).await?;
    Ok(Some(drawn))
}

async fn next_by_id_with_clock_and_wait_mode<F>(
    tx: &mut Transaction,
    env: &Env,
    sequence_id: i64,
    sequence_date: Option<Date>,
    clock: F,
    override_wait_mode: Option<SequenceWaitMode>,
) -> Result<Option<String>>
where
    F: Fn() -> Date,
{
    let sql = r#"
        SELECT id, implementation, prefix, suffix, number_next, number_increment, padding, use_date_range, name, COALESCE(wait_mode, 'nowait')
        FROM ir_sequence
        WHERE id = $1
    "#;

    let rows = tx.query(sql, &[&sequence_id]).await?;
    let Some(row) = rows.first() else {
        return Ok(None);
    };

    let record = SequenceRecordData::from_row(row)?;
    let drawn =
        draw_sequence_record(tx, env, record, sequence_date, clock, override_wait_mode).await?;
    Ok(Some(drawn))
}

/// Draw the next number for sequence code.
pub async fn next_by_code(
    tx: &mut Transaction,
    env: &Env,
    code: &str,
    sequence_date: Option<Date>,
) -> Result<Option<String>> {
    next_by_code_with_clock_and_wait_mode(tx, env, code, sequence_date, default_clock, None).await
}

/// Draw the next number for sequence code with an injected clock function.
pub async fn next_by_code_with_clock(
    tx: &mut Transaction,
    env: &Env,
    code: &str,
    sequence_date: Option<Date>,
    clock: impl Fn() -> Date,
) -> Result<Option<String>> {
    next_by_code_with_clock_and_wait_mode(tx, env, code, sequence_date, clock, None).await
}

/// Draw the next number for sequence code overriding the sequence's declared wait mode.
pub async fn next_by_code_with_wait_mode(
    tx: &mut Transaction,
    env: &Env,
    code: &str,
    sequence_date: Option<Date>,
    wait_mode: SequenceWaitMode,
) -> Result<Option<String>> {
    next_by_code_with_clock_and_wait_mode(
        tx,
        env,
        code,
        sequence_date,
        default_clock,
        Some(wait_mode),
    )
    .await
}

/// Draw the next number for a sequence record id.
pub async fn next_by_id(
    tx: &mut Transaction,
    env: &Env,
    sequence_id: i64,
    sequence_date: Option<Date>,
) -> Result<Option<String>> {
    next_by_id_with_clock_and_wait_mode(tx, env, sequence_id, sequence_date, default_clock, None)
        .await
}

/// Draw the next number for a sequence record id overriding the declared wait mode.
pub async fn next_by_id_with_wait_mode(
    tx: &mut Transaction,
    env: &Env,
    sequence_id: i64,
    sequence_date: Option<Date>,
    wait_mode: SequenceWaitMode,
) -> Result<Option<String>> {
    next_by_id_with_clock_and_wait_mode(
        tx,
        env,
        sequence_id,
        sequence_date,
        default_clock,
        Some(wait_mode),
    )
    .await
}
