//! When the rows of each model last changed in a transaction (PERF.STREAM-WINDOW review H1/DB 1,
//! round 2 findings 1, 2 and 4; design note `perf-stream-window.md` §1.2).
//!
//! A cursor reads the snapshot of its `DECLARE`, also for what its own transaction writes later.
//! A stream remembers the tick at its `DECLARE`; once a model whose cells a window caches (or
//! whose fields its record rule reads) changed after it, the stream takes only the ids from the
//! cursor and reads the values fresh by id. The tick moves for:
//! - every statement that may change rows ([`classify`]): an `UPDATE`, `DELETE`, `INSERT` or
//!   `MERGE` names its table, which moves the models [`TableModels`] maps it to (the models stored
//!   in it, the models delegating to them through `_inherits`, both sides of a many2many link
//!   table, and for a `DELETE` or `MERGE` every model the database changes through `ON DELETE
//!   CASCADE` or `SET NULL`, followed transitively, each with the models delegating to it); a
//!   table the map does not know, and anything else that may change data (a data-changing `WITH`,
//!   `TRUNCATE`, `COPY`, `CALL`, `DO`, DDL, a multi-statement batch, an `EXPLAIN ANALYZE` of a
//!   write, a raw `ROLLBACK`), moves every model;
//! - every invalidation (of a model, of records, of everything) and every rollback to a
//!   savepoint (every model).
//!
//! Not seen: a `SELECT` calling a function that writes, and a trigger writing another table (the
//! same limit as the isolated scopes' classifier, `needs_isolation`). Code that installs such a
//! trigger or calls such a function notifies changed scalar columns with `Records::modified_external` after settling prior ORM work,
//! or invalidates what it changed (`Transaction::invalidate_model`) when no dependency propagation
//! is needed.

use erp_fields::FieldType;
use erp_registry::{ModelId, Registry};
use rustc_hash::{FxHashMap, FxHashSet};

use super::sql_words::SqlWords;
/// The write ticks of one transaction object (never reset: the ticks only grow, and a stream
/// never outlives its transaction).
#[derive(Debug, Default)]
pub(crate) struct WriteGens {
    /// The latest tick handed out.
    tick: u64,
    /// The tick of the latest change that may have touched any model.
    any: u64,
    /// The tick of the latest change of each model (by model index; 0: never).
    models: Vec<u64>,
}

impl WriteGens {
    /// The latest tick: what changes after now has a larger one.
    pub(crate) fn now(&self) -> u64 {
        self.tick
    }

    /// Rows of `model` may have changed.
    pub(crate) fn model_written(&mut self, model: ModelId) {
        self.mark(model.index());
    }

    fn mark(&mut self, at: usize) {
        self.tick += 1;
        if self.models.len() <= at {
            self.models.resize(at + 1, 0);
        }
        self.models[at] = self.tick;
    }

    /// Rows of any model may have changed.
    pub(crate) fn all_written(&mut self) {
        self.tick += 1;
        self.any = self.tick;
    }

    /// The tick of the latest change that may have touched every model.
    #[cfg(feature = "test-faults")]
    pub(crate) fn any_tick(&self) -> u64 {
        self.any
    }

    /// Whether a row of one of `models` may have changed after tick `at`.
    pub(crate) fn moved_since(&self, at: u64, models: &[ModelId]) -> bool {
        self.moved(at, models.iter().map(|m| m.index()))
    }

    fn moved(&self, at: u64, mut indices: impl Iterator<Item = usize>) -> bool {
        self.any > at || indices.any(|i| self.models.get(i).is_some_and(|tick| *tick > at))
    }

    /// Whether any row may have changed after tick `at`.
    pub(crate) fn anything_since(&self, at: u64) -> bool {
        self.tick > at
    }
}

/// The table a statement writes, as written in it (unquoted names compare case-insensitively).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Table<'a> {
    name: &'a str,
    quoted: bool,
}

/// What a statement may change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Target<'a> {
    /// No row: a read, a cursor statement, a transaction or session statement.
    Reads,
    /// Rows of this table; `delete`: rows may be deleted (`DELETE`, `MERGE`), so the database's
    /// `ON DELETE` actions may change other tables.
    Rows { table: Table<'a>, delete: bool },
    /// Rows of tables it does not name in a way this reader follows.
    Unknown,
}

/// One statement as the transaction's bookkeeping sees it, read once ([`classify`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Classified<'a> {
    /// The rows it may change.
    pub(crate) target: Target<'a>,
    /// `INSERT`, `UPDATE`, `DELETE`, `MERGE`, or a `WITH` with one of the first three (the
    /// transaction's mutation count).
    pub(crate) data_change: bool,
    /// Whether the remembered record-rule decisions end with it (review round 3, finding 8): a
    /// statement whose target is not [`Target::Reads`], or one that locks rows (`FOR UPDATE`,
    /// `FOR NO KEY UPDATE`, `FOR SHARE`, `FOR KEY SHARE`). `None` for a `SELECT` or a `DECLARE`:
    /// the caller reads the statement for a lock clause ([`locks_rows`]) only when decisions are
    /// remembered.
    pub(crate) clears_decisions: Option<bool>,
}

/// Heads of statements that change no row, lock none and leave the session's settings alone
/// (`ROLLBACK` and `ABORT` are not here: a `ROLLBACK TO SAVEPOINT` reverts rows, so it is unknown,
/// round 3, finding 2; `SET` and `RESET` are read on their own, round 4, finding 5).
const READ_HEADS: [&str; 18] = [
    "SHOW",
    "SAVEPOINT",
    "RELEASE",
    "BEGIN",
    "START",
    "COMMIT",
    "END",
    "FETCH",
    "MOVE",
    "CLOSE",
    "LOCK",
    "LISTEN",
    "UNLISTEN",
    "NOTIFY",
    "DEALLOCATE",
    "PREPARE",
    "VALUES",
    "TABLE",
];

impl Classified<'static> {
    /// A statement that may change any row: every model moves and the decisions end.
    pub(crate) const UNKNOWN: Self = Self {
        target: Target::Unknown,
        data_change: false,
        clears_decisions: Some(true),
    };
}

/// What `sql` (one statement) may change, whether it is a data change, and whether it ends the
/// remembered record-rule decisions: see the module documentation. One pass over the words at
/// most (a `WITH` or `EXPLAIN`, stopping once the outcome is known), none for the others;
/// nothing allocated.
///
/// Fail closed (round 4): a `WITH` or `EXPLAIN` the word scanner does not read with certainty is
/// unknown. A `SELECT` or `DECLARE` is not read past its head: PostgreSQL allows a data-modifying
/// statement only at the top level of a `WITH`, so nothing a misread could hide in a `SELECT`
/// changes a row (a function that writes is not detectable either way); its lock clause is read
/// only when decisions are remembered ([`locks_rows`], which counts an uncertain statement as
/// locking). An `INSERT`, `UPDATE`, `DELETE` or `MERGE` names its table up front, read by
/// [`table`].
pub(crate) fn classify(sql: &str) -> Classified<'_> {
    let mut words = SqlWords::new(sql);
    let Some(head) = words.next() else {
        return Classified::UNKNOWN;
    };
    if head.quoted {
        // a quoted word is never a statement's keyword
        return Classified::UNKNOWN;
    }
    let is = |w: &str, k: &str| w.eq_ignore_ascii_case(k);
    let (head, rest) = (head.text, &sql[head.end..]);
    if is(head, "SELECT") || is(head, "DECLARE") {
        // a read, or a cursor over one; it may lock rows, which the caller reads only when
        // decisions are remembered
        return Classified {
            target: Target::Reads,
            data_change: false,
            clears_decisions: None,
        };
    }
    if READ_HEADS.iter().any(|k| is(head, k)) {
        return Classified {
            target: Target::Reads,
            data_change: false,
            clears_decisions: Some(false),
        };
    }
    if is(head, "SET") || is(head, "RESET") {
        // `SET CONSTRAINTS … IMMEDIATE` fires deferred triggers, which may write; any other
        // setting may change what later SQL reads (`search_path`), so the decisions end
        if is(head, "SET")
            && words
                .next()
                .is_some_and(|w| !w.quoted && is(w.text, "CONSTRAINTS"))
        {
            return Classified::UNKNOWN;
        }
        return Classified {
            target: Target::Reads,
            data_change: false,
            clears_decisions: Some(true),
        };
    }
    let explain = is(head, "EXPLAIN");
    if explain || is(head, "WITH") {
        let (mut dml, mut other, mut locks, mut analyze) = (false, false, false, false);
        // the row-lock clause being read (`FOR UPDATE`, `FOR NO KEY UPDATE`, `FOR SHARE`, `FOR
        // KEY SHARE`): its words are not writes; one `UPDATE` ends it, so the `UPDATE` of a
        // statement after it is read again. It starts only on an unquoted `FOR` that follows
        // neither `AS` (round 4, finding 3: `AS for` is a column label) nor a `.` (round 5, M1:
        // `x.for` and `(x).for` are a column and a field)
        let mut clause = Lock::None;
        let mut after_as = false;
        let mut decided = false;
        for word in words.by_ref() {
            let w = if word.quoted { "" } else { word.text };
            let next = match clause {
                Lock::For if is(w, "UPDATE") || is(w, "SHARE") => Some(Lock::None),
                Lock::For if is(w, "NO") => Some(Lock::No),
                Lock::For if is(w, "KEY") => Some(Lock::KeyShare),
                Lock::No if is(w, "KEY") => Some(Lock::NoKey),
                Lock::NoKey if is(w, "UPDATE") => Some(Lock::None),
                Lock::KeyShare if is(w, "SHARE") => Some(Lock::None),
                _ => None,
            };
            if let Some(next) = next {
                locks = true;
                clause = next;
                after_as = false;
                continue;
            }
            clause = if is(w, "FOR") && !after_as && !word.after_dot {
                Lock::For
            } else {
                Lock::None
            };
            after_as = is(w, "AS");
            // a quoted option is the option (round 5, M2: PostgreSQL compares `"analyze"` too)
            if explain && (is(word.text, "ANALYZE") || is(word.text, "ANALYSE")) {
                analyze = true;
            }
            // a quoted word counts here: it can only make the statement a write
            if ["INSERT", "UPDATE", "DELETE"]
                .iter()
                .any(|k| is(word.text, k))
            {
                dml = true;
            } else if ["MERGE", "TRUNCATE", "COPY"]
                .iter()
                .any(|k| is(word.text, k))
            {
                other = true;
            }
            // the outcome is known (round 3, finding 5): a `WITH` changing data, or an
            // `EXPLAIN ANALYZE` (its option comes first) running a write
            if (!explain && dml) || (analyze && (dml || other)) {
                decided = true;
                break;
            }
        }
        if !decided && words.uncertain() {
            // not read with certainty: it may hide a write (round 4, fail closed)
            return Classified {
                data_change: !explain,
                ..Classified::UNKNOWN
            };
        }
        // an `EXPLAIN` runs its statement only with `ANALYZE`
        let runs = !explain || analyze;
        let writes = (dml || other) && runs;
        return Classified {
            target: if writes {
                Target::Unknown
            } else {
                Target::Reads
            },
            data_change: !explain && dml,
            clears_decisions: Some(writes || (locks && runs)),
        };
    }
    let (after, delete) = if is(head, "UPDATE") {
        (Some(rest), false)
    } else if is(head, "DELETE") {
        (keyword(rest, "FROM"), true)
    } else if is(head, "INSERT") {
        (keyword(rest, "INTO"), false)
    } else if is(head, "MERGE") {
        (keyword(rest, "INTO"), true)
    } else {
        return Classified::UNKNOWN;
    };
    Classified {
        target: match after.and_then(table) {
            Some(table) => Target::Rows { table, delete },
            None => Target::Unknown,
        },
        data_change: true,
        clears_decisions: Some(true),
    }
}

/// Where [`classify`] is in a row-lock clause.
#[derive(Debug, Clone, Copy)]
enum Lock {
    /// Not in one.
    None,
    /// After `FOR`.
    For,
    /// After `FOR NO`.
    No,
    /// After `FOR NO KEY`.
    NoKey,
    /// After `FOR KEY`.
    KeyShare,
}

/// The words that make `FOR <word>` a row-lock clause.
const LOCK_WORDS: [&str; 4] = ["UPDATE", "NO", "SHARE", "KEY"];

/// Whether `sql` may hold a row-lock clause (`FOR UPDATE`, `FOR NO KEY UPDATE`, `FOR SHARE`, `FOR
/// KEY SHARE`), judged on the words outside comments and literals; a statement the scanner does
/// not read with certainty counts as locking (round 4, fail closed). A false positive only ends
/// the remembered decisions. (Under the transactions' `REPEATABLE READ` a row lock never changes
/// what a statement sees; ending the decisions on one keeps the rule "the decisions end when rows
/// change or are locked" simple, round 4, finding 5.)
pub(crate) fn locks_rows(sql: &str) -> bool {
    let mut words = SqlWords::new(sql);
    let mut after_for = false;
    for word in words.by_ref() {
        let w = word.text;
        if after_for && LOCK_WORDS.iter().any(|k| w.eq_ignore_ascii_case(k)) {
            return true;
        }
        after_for = w.eq_ignore_ascii_case("FOR");
    }
    words.uncertain()
}

/// A character of an unquoted name: PostgreSQL's identifier class (`[A-Za-z0-9_$\200-\377]`:
/// ASCII letters and digits, `_`, `$` and every non-ASCII character), the same as the word
/// scanner's (round 4, finding 2: `t€x` is one name).
fn is_name_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == '$' || !c.is_ascii()
}

/// The first word of `s` (a whole unquoted name) after leading blanks, and what follows it.
fn word(s: &str) -> (&str, &str) {
    let s = s.trim_start();
    let end = s.find(|c: char| !is_name_char(c)).unwrap_or(s.len());
    s.split_at(end)
}

/// What follows keyword `kw` at the start of `s`, `None` when `s` does not start with it as a
/// whole word (`only_items` is a name, not `ONLY`, review round 3, finding 4).
fn keyword<'a>(s: &'a str, kw: &str) -> Option<&'a str> {
    let (w, rest) = word(s);
    w.eq_ignore_ascii_case(kw).then_some(rest)
}

/// The table named at the start of `s` (after an optional `ONLY`; the last part of a qualified
/// name), `None` for anything else (a comment, a quoted name with an escaped quote).
fn table(s: &str) -> Option<Table<'_>> {
    let s = keyword(s, "ONLY").unwrap_or(s).trim_start();
    let (mut found, mut rest) = ident(s)?;
    while let Some(next) = rest.strip_prefix('.') {
        (found, rest) = ident(next)?;
    }
    Some(found)
}

/// One identifier at the start of `s` and what follows it; an unquoted one is a whole name
/// (`tablé`, not `tabl`).
fn ident(s: &str) -> Option<(Table<'_>, &str)> {
    if let Some(quoted) = s.strip_prefix('"') {
        let end = quoted.find('"')?;
        let (name, rest) = quoted.split_at(end);
        let rest = &rest[1..];
        if rest.starts_with('"') {
            // an escaped quote: not followed
            return None;
        }
        return Some((Table { name, quoted: true }, rest));
    }
    let (name, rest) = word(s);
    if name.eq_ignore_ascii_case("u") && rest.starts_with('&') {
        // `U&"…"`: Unicode escapes, not read (round 4, finding 4)
        return None;
    }
    (!name.is_empty() && name.len() == s.len() - rest.len()).then_some((
        Table {
            name,
            quoted: false,
        },
        rest,
    ))
}

/// The models whose rows a write to each table may change, built once per registry (round 2
/// findings 1, 2 and 4): a lookup per statement instead of a scan of every model.
#[derive(Debug, Default)]
pub(crate) struct TableModels {
    by_table: FxHashMap<Box<str>, Changed>,
}

/// What a write to one table changes.
#[derive(Debug, Default)]
struct Changed {
    /// By an `INSERT` or `UPDATE`: the models stored in the table, the models delegating to them
    /// (`_inherits`, transitively), and both sides of a many2many whose link table it is.
    written: Box<[ModelId]>,
    /// By a `DELETE` or `MERGE`: `written`, and every model the database changes through `ON
    /// DELETE`: rows of a model whose stored many2one into a deleted model is `cascade` (deleted
    /// in turn, followed transitively) or `set null`, and both sides of every many2many whose
    /// link rows go with a deleted record.
    deleted: Box<[ModelId]>,
}

impl TableModels {
    /// The map of `registry`'s tables.
    pub(crate) fn build(registry: &Registry) -> Self {
        let models = registry.models();
        let id_of = |name: &str| registry.model_id(name);
        // a model's delegating children (`_inherits`), its referrers by many2one with their
        // `ON DELETE`, and the models sharing a many2many link with it (itself included)
        let mut children: FxHashMap<ModelId, Vec<ModelId>> = FxHashMap::default();
        let mut referrers: FxHashMap<ModelId, Vec<(ModelId, erp_fields::OnDelete)>> =
            FxHashMap::default();
        let mut linked: FxHashMap<ModelId, Vec<ModelId>> = FxHashMap::default();
        let mut written: FxHashMap<&str, FxHashSet<ModelId>> = FxHashMap::default();
        // the models whose own rows a table holds (a link table has none)
        let mut owners: FxHashMap<&str, Vec<ModelId>> = FxHashMap::default();
        for spec in models {
            for (parent, _) in &spec.inherits {
                children.entry(*parent).or_default().push(spec.id);
            }
            if !spec.table.is_empty() {
                written
                    .entry(spec.table.as_str())
                    .or_default()
                    .insert(spec.id);
                owners.entry(spec.table.as_str()).or_default().push(spec.id);
            }
            for field in &spec.fields {
                match &field.ty {
                    FieldType::Many2one { comodel, ondelete } if field.store => {
                        if let Some(target) = id_of(comodel) {
                            referrers
                                .entry(target)
                                .or_default()
                                .push((spec.id, *ondelete));
                        }
                    }
                    FieldType::Many2many {
                        comodel,
                        relation: Some(relation),
                        ..
                    } => {
                        let mut sides = vec![spec.id];
                        sides.extend(id_of(comodel));
                        let link = written.entry(relation.table.as_str()).or_default();
                        link.extend(sides.iter().copied());
                        for side in &sides {
                            linked
                                .entry(*side)
                                .or_default()
                                .extend(sides.iter().copied());
                        }
                    }
                    _ => {}
                }
            }
        }
        // a table's models reach the models delegating to them, transitively
        let mut by_table = FxHashMap::default();
        for (table, own) in written {
            let mut reached: FxHashSet<ModelId> = FxHashSet::default();
            let mut queue: Vec<ModelId> = own.into_iter().collect();
            while let Some(m) = queue.pop() {
                if reached.insert(m) {
                    queue.extend(children.get(&m).into_iter().flatten().copied());
                }
            }
            let written_models: Vec<ModelId> = sorted(&reached);
            // `ON DELETE`, from the models whose own rows the table holds (deleting link rows
            // deletes no record): deleted models cascade to their referrers; `set null` changes
            // the referrer's rows without deleting them; link rows go with either side
            let mut changed = reached;
            let mut deleted_models: FxHashSet<ModelId> = FxHashSet::default();
            let mut queue: Vec<ModelId> = owners.get(table).cloned().unwrap_or_default();
            // every model changed brings the models delegating to it, transitively (round 3,
            // finding 3): their delegated fields read its rows
            while let Some(m) = queue.pop() {
                if !deleted_models.insert(m) {
                    continue;
                }
                // a deleted record's delegating children go with it (their delegate many2one
                // cascades); counted here too, whatever the field says
                add_family(&mut changed, m, &children);
                for (referrer, ondelete) in referrers.get(&m).into_iter().flatten() {
                    match ondelete {
                        erp_fields::OnDelete::Cascade => queue.push(*referrer),
                        erp_fields::OnDelete::SetNull => {
                            add_family(&mut changed, *referrer, &children);
                        }
                        // the delete is refused while a row refers to it: nothing changes
                        erp_fields::OnDelete::Restrict => {}
                    }
                }
                for side in linked.get(&m).into_iter().flatten() {
                    add_family(&mut changed, *side, &children);
                }
            }
            by_table.insert(
                Box::from(table),
                Changed {
                    written: written_models.into(),
                    deleted: sorted(&changed).into(),
                },
            );
        }
        Self { by_table }
    }

    /// The models a write to `table` may change (`delete`: rows may be deleted), `None` for a
    /// table no model of the registry knows (the caller moves every model). Nothing allocated.
    pub(crate) fn models(&self, table: Table<'_>, delete: bool) -> Option<&[ModelId]> {
        let changed = if table.quoted || !table.name.bytes().any(|b| b.is_ascii_uppercase()) {
            self.by_table.get(table.name)
        } else {
            // an unquoted name folds to lower case (ASCII only, as PostgreSQL does for UTF-8;
            // raw SQL only: the ORM quotes its names), on the stack (round 3, finding 6). A name
            // longer than the 64-byte buffer is not looked up: unknown, every model moves (fail
            // closed). PostgreSQL cuts names to 63 bytes; a raw name longer than that misses the
            // registry's names here, so it is unknown too.
            let mut buf = [0u8; 64];
            let name = table.name.as_bytes();
            let folded = buf.get_mut(..name.len())?;
            for (to, from) in folded.iter_mut().zip(name) {
                *to = from.to_ascii_lowercase();
            }
            // lowering ASCII bytes keeps UTF-8 valid; a failure would be unknown, fail closed
            self.by_table.get(std::str::from_utf8(folded).ok()?)
        }?;
        Some(if delete {
            &changed.deleted
        } else {
            &changed.written
        })
    }
}

/// Add `model` and the models delegating to it (`_inherits`), transitively, to `set`.
fn add_family(
    set: &mut FxHashSet<ModelId>,
    model: ModelId,
    children: &FxHashMap<ModelId, Vec<ModelId>>,
) {
    let mut queue = vec![model];
    while let Some(m) = queue.pop() {
        if set.insert(m) {
            queue.extend(children.get(&m).into_iter().flatten().copied());
        }
    }
}

fn sorted(set: &FxHashSet<ModelId>) -> Vec<ModelId> {
    let mut v: Vec<ModelId> = set.iter().copied().collect();
    v.sort_unstable();
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rows_of(sql: &str) -> Option<(String, bool, bool)> {
        match classify(sql).target {
            Target::Rows { table, delete } => Some((table.name.to_owned(), table.quoted, delete)),
            _ => None,
        }
    }

    /// The ORM's writes and common raw writes name their table (a `DELETE` or `MERGE` may
    /// delete); reads and cursor statements change nothing; anything else may change any table.
    /// Data changes and whether decisions end come from the same single reading.
    #[test]
    fn statements_name_what_they_may_change() {
        for (sql, table, quoted, delete) in [
            (
                "UPDATE \"bench_order_line\" SET \"qty\" = $1 WHERE id = $2",
                "bench_order_line",
                true,
                false,
            ),
            (
                "update ONLY public.res_partner set x = 1",
                "res_partner",
                false,
                false,
            ),
            (
                "  DELETE FROM \"public\".\"bench_order\" WHERE id = ANY($1)",
                "bench_order",
                true,
                true,
            ),
            (
                "INSERT INTO bench_tag (code) VALUES ($1)",
                "bench_tag",
                false,
                false,
            ),
            (
                "MERGE INTO t USING s ON t.id = s.id WHEN MATCHED THEN DELETE",
                "t",
                false,
                true,
            ),
            ("-- comment\nUPDATE t SET x = 1", "t", false, false),
            // round 3, finding 4: whole words and whole names
            ("UPDATE only_items SET x = 1", "only_items", false, false),
            (
                "UPDATE ONLY only_items SET x = 1",
                "only_items",
                false,
                false,
            ),
            ("UPDATE tablé SET x = 1", "tablé", false, false),
            ("INSERT INTO t$1 VALUES (1)", "t$1", false, false),
            // round 4, finding 2: PostgreSQL's identifier class
            ("UPDATE t€x SET x = 1", "t€x", false, false),
        ] {
            assert_eq!(
                rows_of(sql),
                Some((table.to_owned(), quoted, delete)),
                "{sql}"
            );
            let class = classify(sql);
            assert!(class.data_change, "{sql}");
            assert_eq!(class.clears_decisions, Some(true), "{sql}");
        }
        for (sql, clears) in [
            ("SELECT id FROM bench_order FOR UPDATE", None),
            (
                "DECLARE erp_c1 NO SCROLL CURSOR WITHOUT HOLD FOR SELECT 1",
                None,
            ),
            ("SHOW TimeZone", Some(false)),
            ("FETCH FORWARD 2000 FROM erp_c1", Some(false)),
            // round 4, finding 5: a raw setting ends the decisions
            ("SET LOCAL cursor_tuple_fraction = 1", Some(true)),
            ("set search_path = other", Some(true)),
            ("RESET search_path", Some(true)),
            ("CLOSE erp_c1", Some(false)),
            ("SAVEPOINT \"erp_sp_1\"", Some(false)),
            ("RELEASE SAVEPOINT \"erp_sp_1\"", Some(false)),
            ("LOCK TABLE t", Some(false)),
            ("WITH t AS (SELECT 1) SELECT * FROM t", Some(false)),
            ("EXPLAIN SELECT 1", Some(false)),
            ("EXPLAIN ANALYZE SELECT 1", Some(false)),
            ("EXPLAIN DELETE FROM t", Some(false)),
            ("EXPLAIN SELECT 1 FROM t FOR UPDATE", Some(false)),
            ("WITH t AS (SELECT 1 FROM x FOR SHARE) SELECT 1", Some(true)),
            ("EXPLAIN ANALYZE SELECT 1 FROM t FOR UPDATE", Some(true)),
            // the words of a lock clause are not writes
            (
                "WITH a AS (SELECT 1 FROM x FOR NO KEY UPDATE) SELECT 1",
                Some(true),
            ),
            (
                "WITH a AS (SELECT 1 FROM x FOR UPDATE OF x SKIP LOCKED) SELECT 1",
                Some(true),
            ),
        ] {
            let class = classify(sql);
            assert_eq!(class.target, Target::Reads, "{sql}");
            assert!(!class.data_change, "{sql}");
            assert_eq!(class.clears_decisions, clears, "{sql}");
        }
        for (sql, data_change) in [
            (
                "WITH moved AS (UPDATE bench_order SET name = 'x' RETURNING id) SELECT * FROM moved",
                true,
            ),
            ("TRUNCATE bench_order", false),
            ("CALL refresh()", false),
            ("DO $$ BEGIN UPDATE t SET x = 1; END $$", false),
            ("UPDATE \"we\"\"ird\" SET x = 1", true),
            ("UPDATE /* c */ t SET x = 1", true),
            ("EXPLAIN ANALYZE UPDATE t SET x = 1", false),
            // a statement after a lock clause is read again
            (
                "WITH a AS (SELECT id FROM t FOR UPDATE) UPDATE t SET x = 1 FROM a",
                true,
            ),
            (
                "WITH a AS (SELECT id FROM t FOR NO KEY UPDATE) UPDATE t SET x = 1",
                true,
            ),
            ("ALTER TABLE t ADD COLUMN x int", false),
            // round 3, finding 2: a rollback reverts rows
            ("ROLLBACK TO SAVEPOINT s", false),
            ("rollback", false),
            ("ABORT", false),
            // round 4: deferred triggers may write; a `U&` name is not read
            ("SET CONSTRAINTS ALL IMMEDIATE", false),
            ("UPDATE u&\"t\" SET x = 1", true),
            ("\"select\" 1", false),
            // round 4, finding 3: `AS for` is a label, not a lock clause
            ("WITH a AS (SELECT 1 AS for) UPDATE t SET x = 1", true),
            ("WITH a AS (SELECT 1 FROM \"for\") UPDATE t SET x = 1", true),
            (
                "WITH a AS (SELECT 1 FROM t FOR UPDATE OF \"for\") UPDATE t SET x = 1",
                true,
            ),
            // round 5, M1: after a dot, `for` is a field or column name
            (
                "WITH a AS (SELECT 1 FROM (SELECT true AS \"for\") x WHERE ((x).for)) UPDATE t SET v = 1",
                true,
            ),
            (
                "WITH a AS (SELECT x.for FROM x ORDER BY x.for) UPDATE t SET v = 1",
                true,
            ),
            (
                "WITH a AS (SELECT 1 FROM x ORDER BY x .\n /* c */ for) UPDATE t SET v = 1",
                true,
            ),
            (
                "WITH a AS (SELECT 1 FROM x ORDER BY x.\u{0B}for) UPDATE t SET v = 1",
                true,
            ),
            // round 5, M2: a quoted EXPLAIN option is the option
            ("EXPLAIN (\"analyze\") DELETE FROM t", false),
            ("EXPLAIN (\"analyse\", VERBOSE) UPDATE t SET v = 1", false),
            // round 4, fail closed: not read with certainty, so it may hide a write
            (r"WITH x AS (SELECT 'a\'), y AS (SELECT 1) SELECT 1", true),
            (r#"WITH x AS (SELECT U&"d\0061ta" FROM t) SELECT 1"#, true),
            ("WITH x AS (SELECT 1 /* open", true),
            ("EXPLAIN SELECT 'open", false),
            ("", false),
        ] {
            let class = classify(sql);
            assert_eq!(class.target, Target::Unknown, "{sql}");
            assert_eq!(class.data_change, data_change, "{sql}");
            assert_eq!(class.clears_decisions, Some(true), "{sql}");
        }
        // round 5, M1: a lock clause after a column named `for` is still one
        let class = classify("WITH a AS (SELECT x.for FROM x FOR UPDATE) SELECT 1");
        assert_eq!(class.target, Target::Reads);
        assert_eq!(class.clears_decisions, Some(true));
        // a keyword inside a literal or comment is not a data change
        let class = classify("WITH t AS (SELECT 'DELETE me' /* UPDATE */) SELECT * FROM t");
        assert_eq!(class.target, Target::Reads);
        assert!(!class.data_change);
        // round 3, finding 1: a quoted name, an escaped quote, a nested comment or a dollar tag
        // with digits hides no write
        for sql in [
            r#"WITH x AS (SELECT 1 AS "a'b"), d AS (DELETE FROM t RETURNING 1) SELECT 'z' FROM d"#,
            r"WITH x AS (SELECT E'\'' AS a), d AS (DELETE FROM t RETURNING 1) SELECT 'z' FROM d",
            "WITH x AS (SELECT 1 /* /* */ ' */), d AS (DELETE FROM t RETURNING 1) SELECT 'z' FROM d",
            "WITH x AS (SELECT $a1$ ' $a1$ AS v), d AS (DELETE FROM t RETURNING 1) SELECT 'z' FROM d",
        ] {
            let class = classify(sql);
            assert_eq!(class.target, Target::Unknown, "{sql}");
            assert!(class.data_change, "{sql}");
        }
    }

    /// Round 4, lead decision: for a corpus of statements that hide a write behind each trick of
    /// the reviews (quoted names holding `'`, `--` or `/*`; `E'\''`; nested comments; dollar tags
    /// with digits; `--` ended by `\r`; names with `€` or `$` next to a dollar tag; `AS for`;
    /// `U&` names; backslashes in plain strings; unterminated text), `classify` never returns
    /// `Reads` and `needs_isolation` always holds; for the same tricks hiding a session-state
    /// call, the guard names the function or the scanner refuses the statement as uncertain.
    #[test]
    fn no_trick_turns_a_write_or_a_session_call_into_a_read() {
        // (opens, closes): text before and after the hidden part that a scanner misreading the
        // trick would take for one string or comment
        let tricks: [(&str, &str); 12] = [
            (r#"1 AS "a'b""#, "'z'"),
            (r#"1 AS "a--b""#, "1"),
            (r#"1 AS "a/*b""#, "1 */"),
            (r#"1 AS "a""'b""#, "'z'"),
            (r"E'\''", "'z'"),
            ("1 /* /* */ ' */", "'z'"),
            ("$a1$ ' $a1$", "'z'"),
            ("1 --\r", "1"),
            ("1 AS €$q$", "1 AS z$q$"),
            ("1 AS €$$", "1 AS z$$"),
            (r"'a\'", "'z'"),
            (r#"U&"d\0061ta""#, "1"),
        ];
        for (opens, closes) in tricks {
            for sql in [
                format!(
                    "WITH x AS (SELECT {opens}), d AS (DELETE FROM t RETURNING 1) SELECT {closes} \
                     FROM d"
                ),
                format!("WITH x AS (SELECT {opens}) UPDATE t SET x = 1 RETURNING {closes}"),
            ] {
                assert_ne!(classify(&sql).target, Target::Reads, "{sql}");
                assert!(super::super::needs_isolation(&sql), "{sql}");
            }
            // an `EXPLAIN ANALYZE` needs isolation whatever follows: only its class is in question
            let explain = format!(
                "EXPLAIN ANALYZE WITH x AS (SELECT {opens}) DELETE FROM t RETURNING {closes}"
            );
            assert_ne!(classify(&explain).target, Target::Reads, "{explain}");
            // statements that start as reads (round 5, L3): the write behind the trick decides
            for sql in [
                format!("SELECT {opens}; DELETE FROM t; SELECT {closes}"),
                format!("SELECT {opens}, 2 INTO TEMP t2 FROM x WHERE {closes} IS NOT NULL"),
                format!("SELECT {opens} FROM x FOR UPDATE; SELECT {closes}"),
            ] {
                assert!(super::super::needs_isolation(&sql), "{sql}");
            }
            let call = format!("SELECT {opens}, pg_advisory_lock(1), {closes}");
            assert!(
                super::super::session_state_function(&call).is_some()
                    || !super::super::sql_words::certain(&call),
                "{call}"
            );
        }
        for sql in [
            "WITH a AS (SELECT 1 AS for) UPDATE t SET x = 1",
            "WITH a AS (SELECT 1 AS FOR) DELETE FROM t",
            "WITH x AS (SELECT 1) --\rDELETE FROM t",
            // round 5, M1 and M2
            "WITH a AS (SELECT 1 FROM (SELECT true AS \"for\") x WHERE ((x).for)) UPDATE t SET v = 1",
            "WITH a AS (SELECT x.for FROM x ORDER BY x.for) UPDATE t SET v = 1",
            "EXPLAIN (\"analyze\") DELETE FROM t",
        ] {
            assert_ne!(classify(sql).target, Target::Reads, "{sql}");
            assert!(super::super::needs_isolation(sql), "{sql}");
        }
    }

    /// A lock clause, judged on words: `FOR UPDATE`, `FOR NO KEY UPDATE`, `FOR SHARE`, `FOR KEY
    /// SHARE`; not inside a literal, nor `FOR SELECT` of a cursor.
    #[test]
    fn lock_clauses_are_found_on_words() {
        for sql in [
            "SELECT id FROM t FOR UPDATE",
            "select id from t for no key update skip locked",
            "SELECT id FROM t FOR SHARE",
            "SELECT id FROM t\nFOR\tKEY SHARE",
            "DECLARE c CURSOR FOR SELECT id FROM t FOR UPDATE",
            // not read with certainty: counted as locking
            r"SELECT 'a\' FROM t",
        ] {
            assert!(locks_rows(sql), "{sql}");
        }
        for sql in [
            "SELECT 'for update' FROM t",
            "SELECT id FROM t -- FOR UPDATE",
            "DECLARE c NO SCROLL CURSOR WITHOUT HOLD FOR SELECT 1",
            "SELECT 1",
        ] {
            assert!(!locks_rows(sql), "{sql}");
        }
    }

    /// The table lookup (review round 2, findings 1 and 2): a table reaches its model and the
    /// models delegating to it; a link table both sides; a `DELETE` of a model's table also the
    /// `cascade` referrers (transitively), the `set null` referrers (not further: their rows stay),
    /// never a `restrict` referrer, and the models sharing a link with a deleted model; deleting link
    /// rows deletes no record. Every model reached brings the models delegating to it (round 3,
    /// finding 3: also a `set null` referrer's and a link side's). Unknown tables are `None`; an
    /// unquoted name folds to lower case without allocating, and a name longer than PostgreSQL
    /// keeps is unknown (round 3, finding 6).
    #[test]
    fn the_table_lookup_follows_links_delegation_and_on_delete() {
        use erp_fields::OnDelete;
        use erp_registry::{AddonDescriptor, FieldDecl, ModelDecl, RegistryBuilder};
        static ADDON: AddonDescriptor = AddonDescriptor {
            name: "tm",
            depends: &["base"],
            exact_only: false,
        };
        static A: [FieldDecl; 2] = [
            FieldDecl::char("name"),
            FieldDecl::many2many("tag_ids", "tm.tag").relation("tm_a_tag_rel", "a_id", "tag_id"),
        ];
        static B: [FieldDecl; 1] =
            [FieldDecl::many2one("a_id", "tm.a").ondelete(OnDelete::Cascade)];
        static C: [FieldDecl; 2] = [
            FieldDecl::many2one("b_id", "tm.b").ondelete(OnDelete::SetNull),
            FieldDecl::many2one("keep_id", "tm.a").ondelete(OnDelete::Restrict),
        ];
        static D: [FieldDecl; 1] =
            [FieldDecl::many2one("c_id", "tm.c").ondelete(OnDelete::Cascade)];
        static E: [FieldDecl; 1] =
            [FieldDecl::many2one("b_id", "tm.b").ondelete(OnDelete::Restrict)];
        static TAG: [FieldDecl; 1] = [FieldDecl::char("name")];
        static CHILD: [FieldDecl; 1] = [FieldDecl::many2one("a_id", "tm.a")
            .delegate(true)
            .required(true)
            .ondelete(OnDelete::Cascade)];
        let decl = |name: &'static str, fields: &'static [FieldDecl]| -> &'static ModelDecl {
            Box::leak(Box::new(ModelDecl::new("tm", name).fields(fields)))
        };
        static C_CHILD: [FieldDecl; 1] = [FieldDecl::many2one("c_id", "tm.c")
            .delegate(true)
            .required(true)
            .ondelete(OnDelete::Cascade)];
        static TAG_CHILD: [FieldDecl; 1] = [FieldDecl::many2one("tag_id", "tm.tag")
            .delegate(true)
            .required(true)
            .ondelete(OnDelete::Cascade)];
        let delegating = |name: &'static str,
                          fields: &'static [FieldDecl],
                          parent: &'static [(&'static str, &'static str)]|
         -> &'static ModelDecl {
            Box::leak(Box::new(
                ModelDecl::new("tm", name).fields(fields).inherits(parent),
            ))
        };
        let registry = RegistryBuilder::new([&ADDON])
            .linked(false)
            .with_decls([
                decl("tm.a", &A),
                decl("tm.b", &B),
                decl("tm.c", &C),
                decl("tm.d", &D),
                decl("tm.e", &E),
                decl("tm.tag", &TAG),
                delegating("tm.child", &CHILD, &[("tm.a", "a_id")]),
                delegating("tm.c_child", &C_CHILD, &[("tm.c", "c_id")]),
                delegating("tm.tag_child", &TAG_CHILD, &[("tm.tag", "tag_id")]),
            ])
            .build()
            .unwrap();
        let lookup = TableModels::build(&registry);
        let id = |model: &str| registry.model(model).unwrap().id;
        let ids = |models: &[&str]| {
            let mut v: Vec<ModelId> = models.iter().map(|m| id(m)).collect();
            v.sort_unstable();
            v
        };
        let models = |name: &str, quoted: bool, delete: bool| {
            lookup
                .models(Table { name, quoted }, delete)
                .map(<[ModelId]>::to_vec)
        };
        assert_eq!(
            models("tm_a", false, false),
            Some(ids(&["tm.a", "tm.child"]))
        );
        assert_eq!(
            models("tm_a", false, true),
            Some(ids(&[
                "tm.a",
                "tm.b",
                "tm.c",
                "tm.c_child",
                "tm.child",
                "tm.tag",
                "tm.tag_child"
            ])),
            "b cascades, c is set null through b (with its delegating child), d and the restrict \
             referrers stay, the delegating child goes, the tags lose their links"
        );
        assert_eq!(
            models("tm_b", false, true),
            Some(ids(&["tm.b", "tm.c", "tm.c_child"]))
        );
        assert_eq!(models("tm_b", false, false), Some(ids(&["tm.b"])));
        assert_eq!(
            models("tm_c", false, false),
            Some(ids(&["tm.c", "tm.c_child"]))
        );
        let link = Some(ids(&["tm.a", "tm.child", "tm.tag", "tm.tag_child"]));
        assert_eq!(models("tm_a_tag_rel", false, false), link);
        assert_eq!(
            models("tm_a_tag_rel", false, true),
            link,
            "no record is deleted"
        );
        assert_eq!(models("TM_B", false, false), Some(ids(&["tm.b"])));
        assert_eq!(
            models("TM_B", true, false),
            None,
            "a quoted name keeps its case"
        );
        assert_eq!(models("Tm_B", false, true), models("tm_b", false, true));
        let long = format!("TM_{}", "x".repeat(70));
        assert_eq!(
            models(&long, false, false),
            None,
            "longer than a name can be"
        );
        assert_eq!(models("tm_view", false, true), None);
    }

    /// A tick moves the models written and, for an unattributed write, every model.
    #[test]
    fn ticks_follow_the_writes() {
        let mut gens = WriteGens::default();
        let at = gens.now();
        assert!(!gens.moved(at, [0, 3].into_iter()));
        gens.mark(3);
        assert!(gens.moved(at, [3].into_iter()));
        assert!(!gens.moved(at, [0, 7].into_iter()));
        assert!(gens.anything_since(at));
        let at = gens.now();
        gens.all_written();
        assert!(gens.moved(at, [0].into_iter()));
    }
}
