//! Browser error reports: `POST /erp/client-error`.
//!
//! The web client posts what it could not show or recover from (an unhandled error, a failed
//! call, a render failure) so the operator can find it next to the server's own log line for the
//! same click. A report is logged at `warn` with the reporting user and never stored or echoed.
//!
//! Adopted:
//! the dialog stays the client's job. Modified: a bounded, authenticated, rate limited report so
//! the server log has the browser's half of the story.

use std::collections::HashMap;
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

/// Largest request body, in bytes.
pub const MAX_BODY_BYTES: usize = 8 * 1024;
/// Longest `message`, in characters; a longer one is cut.
pub const MAX_MESSAGE_CHARS: usize = 1024;
/// Longest `stack`, in characters; a longer one is cut.
pub const MAX_STACK_CHARS: usize = 4096;
/// Longest `path`, in bytes.
pub const MAX_PATH_BYTES: usize = 512;
/// Longest `build`, in bytes.
pub const MAX_BUILD_BYTES: usize = 64;
/// Reports one session may send per [`WINDOW`], per server process.
pub const REPORTS_PER_WINDOW: u32 = 10;
/// The rate limit window.
pub const WINDOW: Duration = Duration::from_secs(60);
/// Sessions the limiter tracks at once; a new session over the bound is refused until a window
/// ends.
pub const TRACKED_SESSIONS: usize = 4096;

/// The kinds of report the client sends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ReportKind {
    /// An uncaught exception (`window.onerror`).
    Error,
    /// An unhandled promise rejection.
    UnhandledRejection,
    /// A call to the server that failed (the report names its request id).
    Rpc,
    /// A component failed to render.
    Render,
}

impl ReportKind {
    /// The wire name.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Error => "error",
            Self::UnhandledRejection => "unhandledrejection",
            Self::Rpc => "rpc",
            Self::Render => "render",
        }
    }
}

/// A validated report.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct ClientReport {
    /// The request id of the failed call, when the report is about one.
    pub request_id: Option<erp_rpc::CorrelationId>,
    /// What kind of failure.
    pub kind: ReportKind,
    /// The error message, cut at [`MAX_MESSAGE_CHARS`].
    pub message: String,
    /// The page path (no query, no fragment).
    pub path: String,
    /// The client build id.
    pub build: String,
    /// The stack, cut at [`MAX_STACK_CHARS`].
    pub stack: String,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Wire {
    request_id: Option<String>,
    kind: String,
    message: String,
    path: String,
    build: String,
    #[serde(default)]
    stack: String,
}

impl ClientReport {
    /// Parse and validate a request body.
    ///
    /// # Errors
    ///
    /// A message naming the problem: a body over [`MAX_BODY_BYTES`], JSON that is not exactly the
    /// documented object (unknown or missing keys, wrong types), an unknown `kind`, a
    /// `request_id` that is not a UUID, a `path` that is not a bare absolute path, or a `path` or
    /// `build` over its bound.
    pub fn parse(body: &[u8]) -> Result<Self, String> {
        if body.len() > MAX_BODY_BYTES {
            return Err(format!("report body exceeds {MAX_BODY_BYTES} bytes"));
        }
        let wire: Wire = serde_json::from_slice(body)
            .map_err(|e| format!("invalid client error report: {e}"))?;
        let kind = match wire.kind.as_str() {
            "error" => ReportKind::Error,
            "unhandledrejection" => ReportKind::UnhandledRejection,
            "rpc" => ReportKind::Rpc,
            "render" => ReportKind::Render,
            other => {
                return Err(format!(
                    "unknown report kind '{}': expected error, unhandledrejection, rpc or render",
                    other.chars().take(32).collect::<String>()
                ));
            }
        };
        let request_id = wire
            .request_id
            .as_deref()
            .map(erp_rpc::CorrelationId::parse)
            .transpose()
            .map_err(|_| "request_id must be a UUID".to_owned())?;
        if wire.path.len() > MAX_PATH_BYTES
            || !wire.path.starts_with('/')
            || wire.path.contains(['?', '#'])
            || wire.path.chars().any(char::is_control)
        {
            return Err(format!(
                "path must be an absolute page path of at most {MAX_PATH_BYTES} bytes, without query or fragment"
            ));
        }
        if wire.build.is_empty()
            || wire.build.len() > MAX_BUILD_BYTES
            || !wire
                .build
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_'))
        {
            return Err(format!(
                "build must be 1 to {MAX_BUILD_BYTES} characters of letters, digits, '.', '-' or '_'"
            ));
        }
        Ok(Self {
            request_id,
            kind,
            message: wire.message.chars().take(MAX_MESSAGE_CHARS).collect(),
            path: wire.path,
            build: wire.build,
            stack: wire.stack.chars().take(MAX_STACK_CHARS).collect(),
        })
    }

    /// Write the report to the log at `warn`, for the user `uid`; `request_id` is the id of the
    /// report's own HTTP request.
    pub fn log(&self, uid: i64, request_id: &str) {
        tracing::warn!(
            target: "erp::client",
            uid,
            request_id,
            kind = self.kind.as_str(),
            reported_request_id = self.request_id.as_ref().map(erp_rpc::CorrelationId::as_str),
            path = self.path,
            build = self.build,
            text = ?self.message,
            stack = ?self.stack,
            "client error report"
        );
    }
}

/// A per-session fixed-window counter: at most [`REPORTS_PER_WINDOW`] per [`WINDOW`], in this
/// process (a deployment of N replicas allows N times that per session; limits are
/// per process unless stated).
#[derive(Default)]
pub struct ReportLimiter {
    windows: Mutex<HashMap<[u8; 16], (Instant, u32)>>,
}

/// Why a report was not counted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum LimitRefusal {
    /// The session sent [`REPORTS_PER_WINDOW`] reports in the current window.
    SessionRate,
    /// [`TRACKED_SESSIONS`] sessions are being tracked and none has an ended window.
    Full,
}

impl ReportLimiter {
    /// Count a report from `session` at `now`.
    ///
    /// # Errors
    ///
    /// [`LimitRefusal`] when the report is over a limit; nothing is counted then.
    pub fn admit(&self, session: [u8; 16], now: Instant) -> Result<(), LimitRefusal> {
        let mut map = self.windows.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some((start, count)) = map.get_mut(&session) {
            if now.duration_since(*start) >= WINDOW {
                *start = now;
                *count = 1;
                return Ok(());
            }
            if *count >= REPORTS_PER_WINDOW {
                return Err(LimitRefusal::SessionRate);
            }
            *count += 1;
            return Ok(());
        }
        if map.len() >= TRACKED_SESSIONS {
            map.retain(|_, (start, _)| now.duration_since(*start) < WINDOW);
            if map.len() >= TRACKED_SESSIONS {
                return Err(LimitRefusal::Full);
            }
        }
        map.insert(session, (now, 1));
        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "test assertions")]
mod tests {
    use super::*;

    fn body(extra: &str) -> String {
        format!(
            r#"{{"kind":"rpc","message":"boom","path":"/erp/action-1","build":"abc-123"{extra}}}"#
        )
    }

    #[test]
    fn a_report_is_exactly_the_documented_object() {
        let ok = ClientReport::parse(
            body(r#","request_id":"0194F5A0-7B1C-7D2E-8F3A-123456789ABC","stack":"at x""#)
                .as_bytes(),
        )
        .unwrap();
        assert_eq!(ok.kind, ReportKind::Rpc);
        assert_eq!(
            ok.request_id.unwrap().as_str(),
            "0194f5a0-7b1c-7d2e-8f3a-123456789abc"
        );
        assert_eq!(ok.stack, "at x");
        for (bad, why) in [
            (body(r#","extra":1"#), "unknown field"),
            (body(r#","request_id":"nope""#), "request_id must be a UUID"),
            (
                body("").replace("\"rpc\"", "\"boom\""),
                "unknown report kind 'boom'",
            ),
            (
                body("").replace("/erp/action-1", "/erp?x=1"),
                "path must be",
            ),
            (body("").replace("/erp/action-1", "erp"), "path must be"),
            (body("").replace("abc-123", "a b"), "build must be"),
            (
                body("").replace("\"message\":\"boom\",", ""),
                "missing field",
            ),
        ] {
            let error = ClientReport::parse(bad.as_bytes()).unwrap_err();
            assert!(error.contains(why), "{why}: {error}");
        }
        assert!(ClientReport::parse(b"[]").is_err());
        assert!(ClientReport::parse(b"").is_err());
    }

    #[test]
    fn size_is_bounded_and_long_text_is_cut() {
        let big = format!(r#","stack":"{}""#, "x".repeat(MAX_BODY_BYTES));
        assert_eq!(
            ClientReport::parse(body(&big).as_bytes()).unwrap_err(),
            "report body exceeds 8192 bytes"
        );
        let long = body(&format!(
            r#","stack":"{}""#,
            "x".repeat(MAX_STACK_CHARS + 100)
        ))
        .replace("boom", &"é".repeat(MAX_MESSAGE_CHARS + 5));
        let report = ClientReport::parse(long.as_bytes()).unwrap();
        assert_eq!(report.stack.chars().count(), MAX_STACK_CHARS);
        assert_eq!(report.message.chars().count(), MAX_MESSAGE_CHARS);
    }

    #[test]
    fn the_limiter_counts_per_session_per_window_and_stays_bounded() {
        let limiter = ReportLimiter::default();
        let t0 = Instant::now();
        let id = |n: u64| {
            let mut bytes = [0; 16];
            bytes[..8].copy_from_slice(&n.to_be_bytes());
            bytes
        };
        for _ in 0..REPORTS_PER_WINDOW {
            limiter.admit(id(1), t0).unwrap();
        }
        assert_eq!(limiter.admit(id(1), t0), Err(LimitRefusal::SessionRate));
        limiter.admit(id(2), t0).unwrap(); // another session is not affected
        limiter.admit(id(1), t0 + WINDOW).unwrap(); // the window ended
        // fill the map with live sessions: a new one is refused, an old one still counts
        let full = ReportLimiter::default();
        for n in 0..TRACKED_SESSIONS as u64 {
            full.admit(id(n), t0).unwrap();
        }
        assert_eq!(
            full.admit(id(TRACKED_SESSIONS as u64), t0),
            Err(LimitRefusal::Full)
        );
        // after the windows end the map makes room
        full.admit(id(TRACKED_SESSIONS as u64), t0 + WINDOW)
            .unwrap();
    }
}
