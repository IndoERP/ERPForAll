//! Staging uploads: the intent row commits first, in its own short
//! transaction on the primary pool, before the first byte reaches the store; the bytes are
//! hashed on their way into staging; the row then becomes `ready` and bound to the subject and
//! actor that staged it. A business transaction links it later ([`super::link`]).
//!
//! Content the server produces while a business transaction is already open (a printed
//! document) is attached by [`Uploader::create`]: that transaction's snapshot predates the upload
//! row, so it links the bytes from what staging returned instead of claiming the row. While `tx`
//! is open the upload stays `ready` (`state = 1`) so the staging cleaner never races uncommitted
//! blob rows; after `tx` commits it transitions to `consumed` (`state = 2`, freeing the subject's
//! pending slot and inline reservation), and if `tx` rolls back it transitions to `failed`
//! (`state = 3`) so the cleaner adopts and sweeps any promoted key.
//!
//! Every staging object has a committed row, so the cleaner never lists the store. A failed or
//! abandoned upload retains its reservation; fenced [`super::gc`] maintenance removes its object.

use std::sync::OnceLock;

use erp_sdk::blob::{
    AttachmentRuntime, DETECT_BYTES, Namespace, StageLimits, StagingKey, resolve, to_hex,
};
use erp_sdk::prelude::*;
use erp_sdk::{Actor, BeginOptions, Env, Isolation, Orm, Transaction};
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::sync::watch;

use super::runtime::{blob_error, col_error, retryable, runtime, store_by_key};
use super::types::{AttachmentContent, NAME_MAX_BYTES, NewAttachment, UploadId};

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

/// What staging wrote, kept in memory for a link in a transaction that cannot see the upload row
/// ([`Uploader::create`]).
#[derive(Clone)]
pub(crate) struct StagedContent {
    pub(crate) upload: UploadId,
    pub(crate) subject: i64,
    pub(crate) actor: String,
    pub(crate) store_id: i16,
    pub(crate) sha256: [u8; 32],
    pub(crate) sha1: [u8; 20],
    pub(crate) size: u64,
    pub(crate) mimetype: String,
    /// The bytes, when they stayed in the upload row (at most the inline threshold).
    pub(crate) inline: Option<Vec<u8>>,
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

static OUTCOME_IN_FLIGHT: OnceLock<watch::Sender<usize>> = OnceLock::new();

fn outcome_watch() -> &'static watch::Sender<usize> {
    OUTCOME_IN_FLIGHT.get_or_init(|| watch::channel(0).0)
}

struct InFlightGuard(&'static watch::Sender<usize>);

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        self.0.send_modify(|n| *n = n.saturating_sub(1));
    }
}

fn spawn_outcome_update(
    orm: Orm,
    env: Env,
    upload_id: UploadId,
    state: i16,
    cleanup_error: Option<&'static str>,
) {
    let Ok(handle) = tokio::runtime::Handle::try_current() else {
        return;
    };
    let tx = outcome_watch();
    tx.send_modify(|n| *n += 1);
    let guard = InFlightGuard(tx);
    handle.spawn(async move {
        let _guard = guard;
        mark_upload_state(&orm, &env, &upload_id, state, cleanup_error).await;
    });
}

/// Wait for all in-flight post-commit/post-rollback upload state updates spawned by
/// [`Uploader::create`]. Concurrent callers all wait until the in-flight counter reaches zero.
pub(crate) async fn drain_outcomes() {
    let mut rx = outcome_watch().subscribe();
    let _ = rx.wait_for(|count| *count == 0).await;
}

/// Defers the `ir_blob_upload` state transition of [`Uploader::create`] until `tx` ends:
/// on commit (`after_commit` runs after `COMMIT`) -> `state = 2` (consumed, releases pending slot);
/// on rollback or drop (`after_commit` is cleared after `ROLLBACK`) -> `state = 3` (failed, cleaner
/// adopts any promoted external key without racing an open transaction).
struct UploadOutcomeGuard {
    orm: Orm,
    env: Env,
    upload_id: UploadId,
    committed: bool,
}

impl Drop for UploadOutcomeGuard {
    fn drop(&mut self) {
        if !self.committed {
            spawn_outcome_update(
                self.orm.clone(),
                self.env.clone(),
                self.upload_id.clone(),
                3,
                Some(
                    "Transaction rolled back after Uploader::create; left to the staging cleaner.",
                ),
            );
        }
    }
}

async fn mark_upload_state(
    orm: &Orm,
    env: &Env,
    upload: &UploadId,
    state: i16,
    cleanup_error: Option<&str>,
) {
    let updated = async {
        let mut tx = orm
            .begin_opts(BeginOptions::for_principal(env.principal().clone()).on_primary())
            .await?;
        tx.execute(
            "UPDATE ir_blob_upload \
             SET state = $2, \
                 reserved_bytes = CASE WHEN inline_data IS NOT NULL THEN 0 ELSE reserved_bytes END, \
                 cleanup_error = $3 \
             WHERE id = $1::text::uuid AND state = 1",
            &[&upload.as_str(), &state, &cleanup_error],
        )
        .await?;
        tx.commit().await
    }
    .await;
    if let Err(error) = updated {
        tracing::warn!(
            target: "erp::attachments",
            upload_id = upload.as_str(),
            target_state = state,
            %error,
            "A linked upload could not update its state after transaction outcome; it expires at its own time"
        );
    }
}

impl Uploader {
    /// An uploader over `orm`'s primary pool.
    pub fn new(orm: &Orm) -> Self {
        Self { orm: orm.clone() }
    }

    /// Wait for any in-flight post-commit/post-rollback upload state transitions to settle.
    pub async fn flush_outcomes() {
        drain_outcomes().await;
    }

    /// Mark a ready staged upload owned by `env` as failed (`state = 3`) so maintenance sweeps it
    /// immediately rather than waiting for its TTL (for example, when an HTTP request fails after
    /// staging the file part).
    pub async fn discard(&self, env: &Env, upload: &UploadId, reason: &str) {
        let uid = env.principal().subject().get();
        let actor = actor_text(env);
        let updated = async {
            let mut tx = self
                .orm
                .begin_opts(BeginOptions::for_principal(env.principal().clone()).on_primary())
                .await?;
            tx.execute(
                "UPDATE ir_blob_upload \
                 SET state = 3, \
                     reserved_bytes = CASE WHEN inline_data IS NOT NULL THEN 0 ELSE reserved_bytes END, \
                     cleanup_error = $4 \
                 WHERE id = $1::text::uuid AND subject_user = $2 AND actor = $3 AND state = 1",
                &[&upload.as_str(), &uid, &actor, &reason],
            )
            .await?;
            tx.commit().await
        }
        .await;
        if let Err(error) = updated {
            tracing::warn!(
                target: "erp::attachments",
                upload_id = upload.as_str(),
                %error,
                "Could not mark rejected staged upload for immediate cleanup; it expires at its TTL"
            );
        }
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
        Ok(self.stage_content(env, body, request).await?.0)
    }

    /// Stream `body` into the write store and attach it inside `tx`, which may have started
    /// before the bytes were staged: the content a server produces during a business transaction
    /// (a printed document, a generated export). The attachment is created as by
    /// [`super::create`] from `item`, whose content must be [`AttachmentContent::Empty`]: the
    /// content is `body`, of `length` bytes when known (the reservation; `None` reserves the
    /// file limit).
    ///
    /// The upload is staged in transactions of its own, as by [`Self::stage`], and linked from
    /// what staging returned, never claimed in `tx` (whose snapshot cannot see a row committed
    /// after it). While `tx` (or its enclosing savepoint) is open the upload stays `ready`
    /// (`state = 1`) so the staging cleaner never races uncommitted blob rows; on commit it moves
    /// to `consumed` (`state = 2`, freeing the pending slot and inline reservation), and on
    /// savepoint or transaction rollback it moves to `failed` (`state = 3`) so the cleaner adopts
    /// and sweeps any promoted key.
    ///
    /// The write store's row must be older than `tx` (the new blob row references it). Only the
    /// first content ever stored in a database can find it missing: the row is then registered
    /// in a transaction of its own and the call fails with the retryable `Concurrency` error,
    /// before anything is staged, so a retry of the work succeeds.
    ///
    /// # Errors
    ///
    /// `Programming` when `item` has content; `Concurrency` (retryable) when the write store was
    /// registered after `tx` began; as [`Self::stage`] and [`super::create`].
    pub async fn create(
        &self,
        tx: &mut Transaction,
        item: NewAttachment,
        body: &mut (dyn AsyncRead + Send + Unpin),
        length: Option<u64>,
    ) -> Result<RecId> {
        if item.content != AttachmentContent::Empty {
            return Err(Error::programming(
                "Uploader::create streams the content from its body: give the attachment no \
                 content (AttachmentContent::Empty).",
            ));
        }
        let env = tx.env();
        super::service::validate_staged_item(tx, &item).await?;
        self.require_visible_store(tx, &env).await?;
        let mut request = StageRequest::named(item.name.clone());
        request
            .declared_mimetype
            .clone_from(&item.declared_mimetype);
        if let Some(bytes) = length {
            request = request.with_declared_length(bytes);
        }
        let (staged, content) = self.stage_content(&env, body, request).await?;
        match super::service::create_staged(tx, item, content).await {
            Ok(id) => {
                // Keep state = 1 while `tx` is open so clean_staging never races uncommitted
                // `ir_blob` rows; transition to state = 2 after COMMIT or state = 3 after ROLLBACK.
                let mut guard = UploadOutcomeGuard {
                    orm: self.orm.clone(),
                    env: env.clone(),
                    upload_id: staged.id,
                    committed: false,
                };
                tx.after_commit(move || {
                    guard.committed = true;
                    spawn_outcome_update(
                        guard.orm.clone(),
                        guard.env.clone(),
                        guard.upload_id.clone(),
                        2,
                        None,
                    );
                });
                Ok(id)
            }
            Err(error) => {
                // `create_staged`'s savepoint already rolled back, so marking state = 3 now cannot
                // race uncommitted `ir_blob` rows.
                mark_upload_state(
                    &self.orm,
                    &env,
                    &staged.id,
                    3,
                    Some("Link failed in Uploader::create; left to the staging cleaner."),
                )
                .await;
                Err(error)
            }
        }
    }

    /// Refuse, retryably, when `tx` cannot see the write store's row; verify topology and register
    /// it first so the retry can.
    async fn require_visible_store(&self, tx: &mut Transaction, env: &Env) -> Result<()> {
        let rt = runtime(env)?;
        let key = rt.write_store_key();
        let visible = tx
            .query(
                "SELECT 1 FROM ir_blob_store WHERE store_key = $1 AND role = 'own'",
                &[&key],
            )
            .await?;
        if !visible.is_empty() {
            return Ok(());
        }
        let mut own = self
            .orm
            .begin_opts(
                BeginOptions::for_principal(env.principal().clone())
                    .on_primary()
                    .isolation(Isolation::ReadCommitted),
            )
            .await?;
        super::topology::verify(&mut own, &rt).await?;
        store_by_key(&mut own, &rt, key).await?;
        own.commit().await?;
        Err(retryable(
            "the attachment store was registered after this transaction began",
        ))
    }

    async fn stage_content(
        &self,
        env: &Env,
        body: &mut (dyn AsyncRead + Send + Unpin),
        request: StageRequest,
    ) -> Result<(StagedUpload, StagedContent)> {
        drain_outcomes().await;
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
        let started = tokio::time::Instant::now();
        let options = || BeginOptions::for_principal(env.principal().clone()).on_primary();
        // The primary READ COMMITTED transaction takes database then subject locks. It commits
        // the intent and reservation before any read; none of these locks spans the stream.
        let mut tx = self
            .orm
            .begin_opts(options().isolation(Isolation::ReadCommitted))
            .await?;
        let policy = super::settings::Policy::load(&mut tx).await?;
        // An absent database override preserves the validated node configuration's legacy value.
        let receive_secs = policy
            .receive
            .unwrap_or(rt.limits().upload_receive_timeout_secs)
            .min(rt.limits().upload_ttl_secs);
        let idle_secs = policy.idle.unwrap_or(rt.limits().upload_idle_timeout_secs);
        let deadline = started
            .checked_add(std::time::Duration::from_secs(receive_secs))
            .ok_or_else(|| {
                Error::programming("Attachment receive deadline exceeds the monotonic clock range.")
            })?;
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
        let mut body =
            super::upload_reader::IdleReader::new(body, std::time::Duration::from_secs(idle_secs));
        let result = async {
            let staged = tokio::select! {
                biased;
                _ = tokio::time::sleep_until(deadline) => Err(blob_error(
                    if receive_secs == rt.limits().upload_ttl_secs { erp_sdk::blob::BlobError::UploadExpired }
                    else { erp_sdk::blob::BlobError::UploadReceiveTimeout }
                )),
                received = self.receive(&rt, &store.namespace, &upload_id, &mut body, &request, declared.as_deref()) => {
                    if body.timed_out() {
                        if let Err(cause)=&received {
                            tracing::error!(target: "erp::attachments", upload_id, error = %cause,
                                "Upload idle timeout interrupted receiving; store diagnostics retained internally");
                        }
                        Err(blob_error(erp_sdk::blob::BlobError::UploadIdleTimeout))
                    } else { received }
                },
            }?;
            let mut tx = self.orm.begin_opts(options()).await?;
            let size = super::quota::sql_bytes(staged.size)?;
            let updated = tx.execute(
                "UPDATE ir_blob_upload SET state=1,sha256=$2,sha1=$3,size=$4,detected_mimetype=$5,inline_data=$6,reserved_bytes=$4 WHERE id=$1::text::uuid AND state=0",
                &[&upload_id,&staged.sha256.to_vec(),&staged.sha1.to_vec(),&size,&staged.mimetype,&staged.inline],
            ).await?;
            if updated != 1 { return Err(Error::internal("The upload intent left receiving state before completion; cleanup is required.")); }
            tx.commit().await?;
            let id = UploadId::from_db(upload_id.clone());
            let upload = StagedUpload { id: id.clone(), size: staged.size, checksum: to_hex(&staged.sha1), mimetype: staged.mimetype.clone() };
            let content = StagedContent { upload: id, subject: uid, actor: actor.clone(), store_id: store.id, sha256: staged.sha256, sha1: staged.sha1, size: staged.size, mimetype: staged.mimetype, inline: staged.inline };
            Ok((upload, content))
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
                    Ok(()) => Err(original),
                    Err(cause) => {
                        tracing::error!(target: "erp::attachments", upload_id, error = %cause, "Failed receive could not record its cleanup state; committed intent requires recovery");
                        Err(original.and_also(&Error::internal(
                            "The upload could not be finalized. Try again shortly.",
                        )))
                    }
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
