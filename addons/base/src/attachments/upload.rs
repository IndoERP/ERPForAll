//! Staging uploads: the intent row commits first, in its own short
//! transaction on the primary pool, before the first byte reaches the store; the bytes are
//! hashed on their way into staging; the row then becomes `ready` and bound to the subject and
//! actor that staged it. A business transaction links it later ([`super::link`]).
//!
//! Every staging object has a committed row, so the cleaner never lists the store. A failed or
//! abandoned upload leaves a row that expires; [`super::gc`] removes its object.

use erp_sdk::blob::{
    AttachmentRuntime, DETECT_BYTES, Namespace, StageLimits, StagingKey, resolve, to_hex,
};
use erp_sdk::prelude::*;
use erp_sdk::{Actor, BeginOptions, Env, ErrorKind, Orm};
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
}

impl StageRequest {
    /// A request with a file name.
    pub fn named(name: impl Into<String>) -> Self {
        Self {
            name: Some(name.into()),
            declared_mimetype: None,
        }
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
    /// maximum of pending uploads; `Programming` when no attachment runtime is attached; store
    /// errors as `Internal`.
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
        // the per-node admission of streaming operations: a full node answers busy before any
        // row exists; the permit is released when the bytes have passed (or on any early return)
        let permit = rt.try_stream_permit().map_err(blob_error)?;
        let declared = match &request.declared_mimetype {
            Some(declared) => Some(
                erp_sdk::blob::normalize_declared(declared)
                    .map_err(|e| Error::validation(e.to_string()))?,
            ),
            None => None,
        };
        let uid = env.principal().subject().get();
        let actor = actor_text(env);
        let options = || BeginOptions::for_principal(env.principal().clone()).on_primary();

        // 1. intent first, in its own short transaction; the first upload of a database races to
        // register the store row, so a serialization failure repeats this short transaction
        let mut attempt = 0u32;
        let (store, upload_id) = loop {
            attempt += 1;
            match self
                .open_intent(&rt, &options, uid, &actor, &request, declared.as_deref())
                .await
            {
                Err(error) if error.sqlstate() == Some("40001") && attempt < 5 => {
                    tokio::time::sleep(std::time::Duration::from_millis(15 * u64::from(attempt)))
                        .await;
                }
                other => break other?,
            }
        };

        // 2. stream, hash, detect (a failure discards the row; expiry covers a failed discard)
        match self
            .receive(
                &rt,
                &store.namespace,
                &upload_id,
                body,
                &request,
                declared.as_deref(),
            )
            .await
        {
            Ok(staged) => {
                drop(permit);
                // 3. ready
                let mut tx = self.orm.begin_opts(options()).await?;
                let updated = tx
                    .execute(
                        "UPDATE ir_blob_upload SET state = 1, sha256 = $2, sha1 = $3, size = $4, \
                         detected_mimetype = $5, inline_data = $6 \
                         WHERE id = $1::text::uuid AND state = 0",
                        &[
                            &upload_id,
                            &staged.sha256.to_vec(),
                            &staged.sha1.to_vec(),
                            &i64::try_from(staged.size).unwrap_or(i64::MAX),
                            &staged.mimetype,
                            &staged.inline,
                        ],
                    )
                    .await?;
                if updated != 1 {
                    let rolled_back = tx.rollback().await;
                    // the row is the write-ahead marker of the staged object: without it the
                    // cleaner never finds the object, so remove it here
                    let mut message = "the upload row left the receiving state while its bytes \
                                       were staged"
                        .to_owned();
                    if staged.inline.is_none()
                        && let Err(error) =
                            self.remove_staged(&rt, &store.namespace, &upload_id).await
                    {
                        message.push_str(&format!(
                            "; its staged object could not be removed and is orphaned: {error}"
                        ));
                    }
                    rolled_back?;
                    return Err(Error::internal(message));
                }
                tx.commit().await?;
                Ok(StagedUpload {
                    id: UploadId::from_db(upload_id),
                    size: staged.size,
                    checksum: to_hex(&staged.sha1),
                    mimetype: staged.mimetype,
                })
            }
            Err(error) => {
                drop(permit);
                self.discard(env, &rt, &store.namespace, &upload_id).await;
                Err(error)
            }
        }
    }

    /// The committed intent row of one upload: store row registered, topology checked, pending
    /// cap enforced. Returns the store and the new upload id.
    async fn open_intent(
        &self,
        rt: &AttachmentRuntime,
        options: &dyn Fn() -> BeginOptions,
        uid: i64,
        actor: &str,
        request: &StageRequest,
        declared: Option<&str>,
    ) -> Result<(super::runtime::StoreRef, String)> {
        let limits = rt.limits().clone();
        let mut tx = self.orm.begin_opts(options()).await?;
        super::topology::verify(&mut tx, rt).await?;
        let store = store_by_key(&mut tx, rt, rt.write_store_key()).await?;
        let pending = tx
            .query(
                "SELECT count(*) FROM ir_blob_upload \
                 WHERE subject_user = $1 AND state IN (0, 1) AND expires_at > now()",
                &[&uid],
            )
            .await?;
        let pending: i64 = pending
            .first()
            .ok_or_else(|| Error::db("the pending upload count is missing"))?
            .try_get(0)
            .map_err(|e| col_error("upload count", e))?;
        if pending >= i64::from(limits.pending_uploads_per_subject) {
            tx.rollback().await?;
            return Err(Error::new(
                ErrorKind::ResourceExhausted,
                format!(
                    "You already have {} pending uploads, the most allowed. Link or discard them \
                     first.",
                    limits.pending_uploads_per_subject
                ),
            ));
        }
        let ttl = i64::try_from(limits.upload_ttl_secs).unwrap_or(86_400);
        let created = tx
            .query(
                "INSERT INTO ir_blob_upload (store_id, subject_user, actor, expires_at, \
                 declared_name, declared_mimetype) \
                 VALUES ($1, $2, $3, now() + make_interval(secs => $4::float8), $5, $6) \
                 RETURNING id::text",
                &[
                    &store.id,
                    &uid,
                    &actor,
                    &(ttl as f64),
                    &request.name,
                    &declared,
                ],
            )
            .await?;
        let upload_id: String = created
            .first()
            .ok_or_else(|| Error::db("the upload row was not created"))?
            .try_get(0)
            .map_err(|e| col_error("upload id", e))?;
        tx.commit().await?;
        Ok((store, upload_id))
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
        // read one byte past the inline threshold to learn which path this content takes
        let probe = limits.inline_max_bytes.saturating_add(1);
        let mut prefix = Vec::new();
        let mut limited = (&mut *body).take(probe as u64);
        limited
            .read_to_end(&mut prefix)
            .await
            .map_err(|e| Error::validation(format!("The upload body could not be read: {e}")))?;
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
            .stage(&key, &mut chained, StageLimits::new(limits.max_file_bytes))
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
            Err(error) => {
                let _ = store.delete_staged(&key).await;
                Err(Error::validation(error.to_string()))
            }
        }
    }

    /// Remove the staged object of an upload whose row is gone.
    async fn remove_staged(
        &self,
        rt: &AttachmentRuntime,
        ns: &Namespace,
        upload_id: &str,
    ) -> Result<()> {
        let store = rt.write_store().map_err(blob_error)?;
        let key = StagingKey::new(ns.clone(), upload_id).map_err(blob_error)?;
        store.delete_staged(&key).await.map_err(blob_error)
    }

    /// Remove the row and the object of an upload that failed; a failure here is covered by the
    /// row's expiry, so it is not an error of the upload.
    async fn discard(&self, env: &Env, rt: &AttachmentRuntime, ns: &Namespace, upload_id: &str) {
        if let Ok(store) = rt.write_store()
            && let Ok(key) = StagingKey::new(ns.clone(), upload_id)
        {
            let _ = store.delete_staged(&key).await;
        }
        let options = BeginOptions::for_principal(env.principal().clone()).on_primary();
        if let Ok(mut tx) = self.orm.begin_opts(options).await {
            let removed = tx
                .execute(
                    "DELETE FROM ir_blob_upload WHERE id = $1::text::uuid AND state = 0",
                    &[&upload_id],
                )
                .await;
            if removed.is_ok() {
                let _ = tx.commit().await;
            }
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
