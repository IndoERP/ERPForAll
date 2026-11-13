//! Staging uploads: the intent row commits first, in its own short
//! transaction on the primary pool, before the first byte reaches the store; the bytes are
//! hashed on their way into staging; the row then becomes `ready` and bound to the subject and
//! actor that staged it. A business transaction links it later ([`super::link`]).
//!
//! Every staging object has a committed row, so the cleaner never lists the store. A failed or
//! abandoned upload retains its reservation; fenced [`super::gc`] maintenance removes its object.

use erp_sdk::blob::{
    AttachmentRuntime, DETECT_BYTES, Namespace, StageLimits, StagingKey, resolve, to_hex,
};
use erp_sdk::prelude::*;
use erp_sdk::{Actor, BeginOptions, Env, Isolation, Orm};
use tokio::io::{AsyncRead, AsyncReadExt};

use super::runtime::{blob_error, col_error, runtime, store_by_key};
use super::types::{NAME_MAX_BYTES, UploadId};

/// What the client says about an upload.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct StageRequest {
    /// The file name, a hint for detection and the attachment's name.
    pub name: Option<String>,
    /// The media type the client declares, checked against the detected one.
    pub declared_mimetype: Option<String>,
    declared_length: Option<u64>,
    file_limit: Option<u64>,
}

impl StageRequest {
    /// A request with a file name.
    pub fn named(name: impl Into<String>) -> Self {
        Self {
            name: Some(name.into()),
            declared_mimetype: None,
            declared_length: None,
            file_limit: None,
        }
    }

    /// Reserve a known body length. Actual bytes may be shorter, but never exceed it.
    #[must_use]
    pub fn with_declared_length(mut self, bytes: u64) -> Self {
        self.declared_length = Some(bytes);
        self
    }

    /// Apply a transport file limit, capped again by the configured attachment limit.
    #[must_use]
    pub fn with_file_limit(mut self, bytes: u64) -> Self {
        self.file_limit = Some(bytes);
        self
    }

    // absent length reserves the file limit; absent transport limit uses
    // the configured store limit. These are specified absence defaults, not error fallbacks.
    fn byte_limits(&self, configured: u64) -> Result<(u64, u64)> {
        let limit = self.file_limit.unwrap_or(configured).min(configured);
        let reserved = self.declared_length.unwrap_or(limit);
        if reserved > limit {
            return Err(Error::validation(format!(
                "object size in bytes exceeds the limit of {limit}"
            )));
        }
        Ok((limit, reserved))
    }

    /// Declare the media type.
    #[must_use]
    pub fn declared(mut self, mimetype: impl Into<String>) -> Self {
        self.declared_mimetype = Some(mimetype.into());
        self
    }
}

/// What staging reports; the same for new and deduplicated content.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct StagedUpload {
    /// The id to link.
    pub id: UploadId,
    /// The size in bytes.
    pub size: u64,
    pub checksum: String,
    /// The detected media type.
    pub mimetype: String,
}

/// The actor of an environment as stored with an upload: empty for the subject acting alone.
pub(crate) fn actor_text(env: &Env) -> String {
    match env.principal().actor() {
        None => String::new(),
        Some(Actor::Impersonator(original)) => format!("imp:{}", original.subject().get()),
        Some(Actor::ApiKey { id, .. }) => format!("key:{}", id.get()),
        Some(Actor::Link { id }) => format!("link:{}", id.get()),
        Some(_) => "other".to_owned(),
    }
}

/// Stages uploads on a pool of its own (not the caller's transaction).
#[derive(Clone)]
pub struct Uploader {
    orm: Orm,
}

/// Validate the name a client gives (1 to 255 bytes, no CR, LF or NUL).
pub(crate) fn check_name(name: &str) -> Result<()> {
    if name.is_empty()
        || name.len() > NAME_MAX_BYTES
        || name.bytes().any(|b| matches!(b, b'\r' | b'\n' | 0))
    {
        return Err(Error::validation(format!(
            "An attachment name is 1 to {NAME_MAX_BYTES} bytes and contains no carriage return, \
             line feed or NUL."
        )));
    }
    Ok(())
}

impl Uploader {
    /// An uploader over `orm`'s primary pool.
    pub fn new(orm: &Orm) -> Self {
        Self { orm: orm.clone() }
    }

    /// Stream `body` into the write store and return the id of the staged upload.
    ///
    /// Content up to the inline threshold stays in the upload row; anything larger streams into
    /// the store with memory bounded by the read buffer. The type is detected from the first
    /// 8 KiB; a declared type that is not active, on active content, is refused naming both
    /// types. `env` names the subject and actor the upload is bound to.
    ///
    /// # Errors
    ///
    /// `Validation` for a bad name, a declared type that does not match, content above
    /// `attachments.max_file_bytes`; `ResourceExhausted` when the subject already holds the
    /// maximum pending count/bytes or storage quota; typed quota errors remain sources.
    /// Failed receives retain their intent/reservation until fenced maintenance succeeds.
    /// `Programming` for invalid runtime configuration; store
    /// errors as `Internal`. `Access` when `env` was switched by `with_user` from a principal that
    /// is not `System` (also an admitted impersonation): its transaction is refused when it opens
    /// (A4); stage for the caller's own environment instead.
    pub async fn stage(
        &self,
        env: &Env,
        body: &mut (dyn AsyncRead + Send + Unpin),
        request: StageRequest,
    ) -> Result<StagedUpload> {
        let rt = runtime(env)?;
        if let Some(name) = &request.name {
            check_name(name)?;
        }
        let (file_limit, reserved) = request.byte_limits(rt.limits().max_file_bytes)?;
        let _permit = rt.try_stream_permit().map_err(blob_error)?;
        let declared = request
            .declared_mimetype
            .as_deref()
            .map(erp_sdk::blob::normalize_declared)
            .transpose()
            .map_err(|e| Error::validation(e.to_string()))?;
        // Start before BEGIN: PostgreSQL now() for expires_at is no earlier than this point.
        // A stalled receive must stop before an expired intent becomes eligible for cleanup.
        let deadline = tokio::time::Instant::now()
            .checked_add(std::time::Duration::from_secs(rt.limits().upload_ttl_secs))
            .ok_or_else(|| {
                Error::programming("attachments.upload_ttl_secs exceeds the monotonic clock range.")
            })?;
        let options = || BeginOptions::for_principal(env.principal().clone()).on_primary();
        // The primary READ COMMITTED transaction takes database then subject locks. It commits
        // the intent and reservation before any read; none of these locks spans the stream.
        let mut tx = self
            .orm
            .begin_opts(options().isolation(Isolation::ReadCommitted))
            .await?;
        super::topology::verify(&mut tx, &rt).await?;
        let store = store_by_key(&mut tx, &rt, rt.write_store_key()).await?;
        let uid = env.principal().subject().get();
        super::quota::admit(&mut tx, rt.limits(), uid, reserved, file_limit).await?;
        let actor = actor_text(env);
        let ttl = rt.limits().upload_ttl_secs as f64;
        let reservation = super::quota::sql_bytes(reserved)?;
        let created = tx.query(
            "INSERT INTO ir_blob_upload(store_id,subject_user,actor,expires_at,declared_name,declared_mimetype,reserved_bytes) VALUES ($1,$2,$3,now()+make_interval(secs=>$4::float8),$5,$6,$7) RETURNING id::text",
            &[&store.id,&uid,&actor,&ttl,&request.name,&declared,&reservation],
        ).await?;
        let upload_id: String = created
            .first()
            .ok_or_else(|| Error::db("The upload intent was not created."))?
            .try_get(0)
            .map_err(|e| col_error("upload id", e))?;
        tx.commit().await?;
        drop(tx);
        let result = async {
            let staged = tokio::select! {
                biased;
                _ = tokio::time::sleep_until(deadline) => Err(blob_error(erp_sdk::blob::BlobError::UploadExpired)),
                received = self.receive(&rt, &store.namespace, &upload_id, body, &request, declared.as_deref()) => received,
            }?;
            let mut tx = self.orm.begin_opts(options()).await?;
            let size = super::quota::sql_bytes(staged.size)?;
            let updated = tx.execute(
                "UPDATE ir_blob_upload SET state=1,sha256=$2,sha1=$3,size=$4,detected_mimetype=$5,inline_data=$6,reserved_bytes=$4 WHERE id=$1::text::uuid AND state=0",
                &[&upload_id,&staged.sha256.to_vec(),&staged.sha1.to_vec(),&size,&staged.mimetype,&staged.inline],
            ).await?;
            if updated != 1 { return Err(Error::internal("The upload intent left receiving state before completion; cleanup is required.")); }
            tx.commit().await?;
            Ok(StagedUpload { id: UploadId::from_db(upload_id.clone()), size: staged.size, checksum: to_hex(&staged.sha1), mimetype: staged.mimetype })
        }.await;
        match result {
            Ok(staged) => Ok(staged),
            Err(original) => {
                // Requests never delete storage or tracking rows: maintenance owns physical
                // cleanup, with the owner fence. A dropped future leaves its receiving marker.
                let cleanup = async {
                    let mut tx = self.orm.begin_opts(options()).await?;
                    let n = tx.execute("UPDATE ir_blob_upload SET state=3,cleanup_error='Receive failed; fenced cleanup pending.' WHERE id=$1::text::uuid AND state IN (0,1)", &[&upload_id]).await?;
                    if n != 1 { return Err(Error::internal("The failed upload cleanup marker is missing or already consumed.")); }
                    tx.commit().await
                }.await;
                match cleanup {
                    Ok(()) => Err(original.and_also(&Error::internal("fenced cleanup pending; the committed reservation is retained"))),
                    Err(cause) => Err(original.and_also(&Error::internal("cleanup state could not be recorded; verify the committed upload intent")).and_also(&cause)),
                }
            }
        }
    }

    async fn receive(
        &self,
        rt: &AttachmentRuntime,
        namespace: &Namespace,
        upload_id: &str,
        body: &mut (dyn AsyncRead + Send + Unpin),
        request: &StageRequest,
        declared: Option<&str>,
    ) -> Result<Received> {
        let limits = rt.limits();
        let (_, byte_limit) = request.byte_limits(limits.max_file_bytes)?;
        // read one byte past the inline threshold to learn which path this content takes
        let probe =
            (limits.inline_max_bytes.saturating_add(1) as u64).min(byte_limit.saturating_add(1));
        let mut prefix = Vec::new();
        let mut limited = (&mut *body).take(probe);
        limited
            .read_to_end(&mut prefix)
            .await
            .map_err(|e| Error::validation(format!("The upload body could not be read: {e}")))?;
        if prefix.len() as u64 > byte_limit {
            return Err(Error::validation(format!(
                "object size in bytes exceeds the limit of {byte_limit}"
            )));
        }
        let detect_name = request.name.as_deref();
        if prefix.len() <= limits.inline_max_bytes {
            // the whole body is in `prefix`
            let head = prefix.get(..DETECT_BYTES.min(prefix.len())).unwrap_or(&[]);
            let detected = resolve(declared, head, detect_name)
                .map_err(|e| Error::validation(e.to_string()))?;
            let (sha256, sha1) = erp_sdk::blob::digests(&prefix);
            return Ok(Received {
                size: prefix.len() as u64,
                sha256,
                sha1,
                mimetype: detected.mimetype,
                inline: Some(prefix),
            });
        }
        let store = rt.write_store().map_err(blob_error)?;
        let key = StagingKey::new(namespace.clone(), upload_id).map_err(blob_error)?;
        let mut chained = std::io::Cursor::new(prefix).chain(body);
        let staged = store
            .stage(&key, &mut chained, StageLimits::new(byte_limit))
            .await
            .map_err(blob_error)?;
        match resolve(declared, staged.head(), detect_name) {
            Ok(detected) => Ok(Received {
                size: staged.size(),
                sha256: *staged.sha256(),
                sha1: *staged.sha1(),
                mimetype: detected.mimetype,
                inline: None,
            }),
            Err(error) => Err(Error::validation(error.to_string())),
        }
    }
}

struct Received {
    size: u64,
    sha256: [u8; 32],
    sha1: [u8; 20],
    mimetype: String,
    inline: Option<Vec<u8>>,
}
