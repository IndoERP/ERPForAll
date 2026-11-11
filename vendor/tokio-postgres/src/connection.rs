use crate::codec::{BackendMessage, BackendMessages, FrontendMessage, PostgresCodec};
use crate::copy_in::CopyInReceiver;
use crate::error::DbError;
use crate::maybe_tls_stream::MaybeTlsStream;
use crate::{AsyncMessage, Error, Notification};
use bytes::BytesMut;
use fallible_iterator::FallibleIterator;
use futures_channel::mpsc;
use futures_util::{Sink, Stream, StreamExt, stream::FusedStream};
use log::{info, trace};
use postgres_protocol::message::backend::Message;
use postgres_protocol::message::frontend;
use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::task::{Context, Poll, ready};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio_util::codec::Framed;

// Lifetime totals survive client/connection retirement. Read only between completed requests;
// allocation tests isolate the process and use a current-thread runtime (A4).
#[cfg(feature = "process-response-batches")]
pub(crate) static PROCESS_RESPONSE_BATCHES: AtomicU64 = AtomicU64::new(0);
#[cfg(feature = "process-response-batches")]
pub(crate) static PROCESS_PARKED_BATCHES: AtomicU64 = AtomicU64::new(0);

pub enum RequestMessages {
    Single(FrontendMessage),
    CopyIn(CopyInReceiver),
}

/// The parameters whose reports [`crate::Client::string_reading_changes`] counts: the settings
/// that decide how the server reads the text of a statement.
const STRING_READING_PARAMETERS: [&str; 2] = ["client_encoding", "standard_conforming_strings"];

/// What the connection counts for its client (A2 and A3), shared by both halves behind
/// one `Arc`, so a new count is a field here, not another constructor parameter.
#[derive(Debug, Default)]
pub(crate) struct Counters {
    /// Reports of a new value of a [`STRING_READING_PARAMETERS`] setting
    /// ([`crate::Client::string_reading_changes`]).
    pub(crate) string_reading: AtomicU64,
    /// Batches of response messages delivered to a request's channel
    /// ([`crate::Client::response_batches`]).
    pub(crate) response_batches: AtomicU64,
    /// Of those, the ones whose delivery parked the connection on a full channel
    /// ([`crate::Client::parked_batches`]).
    pub(crate) parked_batches: AtomicU64,
}

pub struct Request {
    pub messages: RequestMessages,
    pub sender: mpsc::Sender<BackendMessages>,
}

pub struct Response {
    sender: mpsc::Sender<BackendMessages>,
}

#[derive(PartialEq, Debug)]
enum State {
    Active,
    Terminating,
    Closing,
}

/// A connection to a PostgreSQL database.
///
/// This is one half of what is returned when a new connection is established. It performs the actual IO with the
/// server, and should generally be spawned off onto an executor to run in the background.
///
/// `Connection` implements `Future`, and only resolves when the connection is closed, either because a fatal error has
/// occurred, or because its associated `Client` has dropped and all outstanding work has completed.
#[must_use = "futures do nothing unless polled"]
pub struct Connection<S, T> {
    stream: Framed<MaybeTlsStream<S, T>, PostgresCodec>,
    parameters: HashMap<String, String>,
    receiver: mpsc::UnboundedReceiver<Request>,
    pending_request: Option<RequestMessages>,
    pending_responses: VecDeque<BackendMessage>,
    responses: VecDeque<Response>,
    state: State,
    counters: Arc<Counters>,
}

impl<S, T> Connection<S, T>
where
    S: AsyncRead + AsyncWrite + Unpin,
    T: AsyncRead + AsyncWrite + Unpin,
{
    pub(crate) fn new(
        stream: Framed<MaybeTlsStream<S, T>, PostgresCodec>,
        pending_responses: VecDeque<BackendMessage>,
        parameters: HashMap<String, String>,
        receiver: mpsc::UnboundedReceiver<Request>,
        counters: Arc<Counters>,
    ) -> Connection<S, T> {
        Connection {
            stream,
            parameters,
            receiver,
            pending_request: None,
            pending_responses,
            responses: VecDeque::new(),
            state: State::Active,
            counters,
        }
    }

    fn poll_response(
        &mut self,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<BackendMessage, Error>>> {
        if let Some(message) = self.pending_responses.pop_front() {
            trace!("retrying pending response");
            return Poll::Ready(Some(Ok(message)));
        }

        Pin::new(&mut self.stream)
            .poll_next(cx)
            .map(|o| o.map(|r| r.map_err(Error::io)))
    }

    fn poll_read(&mut self, cx: &mut Context<'_>) -> Result<Option<AsyncMessage>, Error> {
        if self.state != State::Active {
            trace!("poll_read: done");
            return Ok(None);
        }

        loop {
            let message = match self.poll_response(cx)? {
                Poll::Ready(Some(message)) => message,
                Poll::Ready(None) => return Err(Error::closed()),
                Poll::Pending => {
                    trace!("poll_read: waiting on response");
                    return Ok(None);
                }
            };

            let (mut messages, request_complete) = match message {
                BackendMessage::Async(Message::NoticeResponse(body)) => {
                    let error = DbError::parse(&mut body.fields()).map_err(Error::parse)?;
                    return Ok(Some(AsyncMessage::Notice(error)));
                }
                BackendMessage::Async(Message::NotificationResponse(body)) => {
                    let notification = Notification {
                        process_id: body.process_id(),
                        channel: body.channel().map_err(Error::parse)?.to_string(),
                        payload: body.message().map_err(Error::parse)?.to_string(),
                    };
                    return Ok(Some(AsyncMessage::Notification(notification)));
                }
                BackendMessage::Async(Message::ParameterStatus(body)) => {
                    let name = body.name().map_err(Error::parse)?;
                    let value = body.value().map_err(Error::parse)?;
                    // counted before the messages after the report are handed over, so a
                    // request that has completed sees its own reports
                    if STRING_READING_PARAMETERS.contains(&name)
                        && self.parameters.get(name).map(String::as_str) != Some(value)
                    {
                        self.counters.string_reading.fetch_add(1, Ordering::Release);
                    }
                    self.parameters.insert(name.to_string(), value.to_string());
                    continue;
                }
                BackendMessage::Async(_) => unreachable!(),
                BackendMessage::Normal {
                    messages,
                    request_complete,
                } => (messages, request_complete),
            };

            let mut response = match self.responses.pop_front() {
                Some(response) => response,
                None => match messages.next().map_err(Error::parse)? {
                    Some(Message::ErrorResponse(error)) => return Err(Error::db(error)),
                    _ => return Err(Error::unexpected_message()),
                },
            };

            match response.sender.poll_ready(cx) {
                Poll::Ready(Ok(())) => {
                    // counted before the batch is sent, so a request that has completed sees
                    // all of its batches (`Client::response_batches`); the channel accepted it
                    // just above, so only a receiver closing in between on another thread (a
                    // multi-thread runtime) leaves it counted and undelivered
                    self.counters.response_batches.fetch_add(1, Ordering::Release);
                    #[cfg(feature = "process-response-batches")]
                    PROCESS_RESPONSE_BATCHES.fetch_add(1, Ordering::Release);
                    let _ = response.sender.start_send(messages);
                    // A send that finds the channel holding a batch already parks the sender
                    // (futures-channel queues a node for it); `poll_ready` is then `Pending`
                    // until the receiver takes one (`Client::parked_batches`). Polled with this
                    // task's own context, so the only waker it can leave in the sender's slot is
                    // this connection's: the receiver's take wakes the connection at worst once
                    // more than needed, and every later `poll_ready` that waits stores the same
                    // waker again, so no wake-up can be lost. Exact on a current-thread runtime,
                    // where the receiver cannot run between the two calls.
                    if response.sender.poll_ready(cx).is_pending() {
                        self.counters.parked_batches.fetch_add(1, Ordering::Release);
                        #[cfg(feature = "process-response-batches")]
                        PROCESS_PARKED_BATCHES.fetch_add(1, Ordering::Release);
                    }
                    if !request_complete {
                        self.responses.push_front(response);
                    }
                }
                Poll::Ready(Err(_)) => {
                    // not counted: the receiver had gone when the channel was checked, so the
                    // batch is dropped undelivered and costs no channel node
                    // we need to keep paging through the rest of the messages even if the receiver's hung up
                    if !request_complete {
                        self.responses.push_front(response);
                    }
                }
                Poll::Pending => {
                    self.responses.push_front(response);
                    self.pending_responses.push_back(BackendMessage::Normal {
                        messages,
                        request_complete,
                    });
                    trace!("poll_read: waiting on sender");
                    return Ok(None);
                }
            }
        }
    }

    fn poll_request(&mut self, cx: &mut Context<'_>) -> Poll<Option<RequestMessages>> {
        if let Some(messages) = self.pending_request.take() {
            trace!("retrying pending request");
            return Poll::Ready(Some(messages));
        }

        if self.receiver.is_terminated() {
            return Poll::Ready(None);
        }

        match self.receiver.poll_next_unpin(cx) {
            Poll::Ready(Some(request)) => {
                trace!("polled new request");
                self.responses.push_back(Response {
                    sender: request.sender,
                });
                Poll::Ready(Some(request.messages))
            }
            Poll::Ready(None) => Poll::Ready(None),
            Poll::Pending => Poll::Pending,
        }
    }

    fn poll_write(&mut self, cx: &mut Context<'_>) -> Result<bool, Error> {
        loop {
            if self.state == State::Closing {
                trace!("poll_write: done");
                return Ok(false);
            }

            if Pin::new(&mut self.stream)
                .poll_ready(cx)
                .map_err(Error::io)?
                .is_pending()
            {
                trace!("poll_write: waiting on socket");
                return Ok(false);
            }

            let request = match self.poll_request(cx) {
                Poll::Ready(Some(request)) => request,
                Poll::Ready(None) if self.responses.is_empty() && self.state == State::Active => {
                    trace!("poll_write: at eof, terminating");
                    self.state = State::Terminating;
                    let mut request = BytesMut::new();
                    frontend::terminate(&mut request);
                    RequestMessages::Single(FrontendMessage::Raw(request.freeze()))
                }
                Poll::Ready(None) => {
                    trace!(
                        "poll_write: at eof, pending responses {}",
                        self.responses.len()
                    );
                    return Ok(true);
                }
                Poll::Pending => {
                    trace!("poll_write: waiting on request");
                    return Ok(true);
                }
            };

            match request {
                RequestMessages::Single(request) => {
                    Pin::new(&mut self.stream)
                        .start_send(request)
                        .map_err(Error::io)?;
                    if self.state == State::Terminating {
                        trace!("poll_write: sent eof, closing");
                        self.state = State::Closing;
                    }
                }
                RequestMessages::CopyIn(mut receiver) => {
                    let message = match receiver.poll_next_unpin(cx) {
                        Poll::Ready(Some(message)) => message,
                        Poll::Ready(None) => {
                            trace!("poll_write: finished copy_in request");
                            continue;
                        }
                        Poll::Pending => {
                            trace!("poll_write: waiting on copy_in stream");
                            self.pending_request = Some(RequestMessages::CopyIn(receiver));
                            return Ok(true);
                        }
                    };
                    Pin::new(&mut self.stream)
                        .start_send(message)
                        .map_err(Error::io)?;
                    self.pending_request = Some(RequestMessages::CopyIn(receiver));
                }
            }
        }
    }

    fn poll_flush(&mut self, cx: &mut Context<'_>) -> Result<(), Error> {
        match Pin::new(&mut self.stream)
            .poll_flush(cx)
            .map_err(Error::io)?
        {
            Poll::Ready(()) => trace!("poll_flush: flushed"),
            Poll::Pending => trace!("poll_flush: waiting on socket"),
        }
        Ok(())
    }

    fn poll_shutdown(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Error>> {
        if self.state != State::Closing {
            return Poll::Pending;
        }

        match Pin::new(&mut self.stream)
            .poll_close(cx)
            .map_err(Error::io)?
        {
            Poll::Ready(()) => {
                trace!("poll_shutdown: complete");
                Poll::Ready(Ok(()))
            }
            Poll::Pending => {
                trace!("poll_shutdown: waiting on socket");
                Poll::Pending
            }
        }
    }

    /// Returns the value of a runtime parameter for this connection.
    pub fn parameter(&self, name: &str) -> Option<&str> {
        self.parameters.get(name).map(|s| &**s)
    }

    fn poll_message_inner(
        &mut self,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<AsyncMessage, Error>>> {
        let message = self.poll_read(cx)?;
        let want_flush = self.poll_write(cx)?;
        if want_flush {
            self.poll_flush(cx)?;
        }
        match message {
            Some(message) => Poll::Ready(Some(Ok(message))),
            None => match self.poll_shutdown(cx) {
                Poll::Ready(Ok(())) => Poll::Ready(None),
                Poll::Ready(Err(e)) => Poll::Ready(Some(Err(e))),
                Poll::Pending => Poll::Pending,
            },
        }
    }

    /// Polls for asynchronous messages from the server.
    ///
    /// The server can send notices as well as notifications asynchronously to the client. Applications that wish to
    /// examine those messages should use this method to drive the connection rather than its `Future` implementation.
    ///
    /// Return values of `None` or `Some(Err(_))` are "terminal"; callers should not invoke this method again after
    /// receiving one of those values.
    pub fn poll_message(
        &mut self,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<AsyncMessage, Error>>> {
        match self.poll_message_inner(cx) {
            nominal @ (Poll::Pending | Poll::Ready(Some(Ok(_)))) => nominal,
            terminal @ (Poll::Ready(None) | Poll::Ready(Some(Err(_)))) => {
                self.receiver.close();
                terminal
            }
        }
    }
}

impl<S, T> Future for Connection<S, T>
where
    S: AsyncRead + AsyncWrite + Unpin,
    T: AsyncRead + AsyncWrite + Unpin,
{
    type Output = Result<(), Error>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Error>> {
        while let Some(message) = ready!(self.poll_message(cx)?) {
            if let AsyncMessage::Notice(notice) = message {
                info!("{}: {}", notice.severity(), notice.message());
            }
        }
        Poll::Ready(Ok(()))
    }
}
