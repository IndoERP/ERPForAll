//! At most one `warn` line a second for one kind of event (security review round 3 R3-d); the
//! counter next to each limiter carries the number. One limiter per kind of line: the two
//! settings expiries (round 4 C4-5) and each retried step (round 4 C4-4) have their own, so a
//! line of one kind never hides a line of another.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

/// The rate limit of one kind of `warn` line: the first event of each second is written, the
/// others only counted by the caller.
#[derive(Debug)]
pub(crate) struct WarnLimit {
    since: Instant,
    /// The last second a line was admitted in, plus one (0: none yet).
    last: AtomicU64,
}

impl Default for WarnLimit {
    fn default() -> Self {
        Self::new()
    }
}

impl WarnLimit {
    pub(crate) fn new() -> Self {
        Self {
            since: Instant::now(),
            last: AtomicU64::new(0),
        }
    }

    /// Whether a line may be written now.
    pub(crate) fn admit(&self) -> bool {
        self.admit_at(self.since.elapsed().as_secs())
    }

    /// Whether a line may be written in second `second` (seconds since the limit was made): the
    /// first time in each second, never again for an earlier second.
    pub(crate) fn admit_at(&self, second: u64) -> bool {
        let mark = second.saturating_add(1);
        self.last.fetch_max(mark, Ordering::Relaxed) < mark
    }
}

/// A test subscriber that keeps the `warn` lines of target `erp::auth` written on this thread
/// (`tracing` has no capture of its own and the workspace takes no subscriber crate for it).
#[cfg(test)]
pub(crate) mod capture {
    use std::fmt;
    use std::sync::{Arc, Mutex, PoisonError};

    use tracing::field::{Field, Visit};
    use tracing::span::{Attributes, Id, Record};
    use tracing::{Event, Level, Metadata, Subscriber};

    /// The lines kept so far.
    #[derive(Clone, Default)]
    pub(crate) struct Lines(Arc<Mutex<Vec<String>>>);

    impl Lines {
        pub(crate) fn take(&self) -> Vec<String> {
            std::mem::take(&mut *self.0.lock().unwrap_or_else(PoisonError::into_inner))
        }
    }

    struct Capture(Lines);

    struct Message(Option<String>);

    impl Visit for Message {
        fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
            if field.name() == "message" {
                self.0 = Some(format!("{value:?}"));
            }
        }
    }

    impl Subscriber for Capture {
        fn enabled(&self, metadata: &Metadata<'_>) -> bool {
            metadata.target() == "erp::auth" && *metadata.level() == Level::WARN
        }
        fn new_span(&self, _: &Attributes<'_>) -> Id {
            Id::from_u64(1)
        }
        fn record(&self, _: &Id, _: &Record<'_>) {}
        fn record_follows_from(&self, _: &Id, _: &Id) {}
        fn event(&self, event: &Event<'_>) {
            let mut message = Message(None);
            event.record(&mut message);
            if let Some(line) = message.0 {
                self.0
                    .0
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .push(line);
            }
        }
        fn enter(&self, _: &Id) {}
        fn exit(&self, _: &Id) {}
    }

    /// Keep this thread's `erp::auth` `warn` lines until the guard is dropped.
    pub(crate) fn start() -> (Lines, tracing::subscriber::DefaultGuard) {
        let lines = Lines::default();
        let guard = tracing::subscriber::set_default(Capture(lines.clone()));
        (lines, guard)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Security review round 3 R3-d: a line is admitted once a second, the first time in each
    /// second, never again for an earlier second.
    #[test]
    fn a_warning_comes_once_a_second() {
        let limit = WarnLimit::new();
        assert!(limit.admit_at(0));
        assert!(!limit.admit_at(0));
        assert!(limit.admit_at(1));
        assert!(!limit.admit_at(1));
        assert!(!limit.admit_at(0), "an earlier second after a later one");
        assert!(limit.admit_at(5));
        assert!(limit.admit_at(u64::MAX));
        assert!(!limit.admit_at(u64::MAX));
    }
}
