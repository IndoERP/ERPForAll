//! An optional transport handoff for an already authorized immutable content key.

use crate::ContentKey;

/// Inputs selected by the attachment transport after its access and HTTP checks.
pub struct DownloadRequest<'a> {
    pub key: &'a ContentKey,
    pub size: u64,
    pub media_type: &'a str,
    pub attachment_disposition: &'a str,
    pub allow_presign: bool,
}

/// A transport capability, absent for ordinary application-streamed reads.
/// Presigned URLs carry credentials and deliberately have no debug representation.
#[non_exhaustive]
pub enum DownloadHandoff {
    InternalProxy(String),
    Presigned(String),
}

pub(crate) fn checked_prefix(prefix: &str) -> bool {
    prefix.len() <= 128
        && prefix
            .strip_prefix('/')
            .and_then(|rest| rest.strip_suffix('/'))
            .is_some_and(|path| {
                !path.is_empty()
                    && path.split('/').all(|part| {
                        !part.is_empty()
                            && part
                                .bytes()
                                .all(|ch| ch.is_ascii_alphanumeric() || ch == b'_' || ch == b'-')
                    })
            })
}
