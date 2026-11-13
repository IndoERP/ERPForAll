// SPDX-License-Identifier: MIT OR Apache-2.0
//! The checked read behind the authenticated content route `GET|HEAD /erp/content/<id>`
//! ("Download").
//!
//! [`lookup_content`] is the only entry: it takes the caller's transaction, refuses callers that
//! are not internal users (portal reads need a link principal, a later slice), runs
//! the existing parent-derived read check of [`super::access`] **before** it reads any content
//! column or touches a store, and answers [`ContentLookup::NotFound`] alike for a missing id, a refused
//! read, a URL attachment and an attachment without stored content, so the answer tells nothing
//! about records the caller cannot read. What it returns ([`RouteContent`]) owns no transaction:
//! the route ends the transaction before any byte is read, so a slow download never holds a
//! database connection. [`RouteContent::open`] takes this node's stream admission and opens the
//! bytes; the stream it returns holds that admission until it is dropped.

use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use erp_sdk::Operation;
use erp_sdk::blob::{AttachmentRuntime, BlobError, BlobReader, ByteRange, StreamPermit};
use erp_sdk::prelude::*;
use tokio::io::{AsyncRead, ReadBuf};

use super::access;
use super::runtime::{blob_error, col_error, runtime};
use super::service::handle_for_blob;
use super::types::BlobHandle;

/// What [`lookup_content`] found for one attachment id.
#[derive(Debug)]
#[non_exhaustive]
pub enum ContentLookup {
    /// The caller may read it and it has stored content.
    Found(RouteContent),
    /// No such attachment, a refused read, a URL attachment or one without stored content: one
    /// answer for all of them (the route's single `404`).
    NotFound,
    /// The caller may read it, but its blob row is gone or has no bytes anywhere
    /// (`500 AttachmentContentMissing`, never empty content).
    Missing {
        /// The attachment, for the operator log only.
        attachment: i64,
        /// Its SHA-1 hex digest (`checksum`), for the operator log only.
        digest: String,
    },
}

/// The content of one readable attachment, resolved inside the caller's transaction and usable
/// after it ended.
pub struct RouteContent {
    name: String,
    handle: BlobHandle,
    runtime: Arc<AttachmentRuntime>,
}

impl std::fmt::Debug for RouteContent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RouteContent")
            .field("handle", &self.handle)
            .finish_non_exhaustive()
    }
}

/// Why [`RouteContent::open`] did not return bytes.
#[derive(Debug)]
#[non_exhaustive]
pub enum ContentOpenError {
    /// Every stream permit of this node is held (`attachments.stream_permits_per_node`): the
    /// route answers `503` with `Retry-After`, it never queues.
    Busy {
        /// The configured per-node bound.
        permits: u32,
    },
    /// The store has no object at the attachment's content key.
    Missing,
    /// Any other store failure.
    Failed(Error),
}

/// A byte stream of attachment content that holds this node's stream admission until dropped.
pub type ContentStream = Pin<Box<dyn AsyncRead + Send>>;

impl RouteContent {
    /// The attachment's id.
    pub fn attachment_id(&self) -> i64 {
        self.handle.attachment_id()
    }

    /// The stored file name (untrusted text: the route encodes it per RFC 6266).
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The stored, detected media type.
    pub fn mimetype(&self) -> &str {
        self.handle.mimetype()
    }

    /// The content size in bytes, from the blob row (no byte is fetched).
    pub fn size(&self) -> u64 {
        self.handle.size()
    }

    pub fn digest(&self) -> &str {
        self.handle.checksum()
    }

    /// Admit one stream on this node, then open the bytes (`range`: one byte range inside the
    /// content, already validated by the caller).
    ///
    /// # Errors
    ///
    /// [`ContentOpenError::Busy`] when no permit is free (checked before the store is called),
    /// [`ContentOpenError::Missing`] when the store has no object, [`ContentOpenError::Failed`]
    /// for any other store failure.
    pub async fn open(
        &self,
        range: Option<ByteRange>,
    ) -> std::result::Result<ContentStream, ContentOpenError> {
        let permit = self
            .runtime
            .try_stream_permit()
            .map_err(|error| match error {
                BlobError::Busy { permits } => ContentOpenError::Busy { permits },
                other => ContentOpenError::Failed(blob_error(other)),
            })?;
        // Enforcement point for the content scan's `block` mode on this
        // route: when scanning lands, content that is not `clean` must be refused here, before the
        // store is read. Today a scan mode other than `off` refuses startup
        // (`crates/erp-blob/src/config.rs`), so nothing unscanned can be served unnoticed.
        match self.handle.open_blob(range).await {
            Ok(read) => Ok(Box::pin(Admitted {
                reader: read.reader,
                _permit: permit,
            })),
            Err(BlobError::NotFound { .. }) => Err(ContentOpenError::Missing),
            Err(other) => Err(ContentOpenError::Failed(blob_error(other))),
        }
    }
}

/// The store's reader together with the stream permit it was admitted under.
struct Admitted {
    reader: BlobReader,
    _permit: StreamPermit,
}

impl AsyncRead for Admitted {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        self.get_mut().reader.as_mut().poll_read(cx, buf)
    }
}

/// Resolve attachment `id` for the content route under the caller's principal (`tx`'s
/// environment): the read check first, then the content columns, then the store row.
///
/// # Errors
///
/// Database errors, an unconfigured attachment store, or a store row that no longer matches its
/// store (fingerprint); never an access error (a refused read is [`ContentLookup::NotFound`]).
pub async fn lookup_content(tx: &mut Transaction, id: i64) -> Result<ContentLookup> {
    if id <= 0 {
        return Ok(ContentLookup::NotFound);
    }
    let env = tx.env();
    // principal or an addon's portal grant, neither of which this route serves yet
    if !env.su() && !tx.has_group(&env, "base.group_user").await? {
        return Ok(ContentLookup::NotFound);
    }
    let targets = access::load_targets(tx, &[id]).await?;
    if targets.len() != 1 {
        return Ok(ContentLookup::NotFound);
    }
    let verdict = access::allowed(tx, &env, &targets, Operation::Read).await?;
    if verdict != [true] {
        return Ok(ContentLookup::NotFound);
    }
    let rows = tx
        .query(
            "SELECT type, blob_id, name, mimetype, checksum FROM ir_attachment WHERE id = $1",
            &[&id],
        )
        .await?;
    // a row deleted after the check (possible under a weaker isolation) gives the same single
    // "nothing to serve"
    let Some(row) = rows.first() else {
        return Ok(ContentLookup::NotFound);
    };
    let read = |e: &dyn std::fmt::Display| col_error("attachment content", e);
    let kind: Option<String> = row.try_get(0).map_err(|e| read(&e))?;
    let blob: Option<i64> = row.try_get(1).map_err(|e| read(&e))?;
    let name: Option<String> = row.try_get(2).map_err(|e| read(&e))?;
    let mimetype: Option<String> = row.try_get(3).map_err(|e| read(&e))?;
    let checksum: Option<String> = row.try_get(4).map_err(|e| read(&e))?;
    // a URL attachment's redirect is the `redirect` class and a later slice; it is not served
    // here and its absence is never reported as missing content
    if kind.as_deref() == Some("url") {
        return Ok(ContentLookup::NotFound);
    }
    let Some(blob) = blob else {
        return Ok(ContentLookup::NotFound);
    };
    let checksum = checksum.unwrap_or_default();
    let mimetype = mimetype.unwrap_or_else(|| "application/octet-stream".to_owned());
    let Some(handle) = handle_for_blob(tx, &env, id, blob, mimetype, checksum.clone()).await?
    else {
        return Ok(ContentLookup::Missing {
            attachment: id,
            digest: checksum,
        });
    };
    Ok(ContentLookup::Found(RouteContent {
        name: name.unwrap_or_default(),
        handle,
        runtime: runtime(&env)?,
    }))
}
