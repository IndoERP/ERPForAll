//! The SQL cursors of a transaction's streams (PERF.STREAM-WINDOW, design note
//! "Closing", `perf-stream-window.md` §1).
//!
//! A cursor stream declares `erp_c<n>`, `n` the lowest number no cursor of the transaction holds
//! (so the `DECLARE` text, and its prepared statement, repeat for every stream of a transaction
//! and across transactions; numbering a fresh name per stream would fill the connection's
//! statement cache with one `DECLARE` per stream), and keeps a [`CursorHandle`]; the transaction
//! keeps the matching entry. A stream takes its number after the pending `CLOSE`s were taken for
//! its first flight, where they go out in front of its `DECLARE`, so the number of a cursor closed
//! in that flight is free again. The two share one atomic state:
//! - a cursor is *declaring* until its first flight answers (`DECLARING` → `OPEN`); a handle
//!   dropped while declaring (a cancelled first flight) is never closed, its number stays taken;
//! - the stream asks for its cursor to be closed when it is exhausted, closed or dropped
//!   (`OPEN` → `CLOSING`; dropping the handle does it, so a dropped stream needs no transaction);
//! - the transaction sends `CLOSE` for every `CLOSING` cursor in front of its next statement
//!   ([`Cursors::take_closing`]); the numbers wait in flight until the statement confirms the
//!   `CLOSE` (free again), the server refuses it, or the statement's future is dropped (both:
//!   kept as *unsure* until the transaction ends, or until a rollback to a savepoint opened before
//!   the cursor's `DECLARE` closes it on the server); a `CLOSE` never sent goes back to `CLOSING`;
//! - `COMMIT`, `ROLLBACK` and a `ROLLBACK TO SAVEPOINT` under which a cursor was declared end it
//!   on the server; the transaction marks it `ENDED` and sends nothing. A rollback to a
//!   savepoint opened before a `FETCH` does not move the cursor back: the cursor is marked
//!   rewound and its stream refuses to go on.
//!
//! A transaction holds at most [`DEFAULT_CURSOR_LIMIT`] cursors at once unless set otherwise
//! (`Transaction::set_cursor_limit`), and sends `SET LOCAL cursor_tuple_fraction = 1` once (again
//! only after a rollback undid it).
//!
//! The transaction object has an owner id and every transaction on it (each `BEGIN`) a serial,
//! both unique in the process: a stream remembers them and refuses another transaction object
//! (owner) or a later transaction on its own (serial).

use std::fmt::Write as _;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};

use erp_fields::{Error, Result};

/// The first flight carrying the cursor's `DECLARE` has not answered yet: whether the server ran
/// it is unknown, so the cursor is never closed from this state (dropped here, its number stays
/// taken until the transaction ends).
const DECLARING: u8 = 0;
/// The cursor is open and its stream may fetch from it.
const OPEN: u8 = 1;
/// The stream is done with it: `CLOSE` rides the transaction's next statement.
const CLOSING: u8 = 2;
/// The cursor no longer exists on the server (closed, or its transaction or savepoint ended).
const ENDED: u8 = 3;
/// The handle was dropped while its `DECLARE` was in flight (a cancelled first flight).
const ABANDONED: u8 = 4;

/// The open cursors a transaction allows by default ([`Cursors::set_limit`]): each holds a server
/// portal and, for a sorted query, up to `work_mem` of executor state for the transaction's life.
pub(crate) const DEFAULT_CURSOR_LIMIT: usize = 32;

/// Source of transaction serials and owner ids (0 is never handed out).
static SERIALS: AtomicU64 = AtomicU64::new(1);

fn fresh_serial() -> u64 {
    SERIALS.fetch_add(1, Ordering::Relaxed)
}

/// Whose transaction a stream reads: the transaction object and the transaction on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TxStamp {
    /// The transaction object (unique in the process, fixed for its life).
    pub(crate) owner: u64,
    /// The transaction on it (changes at every commit, rollback or release).
    pub(crate) serial: u64,
}

/// What a stream and the transaction share about one cursor.
#[derive(Debug)]
struct Shared {
    state: AtomicU8,
    /// A `FETCH` ran inside a savepoint that was rolled back since: the server does not move the
    /// cursor back, so the window it returned cannot be fetched again.
    rewound: AtomicBool,
}

/// One cursor as the transaction sees it.
#[derive(Debug)]
struct Entry {
    number: u32,
    /// The savepoint depth it was declared at: a rollback to a savepoint opened at or above this
    /// depth closes it on the server.
    depth: usize,
    /// The savepoint depth of its last `FETCH` (lowered by a release like `depth`).
    fetched_at: usize,
    shared: Arc<Shared>,
}

impl Entry {
    fn state(&self) -> u8 {
        self.shared.state.load(Ordering::Acquire)
    }

    fn end(&self) {
        self.shared.state.store(ENDED, Ordering::Release);
    }
}

/// The cursors of one transaction and its serial.
#[derive(Debug)]
pub(crate) struct Cursors {
    owner: u64,
    serial: u64,
    open: Vec<Entry>,
    /// `CLOSE`s taken for a statement whose outcome is not known yet: `(number, depth)`. Confirmed
    /// ([`Cursors::closes_confirmed`]), refused ([`Cursors::closes_refused`]) or requeued
    /// ([`Cursors::closes_requeued`]) by the statement; still here at the next statement, the
    /// statement's future was dropped, so the cursors may still exist.
    in_flight: Vec<(u32, usize)>,
    /// Numbers whose cursor may still exist on the server with nobody to close it (a refused
    /// `CLOSE`, a dropped statement or first flight), with the savepoint depth it was declared
    /// at: not taken again before the transaction ends or a rollback past that depth closes it,
    /// and counted against the limit.
    unsure: Vec<(u32, usize)>,
    /// The savepoint depth at which `SET LOCAL cursor_tuple_fraction = 1` was sent in this
    /// transaction (`None`: not sent, or undone by a rollback).
    fraction_at: Option<usize>,
    /// Most cursors open at once ([`DEFAULT_CURSOR_LIMIT`]).
    limit: usize,
}

/// The `CLOSE`s taken for one statement: one simple-query text and the cursor numbers.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Closes {
    /// `CLOSE erp_c1; CLOSE erp_c3`.
    pub(crate) sql: String,
    /// The numbers closed, in the text's order.
    pub(crate) numbers: Vec<u32>,
}

impl Default for Cursors {
    fn default() -> Self {
        Self {
            owner: fresh_serial(),
            serial: fresh_serial(),
            open: Vec::new(),
            in_flight: Vec::new(),
            unsure: Vec::new(),
            fraction_at: None,
            limit: DEFAULT_CURSOR_LIMIT,
        }
    }
}

/// A stream's half of a cursor: its number and the shared state. Dropping it asks for the
/// cursor to be closed (or, while its `DECLARE` is in flight, leaves the number taken).
#[derive(Debug)]
pub(crate) struct CursorHandle {
    number: u32,
    shared: Arc<Shared>,
}

impl CursorHandle {
    /// The cursor number (`erp_c<number>`).
    pub(crate) fn number(&self) -> u32 {
        self.number
    }

    /// Whether the cursor no longer exists (its transaction or savepoint ended it, or its
    /// `CLOSE` was sent).
    pub(crate) fn is_ended(&self) -> bool {
        self.shared.state.load(Ordering::Acquire) == ENDED
    }

    /// Whether a `FETCH` of this cursor was undone by a rollback to a savepoint (the cursor kept
    /// its position).
    pub(crate) fn is_rewound(&self) -> bool {
        self.shared.rewound.load(Ordering::Acquire)
    }

    /// The first flight answered and the server ran the `DECLARE`: the cursor is open.
    pub(crate) fn declared(&self) {
        // only a handle still declaring becomes open; an ended one (its transaction ended
        // meanwhile) stays ended
        let _ = self.shared.state.compare_exchange(
            DECLARING,
            OPEN,
            Ordering::AcqRel,
            Ordering::Acquire,
        );
    }

    /// The cursor was never declared (its first flight failed before the server ran the
    /// `DECLARE`, or the transaction is aborted and ends it): it is forgotten without a `CLOSE`.
    pub(crate) fn forget(&self) {
        self.shared.state.store(ENDED, Ordering::Release);
    }

    /// Ask for the `CLOSE` (in front of the transaction's next statement). Nothing happens to a
    /// cursor that is declaring, already closing or ended.
    pub(crate) fn request_close(&self) {
        // a failed exchange means the cursor is not open: nothing to ask for
        let _ =
            self.shared
                .state
                .compare_exchange(OPEN, CLOSING, Ordering::AcqRel, Ordering::Acquire);
    }
}

impl Drop for CursorHandle {
    fn drop(&mut self) {
        // dropped while declaring: whether the cursor exists is unknown, never `CLOSE` it
        if self
            .shared
            .state
            .compare_exchange(DECLARING, ABANDONED, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            self.request_close();
        }
    }
}

impl Cursors {
    /// The transaction now running (or about to run) on this object.
    pub(crate) fn stamp(&self) -> TxStamp {
        TxStamp {
            owner: self.owner,
            serial: self.serial,
        }
    }

    /// Whether a cursor of this transaction may still be read by a stream (declared, not closed
    /// or ended yet): only then do the write ticks need the models a statement names.
    pub(crate) fn any_open(&self) -> bool {
        !self.open.is_empty()
    }

    /// Allow `limit` cursors open at once (at least 1).
    pub(crate) fn set_limit(&mut self, limit: usize) -> Result<()> {
        if limit == 0 {
            return Err(Error::programming(
                "Transaction::set_cursor_limit: the limit is at least 1 (a stream holds one cursor)",
            ));
        }
        self.limit = limit;
        Ok(())
    }

    /// Bookkeeping left by a statement or a first flight that never finished, at the start of
    /// the next statement: `CLOSE`s still in flight keep their numbers as unsure, then
    /// [`Cursors::sweep`].
    fn settle(&mut self) {
        self.unsure.append(&mut self.in_flight);
        self.sweep();
    }

    /// Ended entries go; handles dropped while declaring keep their numbers as unsure.
    fn sweep(&mut self) {
        let unsure = &mut self.unsure;
        self.open.retain(|entry| match entry.state() {
            ENDED => false,
            ABANDONED => {
                unsure.push((entry.number, entry.depth));
                false
            }
            _ => true,
        });
    }

    /// Register a cursor declared at savepoint depth `depth`, numbered with the lowest number no
    /// cursor of the transaction holds (a cursor whose `CLOSE` is still to send holds its number);
    /// it is declaring until [`CursorHandle::declared`]. Called after the flight's `CLOSE`s were
    /// taken ([`Cursors::take_closing`], which settled what earlier statements left): the cursors
    /// they close go out in front of the `DECLARE`, so they neither count against the limit nor
    /// hold their numbers.
    ///
    /// # Errors
    ///
    /// `Programming` when the transaction already has its limit of open cursors.
    pub(crate) fn declare(&mut self, depth: usize) -> Result<CursorHandle> {
        self.sweep();
        let live = self.open.len() + self.unsure.len();
        if live >= self.limit {
            return Err(Error::programming(format!(
                "RowsStream: this transaction already has {live} open SQL cursors, its limit \
                 (Transaction::set_cursor_limit, default {DEFAULT_CURSOR_LIMIT}); finish or close \
                 a stream before starting another"
            )));
        }
        let mut number = 1u32;
        while self.open.iter().any(|e| e.number == number)
            || self.unsure.iter().any(|(n, _)| *n == number)
        {
            number = number.saturating_add(1);
        }
        let shared = Arc::new(Shared {
            state: AtomicU8::new(DECLARING),
            rewound: AtomicBool::new(false),
        });
        self.open.push(Entry {
            number,
            depth,
            fetched_at: depth,
            shared: Arc::clone(&shared),
        });
        Ok(CursorHandle { number, shared })
    }

    /// Cursor `number` returned a window at savepoint depth `depth`.
    ///
    /// # Errors
    ///
    /// `Programming` when no cursor of that number is registered (an internal error: a stream
    /// fetches only from the cursor it declared, and refuses once it ended).
    pub(crate) fn fetched(&mut self, number: u32, depth: usize) -> Result<()> {
        debug_assert!(
            self.open.iter().any(|e| e.number == number),
            "cursor erp_c{number} fetched but not registered"
        );
        self.note_fetch(number, depth)
    }

    /// [`Cursors::fetched`] without the debug assertion: the release builds' path (review
    /// round 3, finding 9: tested on its own).
    fn note_fetch(&mut self, number: u32, depth: usize) -> Result<()> {
        match self.open.iter_mut().find(|e| e.number == number) {
            Some(entry) => {
                entry.fetched_at = depth;
                Ok(())
            }
            None => Err(Error::programming(format!(
                "cursor erp_c{number} returned a window but is not registered in its transaction \
                 (internal error in erp_orm::transaction::cursors)"
            ))),
        }
    }

    /// The `CLOSE` statements of every cursor whose stream is done with it, as one simple-query
    /// text (`CLOSE erp_c1; CLOSE erp_c3`), and their numbers; the cursors are marked ended for
    /// their streams and their numbers wait in flight for the statement's outcome. `None` when
    /// nothing is to close.
    pub(crate) fn take_closing(&mut self) -> Option<Closes> {
        self.settle();
        if !self.open.iter().any(|e| e.state() == CLOSING) {
            return None;
        }
        let mut sql = String::new();
        let mut numbers = Vec::new();
        let in_flight = &mut self.in_flight;
        self.open.retain(|entry| {
            if entry.state() != CLOSING {
                return true;
            }
            entry.end();
            if !numbers.is_empty() {
                sql.push_str("; ");
            }
            // writing into a `String` cannot fail
            let _ = write!(sql, "CLOSE erp_c{}", entry.number);
            numbers.push(entry.number);
            in_flight.push((entry.number, entry.depth));
            false
        });
        Some(Closes { sql, numbers })
    }

    /// The server ran the `CLOSE`s of `numbers`: their numbers are free.
    pub(crate) fn closes_confirmed(&mut self, numbers: &[u32]) {
        self.in_flight.retain(|(n, _)| !numbers.contains(n));
    }

    /// The server refused the `CLOSE`s of `numbers` (the transaction is aborted): those cursors
    /// may still exist, so their numbers are not taken again before the transaction ends.
    pub(crate) fn closes_refused(&mut self, numbers: &[u32]) {
        let unsure = &mut self.unsure;
        self.in_flight.retain(|(n, depth)| {
            if numbers.contains(n) {
                unsure.push((*n, *depth));
                false
            } else {
                true
            }
        });
    }

    /// The `CLOSE`s of `numbers` were never sent (the statement failed before its executions went
    /// out): the cursors still exist and are closed in front of a later statement (after the
    /// rollback to a savepoint that recovers the transaction, which ends those declared under it).
    pub(crate) fn closes_requeued(&mut self, numbers: &[u32]) {
        let open = &mut self.open;
        self.in_flight.retain(|(number, depth)| {
            if !numbers.contains(number) {
                return true;
            }
            open.push(Entry {
                number: *number,
                depth: *depth,
                fetched_at: *depth,
                shared: Arc::new(Shared {
                    state: AtomicU8::new(CLOSING),
                    rewound: AtomicBool::new(false),
                }),
            });
            false
        });
    }

    /// Whether this transaction still has to send `SET LOCAL cursor_tuple_fraction = 1`.
    pub(crate) fn fraction_needed(&self) -> bool {
        self.fraction_at.is_none()
    }

    /// `SET LOCAL cursor_tuple_fraction = 1` ran at savepoint depth `depth`.
    pub(crate) fn fraction_sent(&mut self, depth: usize) {
        self.fraction_at = Some(depth);
    }

    /// The transaction ended (`COMMIT`, `ROLLBACK`, a released connection) or a new one begins:
    /// every cursor is gone with it, numbering starts again, the planner setting is undone and
    /// the serial changes.
    pub(crate) fn end_all(&mut self) {
        for entry in self.open.drain(..) {
            entry.end();
        }
        self.in_flight.clear();
        self.unsure.clear();
        self.fraction_at = None;
        self.serial = fresh_serial();
    }

    /// A `ROLLBACK TO SAVEPOINT` left `depth` savepoints open: cursors declared deeper are
    /// closed on the server, a `FETCH` made deeper is not undone (its cursor is marked rewound),
    /// and a `SET LOCAL` made deeper is undone.
    pub(crate) fn rolled_back_to(&mut self, depth: usize) {
        self.open.retain_mut(|entry| {
            if entry.depth > depth {
                entry.end();
                return false;
            }
            if entry.fetched_at > depth {
                entry.shared.rewound.store(true, Ordering::Release);
                entry.fetched_at = depth;
            }
            true
        });
        // the server closed the cursors declared under the savepoint, unsure ones included
        self.unsure.retain(|(_, declared)| *declared <= depth);
        if self.fraction_at.is_some_and(|at| at > depth) {
            self.fraction_at = None;
        }
    }

    /// A `RELEASE SAVEPOINT` left `depth` savepoints open: cursors declared deeper (and the
    /// planner setting) now belong to the enclosing level and survive a later rollback of a
    /// savepoint opened after it.
    pub(crate) fn released_to(&mut self, depth: usize) {
        for entry in &mut self.open {
            entry.depth = entry.depth.min(depth);
            entry.fetched_at = entry.fetched_at.min(depth);
        }
        for (_, declared) in &mut self.unsure {
            *declared = (*declared).min(depth);
        }
        if let Some(at) = &mut self.fraction_at {
            *at = (*at).min(depth);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opened(cursors: &mut Cursors, depth: usize) -> CursorHandle {
        let handle = cursors.declare(depth).unwrap();
        handle.declared();
        handle
    }

    /// Exhausted or dropped handles close in front of the next statement, once; a rollback to a
    /// savepoint ends the cursors declared under it without a `CLOSE`, a release moves them to
    /// the enclosing level; the end of the transaction ends the rest and renumbers.
    #[test]
    fn closing_follows_the_handles_and_the_savepoints() {
        let mut cursors = Cursors::default();
        let stamp = cursors.stamp();
        let first = opened(&mut cursors, 0);
        let second = opened(&mut cursors, 1);
        let third = opened(&mut cursors, 1);
        assert_eq!((first.number(), second.number(), third.number()), (1, 2, 3));
        assert_eq!(cursors.take_closing(), None);
        first.request_close();
        drop(third);
        let closes = cursors.take_closing().unwrap();
        assert_eq!(
            closes,
            Closes {
                sql: "CLOSE erp_c1; CLOSE erp_c3".to_owned(),
                numbers: vec![1, 3]
            }
        );
        assert!(first.is_ended());
        cursors.closes_confirmed(&closes.numbers);
        assert_eq!(cursors.take_closing(), None, "closed once");
        // the numbers of closed cursors are taken again, lowest first; a closing one keeps its own
        second.request_close();
        let fourth = opened(&mut cursors, 1);
        assert_eq!(fourth.number(), 1);
        assert_eq!(opened(&mut cursors, 1).number(), 3);
        let closes = cursors.take_closing().unwrap();
        assert_eq!(closes.numbers, [2, 3]);
        cursors.closes_confirmed(&closes.numbers);
        // released from depth 1 to 0, then a new savepoint at depth 1 rolled back: the released
        // cursor survives, the one declared after it ends without a `CLOSE`
        cursors.released_to(0);
        let fifth = opened(&mut cursors, 1);
        assert_eq!(fifth.number(), 2);
        cursors.rolled_back_to(0);
        assert!(!fourth.is_ended());
        assert!(fifth.is_ended());
        drop(fifth);
        assert_eq!(
            cursors.take_closing(),
            None,
            "an ended cursor is never closed"
        );
        // a forgotten cursor is never closed and frees its number
        let sixth = opened(&mut cursors, 0);
        assert_eq!(sixth.number(), 2);
        sixth.forget();
        drop(sixth);
        assert_eq!(cursors.take_closing(), None);
        assert_eq!(opened(&mut cursors, 0).number(), 2);
        cursors.end_all();
        assert!(fourth.is_ended());
        assert_eq!(cursors.stamp().owner, stamp.owner);
        assert_ne!(cursors.stamp().serial, stamp.serial);
        assert_eq!(opened(&mut cursors, 0).number(), 1);
    }

    /// Review M1, DB 5: a `CLOSE` refused or left in flight by a dropped statement keeps its
    /// number unsure until the end; one never sent is sent again; a handle dropped while
    /// declaring is never closed and keeps its number.
    #[test]
    fn unconfirmed_closes_and_abandoned_declares_keep_their_numbers() {
        let mut cursors = Cursors::default();
        let a = opened(&mut cursors, 0);
        let b = opened(&mut cursors, 0);
        let c = opened(&mut cursors, 0);
        drop(a);
        let refused = cursors.take_closing().unwrap();
        cursors.closes_refused(&refused.numbers);
        drop(b);
        let unsent = cursors.take_closing().unwrap();
        cursors.closes_requeued(&unsent.numbers);
        assert_eq!(
            cursors.take_closing().unwrap().numbers,
            [2],
            "a CLOSE never sent goes out with the next statement"
        );
        // that statement's future was dropped: the next statement settles it as unsure
        assert_eq!(cursors.take_closing(), None);
        let d = opened(&mut cursors, 0);
        assert_eq!(d.number(), 4);
        drop(c);
        drop(d);
        let declaring = cursors.declare(0).unwrap();
        assert_eq!(declaring.number(), 5);
        drop(declaring);
        let closes = cursors.take_closing().unwrap();
        assert_eq!(
            closes.numbers,
            [3, 4],
            "the abandoned declare is never closed"
        );
        cursors.closes_confirmed(&closes.numbers);
        // 1 refused, 2 dropped in flight, 5 abandoned: unsure; 3 and 4 confirmed are free again
        let handles: Vec<CursorHandle> = (0..3).map(|_| opened(&mut cursors, 0)).collect();
        let numbers: Vec<u32> = handles.iter().map(CursorHandle::number).collect();
        assert_eq!(numbers, [3, 4, 6]);
    }

    /// Review DB 3, DB 4, DB 6: a rollback past a `FETCH` marks its cursor rewound (not past one
    /// made at the depth kept, nor one released into the enclosing level); the planner setting
    /// is sent once and again after a rollback undid it; the limit counts open and unsure
    /// cursors.
    #[test]
    fn rewind_planner_setting_and_limit() {
        let mut cursors = Cursors::default();
        let outer = opened(&mut cursors, 0);
        cursors.fetched(outer.number(), 1).unwrap();
        cursors.rolled_back_to(1);
        assert!(
            !outer.is_rewound(),
            "a FETCH at the depth kept is not undone"
        );
        cursors.rolled_back_to(0);
        assert!(outer.is_rewound());
        let released = opened(&mut cursors, 0);
        cursors.fetched(released.number(), 1).unwrap();
        cursors.released_to(0);
        cursors.rolled_back_to(0);
        assert!(!released.is_rewound());

        assert!(cursors.fraction_needed());
        cursors.fraction_sent(1);
        assert!(!cursors.fraction_needed());
        cursors.rolled_back_to(0);
        assert!(cursors.fraction_needed(), "undone with its savepoint");
        cursors.fraction_sent(1);
        cursors.released_to(0);
        cursors.rolled_back_to(0);
        assert!(
            !cursors.fraction_needed(),
            "released into the enclosing level"
        );
        cursors.end_all();
        assert!(cursors.fraction_needed());

        cursors.set_limit(2).unwrap();
        let _one = opened(&mut cursors, 0);
        let two = cursors.declare(0).unwrap();
        let refused = cursors.declare(0).unwrap_err();
        assert_eq!(
            refused.message(),
            "RowsStream: this transaction already has 2 open SQL cursors, its limit \
             (Transaction::set_cursor_limit, default 32); finish or close a stream before \
             starting another"
        );
        drop(two);
        assert!(
            cursors.declare(0).is_err(),
            "an abandoned declare stays counted"
        );
        assert!(cursors.set_limit(0).is_err());
    }

    /// Round 2 nit 7: an unsure number keeps the depth its cursor was declared at; a rollback
    /// past it frees the number (the server closed the cursor), a release moves it to the
    /// enclosing level.
    #[test]
    fn unsure_numbers_go_with_their_savepoint() {
        let mut cursors = Cursors::default();
        let outer = opened(&mut cursors, 0);
        // declared at depth 2, abandoned (a cancelled first flight): unsure at depth 2
        let inner = cursors.declare(2).unwrap();
        assert_eq!(inner.number(), 2);
        drop(inner);
        let third = cursors.declare(1).unwrap();
        assert_eq!(third.number(), 3, "the unsure number stays taken");
        drop(third);
        // released into depth 1, then rolled back to depth 0: freed
        cursors.released_to(1);
        cursors.rolled_back_to(1);
        assert_eq!(
            opened(&mut cursors, 0).number(),
            4,
            "still taken at depth 1"
        );
        cursors.rolled_back_to(0);
        let again = opened(&mut cursors, 0);
        assert_eq!(again.number(), 2, "freed by the rollback past its declare");
        assert_eq!(outer.number(), 1);
    }

    /// Round 2 nit 8: a window of a cursor that is not registered is an internal error (a
    /// `Programming` error in release builds).
    #[test]
    #[cfg(debug_assertions)]
    #[should_panic(expected = "cursor erp_c99 fetched but not registered")]
    fn a_window_of_an_unregistered_cursor_is_an_internal_error() {
        let mut cursors = Cursors::default();
        let _ = cursors.fetched(99, 0);
    }

    /// Review round 3, finding 9: the release builds' path of `fetched` returns a `Programming`
    /// error naming the cursor for an unregistered number, and records the depth of a registered
    /// one.
    #[test]
    fn the_release_path_of_an_unregistered_window_is_a_programming_error() {
        let mut cursors = Cursors::default();
        let err = cursors.note_fetch(99, 0).unwrap_err();
        assert_eq!(err.kind(), erp_fields::ErrorKind::Programming);
        assert_eq!(
            err.message(),
            "cursor erp_c99 returned a window but is not registered in its transaction (internal \
             error in erp_orm::transaction::cursors)"
        );
        let handle = cursors.declare(0).unwrap();
        cursors.note_fetch(handle.number(), 2).unwrap();
        assert_eq!(
            cursors
                .open
                .iter()
                .find(|e| e.number == handle.number())
                .map(|e| e.fetched_at),
            Some(2)
        );
    }
}
