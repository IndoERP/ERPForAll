// SPDX-License-Identifier: MIT OR Apache-2.0
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]
use super::*;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[tokio::test(start_paused = true)]
async fn stalled_body_has_an_idle_deadline() {
    let (mut input, _keep_writer) = tokio::io::duplex(8);
    let mut reader = IdleReader::new(&mut input, Duration::from_secs(3));
    let read = tokio::time::timeout(Duration::from_secs(4), reader.read_u8())
        .await
        .expect("idle timer must fire before the outer deadline");
    let error = read.unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
    assert_eq!(
        error.to_string(),
        "No upload data arrived before the timeout. Try uploading again."
    );
}

struct WakeOnly;
impl AsyncRead for WakeOnly {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        _: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        cx.waker().wake_by_ref();
        Poll::Pending
    }
}
#[tokio::test(start_paused = true)]
async fn wakes_without_bytes_do_not_renew_the_deadline() {
    let mut input = WakeOnly;
    let mut reader = IdleReader::new(&mut input, Duration::from_secs(3));
    let read = reader.read_u8();
    tokio::pin!(read);
    // Poll once, then move the clock explicitly: a self-waking reader never goes idle.
    assert!(matches!(
        std::future::poll_fn(|cx| Poll::Ready(read.as_mut().poll(cx))).await,
        Poll::Pending
    ));
    tokio::time::advance(Duration::from_secs(3)).await;
    let result = std::future::poll_fn(|cx| Poll::Ready(read.as_mut().poll(cx))).await;
    let Poll::Ready(Err(error)) = result else {
        panic!("wakes renewed the idle timer")
    };
    assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
    assert_eq!(
        error.to_string(),
        "No upload data arrived before the timeout. Try uploading again."
    );
}
#[tokio::test(start_paused = true)]
async fn positive_progress_resets_the_timer_and_eof_is_not_a_timeout() {
    let (mut input, mut output) = tokio::io::duplex(8);
    let mut reader = IdleReader::new(&mut input, Duration::from_secs(3));
    tokio::time::advance(Duration::from_secs(2)).await;
    output.write_all(b"a").await.unwrap();
    assert_eq!(reader.read_u8().await.unwrap(), b'a');
    tokio::time::advance(Duration::from_secs(2)).await;
    output.write_all(b"b").await.unwrap();
    assert_eq!(reader.read_u8().await.unwrap(), b'b');
    drop(output);
    let mut rest = Vec::new();
    assert_eq!(reader.read_to_end(&mut rest).await.unwrap(), 0);
}
/// A consumer that is busy between reads (the object store writing a part) is not an idle sender:
/// only the time a read waits for bytes counts. A real stall after that still times out.
#[tokio::test(start_paused = true)]
async fn a_busy_consumer_is_not_an_idle_sender() {
    let (mut input, mut output) = tokio::io::duplex(8);
    let mut reader = IdleReader::new(&mut input, Duration::from_secs(3));
    output.write_all(b"a").await.unwrap();
    assert_eq!(reader.read_u8().await.unwrap(), b'a');
    // The consumer does not read for longer than the idle limit; the sender has bytes ready.
    tokio::time::advance(Duration::from_secs(10)).await;
    output.write_all(b"b").await.unwrap();
    assert_eq!(reader.read_u8().await.unwrap(), b'b');
    tokio::time::advance(Duration::from_secs(10)).await;
    // Now the sender stalls while a read waits.
    let read = tokio::time::timeout(Duration::from_secs(4), reader.read_u8())
        .await
        .expect("idle timer must fire before the outer deadline");
    let error = read.unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
    assert!(reader.timed_out());
    drop(output);
}
