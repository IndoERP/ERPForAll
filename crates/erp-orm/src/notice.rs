//! Diagnostic notices and warnings emitted by operations on a [`Transaction`](crate::Transaction).

use std::fmt;
use std::sync::Arc;

/// The most notices one transaction attempt keeps. A notice posted past it is not stored: it
/// is counted ([`Transaction::notices_overflowed`](crate::Transaction::notices_overflowed)) and
/// the first one of an attempt is logged as a warning.
pub const MAX_NOTICES_PER_TRANSACTION: usize = 256;

/// The most bytes one notice message keeps. A longer message is shortened at a character
/// boundary and ends with a visible `… [truncated: N bytes total]` marker; the result, marker
/// included, is at most this long.
pub const MAX_NOTICE_MESSAGE_BYTES: usize = 4096;

/// What a [`Notice`] is about, so a consumer matches on a value and not on message text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum NoticeKind {
    /// An arch references groups that do not exist (view validation warning).
    UnknownGroup,
    /// An x2many sub-view inside a view was not validated (view validation warning).
    SubViewNotValidated,
    /// A warning of a kind that has no kind of its own yet.
    Other,
}

/// A diagnostic notice or warning emitted during an operation on a transaction.
///
/// Models and operations that produce non-fatal warnings (such as view arch validation warnings)
/// post them onto the transaction's notice channel ([`Transaction::post_notice`](crate::Transaction::post_notice))
/// so callers can receive them as values ([`Transaction::notices`](crate::Transaction::notices),
/// [`Transaction::take_notices`](crate::Transaction::take_notices)), replacing log-only warning
/// paths.
///
/// # Lifetime
///
/// Notices belong to one transaction attempt. Commit, rollback and `reset()` clear them; a
/// rollback to a savepoint drops the notices posted inside it, a release keeps them. With
/// `Orm::run` a caller must call `take_notices()` inside the closure, before the commit, and a
/// retried attempt starts empty.
///
/// # Bounds
///
/// A message is limited to [`MAX_NOTICE_MESSAGE_BYTES`] (see [`Notice::new`]) and a transaction
/// keeps at most [`MAX_NOTICES_PER_TRANSACTION`] notices.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Notice {
    kind: NoticeKind,
    target: Arc<str>,
    message: Arc<str>,
}

impl Notice {
    /// Create a new notice of `kind` for the given `target` subsystem and `message`.
    ///
    /// A message longer than [`MAX_NOTICE_MESSAGE_BYTES`] is shortened at a character boundary
    /// and ends with `… [truncated: N bytes total]`, `N` being the original length; the result
    /// is never longer than the cap.
    pub fn new(
        kind: NoticeKind,
        target: impl Into<Arc<str>>,
        message: impl Into<Arc<str>>,
    ) -> Self {
        let message: Arc<str> = message.into();
        let message = if message.len() > MAX_NOTICE_MESSAGE_BYTES {
            truncate_message(&message).into()
        } else {
            message
        };
        Self {
            kind,
            target: target.into(),
            message,
        }
    }

    /// What the notice is about.
    pub fn kind(&self) -> NoticeKind {
        self.kind
    }

    /// The target subsystem that emitted the notice (e.g. `"erp::views"`).
    pub fn target(&self) -> &str {
        &self.target
    }

    /// The notice message text (at most [`MAX_NOTICE_MESSAGE_BYTES`] bytes).
    pub fn message(&self) -> &str {
        &self.message
    }
}

/// `message` shortened to fit [`MAX_NOTICE_MESSAGE_BYTES`], marker included.
fn truncate_message(message: &str) -> String {
    let marker = format!("… [truncated: {} bytes total]", message.len());
    let mut cut = MAX_NOTICE_MESSAGE_BYTES.saturating_sub(marker.len());
    while !message.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}{marker}", &message[..cut])
}

impl fmt::Display for Notice {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.target, self.message)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notice_construction_and_display() {
        let n = Notice::new(
            NoticeKind::SubViewNotValidated,
            "erp::views",
            "sub-view not validated",
        );
        assert_eq!(n.kind(), NoticeKind::SubViewNotValidated);
        assert_eq!(n.target(), "erp::views");
        assert_eq!(n.message(), "sub-view not validated");
        assert_eq!(n.to_string(), "erp::views: sub-view not validated");
    }

    #[test]
    fn a_message_at_the_cap_is_kept_whole() {
        let text = "a".repeat(MAX_NOTICE_MESSAGE_BYTES);
        let n = Notice::new(NoticeKind::Other, "t", text.as_str());
        assert_eq!(n.message(), text);
    }

    #[test]
    fn a_long_message_is_shortened_with_a_visible_marker() {
        let total = MAX_NOTICE_MESSAGE_BYTES + 1000;
        let n = Notice::new(NoticeKind::Other, "t", "a".repeat(total));
        let marker = format!("… [truncated: {total} bytes total]");
        assert!(n.message().ends_with(&marker), "{}", n.message());
        assert_eq!(n.message().len(), MAX_NOTICE_MESSAGE_BYTES);
        assert!(n.message().starts_with("aaaa"));
    }

    #[test]
    fn truncation_stays_on_a_char_boundary() {
        // 3-byte characters never line up with the cut for every offset
        for pad in 0..4 {
            let text = format!(
                "{}{}",
                "a".repeat(pad),
                "€".repeat(MAX_NOTICE_MESSAGE_BYTES)
            );
            let n = Notice::new(NoticeKind::Other, "t", text.as_str());
            assert!(n.message().len() <= MAX_NOTICE_MESSAGE_BYTES);
            assert!(n.message().contains("[truncated: "));
            assert!(
                n.message()
                    .ends_with(&format!("{} bytes total]", text.len()))
            );
        }
    }
}
