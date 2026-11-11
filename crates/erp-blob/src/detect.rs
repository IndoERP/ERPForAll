//! Content-type detection and the declared-type rule.
//!
//! The server decides the type from the bytes: magic numbers (`infer`) on at most the first
//! 8 KiB, then structural checks for text. The client's name and declared type are hints. A
//! declared type that is not itself active, on content detected as HTML, SVG, XML or script, is
//! refused naming both types, so a file cannot be stored as an image and later served as a page.

use crate::stage::DETECT_BYTES;

/// How a detected type behaves in a browser.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ContentClass {
    /// Does not run script or render markup on the application's origin.
    Passive,
    /// HTML, SVG, XML, script: served only as an attachment, never inline.
    Active,
}

/// The result of detection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Detected {
    /// The stored type.
    pub mimetype: String,
    /// Passive or active.
    pub class: ContentClass,
}

/// Why a declared type was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum DetectError {
    /// The declared type is not `type/subtype` of token characters, or is too long.
    #[error("the declared content type is not a valid media type")]
    InvalidDeclared,
    /// A passive declared type on active content.
    #[error("declared content type {declared} does not match the content, which is {detected}")]
    Mismatch {
        /// The declared type, normalized.
        declared: String,
        /// The detected type.
        detected: String,
    },
}

/// The longest declared media type accepted.
pub const MAX_MEDIA_TYPE_BYTES: usize = 127;

const ACTIVE_TYPES: &[&str] = &[
    "text/html",
    "application/xhtml+xml",
    "image/svg+xml",
    "text/xml",
    "application/xml",
    "text/javascript",
    "application/javascript",
    "application/ecmascript",
    "application/wasm",
    "text/x-shellscript",
    "application/x-sh",
];

/// Whether `mimetype` is one of the active types.
pub fn is_active_type(mimetype: &str) -> bool {
    ACTIVE_TYPES.contains(&mimetype)
}

/// Normalize a declared media type: parameters dropped, lowercase, `type/subtype` of token
/// characters.
///
/// # Errors
///
/// [`DetectError::InvalidDeclared`].
pub fn normalize_declared(declared: &str) -> Result<String, DetectError> {
    let base = declared.split(';').next().unwrap_or("").trim();
    let valid_token = |part: &str| {
        !part.is_empty()
            && part
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"!#$&-^_.+".contains(&b))
    };
    let mut parts = base.splitn(2, '/');
    let ok = base.len() <= MAX_MEDIA_TYPE_BYTES
        && match (parts.next(), parts.next()) {
            (Some(kind), Some(sub)) => valid_token(kind) && valid_token(sub),
            _ => false,
        };
    if ok {
        Ok(base.to_ascii_lowercase())
    } else {
        Err(DetectError::InvalidDeclared)
    }
}

fn matchers() -> &'static infer::Infer {
    static MATCHERS: std::sync::OnceLock<infer::Infer> = std::sync::OnceLock::new();
    MATCHERS.get_or_init(infer::Infer::new)
}

fn is_text(head: &[u8]) -> bool {
    if head.contains(&0) {
        return false;
    }
    match std::str::from_utf8(head) {
        Ok(_) => true,
        // the head may end inside a multi-byte character
        Err(error) => error.error_len().is_none(),
    }
}

fn markup_type(text_head: &[u8]) -> Option<&'static str> {
    let trimmed = text_head
        .strip_prefix(b"\xef\xbb\xbf")
        .unwrap_or(text_head)
        .trim_ascii_start();
    if trimmed.first() != Some(&b'<') {
        return None;
    }
    let window: Vec<u8> = trimmed
        .iter()
        .take(1024)
        .map(u8::to_ascii_lowercase)
        .collect();
    let has = |needle: &[u8]| window.windows(needle.len()).any(|w| w == needle);
    if has(b"<svg") {
        return Some("image/svg+xml");
    }
    if has(b"<!doctype html")
        || has(b"<html")
        || has(b"<script")
        || has(b"<iframe")
        || has(b"<body")
        || has(b"<head")
        || has(b"<meta")
        || has(b"<link")
        || has(b"<object")
        || has(b"<embed")
    {
        return Some("text/html");
    }
    if window.starts_with(b"<?xml") || has(b"<?xml") {
        return Some("text/xml");
    }
    None
}

fn extension_type(name: Option<&str>) -> &'static str {
    let extension = name
        .and_then(|n| n.rsplit_once('.'))
        .map(|(_, e)| e.to_ascii_lowercase());
    match extension.as_deref() {
        Some("csv") => "text/csv",
        Some("tsv") => "text/tab-separated-values",
        Some("json") => "application/json",
        Some("md" | "markdown") => "text/markdown",
        _ => "text/plain",
    }
}

/// Detect the type of content whose leading bytes are `head` (only the first 8 KiB are read).
pub fn detect(head: &[u8], name: Option<&str>) -> Detected {
    let head = head.get(..DETECT_BYTES.min(head.len())).unwrap_or(&[]);
    let text = is_text(head);
    if text && let Some(mimetype) = markup_type(head) {
        return Detected {
            mimetype: mimetype.to_owned(),
            class: ContentClass::Active,
        };
    }
    if let Some(kind) = matchers().get(head) {
        let mimetype = kind.mime_type();
        let class = if is_active_type(mimetype) {
            ContentClass::Active
        } else {
            ContentClass::Passive
        };
        return Detected {
            mimetype: mimetype.to_owned(),
            class,
        };
    }
    if text {
        if head.starts_with(b"#!") {
            return Detected {
                mimetype: "text/x-shellscript".to_owned(),
                class: ContentClass::Active,
            };
        }
        return Detected {
            mimetype: extension_type(name).to_owned(),
            class: ContentClass::Passive,
        };
    }
    Detected {
        mimetype: "application/octet-stream".to_owned(),
        class: ContentClass::Passive,
    }
}

/// Detect the type and apply the declared-type rule.
///
/// # Errors
///
/// [`DetectError::InvalidDeclared`] for a malformed declared type; [`DetectError::Mismatch`]
/// when a declared type that is not active is given for active content.
pub fn resolve(
    declared: Option<&str>,
    head: &[u8],
    name: Option<&str>,
) -> Result<Detected, DetectError> {
    let detected = detect(head, name);
    if let Some(declared) = declared {
        let declared = normalize_declared(declared)?;
        if detected.class == ContentClass::Active && !is_active_type(&declared) {
            return Err(DetectError::Mismatch {
                declared,
                detected: detected.mimetype,
            });
        }
    }
    Ok(detected)
}
