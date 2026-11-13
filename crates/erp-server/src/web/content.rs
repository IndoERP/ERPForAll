// SPDX-License-Identifier: MIT OR Apache-2.0
//! The authenticated attachment content route `GET|HEAD /erp/content/<id>` (
//! "Download", threat model M4 §13 `attachment` class).
//!
//! The transport owns the HTTP rules: method, query, session, ranges, headers and the body. What
//! an attachment is and who may read it belongs to the composed [`WebAttachmentContent`] (the base
//! addon's checked read, composed by the native CLI); this crate keeps no attachment knowledge.
//! Order of one request: method and query, then the browser session, then the composed lookup
//! inside that session's transaction (the access check runs there, before anything else), then
//! the transaction ends, and only then are `Range`/`If-Range` evaluated. For `GET` a stream permit
//! is then taken and the store opened; `HEAD` answers from the found row and the range decision
//! and never takes a permit or opens the store, so a store object that is missing behind a found
//! row is reported only by `GET`. A refused read and a missing id give the same `404`.
use super::*;
use std::pin::Pin;
use std::task::{Context, Poll};

pub(super) const CONTENT_PREFIX: &str = crate::routes::CONTENT_PREFIX;

/// The `Content-Security-Policy` of the §13 `attachment` class (no PDF fallback yet: TO-UPL.1).
const ATTACHMENT_CSP: &str = "default-src 'none'; sandbox";

/// Longest `Range` header value evaluated; longer is `416` (untrusted input bound).
const RANGE_HEADER_MAX: usize = 128;

/// Bytes read from the store per body frame.
const BODY_CHUNK: usize = 64 * 1024;

/// A byte stream of attachment content. It holds the node's stream admission until dropped.
pub type ContentReader = Pin<Box<dyn tokio::io::AsyncRead + Send>>;

/// One satisfiable byte range of an attachment, decided by the route after the access check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContentRange {
    start: u64,
    len: u64,
}

impl ContentRange {
    /// The first byte.
    pub fn start(&self) -> u64 {
        self.start
    }
    /// How many bytes; at least one.
    pub fn len(&self) -> u64 {
        self.len
    }
    /// Always false: a range holds at least one byte.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
    fn last(&self) -> u64 {
        self.start + (self.len - 1)
    }
}

/// Why [`ContentOpener::open`] returned no bytes.
#[derive(Debug)]
#[non_exhaustive]
pub enum ContentOpenFailure {
    /// No stream permit of this node is free: `503` with `Retry-After`, never a queue.
    Busy,
    /// The store has no object for the attachment: `500 AttachmentContentMissing`,
    /// never an empty body. Only `GET` opens the store, so only `GET` sees this.
    Missing,
    /// Any other store failure: `500`, detail in the server log only.
    Failed(erp_fields::Error),
}

/// Opens the bytes of one resolved attachment, after the session transaction ended. The route calls
/// it for `GET` only; `HEAD` drops the opener unused.
pub trait ContentOpener: Send {
    /// Take one stream permit of this node, then open the content or `range` of it.
    /// The permit lives inside the returned reader.
    fn open(
        self: Box<Self>,
        range: Option<ContentRange>,
    ) -> erp_orm::BoxFuture<'static, Result<ContentReader, ContentOpenFailure>>;
}

/// A readable attachment with stored content, as the composed lookup found it.
pub struct FoundContent {
    attachment: i64,
    name: String,
    mimetype: String,
    size: u64,
    digest: String,
    opener: Box<dyn ContentOpener>,
}

impl FoundContent {
    /// `name` is untrusted (encoded per RFC 6266 here), `mimetype` the stored detected type,
    /// `size` the content size the response announces, `digest` for logs only.
    pub fn new(
        attachment: i64,
        name: String,
        mimetype: String,
        size: u64,
        digest: String,
        opener: Box<dyn ContentOpener>,
    ) -> Self {
        Self {
            attachment,
            name,
            mimetype,
            size,
            digest,
            opener,
        }
    }
}

/// Result of [`WebAttachmentContent::lookup`].
#[non_exhaustive]
pub enum ContentLookup {
    /// The caller may read it and it has stored content.
    Found(FoundContent),
    /// Not found, not allowed, another company's record, a URL attachment or one without stored
    /// content: one `404` for all of them.
    NotFound,
    /// Readable, but the blob row or its bytes are gone.
    Missing {
        /// For the server log only.
        attachment: i64,
        /// For the server log only.
        digest: String,
    },
}

/// Trusted composition adapter for the attachment content route (the base addon's checked read).
pub trait WebAttachmentContent: Send + Sync {
    /// Resolve attachment `id` under the principal of `tx` (the caller's browser session). The
    /// implementation runs the attachment read check before it reads content columns or calls a
    /// store, and never returns an access error for a refused read: that is
    /// [`ContentLookup::NotFound`]. The route rolls `tx` back after this returns.
    fn lookup<'a>(
        &'a self,
        tx: &'a mut erp_orm::Transaction,
        id: i64,
    ) -> erp_orm::BoxFuture<'a, erp_fields::Result<ContentLookup>>;
}

/// The validated query of one request.
#[derive(Debug, Default, PartialEq, Eq)]
struct ContentQuery {
    download: bool,
    octet_stream: bool,
}

fn parse_query(query: Option<&str>) -> Result<ContentQuery, &'static str> {
    let mut parsed = ContentQuery::default();
    let mut seen_download = false;
    let mut seen_mimetype = false;
    let Some(query) = query else {
        return Ok(parsed);
    };
    for (key, value) in url::form_urlencoded::parse(query.as_bytes()) {
        match key.as_ref() {
            "download" if !seen_download => {
                seen_download = true;
                parsed.download = match value.as_ref() {
                    "true" => true,
                    "false" => false,
                    _ => return Err("The download parameter must be true or false."),
                };
            }
            "mimetype" if !seen_mimetype => {
                seen_mimetype = true;
                if value != "application/octet-stream" {
                    return Err("The mimetype parameter may only be application/octet-stream.");
                }
                parsed.octet_stream = true;
            }
            _ => {
                return Err(
                    "The content route accepts only the query parameters download and mimetype, each at most once.",
                );
            }
        }
    }
    Ok(parsed)
}

/// The attachment id of `/erp/content/<id>`: a positive decimal integer and nothing after it.
fn parse_id(rest: &str) -> Option<i64> {
    if rest.is_empty() || rest.len() > 18 || !rest.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    rest.parse::<i64>().ok().filter(|id| *id > 0)
}

/// How a request's `Range` is answered.
#[derive(Debug, PartialEq, Eq)]
enum RangeDecision {
    Full,
    Partial(ContentRange),
    Unsatisfiable(&'static str),
}

/// Decide the range of a request for content of `size` bytes. `If-Range` makes any `Range`
/// ignored (no validator exists yet, so a partial answer could splice two contents).
fn decide_range(ranges: &[&str], if_range: bool, size: u64) -> RangeDecision {
    use RangeDecision::{Full, Partial, Unsatisfiable};
    let value = match ranges {
        [] => return Full,
        _ if if_range => return Full,
        [value] => value.trim(),
        _ => return Unsatisfiable("Several byte ranges are not served; ask for one range."),
    };
    if value.len() > RANGE_HEADER_MAX {
        return Unsatisfiable("The Range header is too long.");
    }
    let Some((unit, spec)) = value.split_once('=') else {
        return Unsatisfiable("The Range header is malformed.");
    };
    if !unit.trim().eq_ignore_ascii_case("bytes") {
        return Unsatisfiable("Only byte ranges are served.");
    }
    if spec.contains(',') {
        return Unsatisfiable("Several byte ranges are not served; ask for one range.");
    }
    let Some((first, last)) = spec.trim().split_once('-') else {
        return Unsatisfiable("The Range header is malformed.");
    };
    let number = |text: &str| -> Option<u64> {
        (!text.is_empty() && text.len() <= 20 && text.bytes().all(|b| b.is_ascii_digit()))
            .then(|| text.parse().ok())
            .flatten()
    };
    let (first, last) = (first.trim(), last.trim());
    let (start, end) = match (first.is_empty(), last.is_empty()) {
        (true, true) => return Unsatisfiable("The Range header is malformed."),
        (true, false) => {
            let Some(suffix) = number(last) else {
                return Unsatisfiable("The Range header is malformed.");
            };
            if suffix == 0 || size == 0 {
                return Unsatisfiable("The requested range is outside the content.");
            }
            (size - suffix.min(size), size - 1)
        }
        (false, _) => {
            let Some(start) = number(first) else {
                return Unsatisfiable("The Range header is malformed.");
            };
            let end = if last.is_empty() {
                None
            } else {
                match number(last) {
                    Some(end) if end >= start => Some(end),
                    Some(_) => return Unsatisfiable("The Range header is malformed."),
                    None => return Unsatisfiable("The Range header is malformed."),
                }
            };
            if start >= size {
                return Unsatisfiable("The requested range is outside the content.");
            }
            (start, end.map_or(size - 1, |end| end.min(size - 1)))
        }
    };
    Partial(ContentRange {
        start,
        len: end - start + 1,
    })
}

/// Types whose served `Content-Type` would let a browser run or render active content on our
/// origin (T-UPL.2): served as `application/octet-stream`.
fn is_active(essence: &str) -> bool {
    matches!(
        essence,
        "text/html"
            | "application/xhtml+xml"
            | "image/svg+xml"
            | "text/xml"
            | "application/xml"
            | "text/xsl"
            | "application/wasm"
            | "text/x-shellscript"
            | "application/x-sh"
            | "multipart/x-mixed-replace"
    ) || essence.ends_with("+xml")
        || essence.contains("javascript")
        || essence.contains("ecmascript")
}

/// The media type essence of a stored type (`type/subtype`, lowercase, parameters dropped), or
/// `None` when it is not a well-formed type.
fn essence(mimetype: &str) -> Option<String> {
    let base = mimetype.split(';').next().unwrap_or("").trim();
    let token = |part: &str| {
        !part.is_empty()
            && part
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"!#$&-^_.+".contains(&b))
    };
    let (kind, sub) = base.split_once('/')?;
    (base.len() <= 127 && token(kind) && token(sub)).then(|| base.to_ascii_lowercase())
}

/// The served `Content-Type` (T-UPL.2).
fn served_type(stored: &str, octet_stream: bool) -> String {
    match essence(stored) {
        Some(essence) if !octet_stream && !is_active(&essence) => essence,
        _ => "application/octet-stream".to_owned(),
    }
}

/// Whether the content may be shown inline: detected PNG, JPEG, GIF, WebP and PDF only, and only
/// when neither `download=true` nor the octet-stream override was asked.
fn inline_allowed(stored: &str, query: &ContentQuery) -> bool {
    !query.download
        && !query.octet_stream
        && essence(stored).is_some_and(|essence| {
            matches!(
                essence.as_str(),
                "image/png" | "image/jpeg" | "image/gif" | "image/webp" | "application/pdf"
            )
        })
}

/// `Content-Disposition` per RFC 6266 with an ASCII `filename` fallback and the exact name in
/// `filename*` (RFC 8187). Control characters (CR, LF, NUL and the rest) never reach the header.
pub(super) fn disposition(inline: bool, name: &str) -> String {
    let cleaned: String = name.chars().filter(|c| !c.is_control()).collect();
    let cleaned = if cleaned.trim().is_empty() {
        "download".to_owned()
    } else {
        cleaned
    };
    let fallback: String = cleaned
        .chars()
        .map(|c| {
            if c.is_ascii() && c != '"' && c != '\\' && c != '%' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let mut encoded = String::with_capacity(cleaned.len() * 3);
    for byte in cleaned.bytes() {
        if byte.is_ascii_alphanumeric() || b"!#$&+-.^_`|~".contains(&byte) {
            encoded.push(char::from(byte));
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    format!(
        "{}; filename=\"{fallback}\"; filename*=UTF-8''{encoded}",
        if inline { "inline" } else { "attachment" }
    )
}

/// Stamp the §13 `attachment` class on every response of this route, errors included, after the
/// shared exit (`finish_response`) and after any substitute answer (busy, deadline, refresh).
pub(super) fn apply_attachment_class(result: &mut Response) {
    let success = result.status().is_success();
    let headers = result.headers_mut();
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(ATTACHMENT_CSP),
    );
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    headers.insert(
        "cross-origin-resource-policy",
        HeaderValue::from_static("same-origin"),
    );
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static(if success {
            "private, no-store"
        } else {
            "no-store"
        }),
    );
}

fn not_found() -> Response {
    response(StatusCode::NOT_FOUND, "Not found")
}

/// `500 AttachmentContentMissing <correlation id>`: nothing else in the body; the attachment and
/// its digest go to the log.
fn content_missing(
    request: Option<&erp_rpc::CorrelationId>,
    attachment: i64,
    digest: &str,
) -> Response {
    let correlation = request.map_or("", erp_rpc::CorrelationId::as_str);
    tracing::error!(
        target: "erp::attachment",
        attachment,
        digest,
        request_id = correlation,
        "AttachmentContentMissing: a readable attachment has no stored bytes"
    );
    response(
        StatusCode::INTERNAL_SERVER_ERROR,
        format!("AttachmentContentMissing {correlation}"),
    )
}

fn lookup_failure(error: erp_fields::Error) -> Response {
    if error.is_superseded() {
        return auth_error(error);
    }
    tracing::error!(
        target: "erp::attachment",
        kind = ?error.kind(),
        reason = %error,
        request_id = erp_orm::trace::current_request_id().as_deref(),
        "attachment content lookup failed"
    );
    internal()
}

/// Total time one download body may take: the default of `limits.stream_deadline_secs`
/// (67`, repeated:113`),
/// 600 s. A constant until that setting exists in the server configuration.
///
/// The effective bound is lower: `min(STREAM_DEADLINE, max_age + DRAIN - connection age at request
/// start)`. The connection age limit (`max_age`, default 120 s, `crates/erp-server/src/conn.rs:55`)
/// starts a graceful shutdown that drops the in-flight response after `DRAIN` (10 s,
/// `conn.rs:36`, `conn.rs:252-263`). At the defaults a healthy download longer than about 130 s is
/// cut. Lifting that for content connections is the board task.
/// It depends on `.CAP` (the per-user
/// permit cap): an exemption lets a client that stops reading hold a stream permit for up to this
/// deadline (600 s) instead of about 130 s, because the body timers fire only when hyper polls the body.
const STREAM_DEADLINE: Duration = Duration::from_secs(600);

/// Longest wait for one store read, measured from the moment that read first has to wait. A
/// constant until a configurable setting exists.
/// Time spent while hyper does not poll the body (the client reads slowly) never counts. A client that stops reading is not
/// measured here (hyper stops polling the body); the connection age limit bounds it (`conn.rs`).
const STREAM_IDLE: Duration = Duration::from_secs(60);

/// The body of a `GET`: exactly `remaining` bytes of the reader, in frames, within
/// [`STREAM_DEADLINE`] and with at most [`STREAM_IDLE`] between store reads. Content that ends
/// early, a store read error and an expired timer end the body with an error (the connection is
/// cut, the stream permit is released) and are logged with the attachment; never a short
/// successful answer.
struct ReaderBody {
    reader: ContentReader,
    remaining: u64,
    buffer: bytes::BytesMut,
    /// Most bytes one frame carries (at most [`BODY_CHUNK`]).
    chunk: usize,
    attachment: i64,
    digest: String,
    deadline: Pin<Box<tokio::time::Sleep>>,
    idle: Pin<Box<tokio::time::Sleep>>,
    /// A store read is waiting and `idle` was armed when it started.
    waiting: bool,
}

impl ReaderBody {
    fn new(reader: ContentReader, remaining: u64, attachment: i64, digest: String) -> Self {
        let now = tokio::time::Instant::now();
        let chunk = usize::try_from(remaining)
            .unwrap_or(BODY_CHUNK)
            .clamp(1, BODY_CHUNK);
        Self {
            reader,
            remaining,
            buffer: bytes::BytesMut::with_capacity(chunk),
            chunk,
            attachment,
            digest,
            deadline: Box::pin(tokio::time::sleep_until(now + STREAM_DEADLINE)),
            idle: Box::pin(tokio::time::sleep_until(now + STREAM_IDLE)),
            waiting: false,
        }
    }

    fn expired(&self, which: &'static str) -> std::io::Error {
        tracing::warn!(
            target: "erp::attachment",
            attachment = self.attachment,
            digest = %self.digest,
            remaining = self.remaining,
            "attachment download ended: {which} expired"
        );
        std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            format!("attachment download {which} expired"),
        )
    }

    fn failed(&self, error: std::io::Error) -> std::io::Error {
        tracing::error!(
            target: "erp::attachment",
            attachment = self.attachment,
            digest = %self.digest,
            remaining = self.remaining,
            reason = %error,
            "attachment content could not be streamed"
        );
        error
    }
}

impl hyper::body::Body for ReaderBody {
    type Data = bytes::Bytes;
    type Error = std::io::Error;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<hyper::body::Frame<Self::Data>, Self::Error>>> {
        let this = self.get_mut();
        if this.remaining == 0 {
            return Poll::Ready(None);
        }
        if this.deadline.as_mut().poll(cx).is_ready() {
            return Poll::Ready(Some(Err(this.expired("total deadline"))));
        }
        let want = usize::try_from(this.remaining)
            .unwrap_or(usize::MAX)
            .min(this.chunk);
        this.buffer.reserve(want);
        // the limit keeps an over-long reader from putting more than `remaining` bytes in a frame
        let mut limited = bytes::BufMut::limit(&mut this.buffer, want);
        match tokio_util::io::poll_read_buf(this.reader.as_mut(), cx, &mut limited) {
            Poll::Pending => {
                if !this.waiting {
                    // a new store read starts waiting now; backpressure before it does not count
                    this.waiting = true;
                    this.idle
                        .as_mut()
                        .reset(tokio::time::Instant::now() + STREAM_IDLE);
                }
                if this.idle.as_mut().poll(cx).is_ready() {
                    return Poll::Ready(Some(Err(this.expired("idle timeout"))));
                }
                Poll::Pending
            }
            Poll::Ready(Err(error)) => Poll::Ready(Some(Err(this.failed(error)))),
            Poll::Ready(Ok(filled)) => {
                this.waiting = false;
                if filled == 0 {
                    let error = std::io::Error::new(
                        std::io::ErrorKind::UnexpectedEof,
                        "attachment content ended before its recorded size",
                    );
                    return Poll::Ready(Some(Err(this.failed(error))));
                }
                let frame = this.buffer.split().freeze();
                this.remaining -= filled as u64;
                Poll::Ready(Some(Ok(hyper::body::Frame::data(frame))))
            }
        }
    }

    fn is_end_stream(&self) -> bool {
        self.remaining == 0
    }

    fn size_hint(&self) -> hyper::body::SizeHint {
        hyper::body::SizeHint::with_exact(self.remaining)
    }
}

fn header_value(text: &str) -> HttpResult<HeaderValue> {
    HeaderValue::from_str(text).map_err(|_| internal())
}

impl WebApp {
    /// Supply the attachment content route's composition (the base addon's checked read).
    /// Without it the route answers `503` naming the missing composition.
    pub fn with_attachment_content(mut self, content: Arc<dyn WebAttachmentContent>) -> Self {
        self.attachment_content = Some(content);
        self
    }

    /// `GET|HEAD /erp/content/<id>`; `rest` is the path after the prefix.
    pub(super) async fn attachment_content(
        &self,
        parts: &axum::http::request::Parts,
        rest: &str,
        auth: &Arc<Auth>,
    ) -> HttpResult<Response> {
        let head = if parts.method == Method::GET {
            false
        } else if parts.method == Method::HEAD {
            true
        } else {
            let mut refused = response(
                StatusCode::METHOD_NOT_ALLOWED,
                "Only GET and HEAD are served on the content route.",
            );
            refused
                .headers_mut()
                .insert(header::ALLOW, HeaderValue::from_static("GET, HEAD"));
            return Err(refused);
        };
        // `/erp/content/<id>/<filename>`, xmlids and model/id/field paths are not served here
        let id = parse_id(rest).ok_or_else(not_found)?;
        let query = parse_query(parts.uri.query()).map_err(bad)?;
        let headers = &parts.headers;
        if headers.contains_key(header::AUTHORIZATION) {
            return Err(response(
                StatusCode::UNAUTHORIZED,
                "Bearer credentials are not accepted on the content route yet; sign in with a browser session.",
            ));
        }
        let content = self.attachment_content.as_ref().ok_or_else(|| {
            response(
                StatusCode::SERVICE_UNAVAILABLE,
                "attachment content composition is unavailable",
            )
        })?;
        let secret = self
            .session(headers)?
            .ok_or_else(|| response(StatusCode::UNAUTHORIZED, "user is not connected"))?;
        let mut begun = auth
            .begin_session(&secret, CredentialOptions::new())
            .await
            .map_err(auth_error)?;
        let looked_up = content.lookup(&mut begun.tx, id).await;
        // the session transaction ends before any byte is read or any range is evaluated
        let rolled_back = begun.tx.rollback().await;
        drop(begun);
        // a lookup failure is logged first, so a failed rollback cannot hide it
        let looked_up = looked_up.map_err(lookup_failure);
        rolled_back.map_err(auth_error)?;
        let request_id = parts.extensions.get::<erp_rpc::CorrelationId>();
        let found = match looked_up? {
            ContentLookup::Found(found) => found,
            ContentLookup::NotFound => return Err(not_found()),
            ContentLookup::Missing { attachment, digest } => {
                return Err(content_missing(request_id, attachment, &digest));
            }
        };
        answer_found(found, &parts.headers, &query, head, request_id).await
    }
}

/// Answer a found attachment after the session transaction ended: the range decision, then for
/// `GET` the opener (a stream permit and the store), for `HEAD` the same headers from the found
/// row with no body; `HEAD` never calls the opener.
async fn answer_found(
    found: FoundContent,
    headers: &HeaderMap,
    query: &ContentQuery,
    head: bool,
    request_id: Option<&erp_rpc::CorrelationId>,
) -> HttpResult<Response> {
    let ranges = values(headers, "range")?;
    let decision = decide_range(&ranges, headers.contains_key(header::IF_RANGE), found.size);
    let range = match decision {
        RangeDecision::Full => None,
        RangeDecision::Partial(range) => Some(range),
        RangeDecision::Unsatisfiable(reason) => {
            let mut refused = response(StatusCode::RANGE_NOT_SATISFIABLE, reason);
            refused.headers_mut().insert(
                header::CONTENT_RANGE,
                header_value(&format!("bytes */{}", found.size))?,
            );
            return Err(refused);
        }
    };
    let FoundContent {
        attachment,
        name,
        mimetype,
        size,
        digest,
        opener,
    } = found;
    let length = range.map_or(size, |range| range.len());
    let body = if head {
        // HEAD answers from the found row: no stream permit, no store access
        drop(opener);
        Body::empty()
    } else {
        let reader = match opener.open(range).await {
            Ok(reader) => reader,
            Err(ContentOpenFailure::Busy) => {
                let mut refused = response(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "Every attachment download slot of this node is in use; retry shortly.",
                );
                refused
                    .headers_mut()
                    .insert(header::RETRY_AFTER, HeaderValue::from_static("1"));
                return Err(refused);
            }
            Err(ContentOpenFailure::Missing) => {
                return Err(content_missing(request_id, attachment, &digest));
            }
            Err(ContentOpenFailure::Failed(error)) => {
                tracing::error!(
                    target: "erp::attachment",
                    attachment,
                    digest,
                    reason = %error,
                    request_id = request_id.map(erp_rpc::CorrelationId::as_str),
                    "attachment content could not be opened"
                );
                return Err(internal());
            }
        };
        Body::new(ReaderBody::new(reader, length, attachment, digest))
    };
    let mut result = Response::new(body);
    *result.status_mut() = if range.is_some() {
        StatusCode::PARTIAL_CONTENT
    } else {
        StatusCode::OK
    };
    let headers = result.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        header_value(&served_type(&mimetype, query.octet_stream))?,
    );
    headers.insert(header::CONTENT_LENGTH, header_value(&length.to_string())?);
    headers.insert(
        header::CONTENT_DISPOSITION,
        header_value(&disposition(inline_allowed(&mimetype, query), &name))?,
    );
    headers.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    if let Some(range) = range {
        headers.insert(
            header::CONTENT_RANGE,
            header_value(&format!("bytes {}-{}/{size}", range.start(), range.last()))?,
        );
    }
    Ok(result)
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]
mod tests {
    use super::*;

    fn partial(start: u64, len: u64) -> RangeDecision {
        RangeDecision::Partial(ContentRange { start, len })
    }

    #[test]
    fn one_byte_range_is_partial_and_every_other_shape_is_refused_with_a_reason() {
        assert_eq!(decide_range(&[], false, 10), RangeDecision::Full);
        assert_eq!(decide_range(&["bytes=0-3"], false, 10), partial(0, 4));
        assert_eq!(decide_range(&["bytes=4-"], false, 10), partial(4, 6));
        assert_eq!(decide_range(&["bytes=-3"], false, 10), partial(7, 3));
        assert_eq!(decide_range(&["bytes=-30"], false, 10), partial(0, 10));
        assert_eq!(decide_range(&["bytes=5-99"], false, 10), partial(5, 5));
        assert_eq!(decide_range(&["Bytes = 2-2"], false, 10), partial(2, 1));
        let several =
            RangeDecision::Unsatisfiable("Several byte ranges are not served; ask for one range.");
        assert_eq!(decide_range(&["bytes=0-1,4-5"], false, 10), several);
        assert_eq!(
            decide_range(&["bytes=0-1", "bytes=4-5"], false, 10),
            several
        );
        let malformed = RangeDecision::Unsatisfiable("The Range header is malformed.");
        for value in [
            "bytes=",
            "bytes=-",
            "bytes=a-b",
            "bytes=5-2",
            "bytes 0-1",
            "bytes=1-2-3",
        ] {
            assert_eq!(decide_range(&[value], false, 10), malformed, "{value}");
        }
        let outside = RangeDecision::Unsatisfiable("The requested range is outside the content.");
        assert_eq!(decide_range(&["bytes=10-"], false, 10), outside);
        assert_eq!(decide_range(&["bytes=-0"], false, 10), outside);
        assert_eq!(decide_range(&["bytes=0-0"], false, 0), outside);
        assert_eq!(
            decide_range(&["items=0-1"], false, 10),
            RangeDecision::Unsatisfiable("Only byte ranges are served.")
        );
        let long = format!("bytes=0-{}", "1".repeat(RANGE_HEADER_MAX));
        assert_eq!(
            decide_range(&[long.as_str()], false, 10),
            RangeDecision::Unsatisfiable("The Range header is too long.")
        );
        // no validator exists yet: If-Range always means the full content
        assert_eq!(decide_range(&["bytes=0-3"], true, 10), RangeDecision::Full);
        assert_eq!(
            decide_range(&["bytes=0-1,4-5"], true, 10),
            RangeDecision::Full
        );
    }

    #[test]
    fn only_the_five_safe_types_are_inline_and_active_types_are_octet_stream() {
        let plain = ContentQuery::default();
        for safe in [
            "image/png",
            "image/jpeg",
            "image/gif",
            "image/webp",
            "application/pdf",
            "IMAGE/PNG; q=1",
        ] {
            assert!(inline_allowed(safe, &plain), "{safe}");
        }
        for other in [
            "image/svg+xml",
            "text/plain",
            "text/html",
            "image/bmp",
            "bogus",
        ] {
            assert!(!inline_allowed(other, &plain), "{other}");
        }
        let download = ContentQuery {
            download: true,
            octet_stream: false,
        };
        assert!(!inline_allowed("image/png", &download));
        let octet = ContentQuery {
            download: false,
            octet_stream: true,
        };
        assert!(!inline_allowed("image/png", &octet));
        for active in [
            "text/html",
            "application/xhtml+xml",
            "image/svg+xml",
            "text/xml",
            "application/xml",
            "text/javascript",
            "application/javascript",
            "application/x-javascript",
            "application/ecmascript",
            "application/wasm",
            "application/rss+xml",
            "Text/HTML; charset=utf-8",
            "not a type",
            "",
        ] {
            assert_eq!(
                served_type(active, false),
                "application/octet-stream",
                "{active}"
            );
        }
        assert_eq!(served_type("image/png", false), "image/png");
        assert_eq!(
            served_type("text/plain; charset=utf-8", false),
            "text/plain"
        );
        assert_eq!(served_type("image/png", true), "application/octet-stream");
    }

    #[test]
    fn the_query_accepts_download_and_octet_stream_only() {
        assert_eq!(parse_query(None), Ok(ContentQuery::default()));
        assert_eq!(parse_query(Some("")), Ok(ContentQuery::default()));
        assert_eq!(
            parse_query(Some("download=true")),
            Ok(ContentQuery {
                download: true,
                octet_stream: false
            })
        );
        assert_eq!(
            parse_query(Some("download=false&mimetype=application%2Foctet-stream")),
            Ok(ContentQuery {
                download: false,
                octet_stream: true
            })
        );
        assert_eq!(
            parse_query(Some("mimetype=text/html")),
            Err("The mimetype parameter may only be application/octet-stream.")
        );
        assert_eq!(
            parse_query(Some("download=yes")),
            Err("The download parameter must be true or false.")
        );
        let unknown = Err(
            "The content route accepts only the query parameters download and mimetype, each at most once.",
        );
        assert_eq!(parse_query(Some("cids=1")), unknown);
        assert_eq!(parse_query(Some("access_token=x")), unknown);
        assert_eq!(parse_query(Some("download=true&download=false")), unknown);
        assert_eq!(parse_id("42"), Some(42));
        for bad in [
            "",
            "0",
            "-1",
            "4a",
            "42/name.png",
            "1234567890123456789",
            "+4",
        ] {
            assert_eq!(parse_id(bad), None, "{bad}");
        }
    }

    #[test]
    fn the_disposition_encodes_the_name_and_never_carries_control_characters() {
        assert_eq!(
            disposition(true, "photo.png"),
            "inline; filename=\"photo.png\"; filename*=UTF-8''photo.png"
        );
        assert_eq!(
            disposition(false, "Ünïcode \"report\" 100%.pdf"),
            "attachment; filename=\"_n_code _report_ 100_.pdf\"; filename*=UTF-8''%C3%9Cn%C3%AFcode%20%22report%22%20100%25.pdf"
        );
        let injected = disposition(false, "a\r\nSet-Cookie: x=1\u{0}.txt");
        assert_eq!(
            injected,
            "attachment; filename=\"aSet-Cookie: x=1.txt\"; filename*=UTF-8''aSet-Cookie%3A%20x%3D1.txt"
        );
        assert!(HeaderValue::from_str(&injected).is_ok());
        assert_eq!(
            disposition(false, "\r\n"),
            "attachment; filename=\"download\"; filename*=UTF-8''download"
        );
    }

    /// A store reader that never yields a byte and never wakes its task.
    struct Stalled;
    impl tokio::io::AsyncRead for Stalled {
        fn poll_read(
            self: Pin<&mut Self>,
            _: &mut Context<'_>,
            _: &mut tokio::io::ReadBuf<'_>,
        ) -> Poll<std::io::Result<()>> {
            Poll::Pending
        }
    }

    /// A store reader that yields one byte every `period`, never ending.
    struct Drip {
        period: Duration,
        sleep: Pin<Box<tokio::time::Sleep>>,
    }
    impl Drip {
        fn new(period: Duration) -> Self {
            Self {
                period,
                sleep: Box::pin(tokio::time::sleep(period)),
            }
        }
    }
    impl tokio::io::AsyncRead for Drip {
        fn poll_read(
            self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &mut tokio::io::ReadBuf<'_>,
        ) -> Poll<std::io::Result<()>> {
            let this = self.get_mut();
            if this.sleep.as_mut().poll(cx).is_pending() {
                return Poll::Pending;
            }
            this.sleep
                .as_mut()
                .reset(tokio::time::Instant::now() + this.period);
            buf.put_slice(b"x");
            Poll::Ready(Ok(()))
        }
    }

    async fn next_frame(
        body: &mut ReaderBody,
    ) -> Option<Result<hyper::body::Frame<bytes::Bytes>, std::io::Error>> {
        use hyper::body::Body as _;
        std::future::poll_fn(|cx| Pin::new(&mut *body).poll_frame(cx)).await
    }

    #[tokio::test(start_paused = true)]
    async fn a_stalled_store_read_ends_the_body_at_the_idle_timeout() {
        let started = tokio::time::Instant::now();
        let mut body = ReaderBody::new(Box::pin(Stalled), 10, 7, "digest".to_owned());
        let error = next_frame(&mut body).await.unwrap().unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
        assert_eq!(
            error.to_string(),
            "attachment download idle timeout expired"
        );
        assert_eq!(started.elapsed(), STREAM_IDLE);
    }

    #[tokio::test(start_paused = true)]
    async fn a_slow_download_ends_at_the_total_deadline() {
        let started = tokio::time::Instant::now();
        let period = STREAM_IDLE / 2;
        let mut body = ReaderBody::new(Box::pin(Drip::new(period)), 1_000_000, 7, "d".to_owned());
        let mut bytes = 0_u64;
        let error = loop {
            match next_frame(&mut body).await.unwrap() {
                Ok(frame) => bytes += frame.into_data().unwrap().len() as u64,
                Err(error) => break error,
            }
        };
        assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
        assert_eq!(
            error.to_string(),
            "attachment download total deadline expired"
        );
        assert!(bytes > 0, "the idle timer was reset by each read");
        let elapsed = started.elapsed();
        assert!(
            elapsed >= STREAM_DEADLINE && elapsed < STREAM_DEADLINE + period + period,
            "{elapsed:?}"
        );
    }

    /// A store reader whose every read first waits once (and wakes at once), then yields a byte.
    struct WaitOnce {
        waited: bool,
    }
    impl tokio::io::AsyncRead for WaitOnce {
        fn poll_read(
            self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &mut tokio::io::ReadBuf<'_>,
        ) -> Poll<std::io::Result<()>> {
            let this = self.get_mut();
            if !this.waited {
                this.waited = true;
                cx.waker().wake_by_ref();
                return Poll::Pending;
            }
            this.waited = false;
            buf.put_slice(b"y");
            Poll::Ready(Ok(()))
        }
    }

    #[tokio::test(start_paused = true)]
    async fn time_between_polls_does_not_count_as_store_idle() {
        let mut body = ReaderBody::new(Box::pin(WaitOnce { waited: false }), 1, 7, "d".to_owned());
        // one byte per frame: the buffer is sized to the content, so shrink the window per frame
        body.remaining = 3;
        for _ in 0..3 {
            // hyper does not poll the body for longer than the idle timeout (backpressure)
            tokio::time::advance(STREAM_IDLE + STREAM_IDLE / 2).await;
            let frame = next_frame(&mut body).await.unwrap().unwrap();
            assert_eq!(frame.into_data().unwrap().as_ref(), b"y");
        }
        assert!(next_frame(&mut body).await.is_none());
    }

    #[tokio::test]
    async fn content_shorter_than_its_size_ends_the_body_with_an_error() {
        let reader: ContentReader = Box::pin(std::io::Cursor::new(b"abc".to_vec()));
        let mut body = ReaderBody::new(reader, 10, 7, "digest".to_owned());
        let first = next_frame(&mut body).await.unwrap().unwrap();
        assert_eq!(first.into_data().unwrap().as_ref(), b"abc");
        let error = next_frame(&mut body).await.unwrap().unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::UnexpectedEof);
        assert_eq!(
            error.to_string(),
            "attachment content ended before its recorded size"
        );
        // exact content ends cleanly
        let reader: ContentReader = Box::pin(std::io::Cursor::new(b"abc".to_vec()));
        let mut body = ReaderBody::new(reader, 3, 7, "digest".to_owned());
        assert!(next_frame(&mut body).await.unwrap().is_ok());
        assert!(next_frame(&mut body).await.is_none());
    }

    #[tokio::test]
    async fn a_reader_with_more_bytes_than_the_size_is_cut_at_the_size() {
        // a second frame runs with spare buffer capacity, so only the read limit stops the
        // reader's extra bytes from reaching it
        let content: Vec<u8> = (0..BODY_CHUNK * 3).map(|i| (i % 251) as u8).collect();
        let announced = BODY_CHUNK + 4;
        let reader: ContentReader = Box::pin(std::io::Cursor::new(content.clone()));
        let mut body = ReaderBody::new(reader, announced as u64, 7, "digest".to_owned());
        let mut sent = Vec::new();
        while let Some(frame) = next_frame(&mut body).await {
            sent.extend_from_slice(&frame.unwrap().into_data().unwrap());
        }
        assert_eq!(sent.len(), announced);
        assert_eq!(sent, content[..announced]);
    }

    /// An opener that counts its calls and serves `content`.
    struct Counting {
        calls: Arc<std::sync::atomic::AtomicUsize>,
        content: Vec<u8>,
    }
    impl ContentOpener for Counting {
        fn open(
            self: Box<Self>,
            _: Option<ContentRange>,
        ) -> erp_orm::BoxFuture<'static, Result<ContentReader, ContentOpenFailure>> {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let reader: ContentReader = Box::pin(std::io::Cursor::new(self.content));
            Box::pin(async move { Ok(reader) })
        }
    }

    fn counted(calls: &Arc<std::sync::atomic::AtomicUsize>) -> FoundContent {
        let opener = Counting {
            calls: Arc::clone(calls),
            content: b"0123456789".to_vec(),
        };
        FoundContent::new(
            7,
            "note.txt".to_owned(),
            "text/plain".to_owned(),
            10,
            "digest".to_owned(),
            Box::new(opener),
        )
    }

    #[tokio::test]
    async fn head_never_calls_the_opener_and_get_does() {
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let query = ContentQuery::default();
        let mut ranged = HeaderMap::new();
        ranged.insert("range", HeaderValue::from_static("bytes=2-5"));
        for headers in [HeaderMap::new(), ranged.clone()] {
            let head = answer_found(counted(&calls), &headers, &query, true, None)
                .await
                .unwrap();
            assert_eq!(
                calls.load(std::sync::atomic::Ordering::SeqCst),
                0,
                "HEAD took a stream permit or opened the store"
            );
            let get = answer_found(counted(&calls), &headers, &query, false, None)
                .await
                .unwrap();
            assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
            calls.store(0, std::sync::atomic::Ordering::SeqCst);
            assert_eq!(head.status(), get.status());
            assert_eq!(head.headers(), get.headers());
            use hyper::body::Body as _;
            assert!(head.body().is_end_stream(), "HEAD has no body");
        }
        let partial = answer_found(counted(&calls), &ranged, &query, true, None)
            .await
            .unwrap();
        assert_eq!(partial.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(partial.headers()[header::CONTENT_LENGTH], "4");
        assert_eq!(partial.headers()[header::CONTENT_RANGE], "bytes 2-5/10");
    }

    #[test]
    fn the_class_headers_replace_weaker_values_and_errors_are_not_cached() {
        let mut error = response(StatusCode::NOT_FOUND, "Not found");
        error.headers_mut().insert(
            header::REFERRER_POLICY,
            HeaderValue::from_static("same-origin"),
        );
        apply_attachment_class(&mut error);
        let headers = error.headers();
        assert_eq!(headers[header::CONTENT_SECURITY_POLICY], ATTACHMENT_CSP);
        assert_eq!(headers[header::REFERRER_POLICY], "no-referrer");
        assert_eq!(headers[header::X_CONTENT_TYPE_OPTIONS], "nosniff");
        assert_eq!(headers["cross-origin-resource-policy"], "same-origin");
        assert_eq!(headers[header::CACHE_CONTROL], "no-store");
        let mut ok = response(StatusCode::PARTIAL_CONTENT, "");
        apply_attachment_class(&mut ok);
        assert_eq!(ok.headers()[header::CACHE_CONTROL], "private, no-store");
    }
}
