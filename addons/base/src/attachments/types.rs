//! The values of the attachments API.

use std::io::Cursor;
use std::sync::Arc;

use erp_sdk::blob::{AttachmentStore, BlobRead, ByteRange, ContentKey, LimitKind, digests, to_hex};
use erp_sdk::prelude::*;
use tokio::io::AsyncReadExt;

use super::runtime::blob_error;

/// An upload staged by [`super::Uploader::stage`]: a lowercase hyphenated UUID, bound to the
/// subject and actor that staged it.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct UploadId(String);

impl UploadId {
    /// Parse the text of an upload id.
    ///
    /// # Errors
    ///
    /// `Validation` when it is not a lowercase hyphenated UUID (one message, so a malformed id
    /// and an unknown one cannot be told apart by their error).
    pub fn parse(text: &str) -> Result<Self> {
        let ok = text.len() == 36
            && text.bytes().enumerate().all(|(i, b)| match i {
                8 | 13 | 18 | 23 => b == b'-',
                _ => b.is_ascii_digit() || (b'a'..=b'f').contains(&b),
            });
        if ok {
            Ok(Self(text.to_owned()))
        } else {
            Err(upload_not_usable())
        }
    }

    pub(crate) fn from_db(text: String) -> Self {
        Self(text)
    }

    /// The text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for UploadId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// The one error for an upload id that cannot be linked: unknown, malformed, expired, already
/// used, or another user's.
pub(crate) fn upload_not_usable() -> Error {
    Error::validation(
        "The upload is not usable: it does not exist, has expired, was already used, or belongs \
         to another user.",
    )
}

/// Where the content of a new attachment comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum AttachmentContent {
    /// No content: an empty file (no blob is stored).
    Empty,
    /// Bytes in memory, up to the inline threshold (`attachments.inline_max_bytes`, 2 KiB
    /// default). Larger content is staged with [`super::Uploader`] and linked by [`Self::Upload`].
    Bytes(Vec<u8>),
    /// A staged upload of the same subject and actor.
    Upload(UploadId),
    Base64(String),
    /// An external URL (`type = url`); no bytes are stored.
    Url(String),
}

/// The record an attachment belongs to.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ParentRef {
    /// The model's technical name.
    pub model: String,
    /// The record's id.
    pub id: i64,
}

impl ParentRef {
    /// The record `id` of `model`.
    pub fn new(model: impl Into<String>, id: i64) -> Self {
        Self {
            model: model.into(),
            id,
        }
    }
}

/// One attachment to create.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct NewAttachment {
    /// The file name (1 to 255 bytes, no CR, LF or NUL).
    pub name: String,
    /// The content.
    pub content: AttachmentContent,
    /// The record it belongs to; `None` is a standalone attachment (its creator and the system
    /// group read it).
    pub parent: Option<ParentRef>,
    /// The binary field of the parent this content stores (`res_field`); needs a parent.
    pub res_field: Option<String>,
    /// The type the client declares: a hint only, checked against the detected type.
    pub declared_mimetype: Option<String>,
    /// Readable without a session through the content route (the route's own grant decides).
    pub public: bool,
    /// Free text.
    pub description: Option<String>,
    /// The company; the environment's current company when `None`.
    pub company_id: Option<i64>,
    /// Excludes [`Self::parent`].
    pub pending_model: Option<String>,
}

impl NewAttachment {
    /// An attachment `name` with `content` and no parent.
    pub fn new(name: impl Into<String>, content: AttachmentContent) -> Self {
        Self {
            name: name.into(),
            content,
            parent: None,
            res_field: None,
            declared_mimetype: None,
            public: false,
            description: None,
            company_id: None,
            pending_model: None,
        }
    }

    /// Stage the file for a record that does not exist yet: the row names `model` and record id
    /// 0 (see [`Self::pending_model`]). Not combined with [`Self::on`] or [`Self::field`].
    #[must_use]
    pub fn pending_in(mut self, model: impl Into<String>) -> Self {
        self.pending_model = Some(model.into());
        self
    }

    /// Attach to record `parent`.
    #[must_use]
    pub fn on(mut self, parent: ParentRef) -> Self {
        self.parent = Some(parent);
        self
    }

    /// Store the content of binary field `field` of the parent.
    #[must_use]
    pub fn field(mut self, field: impl Into<String>) -> Self {
        self.res_field = Some(field.into());
        self
    }

    /// Declare the media type the client names.
    #[must_use]
    pub fn declared(mut self, mimetype: impl Into<String>) -> Self {
        self.declared_mimetype = Some(mimetype.into());
        self
    }

    /// Mark it public.
    #[must_use]
    pub fn public(mut self, public: bool) -> Self {
        self.public = public;
        self
    }
}

/// What a listing returns about an attachment.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct AttachmentMeta {
    /// The record id.
    pub id: i64,
    /// The file name.
    pub name: String,
    /// The parent model.
    pub res_model: Option<String>,
    /// The parent record.
    pub res_id: Option<i64>,
    /// The binary field it stores.
    pub res_field: Option<String>,
    /// The stored (detected) media type.
    pub mimetype: Option<String>,
    /// The size in bytes.
    pub file_size: i64,
    pub checksum: Option<String>,
    /// `binary` or `url`.
    pub kind: String,
    /// The URL of a `url` attachment.
    pub url: Option<String>,
    /// Whether the content route may serve it without a session.
    pub public: bool,
    /// The user who created it.
    pub create_uid: i64,
}

/// What [`super::for_record`] lists.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct ListOptions {
    pub res_field: Option<String>,
    /// The most rows (1 to 10,000).
    pub limit: usize,
}

impl Default for ListOptions {
    fn default() -> Self {
        Self {
            res_field: None,
            limit: 1000,
        }
    }
}

/// The most attachments one batch call takes.
pub const MAX_BATCH: usize = 1000;

/// The longest name, in bytes.
pub const NAME_MAX_BYTES: usize = 255;
pub const URL_MAX_BYTES: usize = 1024;

/// Reads the content of one attachment: lazily, in ranges, without loading a whole file.
#[derive(Clone)]
pub struct BlobHandle {
    attachment: i64,
    size: u64,
    mimetype: String,
    checksum: String,
    source: HandleSource,
}

#[derive(Clone)]
pub(crate) enum HandleSource {
    Empty,
    Inline(Arc<Vec<u8>>),
    Store {
        store: Arc<dyn AttachmentStore>,
        key: ContentKey,
    },
}

impl std::fmt::Debug for BlobHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BlobHandle")
            .field("attachment", &self.attachment)
            .field("size", &self.size)
            .field("mimetype", &self.mimetype)
            .finish_non_exhaustive()
    }
}

impl BlobHandle {
    pub(crate) fn new(
        attachment: i64,
        size: u64,
        mimetype: String,
        checksum: String,
        source: HandleSource,
    ) -> Self {
        Self {
            attachment,
            size,
            mimetype,
            checksum,
            source,
        }
    }

    /// The attachment's id.
    pub fn attachment_id(&self) -> i64 {
        self.attachment
    }

    /// The size in bytes (from the database, before any byte is fetched).
    pub fn size(&self) -> u64 {
        self.size
    }

    /// The stored (detected) media type.
    pub fn mimetype(&self) -> &str {
        &self.mimetype
    }

    pub fn checksum(&self) -> &str {
        &self.checksum
    }

    /// Open the bytes, or a range of them.
    ///
    /// # Errors
    ///
    /// `Internal` (`AttachmentContentMissing`, never empty content) when the store has no such
    /// object; `Validation` for a range outside the content.
    pub async fn open(&self, range: Option<ByteRange>) -> Result<BlobRead> {
        match &self.source {
            HandleSource::Empty => Ok(BlobRead {
                total_size: 0,
                reader: Box::pin(Cursor::new(Vec::new())),
            }),
            HandleSource::Inline(bytes) => {
                let total = bytes.len() as u64;
                let slice = match range {
                    None => bytes.to_vec(),
                    Some(range) => {
                        let end = range.start.checked_add(range.len);
                        match end {
                            Some(end) if range.len > 0 && end <= total => {
                                let start = usize::try_from(range.start).unwrap_or(usize::MAX);
                                let end = usize::try_from(end).unwrap_or(usize::MAX);
                                bytes.get(start..end).unwrap_or(&[]).to_vec()
                            }
                            _ => {
                                return Err(blob_error(erp_sdk::blob::BlobError::InvalidRange {
                                    total,
                                }));
                            }
                        }
                    }
                };
                Ok(BlobRead {
                    total_size: total,
                    reader: Box::pin(Cursor::new(slice)),
                })
            }
            HandleSource::Store { store, key } => store.get(key, range).await.map_err(|e| {
                let missing = matches!(e, erp_sdk::blob::BlobError::NotFound { .. });
                let error = blob_error(e);
                if missing {
                    Error::internal(format!(
                        "AttachmentContentMissing: the content of attachment {} is missing \
                         from the attachment store",
                        self.attachment
                    ))
                } else {
                    error
                }
            }),
        }
    }

    /// Read the whole content into memory, at most `max_bytes`, and check its SHA-256.
    ///
    /// # Errors
    ///
    /// `Validation` before any byte is fetched when the content is larger than `max_bytes`;
    /// `Internal` when the store has no such object or the bytes fail their digest.
    pub async fn read_to_vec(&self, max_bytes: u64) -> Result<Vec<u8>> {
        if self.size > max_bytes {
            return Err(blob_error(erp_sdk::blob::BlobError::LimitExceeded {
                kind: LimitKind::ObjectBytes,
                limit: max_bytes,
            }));
        }
        let mut read = self.open(None).await?;
        let mut out = Vec::with_capacity(usize::try_from(self.size).unwrap_or(0));
        read.reader.read_to_end(&mut out).await.map_err(|e| {
            Error::internal(format!("The attachment content could not be read: {e}"))
        })?;
        if let HandleSource::Store { key, .. } = &self.source {
            let (sha256, _) = digests(&out);
            if &sha256 != key.sha256() {
                return Err(blob_error(erp_sdk::blob::BlobError::Integrity {
                    key: format!("attachment {}", self.attachment),
                    expected: to_hex(key.sha256()),
                    actual: to_hex(&sha256),
                }));
            }
        }
        Ok(out)
    }
}
