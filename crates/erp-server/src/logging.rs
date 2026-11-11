//! The server's log output: one line per event on stderr, filtered by
//! `RUST_LOG`.
//!
//! Until this module the server installed no `tracing` subscriber, so every `tracing::` event in
//! every crate was discarded and `RUST_LOG=warn` in the launchd plist changed nothing (checked
//! 2026-10-28: no subscriber crate in `Cargo.lock`, no `set_global_default` outside tests).
//!
//! Adopted: the line shape and the `target=level` directive syntax.
//! Modified: no dependency (a few dozen lines instead of a filter engine); a directive that does
//! not parse stops the start instead of being skipped; every value that could carry a newline is
//! written escaped, so one event is always one line (a guest addon chooses its own log text).
//!
//! Directive grammar: comma-separated `level` (the default for every target) or `target=level`
//! (applies to that target and everything under it, `erp::rpc` covers `erp::rpc::x`); `level` is
//! one of `error`, `warn`, `info`, `debug`, `trace`, `off`. The longest matching target wins. An
//! unset `RUST_LOG` means `warn`.

use std::fmt::{self, Write as _};
use std::io::Write;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};

use tracing::field::{Field, Visit};
use tracing::level_filters::LevelFilter;
use tracing::span::{Attributes, Id, Record};
use tracing::{Event, Metadata, Subscriber};

/// A `RUST_LOG` value that cannot be used.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum LogFilterError {
    /// A directive is not `level` or `target=level`.
    #[error("RUST_LOG directive '{0}' is not 'level' or 'target=level'")]
    Directive(String),
    /// A level name is not one of error, warn, info, debug, trace, off.
    #[error("RUST_LOG level '{0}' is not one of error, warn, info, debug, trace, off")]
    Level(String),
    /// `RUST_LOG` is not valid Unicode.
    #[error("RUST_LOG is not valid Unicode")]
    NotUnicode,
}

/// A parsed `RUST_LOG`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogFilter {
    default: LevelFilter,
    targets: Vec<(String, LevelFilter)>,
}

impl LogFilter {
    /// Parse a `RUST_LOG` value.
    ///
    /// # Errors
    ///
    /// [`LogFilterError`] naming the directive that does not parse.
    pub fn parse(text: &str) -> Result<Self, LogFilterError> {
        let mut default = None;
        let mut targets = Vec::new();
        for directive in text.split(',').map(str::trim).filter(|d| !d.is_empty()) {
            match directive.split_once('=') {
                None => default = Some(level(directive)?),
                Some((target, lvl)) => {
                    let target = target.trim();
                    if target.is_empty() || lvl.contains('=') {
                        return Err(LogFilterError::Directive(directive.to_owned()));
                    }
                    targets.push((target.to_owned(), level(lvl.trim())?));
                }
            }
        }
        // longest target first: the first match is the most specific
        targets.sort_by_key(|(t, _)| std::cmp::Reverse(t.len()));
        Ok(Self {
            // absent, not wrong: the documented default is warn
            default: default.unwrap_or(LevelFilter::WARN),
            targets,
        })
    }

    /// The filter from the process environment.
    ///
    /// # Errors
    ///
    /// [`LogFilterError`] when `RUST_LOG` is set and unusable.
    pub fn from_env() -> Result<Self, LogFilterError> {
        match std::env::var("RUST_LOG") {
            Ok(text) => Self::parse(&text),
            Err(std::env::VarError::NotPresent) => Self::parse("warn"),
            Err(std::env::VarError::NotUnicode(_)) => Err(LogFilterError::NotUnicode),
        }
    }

    fn allows(&self, metadata: &Metadata<'_>) -> bool {
        let target = metadata.target();
        let limit = self
            .targets
            .iter()
            .find(|(t, _)| {
                target == t
                    || target
                        .strip_prefix(t.as_str())
                        .is_some_and(|rest| rest.starts_with("::"))
            })
            .map_or(self.default, |(_, l)| *l);
        *metadata.level() <= limit
    }

    fn most_verbose(&self) -> LevelFilter {
        self.targets
            .iter()
            .map(|(_, l)| *l)
            .chain(std::iter::once(self.default))
            .max()
            // the chain always has the default
            .unwrap_or(self.default)
    }
}

fn level(text: &str) -> Result<LevelFilter, LogFilterError> {
    match text.to_ascii_lowercase().as_str() {
        "error" => Ok(LevelFilter::ERROR),
        "warn" => Ok(LevelFilter::WARN),
        "info" => Ok(LevelFilter::INFO),
        "debug" => Ok(LevelFilter::DEBUG),
        "trace" => Ok(LevelFilter::TRACE),
        "off" => Ok(LevelFilter::OFF),
        _ => Err(LogFilterError::Level(text.to_owned())),
    }
}

/// The subscriber: formats each event as one line and writes it with a single `write_all`.
pub struct LineLogger<W: Write + Send + 'static> {
    filter: LogFilter,
    out: Mutex<W>,
    next_span: AtomicU64,
}

impl<W: Write + Send + 'static> LineLogger<W> {
    /// A logger writing to `out`.
    pub fn new(filter: LogFilter, out: W) -> Self {
        Self {
            filter,
            out: Mutex::new(out),
            next_span: AtomicU64::new(1),
        }
    }
}

impl<W: Write + Send + 'static> Subscriber for LineLogger<W> {
    fn enabled(&self, metadata: &Metadata<'_>) -> bool {
        self.filter.allows(metadata)
    }

    fn max_level_hint(&self) -> Option<LevelFilter> {
        Some(self.filter.most_verbose())
    }

    fn new_span(&self, _: &Attributes<'_>) -> Id {
        Id::from_u64(self.next_span.fetch_add(1, Ordering::Relaxed))
    }

    fn record(&self, _: &Id, _: &Record<'_>) {}

    fn record_follows_from(&self, _: &Id, _: &Id) {}

    fn event(&self, event: &Event<'_>) {
        let mut line = String::with_capacity(256);
        format_event(&mut line, SystemTime::now(), event);
        line.push('\n');
        let mut out = self.out.lock().unwrap_or_else(PoisonError::into_inner);
        // A logger has no caller to return an error to, and a failing stderr must not take the
        // request down with it; the write is one `write_all`, so lines never interleave.
        out.write_all(line.as_bytes()).ok();
    }

    fn enter(&self, _: &Id) {}

    fn exit(&self, _: &Id) {}
}

/// Install the logger as the process-wide subscriber, filtered by `RUST_LOG` (default `warn`),
/// writing to stderr.
///
/// # Errors
///
/// An unusable `RUST_LOG`, or another subscriber already installed.
pub fn install_from_env() -> Result<(), String> {
    let filter = LogFilter::from_env().map_err(|e| e.to_string())?;
    let logger = LineLogger::new(filter, std::io::stderr());
    tracing::subscriber::set_global_default(logger)
        .map_err(|_| "a log subscriber is already installed in this process".to_owned())
}

fn format_event(line: &mut String, now: SystemTime, event: &Event<'_>) {
    let metadata = event.metadata();
    let secs = now
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(0));
    push_timestamp(line, secs);
    push(
        line,
        format_args!(" {:<5} {}:", metadata.level(), metadata.target()),
    );
    let mut fields = Fields::default();
    event.record(&mut fields);
    if !fields.message.is_empty() {
        line.push(' ');
        line.push_str(&fields.message);
    }
    for (name, value) in fields.pairs {
        push(line, format_args!(" {name}={value}"));
    }
}

/// `2026-10-28T08:15:03Z` (UTC) for Unix seconds. A clock before 1970 prints the epoch.
fn push_timestamp(line: &mut String, secs: i64) {
    let days = secs.div_euclid(86_400);
    let rest = secs.rem_euclid(86_400);
    // civil-from-days (H. Hinnant), exact for the proleptic Gregorian calendar
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    push(
        line,
        format_args!(
            "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
            rest / 3600,
            rest % 3600 / 60,
            rest % 60
        ),
    );
}

fn push(line: &mut String, args: fmt::Arguments<'_>) {
    // writing into a String cannot fail
    line.write_fmt(args).ok();
}

#[derive(Default)]
struct Fields {
    message: String,
    pairs: Vec<(&'static str, String)>,
}

impl Visit for Fields {
    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "message" {
            self.message = escaped(value);
        } else {
            self.pairs.push((field.name(), quoted_if_needed(value)));
        }
    }

    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        let text = format!("{value:?}");
        if field.name() == "message" {
            self.message = escaped(&text);
        } else {
            self.pairs.push((field.name(), escaped(&text)));
        }
    }
}

/// `value` with every control character written as an escape, so it stays on one line.
fn escaped(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for c in value.chars() {
        if c.is_control() {
            out.extend(c.escape_default());
        } else {
            out.push(c);
        }
    }
    out
}

/// A plain token as is; anything with a space, quote, `=`, control character or nothing at all in
/// double quotes (Rust debug escaping).
fn quoted_if_needed(value: &str) -> String {
    let plain = !value.is_empty()
        && value
            .chars()
            .all(|c| !c.is_control() && !c.is_whitespace() && c != '"' && c != '=');
    if plain {
        value.to_owned()
    } else {
        format!("{value:?}")
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "test assertions")]
mod tests {
    use super::*;
    use std::sync::Arc;

    /// A writer tests can read back.
    #[derive(Clone, Default)]
    pub(crate) struct Sink(pub(crate) Arc<Mutex<Vec<u8>>>);
    impl Write for Sink {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    impl Sink {
        pub(crate) fn text(&self) -> String {
            String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
        }
    }

    fn logged(filter: &str, emit: impl FnOnce()) -> String {
        let sink = Sink::default();
        let logger = LineLogger::new(LogFilter::parse(filter).unwrap(), sink.clone());
        tracing::subscriber::with_default(logger, emit);
        sink.text()
    }

    #[test]
    fn the_filter_defaults_to_warn_and_the_longest_target_wins() {
        let out = logged("warn,erp::rpc=info", || {
            tracing::info!(target: "erp::rpc", "kept");
            tracing::info!(target: "erp::rpc::deep", "kept too");
            tracing::info!(target: "erp::rpcx", "dropped: not under erp::rpc");
            tracing::info!(target: "erp::orm", "dropped");
            tracing::warn!(target: "erp::orm", "kept warn");
        });
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 3, "{out}");
        assert!(lines[0].ends_with("INFO  erp::rpc: kept"), "{out}");
        assert!(
            lines[1].ends_with("INFO  erp::rpc::deep: kept too"),
            "{out}"
        );
        assert!(lines[2].ends_with("WARN  erp::orm: kept warn"), "{out}");
        assert_eq!(
            LogFilter::parse("").unwrap(),
            LogFilter::parse("warn").unwrap()
        );
    }

    #[test]
    fn an_unusable_directive_is_an_error_naming_it() {
        assert_eq!(
            LogFilter::parse("loud").unwrap_err(),
            LogFilterError::Level("loud".into())
        );
        assert_eq!(
            LogFilter::parse("erp=warn=x").unwrap_err(),
            LogFilterError::Directive("erp=warn=x".into())
        );
        assert_eq!(
            LogFilter::parse("=warn").unwrap_err(),
            LogFilterError::Directive("=warn".into())
        );
    }

    #[test]
    fn a_value_with_a_newline_cannot_split_the_line() {
        let out = logged("warn", || {
            tracing::warn!(target: "erp::wasm_guest", note = "a\nb c", "guest said:\nfake line");
        });
        assert_eq!(out.lines().count(), 1, "{out}");
        assert!(
            out.contains(r#"guest said:\nfake line note="a\nb c""#),
            "{out}"
        );
    }

    #[test]
    fn the_timestamp_is_utc_iso_8601() {
        let mut line = String::new();
        push_timestamp(&mut line, 1_793_146_503);
        assert_eq!(line, "2026-10-28T00:15:03Z");
        let mut epoch = String::new();
        push_timestamp(&mut epoch, 0);
        assert_eq!(epoch, "1970-01-01T00:00:00Z");
        let mut leap = String::new();
        push_timestamp(&mut leap, 951_782_400);
        assert_eq!(leap, "2000-02-29T00:00:00Z");
    }
}
