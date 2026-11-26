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
use std::sync::atomic::AtomicU64;
use std::sync::{Mutex as StdMutex, PoisonError};
use std::task::{Context, Poll};

pub(super) const CONTENT_PREFIX: &str = crate::routes::CONTENT_PREFIX;

/// The `Content-Security-Policy` of the §13 `attachment` class (no PDF fallback yet: TO-UPL.1).
const ATTACHMENT_CSP: &str = "default-src 'none'; sandbox";

/// Longest `Range` header value evaluated; longer is `416` (untrusted input bound).
const RANGE_HEADER_MAX: usize = 128;

/// Bytes read from the store per body frame.
const BODY_CHUNK: usize = 64 * 1024;

// Process counters are emitted as structured log fields so a collector can export them
// without exposing an unauthenticated diagnostics route or recording request credentials.
static MISSING_OBJECTS: AtomicU64 = AtomicU64::new(0);
static FAILED_READS: AtomicU64 = AtomicU64::new(0);
static EXHAUSTED_LIMITS: AtomicU64 = AtomicU64::new(0);

fn next_total(counter: &AtomicU64) -> u64 {
    counter.fetch_add(1, Ordering::Relaxed).wrapping_add(1)
}

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
    /// An optional handoff for a full read after authorization. The default streams in the app.
    fn handoff<'a>(
        &'a self,
        _request: ContentHandoffRequest<'a>,
    ) -> erp_orm::BoxFuture<'a, Result<Option<ContentHandoff>, ContentOpenFailure>> {
        Box::pin(async { Ok(None) })
    }

    /// Take one stream permit of this node, then open the content or `range` of it.
    /// The permit lives inside the returned reader.
    fn open(
        self: Box<Self>,
        range: Option<ContentRange>,
    ) -> erp_orm::BoxFuture<'static, Result<ContentReader, ContentOpenFailure>>;
}

/// The media type and forced attachment disposition the HTTP transport has chosen.
pub struct ContentHandoffRequest<'a> {
    pub media_type: &'a str,
    pub attachment_disposition: &'a str,
    pub allow_presign: bool,
}

/// An internal proxy path or a short-lived remote redirect. Contains no loggable data.
pub enum ContentHandoff {
    InternalProxy(String),
    Presigned(String),
}

/// A readable attachment with stored content, as the composed lookup found it.
pub struct FoundContent {
    attachment: i64,
    name: String,
    mimetype: String,
    size: u64,
    digest: String,
    opener: Box<dyn ContentOpener>,
    sha256: Option<[u8; 32]>,
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
            sha256: None,
        }
    }
    /// Supply a strong validator from the verified blob identity.
    pub fn with_sha256(mut self, sha256: [u8; 32]) -> Self {
        self.sha256 = Some(sha256);
        self
    }
}

/// Result of [`WebAttachmentContent::lookup`].
#[non_exhaustive]
pub enum ContentLookup {
    /// A readable URL attachment. Only a same-origin link to a separately authorized binary may redirect.
    Url(String),
    /// The transport verified that both ends of the one-hop redirect are readable.
    Redirect(i64),
    /// The caller may read it and it has stored content.
    Found(FoundContent),
    /// A permitted inline field exceeds the bounded materialization size.
    TooLarge,
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

    /// Resolve a binary field under the same record and field access rules as an ORM read.
    fn lookup_field<'a>(
        &'a self,
        _tx: &'a mut erp_orm::Transaction,
        _model: &'a str,
        _id: i64,
        _field: &'a str,
        _headers_only: bool,
    ) -> erp_orm::BoxFuture<'a, erp_fields::Result<ContentLookup>> {
        Box::pin(async { Ok(ContentLookup::NotFound) })
    }

    /// Redeem a public or token grant. Return the issuer used for stream admission and a fresh checked lookup.
    fn lookup_shared<'a>(
        &'a self,
        _orm: &'a erp_orm::Orm,
        _id: i64,
        _token: Option<&'a str>,
    ) -> erp_orm::BoxFuture<'a, erp_fields::Result<Option<(i64, ContentLookup)>>> {
        Box::pin(async { Ok(None) })
    }
}

/// The validated query of one request.
#[derive(Debug, Default, PartialEq, Eq)]
struct ContentQuery {
    download: bool,
    octet_stream: bool,
    access_token: Option<zeroize::Zeroizing<String>>,
    presign_for_principal: bool,
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
            "access_token" if parsed.access_token.is_none() && value.len() <= 128 => {
                parsed.access_token = Some(zeroize::Zeroizing::new(value.into_owned()));
            }
            _ => {
                return Err(
                    "The content route accepts download, mimetype and access_token, each at most once.",
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

#[derive(Debug, PartialEq, Eq)]
enum ContentAddress<'a> {
    Attachment(i64),
    Field {
        model: &'a str,
        id: i64,
        field: &'a str,
    },
}

fn content_address(path: &str) -> Option<ContentAddress<'_>> {
    if let Some(id) = parse_id(path) {
        return Some(ContentAddress::Attachment(id));
    }
    let (model, rest) = path.split_once('/')?;
    let (id, field) = rest.split_once('/')?;
    let ident = |s: &str| {
        s.len() <= 63
            && s.as_bytes().first().is_some_and(u8::is_ascii_lowercase)
            && s.bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
    };
    if model.len() > 128 || !model.split('.').all(ident) || !ident(field) {
        return None;
    }
    Some(ContentAddress::Field {
        model,
        id: parse_id(id)?,
        field,
    })
}

// Only canonical same-origin attachment URLs can redirect. Rebuilding the Location from
// the ID keeps credentials, encoded paths and arbitrary query parameters out of the response.
fn redirect_target(url: &str, origin: &str) -> Option<i64> {
    parse_id(
        url.strip_prefix(origin.trim_end_matches('/'))?
            .strip_prefix(CONTENT_PREFIX)?,
    )
}

fn redirect_location(id: i64, query: &ContentQuery) -> String {
    let mut location = format!("{CONTENT_PREFIX}{id}");
    let mut params = url::form_urlencoded::Serializer::new(String::new());
    if query.download {
        params.append_pair("download", "true");
    }
    if query.octet_stream {
        params.append_pair("mimetype", "application/octet-stream");
    }
    let query = params.finish();
    if !query.is_empty() {
        location.push('?');
        location.push_str(&query);
    }
    location
}

/// How a request's `Range` is answered.
#[derive(Debug, PartialEq, Eq)]
enum RangeDecision {
    Full,
    Partial(ContentRange),
    Unsatisfiable(&'static str),
}

/// Decide the range after verifying any If-Range condition against the current representation.
fn decide_range(ranges: &[&str], ignore_range: bool, size: u64) -> RangeDecision {
    use RangeDecision::{Full, Partial, Unsatisfiable};
    let value = match ranges {
        [] => return Full,
        _ if ignore_range => return Full,
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
    let attachment_missing_total = next_total(&MISSING_OBJECTS);
    tracing::error!(
        target: "erp::attachment",
        attachment,
        digest,
        request_id = correlation,
        attachment_missing_total,
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
    let attachment_read_failed_total = next_total(&FAILED_READS);
    tracing::error!(
        target: "erp::attachment",
        attachment_read_failed_total,
        kind = ?error.kind(),
        reason = %error,
        request_id = erp_orm::trace::current_request_id().as_deref(),
        "attachment content lookup failed"
    );
    internal()
}

#[derive(Clone, Copy)]
struct StreamTimes {
    deadline: Duration,
    idle: Duration,
}

/// One entry per user with an admitted download. Dropping a response frees its slot
/// even when the socket closes before hyper polls the next frame.
pub(super) struct Downloads {
    active: Arc<StdMutex<BTreeMap<i64, u64>>>,
    per_user: u64,
    times: StreamTimes,
}

impl Downloads {
    pub(super) fn new(limits: &crate::startup_limits::StartupLimits) -> Self {
        Self {
            active: Arc::default(),
            per_user: limits.streams_per_user(),
            times: StreamTimes {
                deadline: Duration::from_secs(limits.stream_deadline_secs()),
                idle: Duration::from_secs(limits.stream_idle_secs()),
            },
        }
    }

    fn admit(&self, uid: i64) -> HttpResult<UserStream> {
        let mut users = self.active.lock().unwrap_or_else(PoisonError::into_inner);
        let count = users.entry(uid).or_default();
        if *count >= self.per_user {
            let attachment_limit_exhausted_total = next_total(&EXHAUSTED_LIMITS);
            tracing::warn!(
                target: "erp::attachment", limit = "streams_per_user",
                attachment_limit_exhausted_total,
                "attachment stream admission refused"
            );
            let mut refusal = response(
                StatusCode::TOO_MANY_REQUESTS,
                "Attachment download limit reached for this user.",
            );
            refusal
                .headers_mut()
                .insert(header::RETRY_AFTER, HeaderValue::from_static("1"));
            return Err(refusal);
        }
        *count += 1;
        Ok(UserStream {
            users: Arc::clone(&self.active),
            uid,
        })
    }
}

struct UserStream {
    users: Arc<StdMutex<BTreeMap<i64, u64>>>,
    uid: i64,
}
impl Drop for UserStream {
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

/// A bounded body, holding admission until completion or cancellation.
struct ReaderBody {
    reader: ContentReader,
    _user_stream: Option<UserStream>,
    idle_duration: Duration,
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
    fn new(
        reader: ContentReader,
        remaining: u64,
        attachment: i64,
        digest: String,
        times: StreamTimes,
    ) -> Self {
        let now = tokio::time::Instant::now();
        let chunk = usize::try_from(remaining)
            .unwrap_or(BODY_CHUNK)
            .clamp(1, BODY_CHUNK);
        Self {
            reader,
            _user_stream: None,
            idle_duration: times.idle,
            remaining,
            buffer: bytes::BytesMut::with_capacity(chunk),
            chunk,
            attachment,
            digest,
            deadline: Box::pin(tokio::time::sleep_until(now + times.deadline)),
            idle: Box::pin(tokio::time::sleep_until(now + times.idle)),
            waiting: false,
        }
    }

    fn expired(&mut self, which: &'static str) -> std::io::Error {
        let attachment_limit_exhausted_total = next_total(&EXHAUSTED_LIMITS);
        tracing::warn!(
            target: "erp::attachment",
            attachment = self.attachment,
            digest = %self.digest,
            remaining = self.remaining,
            limit = which,
            attachment_limit_exhausted_total,
            "attachment download ended: {which} expired"
        );
        self.remaining = 0;
        std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            format!("attachment download {which} expired"),
        )
    }

    fn failed(&mut self, error: std::io::Error) -> std::io::Error {
        let attachment_read_failed_total = next_total(&FAILED_READS);
        tracing::error!(
            target: "erp::attachment",
            attachment = self.attachment,
            digest = %self.digest,
            remaining = self.remaining,
            reason = %error,
            attachment_read_failed_total,
            "attachment content could not be streamed"
        );
        self.remaining = 0;
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
                        .reset(tokio::time::Instant::now() + this.idle_duration);
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
        client: IpAddr,
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
        let address = content_address(rest).ok_or_else(not_found)?;
        let mut query = parse_query(parts.uri.query()).map_err(bad)?;
        let headers = &parts.headers;
        let content = self.attachment_content.as_ref().ok_or_else(|| {
            response(
                StatusCode::SERVICE_UNAVAILABLE,
                "attachment content composition is unavailable",
            )
        })?;
        let token = query.access_token.as_ref().map(|value| value.as_str());
        let has_bearer = headers.contains_key(header::AUTHORIZATION);
        if has_bearer && token.is_some() {
            return Err(bad("Present one content credential."));
        }
        if token.is_some() || (!has_bearer && self.session(headers)?.is_none()) {
            let ContentAddress::Attachment(id) = address else {
                return Err(not_found());
            };
            let (uid, lookup) = content
                .lookup_shared(auth.orm(), id, token)
                .await
                .map_err(lookup_failure)?
                .ok_or_else(not_found)?;
            return self
                .content_response(parts, &query, head, uid, lookup)
                .await;
        }
        query.presign_for_principal = true;
        let bearer = if headers.contains_key(header::AUTHORIZATION) {
            Some(self.bearer_credential(headers, client, auth).await?)
        } else {
            None
        };
        let mut tx = if let Some(key) = &bearer {
            self.open_document_bearer(key).await?
        } else {
            let secret = self
                .session(headers)?
                .ok_or_else(|| response(StatusCode::UNAUTHORIZED, "user is not connected"))?;
            auth.begin_session(&secret, CredentialOptions::new())
                .await
                .map_err(auth_error)?
                .tx
        };
        let uid = tx.env().uid();
        let looked_up = if unrestricted_document_key(tx.env().principal().scope()) {
            match address {
                ContentAddress::Attachment(id) => content.lookup(&mut tx, id).await,
                ContentAddress::Field { model, id, field } => {
                    content.lookup_field(&mut tx, model, id, field, head).await
                }
            }
        } else {
            Ok(ContentLookup::NotFound)
        };
        let looked_up = match looked_up {
            Ok(ContentLookup::Url(url)) => {
                if let Some(id) = redirect_target(&url, self.config.origin()) {
                    match content.lookup(&mut tx, id).await {
                        Ok(ContentLookup::Found(_)) => Ok(ContentLookup::Redirect(id)),
                        Ok(_) => Ok(ContentLookup::NotFound),
                        Err(error) => Err(error),
                    }
                } else {
                    Ok(ContentLookup::NotFound)
                }
            }
            other => other,
        };
        let rolled_back = tx.rollback().await;
        drop(tx);
        let drop_cookie = bearer
            .as_ref()
            .is_some_and(|key| key.drop_cookie.load(Ordering::Relaxed));
        drop(bearer);
        let result = async {
            // a lookup failure is logged first, so a failed rollback cannot hide it
            let looked_up = looked_up.map_err(lookup_failure);
            rolled_back.map_err(auth_error)?;
            let mut result = looked_up?;
            // Published attachments remain public when a visitor signs in through
            // the portal. A verified bearer always stays inside its own authority.
            if !has_bearer
                && matches!(result, ContentLookup::NotFound)
                && let ContentAddress::Attachment(id) = address
                && let Some((_, public)) = content
                    .lookup_shared(auth.orm(), id, None)
                    .await
                    .map_err(lookup_failure)?
            {
                result = public;
            }
            self.content_response(parts, &query, head, uid, result)
                .await
        }
        .await;
        if drop_cookie {
            let mut result = result;
            let response = match &mut result {
                Ok(response) | Err(response) => response,
            };
            self.set_cookie(response, "session_id", "", 0)?;
            return result;
        }
        result
    }

    async fn content_response(
        &self,
        parts: &axum::http::request::Parts,
        query: &ContentQuery,
        head: bool,
        uid: i64,
        lookup: ContentLookup,
    ) -> HttpResult<Response> {
        let request_id = parts.extensions.get::<erp_rpc::CorrelationId>();
        let found = match lookup {
            ContentLookup::Found(found) => found,
            ContentLookup::Redirect(id) => {
                let mut result = response(StatusCode::TEMPORARY_REDIRECT, "");
                result.headers_mut().insert(
                    header::LOCATION,
                    header_value(&redirect_location(id, query))?,
                );
                return Ok(result);
            }
            ContentLookup::Url(_) | ContentLookup::NotFound => return Err(not_found()),
            ContentLookup::TooLarge => {
                let attachment_limit_exhausted_total = next_total(&EXHAUSTED_LIMITS);
                tracing::warn!(
                    target: "erp::attachment", limit = "field_bytes",
                    attachment_limit_exhausted_total,
                    "inline field exceeds download limit"
                );
                return Err(response(
                    StatusCode::PAYLOAD_TOO_LARGE,
                    "This field exceeds the inline download limit.",
                ));
            }
            ContentLookup::Missing { attachment, digest } => {
                return Err(content_missing(request_id, attachment, &digest));
            }
        };
        answer_found(
            found,
            &parts.headers,
            query,
            head,
            request_id,
            &self.downloads,
            uid,
        )
        .await
    }
}

/// Compare complete quoted entity tags. A comma inside a tag belongs to the opaque value.
fn entity_matches(fields: &[&str], tag: Option<&str>, weak: bool) -> bool {
    if let [field] = fields
        && field.trim() == "*"
    {
        return true;
    }
    let Some(tag) = tag else { return false };
    fields.iter().any(|field| {
        let mut rest = field.trim();
        while !rest.is_empty() {
            rest = rest.trim_start_matches([',', ' ', '\t']);
            let strong = !rest.starts_with("W/");
            let quoted = rest.strip_prefix("W/").unwrap_or(rest);
            let Some(opaque) = quoted.strip_prefix('"') else {
                break;
            };
            let Some(end) = opaque.find('"') else { break };
            let (item, tail) = quoted.split_at(end + 2);
            rest = tail.trim_start();
            if !rest.is_empty() && !rest.starts_with(',') {
                break;
            }
            if (strong || weak) && item == tag {
                return true;
            }
        }
        false
    })
}

fn content_open_failure(
    error: ContentOpenFailure,
    request_id: Option<&erp_rpc::CorrelationId>,
    attachment: i64,
    digest: &str,
) -> Response {
    match error {
        ContentOpenFailure::Busy => {
            let attachment_limit_exhausted_total = next_total(&EXHAUSTED_LIMITS);
            tracing::warn!(
                target: "erp::attachment", limit = "node_streams",
                attachment_limit_exhausted_total,
                "attachment stream admission refused"
            );
            let mut refused = response(
                StatusCode::SERVICE_UNAVAILABLE,
                "Every attachment download slot of this node is in use; retry shortly.",
            );
            refused
                .headers_mut()
                .insert(header::RETRY_AFTER, HeaderValue::from_static("1"));
            refused
        }
        ContentOpenFailure::Missing => content_missing(request_id, attachment, digest),
        ContentOpenFailure::Failed(error) => {
            let attachment_read_failed_total = next_total(&FAILED_READS);
            tracing::error!(
                target: "erp::attachment", attachment, digest, reason = %error,
                request_id = request_id.map(erp_rpc::CorrelationId::as_str),
                attachment_read_failed_total, "attachment content could not be opened"
            );
            internal()
        }
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
    downloads: &Downloads,
    uid: i64,
) -> HttpResult<Response> {
    let etag = found
        .sha256
        .map(|sha| format!("\"{}\"", crate::assets::hex(&sha)));
    // Preconditions apply only after authorization, before opening a stream or evaluating ranges.
    for (name, weak, status) in [
        ("if-match", false, StatusCode::PRECONDITION_FAILED),
        ("if-none-match", true, StatusCode::NOT_MODIFIED),
    ] {
        let fields = values(headers, name)?;
        if !fields.is_empty() && entity_matches(&fields, etag.as_deref(), weak) == weak {
            let mut result = Response::new(Body::empty());
            *result.status_mut() = status;
            if let Some(tag) = &etag {
                result
                    .headers_mut()
                    .insert(header::ETAG, header_value(tag)?);
            }
            return Ok(result);
        }
    }
    let ranges = values(headers, "range")?;
    let ignore_range = one(headers, "if-range")?
        .is_some_and(|condition| Some(condition.trim()) != etag.as_deref());
    let decision = decide_range(&ranges, ignore_range, found.size);
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
        ..
    } = found;
    let media_type = served_type(&mimetype, query.octet_stream);
    let content_disposition = disposition(inline_allowed(&mimetype, query), &name);
    let length = range.map_or(size, |range| range.len());
    let body = if head {
        // HEAD answers from the found row: no stream permit, no store access
        drop(opener);
        Body::empty()
    } else {
        let user_stream = downloads.admit(uid)?;
        let on_failure = |error| content_open_failure(error, request_id, attachment, &digest);
        // Keep representation-specific conditions and range framing in this route. An internal
        // redirect or S3 reply can use a different object identifier for its own validators.
        let full = [
            "range",
            "if-range",
            "if-match",
            "if-none-match",
            "if-modified-since",
            "if-unmodified-since",
        ]
        .into_iter()
        .all(|name| !headers.contains_key(name));
        if full {
            let forced = disposition(false, &name);
            let request = ContentHandoffRequest {
                media_type: &media_type,
                attachment_disposition: &forced,
                allow_presign: query.presign_for_principal,
            };
            let handoff = tokio::time::timeout(downloads.times.deadline, opener.handoff(request))
                .await
                .map_err(|_| {
                    on_failure(ContentOpenFailure::Failed(erp_fields::Error::internal(
                        "attachment handoff deadline expired",
                    )))
                })?
                .map_err(on_failure)?;
            if let Some(handoff) = handoff {
                let mut answer = Response::new(Body::empty());
                let (disposition, location, target) = match handoff {
                    ContentHandoff::InternalProxy(target)
                        if target.starts_with('/') && !target.starts_with("//") =>
                    {
                        answer
                            .headers_mut()
                            .insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
                        (&content_disposition, "x-accel-redirect", target)
                    }
                    ContentHandoff::Presigned(target) if query.presign_for_principal => {
                        *answer.status_mut() = StatusCode::FOUND;
                        (&forced, "location", target)
                    }
                    ContentHandoff::InternalProxy(_) | ContentHandoff::Presigned(_) => {
                        return Err(internal());
                    }
                };
                let headers = answer.headers_mut();
                let mut target = header_value(&target)?;
                target.set_sensitive(true);
                headers.insert(location, target);
                headers.insert(header::CONTENT_TYPE, header_value(&media_type)?);
                headers.insert(header::CONTENT_DISPOSITION, header_value(disposition)?);
                headers.insert(
                    header::CACHE_CONTROL,
                    HeaderValue::from_static("private, no-store"),
                );
                headers.insert(
                    "x-content-type-options",
                    HeaderValue::from_static("nosniff"),
                );
                headers.insert(
                    header::REFERRER_POLICY,
                    HeaderValue::from_static("no-referrer"),
                );
                if let Some(tag) = &etag {
                    headers.insert(header::ETAG, header_value(tag)?);
                }
                return Ok(answer);
            }
        }
        let reader = opener.open(range).await.map_err(on_failure)?;
        let mut body = ReaderBody::new(reader, length, attachment, digest, downloads.times);
        body._user_stream = Some(user_stream);
        Body::new(body)
    };
    let mut result = Response::new(body);
    *result.status_mut() = if range.is_some() {
        StatusCode::PARTIAL_CONTENT
    } else {
        StatusCode::OK
    };
    let headers = result.headers_mut();
    headers.insert(header::CONTENT_TYPE, header_value(&media_type)?);
    headers.insert(header::CONTENT_LENGTH, header_value(&length.to_string())?);
    headers.insert(
        header::CONTENT_DISPOSITION,
        header_value(&content_disposition)?,
    );
    if let Some(tag) = &etag {
        headers.insert(header::ETAG, header_value(tag)?);
    }
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

    const TIMES: StreamTimes = StreamTimes {
        deadline: Duration::from_secs(17),
        idle: Duration::from_secs(3),
    };
    fn test_downloads() -> Downloads {
        Downloads {
            active: Arc::default(),
            per_user: 1,
            times: TIMES,
        }
    }

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
        // A failed If-Range condition selects the whole representation.
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
            ..ContentQuery::default()
        };
        assert!(!inline_allowed("image/png", &download));
        let octet = ContentQuery {
            octet_stream: true,
            ..ContentQuery::default()
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
    fn the_query_admits_download_mimetype_and_one_scoped_credential() {
        assert_eq!(parse_query(None), Ok(ContentQuery::default()));
        assert_eq!(parse_query(Some("")), Ok(ContentQuery::default()));
        assert_eq!(
            parse_query(Some("download=true")),
            Ok(ContentQuery {
                download: true,
                ..ContentQuery::default()
            })
        );
        assert_eq!(
            parse_query(Some("download=false&mimetype=application%2Foctet-stream")),
            Ok(ContentQuery {
                octet_stream: true,
                ..ContentQuery::default()
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
            "The content route accepts download, mimetype and access_token, each at most once.",
        );
        assert_eq!(parse_query(Some("cids=1")), unknown);
        assert_eq!(
            parse_query(Some("access_token=x"))
                .unwrap()
                .access_token
                .as_deref()
                .map(String::as_str),
            Some("x")
        );
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
        let mut body = ReaderBody::new(Box::pin(Stalled), 10, 7, "digest".to_owned(), TIMES);
        let error = next_frame(&mut body).await.unwrap().unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
        assert_eq!(
            error.to_string(),
            "attachment download idle timeout expired"
        );
        assert_eq!(started.elapsed(), TIMES.idle);
    }

    #[tokio::test(start_paused = true)]
    async fn a_slow_download_ends_at_the_total_deadline() {
        let started = tokio::time::Instant::now();
        let period = TIMES.idle / 2;
        let mut body = ReaderBody::new(
            Box::pin(Drip::new(period)),
            1_000_000,
            7,
            "d".to_owned(),
            TIMES,
        );
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
            elapsed >= TIMES.deadline && elapsed < TIMES.deadline + period + period,
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
        let mut body = ReaderBody::new(
            Box::pin(WaitOnce { waited: false }),
            1,
            7,
            "d".to_owned(),
            TIMES,
        );
        // one byte per frame: the buffer is sized to the content, so shrink the window per frame
        body.remaining = 3;
        for _ in 0..3 {
            // hyper does not poll the body for longer than the idle timeout (backpressure)
            tokio::time::advance(TIMES.idle + TIMES.idle / 2).await;
            let frame = next_frame(&mut body).await.unwrap().unwrap();
            assert_eq!(frame.into_data().unwrap().as_ref(), b"y");
        }
        assert!(next_frame(&mut body).await.is_none());
    }

    #[tokio::test]
    async fn content_shorter_than_its_size_ends_the_body_with_an_error() {
        let reader: ContentReader = Box::pin(std::io::Cursor::new(b"abc".to_vec()));
        let mut body = ReaderBody::new(reader, 10, 7, "digest".to_owned(), TIMES);
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
        let mut body = ReaderBody::new(reader, 3, 7, "digest".to_owned(), TIMES);
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
        let mut body = ReaderBody::new(reader, announced as u64, 7, "digest".to_owned(), TIMES);
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

    struct Remote;

    impl ContentOpener for Remote {
        fn handoff<'a>(
            &'a self,
            request: ContentHandoffRequest<'a>,
        ) -> erp_orm::BoxFuture<'a, Result<Option<ContentHandoff>, ContentOpenFailure>> {
            Box::pin(async move {
                assert_eq!(request.media_type, "image/png");
                assert!(request.attachment_disposition.starts_with("attachment;"));
                Ok(request.allow_presign.then(|| {
                    ContentHandoff::Presigned(
                        "https://store.example.invalid/file?signature=test-marker".into(),
                    )
                }))
            })
        }

        fn open(
            self: Box<Self>,
            _: Option<ContentRange>,
        ) -> erp_orm::BoxFuture<'static, Result<ContentReader, ContentOpenFailure>> {
            Box::pin(async { Ok(Box::pin(std::io::Cursor::new(vec![0_u8])) as ContentReader) })
        }
    }

    #[tokio::test]
    async fn presigned_handoff_is_sensitive_attachment_and_requires_a_principal() {
        let found = || {
            FoundContent::new(
                1,
                "note.png".to_owned(),
                "image/png".to_owned(),
                1,
                "digest".to_owned(),
                Box::new(Remote),
            )
        };
        let downloads = test_downloads();
        let query = ContentQuery {
            presign_for_principal: true,
            ..ContentQuery::default()
        };
        let headers = HeaderMap::new();
        let signed = answer_found(found(), &headers, &query, false, None, &downloads, 1)
            .await
            .unwrap();
        assert_eq!(signed.status(), StatusCode::FOUND);
        assert!(signed.headers()[header::LOCATION].is_sensitive());
        assert_eq!(signed.headers()[header::CACHE_CONTROL], "private, no-store");
        assert_eq!(signed.headers()[header::CONTENT_TYPE], "image/png");
        assert!(
            signed.headers()[header::CONTENT_DISPOSITION]
                .to_str()
                .unwrap()
                .starts_with("attachment;")
        );

        let shared = answer_found(
            found(),
            &headers,
            &ContentQuery::default(),
            false,
            None,
            &downloads,
            1,
        )
        .await
        .unwrap();
        assert_eq!(shared.status(), StatusCode::OK);
        assert!(!shared.headers().contains_key(header::LOCATION));
        let mut ranged = HeaderMap::new();
        ranged.insert(header::RANGE, HeaderValue::from_static("bytes=0-0"));
        let partial = answer_found(found(), &ranged, &query, false, None, &downloads, 2)
            .await
            .unwrap();
        assert_eq!(partial.status(), StatusCode::PARTIAL_CONTENT);
        assert!(!partial.headers().contains_key(header::LOCATION));
    }

    #[tokio::test]
    async fn head_never_calls_the_opener_and_get_does() {
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let query = ContentQuery::default();
        let downloads = test_downloads();
        let mut ranged = HeaderMap::new();
        ranged.insert("range", HeaderValue::from_static("bytes=2-5"));
        for headers in [HeaderMap::new(), ranged.clone()] {
            let head = answer_found(counted(&calls), &headers, &query, true, None, &downloads, 4)
                .await
                .unwrap();
            assert_eq!(
                calls.load(std::sync::atomic::Ordering::SeqCst),
                0,
                "HEAD took a stream permit or opened the store"
            );
            let get = answer_found(
                counted(&calls),
                &headers,
                &query,
                false,
                None,
                &downloads,
                4,
            )
            .await
            .unwrap();
            assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
            calls.store(0, std::sync::atomic::Ordering::SeqCst);
            assert_eq!(head.status(), get.status());
            assert_eq!(head.headers(), get.headers());
            use hyper::body::Body as _;
            assert!(head.body().is_end_stream(), "HEAD has no body");
        }
        let partial = answer_found(counted(&calls), &ranged, &query, true, None, &downloads, 4)
            .await
            .unwrap();
        assert_eq!(partial.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(partial.headers()[header::CONTENT_LENGTH], "4");
        assert_eq!(partial.headers()[header::CONTENT_RANGE], "bytes 2-5/10");
    }

    #[tokio::test]
    async fn user_slots_are_independent_and_cancelled_bodies_release_them() {
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let downloads = test_downloads();
        let query = ContentQuery::default();
        let headers = HeaderMap::new();
        let get = |uid| {
            answer_found(
                counted(&calls),
                &headers,
                &query,
                false,
                None,
                &downloads,
                uid,
            )
        };
        let held = get(1).await.unwrap();
        let full = get(1).await.unwrap_err();
        assert_eq!(full.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(full.headers()[header::RETRY_AFTER], "1");
        let other = get(2).await.unwrap();
        assert_eq!(other.status(), StatusCode::OK);
        drop(held);
        assert_eq!(get(1).await.unwrap().status(), StatusCode::OK);
        drop(other);
        assert!(downloads.active.lock().unwrap().is_empty());
    }

    #[test]
    fn redirects_are_one_canonical_id_on_the_configured_origin() {
        let origin = "https://erp.example";
        assert_eq!(
            redirect_target("https://erp.example/erp/content/8", origin),
            Some(8)
        );
        for url in [
            "https://erp.example.evil/erp/content/8",
            "https://erp.example@evil/erp/content/8",
            "https://erp.example/erp/content/8?access_token=other",
            "//erp.example/erp/content/8",
            "https://erp.example/erp/content/%2f8",
        ] {
            assert_eq!(redirect_target(url, origin), None);
        }
    }

    #[test]
    fn metric_events_publish_a_monotonic_process_total() {
        let counter = AtomicU64::new(0);
        assert_eq!(next_total(&counter), 1);
        assert_eq!(next_total(&counter), 2);
    }

    #[test]
    fn entity_tag_lists_compare_whole_tags_with_method_appropriate_strength() {
        let current = Some("\"new\"");
        assert!(entity_matches(&["\"part,one\", W/\"new\""], current, true));
        assert!(!entity_matches(&["W/\"new\""], current, false));
        assert!(entity_matches(&["\"old\"", "\"new\""], current, false));
        assert!(!entity_matches(&["\"odd,new,tag\""], current, true));
        assert!(entity_matches(&["*"], None, false));
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
