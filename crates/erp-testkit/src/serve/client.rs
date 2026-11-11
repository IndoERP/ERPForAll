//! [`RawClient`]: a blocking HTTP/1.1 client that allocates nothing once built.
//!
//! The harness counts the allocations of the whole process, so the client that drives the server
//! must not add any: its request bytes and its response buffer are allocated when it is built, the
//! response is framed by hand (`Content-Length` or chunked) inside that buffer, and every error it
//! can return on the request path is a fixed value. The self-test
//! `client_probe_allocates_nothing` proves it on every run of the suite.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::Duration;

use super::ServeError;

/// How long a read or a write on the client socket may block before the request fails.
pub const CLIENT_IO_TIMEOUT: Duration = Duration::from_secs(30);

/// The request method as far as framing the response needs it: a response to `HEAD` has no body,
/// whatever its `Content-Length` or `Transfer-Encoding` say (RFC 9112 §6.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum RequestMethod {
    /// `HEAD`.
    Head,
    /// Every other method.
    Other,
}

impl RequestMethod {
    /// The method of a request given as its bytes on the wire.
    #[must_use]
    pub fn of_request(request: &[u8]) -> Self {
        if request.starts_with(b"HEAD ") {
            Self::Head
        } else {
            Self::Other
        }
    }
}

/// Where the body of a framed response lies, and how it was framed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct ResponseMeta {
    /// The status code.
    pub status: u16,
    /// Bytes of the head, the empty line included.
    pub head_len: usize,
    /// Bytes of the whole response on the wire (head and framed body).
    pub wire_len: usize,
    /// Bytes of the body after removing the chunked framing (0 for a response that has no body:
    /// to `HEAD`, 204, 304).
    pub body_len: usize,
    /// The body was sent with `Transfer-Encoding: chunked` (false when the response has no body).
    pub chunked: bool,
    /// The server sent `Connection: close`: the connection ends after this response.
    pub close: bool,
}

/// A response read by [`RawClient::send`], borrowing the client's buffer.
#[derive(Debug, Clone, Copy)]
pub struct Response<'a> {
    meta: ResponseMeta,
    wire: &'a [u8],
}

impl<'a> Response<'a> {
    /// The status code.
    #[must_use]
    pub fn status(&self) -> u16 {
        self.meta.status
    }

    /// The framing facts of the response.
    #[must_use]
    pub fn meta(&self) -> ResponseMeta {
        self.meta
    }

    /// The head as received (status line and headers, the empty line included).
    #[must_use]
    pub fn head(&self) -> &'a [u8] {
        &self.wire[..self.meta.head_len]
    }

    /// The body of a `Content-Length` response; `None` for a chunked one (its bytes are framed;
    /// [`ResponseMeta::body_len`] gives its decoded length).
    #[must_use]
    pub fn content(&self) -> Option<&'a [u8]> {
        if self.meta.chunked {
            None
        } else {
            Some(&self.wire[self.meta.head_len..self.meta.wire_len])
        }
    }

    /// The value of the first header named `name` (ASCII case-insensitive), without surrounding
    /// whitespace.
    #[must_use]
    pub fn header(&self, name: &str) -> Option<&'a [u8]> {
        header_lines(self.head())
            .find_map(|(n, v)| n.eq_ignore_ascii_case(name.as_bytes()).then_some(v))
    }
}

/// A blocking HTTP/1.1 client over one keep-alive connection with buffers allocated up front.
///
/// [`RawClient::send`] writes the request given at construction and reads exactly one response
/// into the fixed buffer; nothing on that path allocates. A response that does not fit is an error
/// ([`ServeError::ResponseTooLarge`]), never a growing buffer. When the server answers with
/// `Connection: close`, the next send connects again (connecting does not allocate either).
#[derive(Debug)]
pub struct RawClient {
    addr: SocketAddr,
    stream: Option<TcpStream>,
    request: Vec<u8>,
    method: RequestMethod,
    buffer: Vec<u8>,
    /// The only status a measurement accepts (a refusal path); `None`: 2xx and 3xx.
    expected: Option<u16>,
}

impl RawClient {
    /// Connect to `addr` with the request bytes `request` (a complete HTTP/1.1 request, head and
    /// body) and a response buffer of `response_capacity` bytes.
    ///
    /// # Errors
    /// [`ServeError::BadArgument`] for an empty request or a zero capacity, [`ServeError::Io`]
    /// when the connection fails.
    pub fn connect(
        addr: SocketAddr,
        request: impl Into<Vec<u8>>,
        response_capacity: usize,
    ) -> Result<Self, ServeError> {
        let request = request.into();
        if request.is_empty() {
            return Err(ServeError::BadArgument("the request is empty"));
        }
        if response_capacity == 0 {
            return Err(ServeError::BadArgument("the response capacity is zero"));
        }
        let mut client = Self {
            addr,
            stream: None,
            method: RequestMethod::of_request(&request),
            request,
            buffer: vec![0; response_capacity],
            expected: None,
        };
        client.ensure_connected()?;
        Ok(client)
    }

    /// The server's address.
    #[must_use]
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// Make the measurements accept exactly `status` instead of a 2xx or 3xx status: for a server
    /// measured on a refusal path it is meant to take (a 429 or 503). Any other status, a success
    /// included, then fails the measurement with [`ServeError::UnexpectedStatus`].
    pub fn expect_status(&mut self, status: u16) -> &mut Self {
        self.expected = Some(status);
        self
    }

    /// The status set with [`RawClient::expect_status`]; `None` when the measurements accept any
    /// 2xx or 3xx status.
    #[must_use]
    pub fn expected_status(&self) -> Option<u16> {
        self.expected
    }

    /// Replace the request bytes. Copies into the existing buffer when it is large enough, so it
    /// allocates only when the new request is longer than every earlier one.
    ///
    /// # Errors
    /// [`ServeError::BadArgument`] for an empty request.
    pub fn set_request(&mut self, request: &[u8]) -> Result<(), ServeError> {
        if request.is_empty() {
            return Err(ServeError::BadArgument("the request is empty"));
        }
        self.request.clear();
        self.request.extend_from_slice(request);
        self.method = RequestMethod::of_request(request);
        Ok(())
    }

    /// Send the request and read one whole response.
    ///
    /// # Errors
    /// [`ServeError::Io`] on a socket error or timeout, [`ServeError::ConnectionClosed`] when the
    /// server closes before the response is complete, [`ServeError::ResponseTooLarge`],
    /// [`ServeError::MalformedResponse`], [`ServeError::UnsupportedFraming`] and
    /// [`ServeError::TrailingBytes`] for a response the client cannot frame exactly.
    pub fn send(&mut self) -> Result<Response<'_>, ServeError> {
        self.ensure_connected()?;
        let Some(stream) = self.stream.as_mut() else {
            return Err(ServeError::NotConnected);
        };
        let written = stream.write_all(&self.request);
        if let Err(source) = written {
            self.stream = None;
            return Err(ServeError::Io {
                during: "writing the request",
                source,
            });
        }
        let meta = match read_response(stream, &mut self.buffer, self.method) {
            Ok(meta) => meta,
            Err(error) => {
                self.stream = None;
                return Err(error);
            }
        };
        if meta.close {
            self.stream = None;
        }
        Ok(Response {
            meta,
            wire: &self.buffer[..meta.wire_len],
        })
    }

    fn ensure_connected(&mut self) -> Result<(), ServeError> {
        if self.stream.is_none() {
            self.stream = Some(connect(self.addr)?);
        }
        Ok(())
    }
}

/// A blocking connection with the client's timeouts and `TCP_NODELAY`.
pub(crate) fn connect(addr: SocketAddr) -> Result<TcpStream, ServeError> {
    let io = |during| move |source| ServeError::Io { during, source };
    let stream = TcpStream::connect(addr).map_err(io("connecting"))?;
    stream
        .set_nodelay(true)
        .map_err(io("setting TCP_NODELAY"))?;
    stream
        .set_read_timeout(Some(CLIENT_IO_TIMEOUT))
        .map_err(io("setting the read timeout"))?;
    stream
        .set_write_timeout(Some(CLIENT_IO_TIMEOUT))
        .map_err(io("setting the write timeout"))?;
    Ok(stream)
}

/// Read one response to a `method` request from `stream` into `buffer`; the response must end
/// exactly at the last byte received.
pub(crate) fn read_response(
    stream: &mut TcpStream,
    buffer: &mut [u8],
    method: RequestMethod,
) -> Result<ResponseMeta, ServeError> {
    let mut filled = 0;
    loop {
        match frame(&buffer[..filled], method)? {
            Framing::Complete(meta) => {
                if filled > meta.wire_len {
                    return Err(ServeError::TrailingBytes {
                        extra: filled - meta.wire_len,
                    });
                }
                return Ok(meta);
            }
            Framing::Partial => {}
        }
        if filled == buffer.len() {
            return Err(ServeError::ResponseTooLarge {
                capacity: buffer.len(),
            });
        }
        let n = stream
            .read(&mut buffer[filled..])
            .map_err(|source| ServeError::Io {
                during: "reading the response",
                source,
            })?;
        if n == 0 {
            return Err(ServeError::ConnectionClosed { received: filled });
        }
        filled += n;
    }
}

/// Frame one HTTP/1.1 response to a `method` request at the start of `bytes` without a socket:
/// `Ok(None)` when more bytes are needed, the response's facts when it is complete (bytes after
/// [`ResponseMeta::wire_len`] belong to the next response). For tests that read several responses
/// from one stream (pipelining).
///
/// # Errors
/// As [`RawClient::send`] for a response the client cannot frame exactly.
pub fn frame_response(
    bytes: &[u8],
    method: RequestMethod,
) -> Result<Option<ResponseMeta>, ServeError> {
    Ok(match frame(bytes, method)? {
        Framing::Complete(meta) => Some(meta),
        Framing::Partial => None,
    })
}

/// The outcome of framing the bytes received so far.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Framing {
    /// More bytes are needed.
    Partial,
    /// A whole response; it may be followed by bytes that belong to nothing.
    Complete(ResponseMeta),
}

/// Frame an HTTP/1.1 response to a `method` request at the start of `bytes`.
pub(crate) fn frame(bytes: &[u8], method: RequestMethod) -> Result<Framing, ServeError> {
    let Some(head_end) = find(bytes, b"\r\n\r\n") else {
        return Ok(Framing::Partial);
    };
    let head_len = head_end + 4;
    let head = &bytes[..head_len];
    let status = status_of(head)?;
    if raw_header_lines(head).any(|line| !line.contains(&b':')) {
        return Err(ServeError::MalformedResponse(
            "a header line without a colon",
        ));
    }
    let mut content_length: Option<usize> = None;
    let mut chunked = false;
    let mut close = false;
    for (name, value) in header_lines(head) {
        if name.eq_ignore_ascii_case(b"content-length") {
            if content_length.is_some() {
                return Err(ServeError::MalformedResponse("two Content-Length headers"));
            }
            content_length = Some(parse_decimal(value)?);
        } else if name.eq_ignore_ascii_case(b"transfer-encoding") {
            if !value.eq_ignore_ascii_case(b"chunked") {
                return Err(ServeError::UnsupportedFraming(
                    "a Transfer-Encoding other than exactly `chunked`",
                ));
            }
            chunked = true;
        } else if name.eq_ignore_ascii_case(b"connection") && value.eq_ignore_ascii_case(b"close") {
            close = true;
        }
    }
    if (100..200).contains(&status) {
        return Err(ServeError::UnsupportedFraming(
            "an informational (1xx) response",
        ));
    }
    if content_length.is_some() && chunked {
        return Err(ServeError::MalformedResponse(
            "both Content-Length and Transfer-Encoding",
        ));
    }
    // RFC 9112 §6.3: a response to HEAD, and every 204 and 304, ends with its head; a
    // Content-Length there describes the representation (HEAD, 304), not bytes that follow
    if method == RequestMethod::Head || status == 204 || status == 304 {
        return Ok(Framing::Complete(ResponseMeta {
            status,
            head_len,
            wire_len: head_len,
            body_len: 0,
            chunked: false,
            close,
        }));
    }
    let (wire_len, body_len) = match (content_length, chunked) {
        (Some(length), _) => {
            let Some(end) = head_len.checked_add(length) else {
                return Err(ServeError::MalformedResponse("Content-Length overflows"));
            };
            if bytes.len() < end {
                return Ok(Framing::Partial);
            }
            (end, length)
        }
        (None, true) => match dechunk(&bytes[head_len..])? {
            Some((wire, body)) => (head_len + wire, body),
            None => return Ok(Framing::Partial),
        },
        (None, false) => {
            return Err(ServeError::UnsupportedFraming(
                "a response without Content-Length or chunked framing",
            ));
        }
    };
    Ok(Framing::Complete(ResponseMeta {
        status,
        head_len,
        wire_len,
        body_len,
        chunked,
        close,
    }))
}

/// The status code of a status line `HTTP/1.x NNN reason`.
fn status_of(head: &[u8]) -> Result<u16, ServeError> {
    // `frame` found the head's CRLF CRLF, so a CRLF is always there
    let line_end = find(head, b"\r\n").unwrap_or(head.len());
    let line = &head[..line_end];
    let malformed = ServeError::MalformedResponse("a status line other than `HTTP/1.x NNN ...`");
    if line.len() < 12 || !(line.starts_with(b"HTTP/1.1 ") || line.starts_with(b"HTTP/1.0 ")) {
        return Err(malformed);
    }
    let code = &line[9..12];
    if !code.iter().all(u8::is_ascii_digit) || line.get(12).is_some_and(|&b| b != b' ') {
        return Err(malformed);
    }
    Ok(code
        .iter()
        .fold(0u16, |n, &digit| n * 10 + u16::from(digit - b'0')))
}

/// The header lines of a head: the status line and the final empty line skipped, the line ending
/// removed (CRLF, or a bare LF, which the client accepts from a server).
fn raw_header_lines(head: &[u8]) -> impl Iterator<Item = &[u8]> {
    head.split(|&b| b == b'\n')
        .skip(1)
        .map(|line| line.strip_suffix(b"\r").unwrap_or(line))
        .take_while(|line| !line.is_empty())
}

/// The header lines of a head as name and value with surrounding spaces and tabs trimmed.
/// [`frame`] has refused heads with a line without a colon, so no line is skipped here for a
/// response the client accepted.
fn header_lines(head: &[u8]) -> impl Iterator<Item = (&[u8], &[u8])> {
    raw_header_lines(head).filter_map(|line| {
        let colon = line.iter().position(|&b| b == b':')?;
        Some((trim(&line[..colon]), trim(&line[colon + 1..])))
    })
}

fn trim(bytes: &[u8]) -> &[u8] {
    let start = bytes
        .iter()
        .position(|&b| b != b' ' && b != b'\t')
        .unwrap_or(bytes.len());
    let end = bytes
        .iter()
        .rposition(|&b| b != b' ' && b != b'\t')
        .map_or(start, |i| i + 1);
    &bytes[start..end]
}

/// A non-empty decimal number that fits `usize`.
fn parse_decimal(value: &[u8]) -> Result<usize, ServeError> {
    if value.is_empty() || !value.iter().all(u8::is_ascii_digit) {
        return Err(ServeError::MalformedResponse(
            "a Content-Length that is not a number",
        ));
    }
    value.iter().try_fold(0usize, |n, &digit| {
        n.checked_mul(10)
            .and_then(|n| n.checked_add(usize::from(digit - b'0')))
            .ok_or(ServeError::MalformedResponse("Content-Length overflows"))
    })
}

/// The wire length and decoded length of a chunked body at the start of `bytes`, or `None` when
/// it is not complete yet. Chunk extensions are skipped; trailers are read up to the empty line.
fn dechunk(bytes: &[u8]) -> Result<Option<(usize, usize)>, ServeError> {
    let mut at = 0;
    let mut body = 0usize;
    loop {
        let Some(line_len) = find(&bytes[at..], b"\r\n") else {
            return Ok(None);
        };
        let line = &bytes[at..at + line_len];
        let size_text = line.split(|&b| b == b';').next().unwrap_or(line);
        let size = parse_hex(trim(size_text))?;
        at += line_len + 2;
        if size == 0 {
            // trailers, then the empty line
            loop {
                let Some(trailer_len) = find(&bytes[at..], b"\r\n") else {
                    return Ok(None);
                };
                at += trailer_len + 2;
                if trailer_len == 0 {
                    return Ok(Some((at, body)));
                }
            }
        }
        // the chunk's data and its CRLF
        let Some((data_end, chunk_end)) = at
            .checked_add(size)
            .and_then(|data_end| Some((data_end, data_end.checked_add(2)?)))
        else {
            return Err(ServeError::MalformedResponse("a chunk size that overflows"));
        };
        if bytes.len() < chunk_end {
            return Ok(None);
        }
        if &bytes[data_end..chunk_end] != b"\r\n" {
            return Err(ServeError::MalformedResponse(
                "a chunk not followed by CRLF",
            ));
        }
        body = body.checked_add(size).ok_or(ServeError::MalformedResponse(
            "a chunked body that overflows",
        ))?;
        at = chunk_end;
    }
}

fn parse_hex(text: &[u8]) -> Result<usize, ServeError> {
    if text.is_empty() {
        return Err(ServeError::MalformedResponse("an empty chunk size"));
    }
    text.iter().try_fold(0usize, |n, &c| {
        let digit = match c {
            b'0'..=b'9' => c - b'0',
            b'a'..=b'f' => c - b'a' + 10,
            b'A'..=b'F' => c - b'A' + 10,
            _ => {
                return Err(ServeError::MalformedResponse(
                    "a chunk size that is not hex",
                ));
            }
        };
        n.checked_mul(16)
            .and_then(|n| n.checked_add(usize::from(digit)))
            .ok_or(ServeError::MalformedResponse("a chunk size that overflows"))
    })
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]
mod tests {
    use super::*;

    fn complete(bytes: &[u8]) -> ResponseMeta {
        complete_to(bytes, RequestMethod::Other)
    }

    fn complete_to(bytes: &[u8], method: RequestMethod) -> ResponseMeta {
        match frame(bytes, method).expect("frames") {
            Framing::Complete(meta) => meta,
            Framing::Partial => panic!("partial: {:?}", String::from_utf8_lossy(bytes)),
        }
    }

    fn error_of(bytes: &[u8]) -> String {
        frame(bytes, RequestMethod::Other)
            .expect_err("refused")
            .to_string()
    }

    #[test]
    fn content_length_responses_frame_at_every_split() {
        let wire = b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\ndate: x\r\n\r\nhello";
        for split in 0..wire.len() {
            assert_eq!(
                frame(&wire[..split], RequestMethod::Other).unwrap(),
                Framing::Partial,
                "{split}"
            );
        }
        let meta = complete(wire);
        assert_eq!(
            (
                meta.status,
                meta.wire_len,
                meta.body_len,
                meta.chunked,
                meta.close
            ),
            (200, wire.len(), 5, false, false)
        );
        assert_eq!(meta.head_len, wire.len() - 5);
    }

    #[test]
    fn chunked_responses_frame_with_extensions_and_trailers() {
        let wire = b"HTTP/1.1 200 OK\r\ntransfer-encoding: chunked\r\n\r\n\
            5;ext=1\r\nhello\r\nA\r\n0123456789\r\n0\r\nx-trailer: y\r\n\r\n";
        for split in 0..wire.len() {
            assert_eq!(
                frame(&wire[..split], RequestMethod::Other).unwrap(),
                Framing::Partial,
                "{split}"
            );
        }
        let meta = complete(wire);
        assert_eq!(
            (meta.wire_len, meta.body_len, meta.chunked),
            (wire.len(), 15, true)
        );
        let plain = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n0\r\n\r\n";
        assert_eq!(complete(plain).body_len, 0);
    }

    #[test]
    fn bodiless_statuses_and_connection_close() {
        let meta = complete(b"HTTP/1.1 304 Not Modified\r\nConnection: close\r\n\r\n");
        assert_eq!((meta.status, meta.body_len, meta.close), (304, 0, true));
        assert_eq!(complete(b"HTTP/1.1 204 No Content\r\n\r\n").body_len, 0);
    }

    /// RFC 9112 §6.3: responses to HEAD and 204/304 responses end with the head, whatever their
    /// Content-Length or Transfer-Encoding say; the next response follows at once.
    #[test]
    fn bodiless_responses_ignore_content_length_and_chunked() {
        let next = b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\nok";
        for (head, method) in [
            (
                &b"HTTP/1.1 304 Not Modified\r\ncontent-length: 4096\r\n\r\n"[..],
                RequestMethod::Other,
            ),
            (
                b"HTTP/1.1 204 No Content\r\ntransfer-encoding: chunked\r\n\r\n",
                RequestMethod::Other,
            ),
            (
                b"HTTP/1.1 200 OK\r\ncontent-length: 4096\r\n\r\n",
                RequestMethod::Head,
            ),
            (
                b"HTTP/1.1 200 OK\r\ntransfer-encoding: chunked\r\n\r\n",
                RequestMethod::Head,
            ),
        ] {
            let mut wire = head.to_vec();
            wire.extend_from_slice(next);
            let meta = complete_to(&wire, method);
            assert_eq!(
                (meta.wire_len, meta.body_len, meta.chunked),
                (head.len(), 0, false),
                "{:?}",
                String::from_utf8_lossy(head)
            );
            assert_eq!(complete(&wire[meta.wire_len..]).body_len, 2);
        }
        // the same 200 to a GET waits for its body
        assert_eq!(
            frame(
                b"HTTP/1.1 200 OK\r\ncontent-length: 4096\r\n\r\n",
                RequestMethod::Other
            )
            .unwrap(),
            Framing::Partial
        );
        assert_eq!(
            RequestMethod::of_request(b"HEAD / HTTP/1.1\r\n\r\n"),
            RequestMethod::Head
        );
        assert_eq!(
            RequestMethod::of_request(b"HEADER / HTTP/1.1\r\n\r\n"),
            RequestMethod::Other
        );
    }

    /// A chunk size whose data end is the last `usize` overflows on the CRLF after it; refused,
    /// never wrapped.
    #[test]
    fn a_chunk_ending_at_the_last_usize_is_refused() {
        let width = format!("{:x}", usize::MAX).len();
        // the size line is `width` hex digits and CRLF: the data starts at width + 2
        let size = usize::MAX - (width + 2);
        let mut wire = b"HTTP/1.1 200 OK\r\ntransfer-encoding: chunked\r\n\r\n".to_vec();
        wire.extend_from_slice(format!("{size:x}\r\n").as_bytes());
        assert_eq!(format!("{size:x}").len(), width);
        assert_eq!(
            error_of(&wire),
            "malformed response: a chunk size that overflows"
        );
    }

    #[test]
    fn responses_the_client_cannot_frame_exactly_are_refused() {
        assert_eq!(
            error_of(b"HTTP/1.1 200 OK\r\n\r\n"),
            "unsupported response framing: a response without Content-Length or chunked framing"
        );
        assert_eq!(
            error_of(b"HTTP/1.1 200 OK\r\ncontent-length: 1\r\ncontent-length: 1\r\n\r\nx"),
            "malformed response: two Content-Length headers"
        );
        assert_eq!(
            error_of(b"HTTP/1.1 200 OK\r\ncontent-length: 1\r\ntransfer-encoding: chunked\r\n\r\n"),
            "malformed response: both Content-Length and Transfer-Encoding"
        );
        assert_eq!(
            error_of(b"HTTP/1.1 200 OK\r\ntransfer-encoding: gzip, chunked\r\n\r\n"),
            "unsupported response framing: a Transfer-Encoding other than exactly `chunked`"
        );
        assert_eq!(
            error_of(b"HTTP/1.1 200 OK\r\ncontent-length: -1\r\n\r\n"),
            "malformed response: a Content-Length that is not a number"
        );
        assert_eq!(
            error_of(b"HTTP/1.1 200 OK\r\ncontent-length: 99999999999999999999999\r\n\r\n"),
            "malformed response: Content-Length overflows"
        );
        assert_eq!(
            error_of(b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\nno colon here\r\n\r\n"),
            "malformed response: a header line without a colon"
        );
        assert_eq!(
            error_of(b"HTTP/1.1 100 Continue\r\n\r\n"),
            "unsupported response framing: an informational (1xx) response"
        );
        assert_eq!(
            error_of(b"HTTP/2 200\r\ncontent-length: 0\r\n\r\n"),
            "malformed response: a status line other than `HTTP/1.x NNN ...`"
        );
        assert_eq!(
            error_of(b"HTTP/1.1 20x OK\r\ncontent-length: 0\r\n\r\n"),
            "malformed response: a status line other than `HTTP/1.x NNN ...`"
        );
        assert_eq!(
            error_of(b"HTTP/1.1 200 OK\r\ntransfer-encoding: chunked\r\n\r\nzz\r\n"),
            "malformed response: a chunk size that is not hex"
        );
        assert_eq!(
            error_of(b"HTTP/1.1 200 OK\r\ntransfer-encoding: chunked\r\n\r\n1\r\nab\r\n"),
            "malformed response: a chunk not followed by CRLF"
        );
        assert_eq!(
            error_of(b"HTTP/1.1 200 OK\r\ntransfer-encoding: chunked\r\n\r\nFFFFFFFFFFFFFFFFF\r\n"),
            "malformed response: a chunk size that overflows"
        );
    }

    #[test]
    fn headers_are_found_case_insensitively_and_trimmed() {
        let wire = b"HTTP/1.1 200 OK\r\nX-Test:  a b \t\r\ncontent-length: 0\r\n\r\n";
        let meta = complete(wire);
        let response = Response {
            meta,
            wire: &wire[..],
        };
        assert_eq!(response.header("x-test"), Some(&b"a b"[..]));
        assert_eq!(response.header("missing"), None);
        assert_eq!(response.content(), Some(&b""[..]));
    }
}
