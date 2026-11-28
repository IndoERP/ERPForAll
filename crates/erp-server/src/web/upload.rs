// SPDX-License-Identifier: MIT OR Apache-2.0
//! Authenticated streaming attachment upload route `POST /erp/attachments/upload`.
//!
//! Accepts a single `multipart/form-data` `file` part from a browser session (same-origin fetch
//! plus CSRF token), checks `ir.attachment` create access and the effective file size limit in a
//! short transaction before reading body bytes, and streams the part directly into the composed
//! [`WebAttachmentUpload`] adapter (`attachments::Uploader::stage`) with bounded memory.
//!
//! Concurrency and memory defenses:
//! - Runs outside the 16-permit RPC semaphore so slow uploads cannot starve interactive RPCs.
//! - Enforces both a node-wide upload slot cap and a per-user concurrent upload cap
//!   (`StartupLimits::streams_per_user`, matching `GET /erp/content/<id>`).
//! - Wraps the HTTP body stream in a phase-aware byte budget (`FORM_OVERHEAD` during preamble and
//!   part headers, expanded by `max_file_bytes` during the `file` body, clamped to
//!   `TRAILING_OVERHEAD` after the file completes) so `multer` can never buffer `max_file_bytes`
//!   in heap memory while scanning for boundaries or header terminators.
//! - Bounds both `next_field()` header/epilogue waits with the stream idle timeout and discards
//!   staged uploads if trailing multipart framing is rejected.
use std::collections::BTreeMap;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::task::{Context, Poll};
use std::time::Duration;

use super::*;
use bytes::Bytes;
use erp_fields::Result as DomainResult;
use erp_orm::{BoxFuture, Env, Orm, Transaction};
use futures_util::Stream;
use serde::Serialize;

/// Maximum bytes delivered per poll during header/epilogue parsing so an eager second poll inside
/// `multer` does not exhaust `FORM_OVERHEAD` on a single large HTTP frame.
const HEADER_STEP: u64 = 4 * 1024;
/// Multipart framing allowance before the `file` part headers complete.
const FORM_OVERHEAD: u64 = 16 * 1024;
/// Multipart framing allowance after the `file` part finishes (closing boundary + CRLF).
const TRAILING_OVERHEAD: u64 = 4 * 1024;
/// Maximum concurrent streaming uploads allowed across one server node.
const MAX_NODE_UPLOADS: usize = 32;

/// Borrowed async reader streaming the uploaded file bytes directly from the HTTP body.
pub type UploadReader<'a> = &'a mut (dyn tokio::io::AsyncRead + Send + Unpin);

/// Result of the pre-stream admission check in the caller's session transaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UploadAdmission {
    /// Effective maximum size in bytes for a single uploaded attachment.
    pub max_file_bytes: u64,
}

/// Validated metadata for one streamed upload handed to [`WebAttachmentUpload::stage`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UploadSpec {
    /// Untrusted filename declared by the browser's multipart part (checked by staging).
    pub name: String,
    /// Optional MIME media type declared by the multipart part.
    pub declared_mimetype: Option<String>,
    /// Effective file size limit enforced for this upload.
    pub file_limit: u64,
}

/// Nonsecret summary returned as JSON after staging succeeds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct StagedUploadSummary {
    /// Opaque staged upload identifier bound to the caller's subject and actor.
    pub upload_id: String,
    /// Actual stored byte length.
    pub size: u64,
    /// Hex SHA-1 digest of the stored content.
    pub checksum: String,
    /// Server-detected MIME media type.
    pub mimetype: String,
}

/// Trusted composition adapter for `POST /erp/attachments/upload` (the base addon's uploader).
pub trait WebAttachmentUpload: Send + Sync {
    /// Verify the caller may create attachments and load the effective `max_file_bytes` policy.
    /// The route rolls the transaction back before reading any upload bytes.
    fn admit<'a>(&'a self, tx: &'a mut Transaction)
    -> BoxFuture<'a, DomainResult<UploadAdmission>>;

    /// Stream `body` into attachment staging under `env` and return the staged upload summary.
    fn stage<'a>(
        &'a self,
        orm: &'a Orm,
        env: &'a Env,
        body: UploadReader<'a>,
        spec: UploadSpec,
    ) -> BoxFuture<'a, DomainResult<StagedUploadSummary>>;

    /// Mark an already-staged upload as failed when trailing HTTP request validation fails.
    fn discard<'a>(&'a self, orm: &'a Orm, env: &'a Env, upload_id: &'a str) -> BoxFuture<'a, ()> {
        let _ = (orm, env, upload_id);
        Box::pin(async {})
    }
}

/// Per-user and node-wide concurrency gate for streaming uploads. Dropping the guard frees both
/// slots even when a connection closes or times out mid-stream.
pub(super) struct Uploads {
    active: Arc<Mutex<BTreeMap<i64, u64>>>,
    per_user: u64,
    slots: Arc<Semaphore>,
    deadline: Duration,
    idle: Duration,
}

impl Uploads {
    pub(super) fn new(limits: &crate::startup_limits::StartupLimits) -> Self {
        Self {
            active: Arc::default(),
            per_user: limits.streams_per_user(),
            slots: Arc::new(Semaphore::new(MAX_NODE_UPLOADS)),
            deadline: Duration::from_secs(limits.stream_deadline_secs()),
            idle: Duration::from_secs(limits.stream_idle_secs()),
        }
    }

    pub(super) fn deadline(&self) -> Duration {
        self.deadline
    }

    pub(super) fn idle(&self) -> Duration {
        self.idle
    }

    fn admit(&self, uid: i64) -> HttpResult<UserUpload> {
        let slot = Arc::clone(&self.slots).try_acquire_owned().map_err(|_| {
            let mut refusal = response(
                StatusCode::SERVICE_UNAVAILABLE,
                "Attachment upload capacity reached; try again shortly.",
            );
            refusal
                .headers_mut()
                .insert(header::RETRY_AFTER, HeaderValue::from_static("1"));
            refusal
        })?;
        let mut users = self.active.lock().unwrap_or_else(PoisonError::into_inner);
        let count = users.entry(uid).or_default();
        if *count >= self.per_user {
            let mut refusal = response(
                StatusCode::TOO_MANY_REQUESTS,
                "Attachment upload limit reached for this user.",
            );
            refusal
                .headers_mut()
                .insert(header::RETRY_AFTER, HeaderValue::from_static("1"));
            return Err(refusal);
        }
        *count += 1;
        Ok(UserUpload {
            users: Arc::clone(&self.active),
            uid,
            _slot: slot,
        })
    }
}

struct UserUpload {
    users: Arc<Mutex<BTreeMap<i64, u64>>>,
    uid: i64,
    _slot: tokio::sync::OwnedSemaphorePermit,
}

impl Drop for UserUpload {
    fn drop(&mut self) {
        let mut users = self.users.lock().unwrap_or_else(PoisonError::into_inner);
        if let std::collections::btree_map::Entry::Occupied(mut user) = users.entry(self.uid) {
            *user.get_mut() -= 1;
            if *user.get() == 0 {
                user.remove();
            }
        }
    }
}

/// Phase-aware byte budget shared between the multipart driver and the underlying HTTP body stream.
#[derive(Clone)]
struct StreamBudget {
    remaining: Arc<AtomicU64>,
    step_limit: Arc<AtomicU64>,
    exceeded: Arc<AtomicBool>,
}

impl StreamBudget {
    fn new(initial: u64, step_limit: u64) -> Self {
        Self {
            remaining: Arc::new(AtomicU64::new(initial)),
            step_limit: Arc::new(AtomicU64::new(step_limit)),
            exceeded: Arc::new(AtomicBool::new(false)),
        }
    }

    fn enter_body(&self, extra: u64) {
        self.step_limit.store(u64::MAX, Ordering::Release);
        let _ = self
            .remaining
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |cur| {
                Some(cur.saturating_add(extra))
            });
    }

    fn enter_epilogue(&self, cap: u64) {
        self.step_limit.store(HEADER_STEP, Ordering::Release);
        let _ = self
            .remaining
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |cur| {
                Some(cur.min(cap))
            });
    }

    fn was_exceeded(&self) -> bool {
        self.exceeded.load(Ordering::Acquire)
    }

    fn take_up_to(&self, max: u64) -> Option<u64> {
        let step = self.step_limit.load(Ordering::Acquire);
        let mut cur = self.remaining.load(Ordering::Acquire);
        loop {
            if cur == 0 {
                self.exceeded.store(true, Ordering::Release);
                return None;
            }
            let take = cur.min(max).min(step);
            match self.remaining.compare_exchange_weak(
                cur,
                cur - take,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return Some(take),
                Err(next) => cur = next,
            }
        }
    }
}

struct BudgetedStream<S> {
    inner: Pin<Box<S>>,
    pending: Bytes,
    budget: StreamBudget,
    /// Pause once after delivering a header/epilogue slice so `multer`'s greedy drain loop checks
    /// for `\r\n\r\n` or EOF before pulling another chunk against `FORM_OVERHEAD`.
    pause_next: bool,
}

impl<S> BudgetedStream<S> {
    fn new(inner: S, budget: StreamBudget) -> Self {
        Self {
            inner: Box::pin(inner),
            pending: Bytes::new(),
            budget,
            pause_next: false,
        }
    }
}

impl<S, E> Stream for BudgetedStream<S>
where
    S: Stream<Item = Result<Bytes, E>>,
    E: std::error::Error + Send + Sync + 'static,
{
    type Item = Result<Bytes, std::io::Error>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if self.pause_next {
            self.pause_next = false;
            if self.budget.step_limit.load(Ordering::Acquire) < u64::MAX {
                cx.waker().wake_by_ref();
                return Poll::Pending;
            }
        }
        loop {
            if !self.pending.is_empty() {
                let len = self.pending.len() as u64;
                let Some(take) = self.budget.take_up_to(len) else {
                    return Poll::Ready(Some(Err(std::io::Error::other("upload frame limit"))));
                };
                let chunk = self.pending.split_to(take as usize);
                if self.budget.step_limit.load(Ordering::Acquire) < u64::MAX {
                    self.pause_next = true;
                }
                return Poll::Ready(Some(Ok(chunk)));
            }
            match self.inner.as_mut().poll_next(cx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(None) => return Poll::Ready(None),
                Poll::Ready(Some(Err(err))) => {
                    return Poll::Ready(Some(Err(std::io::Error::other(err))));
                }
                Poll::Ready(Some(Ok(chunk))) => {
                    if chunk.is_empty() {
                        continue;
                    }
                    self.pending = chunk;
                }
            }
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum MulterFailure {
    TooLarge,
    UnknownField,
    StreamRead,
    Invalid,
}

fn classify_multer(error: &multer::Error) -> MulterFailure {
    tracing::debug!(error = %error, "multipart upload form rejected");
    match error {
        multer::Error::FieldSizeExceeded { .. } | multer::Error::StreamSizeExceeded { .. } => {
            MulterFailure::TooLarge
        }
        multer::Error::UnknownField { .. } => MulterFailure::UnknownField,
        multer::Error::StreamReadFailed(_) => MulterFailure::StreamRead,
        _ => MulterFailure::Invalid,
    }
}

fn multer_response(failure: MulterFailure, max_file_bytes: u64) -> Response {
    match failure {
        MulterFailure::TooLarge => response(
            StatusCode::PAYLOAD_TOO_LARGE,
            format!(
                "The uploaded file exceeds the maximum allowed size of {} bytes.",
                max_file_bytes
            ),
        ),
        MulterFailure::UnknownField => bad("Expected a single file part."),
        MulterFailure::StreamRead => bad("The upload stream ended unexpectedly."),
        MulterFailure::Invalid => bad("Invalid multipart upload form."),
    }
}

trait UploadSink: Send + Sync {
    fn stage<'a>(
        &'a self,
        body: UploadReader<'a>,
        spec: UploadSpec,
    ) -> BoxFuture<'a, DomainResult<StagedUploadSummary>>;

    fn discard<'a>(&'a self, upload_id: &'a str) -> BoxFuture<'a, ()>;
}

struct AdapterSink<'s> {
    adapter: &'s dyn WebAttachmentUpload,
    orm: &'s Orm,
    env: &'s Env,
}

impl UploadSink for AdapterSink<'_> {
    fn stage<'a>(
        &'a self,
        body: UploadReader<'a>,
        spec: UploadSpec,
    ) -> BoxFuture<'a, DomainResult<StagedUploadSummary>> {
        self.adapter.stage(self.orm, self.env, body, spec)
    }

    fn discard<'a>(&'a self, upload_id: &'a str) -> BoxFuture<'a, ()> {
        self.adapter.discard(self.orm, self.env, upload_id)
    }
}

async fn stream_multipart_upload(
    content_type: &str,
    body: Body,
    max_file_bytes: u64,
    phase_timeout: Duration,
    sink: &dyn UploadSink,
) -> Result<StagedUploadSummary, StreamUploadError> {
    let boundary = multer::parse_boundary(content_type).map_err(|_| {
        StreamUploadError::Http(response(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "Expected multipart/form-data with a boundary.",
        ))
    })?;
    let limits = multer::SizeLimit::new()
        .whole_stream(max_file_bytes.saturating_add(FORM_OVERHEAD))
        .for_field("file", max_file_bytes);
    let constraints = multer::Constraints::new()
        .allowed_fields(vec!["file"])
        .size_limit(limits);
    // Start with only the framing budget so malformed preambles or headers without `\r\n\r\n`
    // cannot force `multer` to buffer `max_file_bytes` in memory.
    let budget = StreamBudget::new(FORM_OVERHEAD, HEADER_STEP);
    let budgeted = BudgetedStream::new(body.into_data_stream(), budget.clone());
    let mut form = multer::Multipart::with_constraints(budgeted, boundary, constraints);
    let next = tokio::time::timeout(phase_timeout, form.next_field())
        .await
        .map_err(|_| {
            StreamUploadError::Http(response(
                StatusCode::REQUEST_TIMEOUT,
                "Timed out waiting for upload headers.",
            ))
        })?;
    let field = next
        .map_err(|err| {
            if budget.was_exceeded() {
                StreamUploadError::Http(bad("Multipart upload headers exceed the allowed size."))
            } else {
                StreamUploadError::Http(multer_response(classify_multer(&err), max_file_bytes))
            }
        })?
        .ok_or_else(|| StreamUploadError::Http(bad("Missing upload file.")))?;
    if field.name() != Some("file") {
        return Err(StreamUploadError::Http(bad("Expected a single file part.")));
    }
    let name = field
        .file_name()
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .ok_or_else(|| StreamUploadError::Http(bad("Upload file part requires a filename.")))?
        .to_owned();
    let declared_mimetype = field
        .content_type()
        .map(|mime| mime.essence_str().to_owned())
        .filter(|mime| !mime.is_empty());
    let spec = UploadSpec {
        name,
        declared_mimetype,
        file_limit: max_file_bytes,
    };

    // Expand the stream budget for the file payload now that part headers have been bounded.
    budget.enter_body(max_file_bytes);
    let failure = Arc::new(Mutex::new(None::<MulterFailure>));
    let failure_for_stream = Arc::clone(&failure);
    let budget_for_stream = budget.clone();
    let stream = Box::pin(futures_util::stream::try_unfold(field, move |mut field| {
        let failure = Arc::clone(&failure_for_stream);
        let budget = budget_for_stream.clone();
        async move {
            match field.chunk().await {
                Ok(Some(chunk)) => Ok(Some((chunk, field))),
                Ok(None) => Ok(None),
                Err(err) => {
                    let kind = if budget.was_exceeded() {
                        MulterFailure::TooLarge
                    } else {
                        classify_multer(&err)
                    };
                    *failure.lock().unwrap_or_else(PoisonError::into_inner) = Some(kind);
                    Err(std::io::Error::other("multipart field read failed"))
                }
            }
        }
    }));
    let mut reader = tokio_util::io::StreamReader::new(stream);
    let staged = sink.stage(&mut reader, spec).await;
    drop(reader);
    if let Some(stream_err) = failure
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .take()
    {
        return Err(StreamUploadError::Http(multer_response(
            stream_err,
            max_file_bytes,
        )));
    }
    let summary = staged.map_err(StreamUploadError::Domain)?;

    // Clamp remaining bytes before checking for trailing parts so an unclosed second part header
    // cannot buffer `max_file_bytes` in RAM.
    budget.enter_epilogue(TRAILING_OVERHEAD);
    let trailing = tokio::time::timeout(phase_timeout, form.next_field()).await;
    match trailing {
        Ok(Ok(None)) => Ok(summary),
        Ok(Ok(Some(_))) => {
            sink.discard(&summary.upload_id).await;
            Err(StreamUploadError::Http(bad("Expected a single file part.")))
        }
        Ok(Err(err)) => {
            sink.discard(&summary.upload_id).await;
            if budget.was_exceeded() {
                Err(StreamUploadError::Http(bad(
                    "Trailing multipart upload data exceeds the allowed size.",
                )))
            } else {
                Err(StreamUploadError::Http(multer_response(
                    classify_multer(&err),
                    max_file_bytes,
                )))
            }
        }
        Err(_) => {
            sink.discard(&summary.upload_id).await;
            Err(StreamUploadError::Http(response(
                StatusCode::REQUEST_TIMEOUT,
                "Timed out waiting for end of upload.",
            )))
        }
    }
}

enum StreamUploadError {
    Http(Response),
    Domain(erp_fields::Error),
}

impl WebApp {
    /// Supply the streaming attachment upload adapter (the base addon's uploader).
    pub fn with_attachment_upload(mut self, upload: Arc<dyn WebAttachmentUpload>) -> Self {
        self.attachment_upload = Some(upload);
        self
    }

    pub(super) async fn attachment_upload(
        &self,
        parts: &axum::http::request::Parts,
        body: Body,
        auth: &Arc<Auth>,
    ) -> HttpResult<Response> {
        if parts.method != Method::POST {
            let mut err = response(StatusCode::METHOD_NOT_ALLOWED, "POST required");
            err.headers_mut()
                .insert(header::ALLOW, HeaderValue::from_static("POST"));
            return Err(err);
        }
        let headers = &parts.headers;
        boundary::same_origin_fetch(
            one(headers, "sec-fetch-site")?,
            one(headers, "origin")?,
            self.config.origin(),
        )
        .map_err(|e| response(StatusCode::FORBIDDEN, e.to_string()))?;
        if headers.contains_key(header::AUTHORIZATION) {
            return Err(response(
                StatusCode::UNAUTHORIZED,
                "Sign in with a browser session.",
            ));
        }
        let secret = self
            .session(headers)?
            .ok_or_else(|| response(StatusCode::UNAUTHORIZED, "User is not connected."))?;
        let token = one(headers, "x-csrf-token")?
            .ok_or_else(|| response(StatusCode::FORBIDDEN, "A CSRF token is required."))?;
        self.verify_csrf(headers, token).await?;
        let adapter = self.attachment_upload.as_ref().ok_or_else(|| {
            response(
                StatusCode::SERVICE_UNAVAILABLE,
                "Attachment upload composition is unavailable.",
            )
        })?;
        let content_type = one(headers, "content-type")?.unwrap_or_default();
        if multer::parse_boundary(content_type).is_err() {
            return Err(response(
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                "Expected multipart/form-data with a boundary.",
            ));
        }
        // Admit the caller and read the effective file limit before receiving upload bytes so
        // slow uploads never hold a pooled database transaction open.
        let mut begun = auth
            .begin_session(&secret, CredentialOptions::new())
            .await
            .map_err(auth_error)?;
        let env = begun.tx.env();
        let uid = env.principal().subject().get();
        let admitted = adapter.admit(&mut begun.tx).await;
        let rolled_back = begun.tx.rollback().await;
        drop(begun);
        let admission = match admitted {
            Ok(admission) => admission,
            Err(error) => return Err(self.rpc_error(error).await),
        };
        rolled_back.map_err(auth_error)?;
        let _upload_guard = self.uploads.admit(uid)?;

        let sink = AdapterSink {
            adapter: adapter.as_ref(),
            orm: auth.orm(),
            env: &env,
        };
        let summary = match stream_multipart_upload(
            content_type,
            body,
            admission.max_file_bytes,
            self.uploads.idle(),
            &sink,
        )
        .await
        {
            Ok(summary) => summary,
            Err(StreamUploadError::Http(resp)) => return Err(resp),
            Err(StreamUploadError::Domain(err)) => return Err(self.rpc_error(err).await),
        };

        let mut reply = json_response(StatusCode::OK, &summary)?;
        reply.headers_mut().insert(
            header::CACHE_CONTROL,
            HeaderValue::from_static("private, no-store"),
        );
        reply.headers_mut().insert(
            header::X_CONTENT_TYPE_OPTIONS,
            HeaderValue::from_static("nosniff"),
        );
        Ok(reply)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncReadExt;

    type MultipartPart<'a> = (&'a str, Option<&'a str>, Option<&'a str>, &'a [u8]);

    fn multipart_body(parts: &[MultipartPart<'_>]) -> Body {
        let mut out = Vec::new();
        for (name, filename, content_type, data) in parts {
            out.extend_from_slice(b"--boundary\r\n");
            out.extend_from_slice(
                format!("Content-Disposition: form-data; name=\"{name}\"").as_bytes(),
            );
            if let Some(filename) = filename {
                out.extend_from_slice(format!("; filename=\"{filename}\"").as_bytes());
            }
            out.extend_from_slice(b"\r\n");
            if let Some(ct) = content_type {
                out.extend_from_slice(format!("Content-Type: {ct}\r\n").as_bytes());
            }
            out.extend_from_slice(b"\r\n");
            out.extend_from_slice(data);
            out.extend_from_slice(b"\r\n");
        }
        out.extend_from_slice(b"--boundary--\r\n");
        Body::from(out)
    }

    struct TestSink {
        expected_name: Option<String>,
        expected_mime: Option<String>,
        expected_limit: Option<u64>,
        upload_id: String,
        allow_stage: bool,
        discarded: Arc<Mutex<Vec<String>>>,
    }

    impl TestSink {
        fn accepting(upload_id: &str) -> Self {
            Self {
                expected_name: None,
                expected_mime: None,
                expected_limit: None,
                upload_id: upload_id.to_owned(),
                allow_stage: true,
                discarded: Arc::default(),
            }
        }

        fn forbidden() -> Self {
            Self {
                expected_name: None,
                expected_mime: None,
                expected_limit: None,
                upload_id: String::new(),
                allow_stage: false,
                discarded: Arc::default(),
            }
        }
    }

    impl UploadSink for TestSink {
        fn stage<'a>(
            &'a self,
            body: UploadReader<'a>,
            spec: UploadSpec,
        ) -> BoxFuture<'a, DomainResult<StagedUploadSummary>> {
            Box::pin(async move {
                assert!(self.allow_stage, "staging must not run for this request");
                if let Some(name) = &self.expected_name {
                    assert_eq!(&spec.name, name);
                }
                if let Some(mime) = &self.expected_mime {
                    assert_eq!(spec.declared_mimetype.as_deref(), Some(mime.as_str()));
                }
                if let Some(limit) = self.expected_limit {
                    assert_eq!(spec.file_limit, limit);
                }
                let mut read = Vec::new();
                body.read_to_end(&mut read)
                    .await
                    .map_err(|e| erp_fields::Error::internal(e.to_string()))?;
                Ok(StagedUploadSummary {
                    upload_id: self.upload_id.clone(),
                    size: read.len() as u64,
                    checksum: "deadbeef".to_owned(),
                    mimetype: spec
                        .declared_mimetype
                        .unwrap_or_else(|| "application/octet-stream".to_owned()),
                })
            })
        }

        fn discard<'a>(&'a self, upload_id: &'a str) -> BoxFuture<'a, ()> {
            let id = upload_id.to_owned();
            Box::pin(async move {
                self.discarded.lock().unwrap().push(id);
            })
        }
    }

    #[tokio::test]
    async fn streams_files_above_inline_limit_and_enforces_max_file_bytes() {
        const CT: &str = "multipart/form-data; boundary=boundary";
        let payload = vec![b'a'; 16 * 1024];
        let mut sink = TestSink::accepting("01950000-0000-7000-8000-000000000001");
        sink.expected_name = Some("photo.jpg".to_owned());
        sink.expected_mime = Some("image/jpeg".to_owned());
        sink.expected_limit = Some(16 * 1024);
        let summary = match stream_multipart_upload(
            CT,
            multipart_body(&[("file", Some("photo.jpg"), Some("image/jpeg"), &payload)]),
            16 * 1024,
            Duration::from_secs(5),
            &sink,
        )
        .await
        {
            Ok(summary) => summary,
            Err(StreamUploadError::Http(resp)) => {
                let status = resp.status();
                let body = to_bytes(resp.into_body(), 4096).await.unwrap_or_default();
                panic!(
                    "16 KiB upload failed with HTTP {status}: {}",
                    String::from_utf8_lossy(&body)
                );
            }
            Err(StreamUploadError::Domain(err)) => {
                panic!("16 KiB upload failed with domain error: {err}");
            }
        };
        assert_eq!(summary.size, 16 * 1024);
        assert_eq!(summary.upload_id, "01950000-0000-7000-8000-000000000001");
        assert!(sink.discarded.lock().unwrap().is_empty());

        // 1 byte over max_file_bytes is refused with 413 Payload Too Large.
        let too_big = vec![b'b'; 16 * 1024 + 1];
        let oversize_sink = TestSink::accepting("never");
        let err = match stream_multipart_upload(
            CT,
            multipart_body(&[("file", Some("big.jpg"), Some("image/jpeg"), &too_big)]),
            16 * 1024,
            Duration::from_secs(5),
            &oversize_sink,
        )
        .await
        {
            Ok(_) => panic!("oversized file must fail"),
            Err(err) => err,
        };
        match err {
            StreamUploadError::Http(resp) => {
                assert_eq!(resp.status(), StatusCode::PAYLOAD_TOO_LARGE);
            }
            StreamUploadError::Domain(e) => panic!("expected HTTP 413, got domain error: {e}"),
        }
        assert!(oversize_sink.discarded.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn rejects_oversized_preamble_before_staging_and_discards_on_extra_part() {
        const CT: &str = "multipart/form-data; boundary=boundary";
        // Unterminated header larger than FORM_OVERHEAD must fail before staging, even when
        // max_file_bytes is 128 MiB.
        let mut huge_header = b"--boundary\r\nX-Pad: ".to_vec();
        huge_header.resize(FORM_OVERHEAD as usize + 1024, b'x');
        let forbidden = TestSink::forbidden();
        let err = stream_multipart_upload(
            CT,
            Body::from(huge_header),
            128 * 1024 * 1024,
            Duration::from_secs(5),
            &forbidden,
        )
        .await
        .err()
        .unwrap();
        assert!(matches!(err, StreamUploadError::Http(r) if r.status() == StatusCode::BAD_REQUEST));

        // Extra trailing part must trigger discard of the staged upload id.
        let extra_sink = TestSink::accepting("u-extra-1");
        let err = stream_multipart_upload(
            CT,
            multipart_body(&[
                ("file", Some("one.txt"), Some("text/plain"), b"1"),
                ("file", Some("two.txt"), Some("text/plain"), b"2"),
            ]),
            1024,
            Duration::from_secs(5),
            &extra_sink,
        )
        .await
        .err()
        .unwrap();
        assert!(matches!(err, StreamUploadError::Http(r) if r.status() == StatusCode::BAD_REQUEST));
        assert_eq!(
            *extra_sink.discarded.lock().unwrap(),
            vec!["u-extra-1".to_owned()]
        );
    }

    #[tokio::test]
    async fn rejects_non_multipart_and_missing_filename_and_bounds_user_concurrency() {
        let forbidden = TestSink::forbidden();
        let err = stream_multipart_upload(
            "application/octet-stream",
            Body::from("raw"),
            1024,
            Duration::from_secs(5),
            &forbidden,
        )
        .await
        .err()
        .unwrap();
        assert!(
            matches!(err, StreamUploadError::Http(r) if r.status() == StatusCode::UNSUPPORTED_MEDIA_TYPE)
        );

        const CT: &str = "multipart/form-data; boundary=boundary";
        let err = stream_multipart_upload(
            CT,
            multipart_body(&[("file", None, None, b"abc")]),
            1024,
            Duration::from_secs(5),
            &forbidden,
        )
        .await
        .err()
        .unwrap();
        assert!(matches!(err, StreamUploadError::Http(r) if r.status() == StatusCode::BAD_REQUEST));

        let uploads = Uploads {
            active: Arc::default(),
            per_user: 2,
            slots: Arc::new(Semaphore::new(4)),
            deadline: Duration::from_secs(30),
            idle: Duration::from_secs(5),
        };
        let g1 = uploads.admit(42).unwrap();
        let g2 = uploads.admit(42).unwrap();
        let refused = uploads.admit(42).err().unwrap();
        assert_eq!(refused.status(), StatusCode::TOO_MANY_REQUESTS);
        // Another user still gets a slot while user 42 is capped.
        let _other = uploads.admit(99).unwrap();
        drop(g1);
        let _g3 = uploads.admit(42).unwrap();
        drop(g2);
    }
}
