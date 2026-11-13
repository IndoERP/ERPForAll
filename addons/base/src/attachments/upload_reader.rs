// SPDX-License-Identifier: MIT OR Apache-2.0
//! A body reader whose idle deadline counts only the time a read waits for body bytes: the time
//! its consumer spends between reads (an object store writing a part) is not the sender's idleness.
use std::{
    future::Future,
    pin::Pin,
    task::{Context, Poll},
    time::Duration,
};
use tokio::io::{AsyncRead, ReadBuf};
use tokio::time::{Instant, Sleep};

pub(super) struct IdleReader<'a> {
    body: &'a mut (dyn AsyncRead + Send + Unpin),
    idle: Duration,
    deadline: Pin<Box<Sleep>>,
    timed_out: bool,
    /// The last poll of the body returned `Pending`: the read is still outstanding.
    waiting: bool,
}
impl<'a> IdleReader<'a> {
    pub(super) fn new(body: &'a mut (dyn AsyncRead + Send + Unpin), idle: Duration) -> Self {
        Self {
            body,
            idle,
            deadline: Box::pin(tokio::time::sleep(idle)),
            timed_out: false,
            waiting: false,
        }
    }
    pub(super) fn timed_out(&self) -> bool {
        self.timed_out
    }
}
impl AsyncRead for IdleReader<'_> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        if buf.remaining() == 0 {
            return Poll::Ready(Ok(()));
        }
        let timed_out = || {
            Poll::Ready(Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                erp_sdk::blob::BlobError::UploadIdleTimeout,
            )))
        };
        if self.timed_out {
            return timed_out();
        }
        if !self.waiting {
            // A new read begins: the idle time starts now, not at the last byte.
            let next = Instant::now() + self.idle;
            self.deadline.as_mut().reset(next);
        }
        let result = Pin::new(&mut *self.body).poll_read(cx, buf);
        if result.is_ready() {
            self.waiting = false;
            return result;
        }
        // Wakes without bytes do not renew the deadline; only a completed read does.
        self.waiting = true;
        if self.deadline.as_mut().poll(cx).is_ready() {
            self.timed_out = true;
            return timed_out();
        }
        Poll::Pending
    }
}

#[cfg(test)]
#[path = "upload_reader_tests.rs"]
mod tests;
