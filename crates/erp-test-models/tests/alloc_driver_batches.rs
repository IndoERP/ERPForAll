//! The blocks the PostgreSQL driver allocates per delivered batch are
//! `erp_testkit::driver::DRIVER_MIN_BLOCKS_PER_BATCH`, and `DRIVER_MAX_BLOCKS_PER_BATCH` for a
//! batch that parks the connection, and the driver's parked count (`Client::parked_batches`) is
//! exact (amendment A3, lesson; review rust-3): the constants and the count every
//! ORM allocation test takes out of what it measures.
//!
//! A scripted server answers the same simple query (`k` statements: `k` `CommandComplete` and one
//! `ReadyForQuery`, the same bytes each time) three ways, and the receiver (a simple-query
//! stream polled once per turn) takes the batches in step with it:
//!
//! - **whole**: every message in one write, one batch;
//! - **split**: one message per write; each is written once the previous one was delivered and
//!   taken off the channel, so every batch finds the channel empty: `k + 1` batches at the
//!   fewest blocks ([`DRIVER_MIN_BLOCKS_PER_BATCH`]);
//! - **parked**: the same writes, but the receiver takes each batch only once the next one is
//!   delivered, so every batch after the first finds one waiting and parks the connection: `k + 1`
//!   batches, `k` of them at the most blocks ([`DRIVER_MAX_BLOCKS_PER_BATCH`]).
//!
//! The driver's parked count must read 0 for the split answer and `k` for the parked one.
//!
//! An answer that straddles the end of the driver's 8 KiB read buffer is read in two parts and
//! delivered as one batch more (about every seventh round for the whole answer here), so the
//! test takes the batches each window was counted at, not the ones the script aims for. The
//! messages and their parsing are the same in the three windows, so split − whole is the
//! channel's node for each added batch, and parked − split the parking node for each parked one,
//! plus at most one read-buffer refill (a new buffer and its shared header: 2 blocks) either
//! way. Every write and every read buffer of the server is made before the windows, and every wait
//! yields without allocating (the runtime's list of yielded tasks, made on the first yield of a
//! `block_on`, is in every window: each starts with a yield).
//!
//! One binary, run on the main thread by `erp_alloc_budget::harness`; the runtime is
//! current-thread and built outside the windows, so the server and the connection run inside the
//! window on the main thread, as part of the measured work.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::print_stdout,
    reason = "tests"
)]

use std::future::poll_fn;
use std::pin::pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::task::Poll;

use erp_testkit::alloc::{self, AllocStats};
use erp_testkit::driver::{DRIVER_MAX_BLOCKS_PER_BATCH, DRIVER_MIN_BLOCKS_PER_BATCH};
use futures_core::Stream;
use tokio::io::{AsyncReadExt, AsyncWriteExt, DuplexStream};
use tokio::sync::oneshot;

#[global_allocator]
static ALLOC: alloc::Alloc = alloc::Alloc;

alloc::harness!(
    a_delivered_batch_costs_the_driver_one_or_two_blocks,
    watchdog_wakeups_allocate_nothing,
    watchdog_timeout_fails_the_process,
);

/// Statements per query: the batches the split and parked answers add.
const K: u64 = 40;
/// Measured rounds (one query of each answer) after the warm-up ones.
const ROUNDS: usize = 8;
/// Unmeasured rounds first: the driver's buffers and the connection's state settled.
const WARM: usize = 4;
/// Blocks a read-buffer refill can add to one window: the new 8 KiB buffer and its shared header,
/// measured per platform (TEST.FLAKE-ALLOC-RUNNER round 2):
/// 2 on Linux; 3 on macOS, where the first measured round's parked window also grows once more.
/// `None` on a platform nobody has measured: the test fails there, it never skips.
fn refill() -> u64 {
    let measured = if cfg!(target_os = "linux") {
        Some(2)
    } else if cfg!(target_os = "macos") {
        Some(3)
    } else {
        None
    };
    measured.unwrap_or_else(|| {
        panic!(
            "no measured refill bound for {}: measure alloc_driver_batches here and pin it",
            std::env::consts::OS
        )
    })
}
/// The longest the test may run before it fails instead of hanging.
const LIMIT: std::time::Duration = std::time::Duration::from_secs(30);

/// No channel registration may occur while a window counts every thread. The
/// watchdog warms its thread parker and acknowledges readiness before returning.
struct WatchState {
    stop: AtomicBool,
    turns: AtomicU64,
}

struct Watchdog {
    state: Arc<WatchState>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Drop for Watchdog {
    fn drop(&mut self) {
        self.state.stop.store(true, Ordering::Release);
        let thread = self.thread.take().expect("watchdog thread");
        thread.thread().unpark();
        thread.join().expect("watchdog panicked");
    }
}

fn watchdog(what: &'static str) -> Watchdog {
    watchdog_for(what, LIMIT)
}

/// Bounds the whole test without timers or lazy channel allocations in its windows.
/// Spurious wakeups retain the original deadline; dropping the guard wakes and joins it.
fn watchdog_for(what: &'static str, limit: std::time::Duration) -> Watchdog {
    let state = Arc::new(WatchState {
        stop: AtomicBool::new(false),
        turns: AtomicU64::new(0),
    });
    let worker = Arc::clone(&state);
    let thread = std::thread::spawn(move || {
        std::thread::park_timeout(std::time::Duration::ZERO);
        let deadline = std::time::Instant::now()
            .checked_add(limit)
            .expect("watchdog deadline");
        loop {
            if worker.stop.load(Ordering::Acquire) {
                return;
            }
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                eprintln!(
                    "{what}: still running after {limit:?}: a count the test waits on stopped"
                );
                std::process::exit(101);
            }
            worker.turns.fetch_add(1, Ordering::Release);
            std::thread::park_timeout(remaining);
        }
    });
    while state.turns.load(Ordering::Acquire) == 0 {
        assert!(
            !thread.is_finished(),
            "watchdog exited before becoming ready"
        );
        std::thread::yield_now();
    }
    Watchdog {
        state,
        thread: Some(thread),
    }
}

/// Harness correctness: force repeated watchdog waits inside a counted window.
fn watchdog_wakeups_allocate_nothing() {
    let watchdog = watchdog("watchdog_wakeups_allocate_nothing");
    let (stats, ()) = alloc::measure(|| {
        for _ in 0..100 {
            let before = watchdog.state.turns.load(Ordering::Acquire);
            watchdog.thread.as_ref().unwrap().thread().unpark();
            while watchdog.state.turns.load(Ordering::Acquire) == before {
                std::thread::yield_now();
            }
        }
    })
    .expect("measure watchdog waits");
    assert_eq!(
        stats.total_blocks, 0,
        "watchdog waits must not contaminate allocation windows"
    );
    assert_eq!(stats.total_bytes, 0);
    drop(watchdog);
}

/// The guard still fails a hung child, rather than returning a successful test result.
fn watchdog_timeout_fails_the_process() {
    const CHILD: &str = "ERP_TEST_DRIVER_WATCHDOG_CHILD";
    if std::env::var_os(CHILD).is_some() {
        let _watchdog = watchdog_for(
            "forced-watchdog-timeout",
            std::time::Duration::from_millis(20),
        );
        std::thread::sleep(std::time::Duration::from_secs(1));
        panic!("watchdog did not terminate its process");
    }
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "watchdog_timeout_fails_the_process"])
        .env(CHILD, "1")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(101));
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert_eq!(
        stderr,
        "forced-watchdog-timeout: still running after 20ms: a count the test waits on stopped\n"
    );
}

/// One backend message: tag, length (itself included), body.
fn message(tag: u8, body: &[u8]) -> Vec<u8> {
    let len = i32::try_from(body.len() + 4).unwrap();
    let mut out = vec![tag];
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(body);
    out
}

/// How the server answers one query, and how the receiver takes it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Answer {
    /// Every message in one write: one batch.
    Whole,
    /// One message per write, taken off the channel before the next is written: `K + 1` batches,
    /// none of them parks the connection.
    Split,
    /// One message per write, each taken off the channel once the next one is delivered: `K + 1`
    /// batches, `K` of them park the connection.
    Parked,
}

const ANSWERS: [Answer; 3] = [Answer::Whole, Answer::Split, Answer::Parked];

/// The turn between the server and the receiver: `true` while the receiver may take one batch.
type Turn = Arc<AtomicBool>;

/// Hands the turn to the receiver and waits until it has taken one batch.
async fn receiver_takes_one(turn: &AtomicBool) {
    turn.store(true, Ordering::Release);
    while turn.load(Ordering::Acquire) {
        tokio::task::yield_now().await;
    }
}

/// The server: the startup, then one answer per entry of `answers`, then the client's `X`.
async fn serve(
    mut server: DuplexStream,
    client: oneshot::Receiver<Arc<tokio_postgres::Client>>,
    answers: Vec<Answer>,
    turn: Turn,
) {
    let len = usize::try_from(server.read_i32().await.unwrap()).unwrap();
    let mut body = vec![0; 64 * 1024];
    server.read_exact(&mut body[..len - 4]).await.unwrap();
    let mut out = message(b'R', &0i32.to_be_bytes());
    out.extend(message(b'Z', b"I"));
    server.write_all(&out).await.unwrap();
    // everything written later is made now, outside the windows
    let complete = message(b'C', b"SELECT 0\0");
    let ready = message(b'Z', b"I");
    let mut whole = Vec::new();
    for _ in 0..K {
        whole.extend_from_slice(&complete);
    }
    whole.extend_from_slice(&ready);
    let client = client.await.unwrap();
    for answer in answers {
        let tag = server.read_u8().await.unwrap();
        assert_eq!(tag, b'Q');
        let len = usize::try_from(server.read_i32().await.unwrap()).unwrap();
        server.read_exact(&mut body[..len - 4]).await.unwrap();
        if answer == Answer::Whole {
            server.write_all(&whole).await.unwrap();
            continue;
        }
        let start = client.response_batches();
        for i in 0..=K {
            server
                .write_all(if i < K { &complete } else { &ready })
                .await
                .unwrap();
            while client.response_batches() < start + i + 1 {
                tokio::task::yield_now().await;
            }
            // parked: the receiver stays one batch behind, so the next one finds this one waiting
            if answer == Answer::Split || i > 0 {
                receiver_takes_one(&turn).await;
            }
        }
    }
    drop(client);
    assert_eq!(
        server.read_u8().await.unwrap(),
        b'X',
        "the client terminates"
    );
}

/// Takes one message off a stream whose batch is already delivered.
async fn take_delivered<S>(stream: std::pin::Pin<&mut S>) -> Option<()>
where
    S: Stream<Item = Result<tokio_postgres::SimpleQueryMessage, tokio_postgres::Error>>,
{
    let mut stream = stream;
    poll_fn(|cx| match stream.as_mut().poll_next(cx) {
        Poll::Ready(Some(message)) => {
            message.unwrap();
            Poll::Ready(Some(()))
        }
        Poll::Ready(None) => Poll::Ready(None),
        Poll::Pending => panic!("the turn came before the batch was delivered"),
    })
    .await
}

/// Waits for the receiver's turn, then takes one message, and hands the turn back.
async fn take_on_turn<S>(stream: std::pin::Pin<&mut S>, turn: &AtomicBool) -> Option<()>
where
    S: Stream<Item = Result<tokio_postgres::SimpleQueryMessage, tokio_postgres::Error>>,
{
    while !turn.load(Ordering::Acquire) {
        tokio::task::yield_now().await;
    }
    let taken = take_delivered(stream).await;
    turn.store(false, Ordering::Release);
    taken
}

/// The receiver's side of one query.
async fn receive(client: &tokio_postgres::Client, sql: &str, answer: Answer, turn: &AtomicBool) {
    // tokio's current-thread `block_on` makes its list of yielded tasks on the first yield of
    // each call; the split and parked windows yield, so every window yields once first and the
    // three carry that block alike
    tokio::task::yield_now().await;
    let stream = client.simple_query_raw(sql).await.unwrap();
    let mut stream = pin!(stream);
    match answer {
        Answer::Whole => {
            let mut stream = stream.as_mut();
            while poll_fn(|cx| stream.as_mut().poll_next(cx)).await.is_some() {}
        }
        // one message per turn, the `ReadyForQuery` (the end) on the last
        Answer::Split => while take_on_turn(stream.as_mut(), turn).await.is_some() {},
        // one turn fewer: the last `CommandComplete` on the last turn, then the waiting end
        Answer::Parked => {
            for _ in 0..K {
                assert!(take_on_turn(stream.as_mut(), turn).await.is_some());
            }
            assert!(take_delivered(stream.as_mut()).await.is_none(), "the end");
        }
    }
}

/// What one measured query cost and how the driver delivered it.
struct Window {
    stats: AllocStats,
    /// Batches delivered.
    batches: u64,
    /// Of those, the ones that parked the connection.
    parked: u64,
}

/// One measured query and the batches it was delivered in.
fn query(
    rt: &tokio::runtime::Runtime,
    client: &tokio_postgres::Client,
    sql: &str,
    answer: Answer,
    turn: &AtomicBool,
) -> Window {
    let (batches_before, parked_before) = (client.response_batches(), client.parked_batches());
    let (stats, ()) =
        alloc::measure(|| rt.block_on(receive(client, sql, answer, turn))).expect("measure");
    let grown = |now: u64, before: u64| now.checked_sub(before).expect("the counts only grow");
    Window {
        stats,
        batches: grown(client.response_batches(), batches_before),
        parked: grown(client.parked_batches(), parked_before),
    }
}

fn a_delivered_batch_costs_the_driver_one_or_two_blocks() {
    let _watchdog = watchdog("a_delivered_batch_costs_the_driver_one_or_two_blocks");
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let sql = vec!["SELECT 0"; usize::try_from(K).unwrap()].join("; ");
    let answers: Vec<Answer> = (0..WARM + ROUNDS).flat_map(|_| ANSWERS).collect();
    let turn: Turn = Arc::new(AtomicBool::new(false));
    let (client_side, server_side) = tokio::io::duplex(64 * 1024);
    let (give_client, take_client) = oneshot::channel();
    let server = rt.spawn(serve(server_side, take_client, answers, Arc::clone(&turn)));
    let mut config = tokio_postgres::Config::new();
    config
        .user("erp")
        .dbname("erp")
        .ssl_mode(tokio_postgres::config::SslMode::Disable);
    let (client, connection) = rt
        .block_on(config.connect_raw(client_side, tokio_postgres::NoTls))
        .unwrap();
    let driver = rt.spawn(connection);
    let client = Arc::new(client);
    give_client.send(Arc::clone(&client)).unwrap();
    let parking = DRIVER_MAX_BLOCKS_PER_BATCH - DRIVER_MIN_BLOCKS_PER_BATCH;
    let mut measured = 0;
    for round in 0..WARM + ROUNDS {
        let whole = query(&rt, &client, &sql, Answer::Whole, &turn);
        let split = query(&rt, &client, &sql, Answer::Split, &turn);
        let parked = query(&rt, &client, &sql, Answer::Parked, &turn);
        // one batch more when the whole answer straddles the end of the read buffer, and that
        // second batch may find the first still waiting
        assert!(
            (1..=2).contains(&whole.batches) && whole.parked < whole.batches,
            "round {round}: the whole answer is one batch (two across a refill), not {} ({} parked)",
            whole.batches,
            whole.parked
        );
        assert_eq!(split.batches, K + 1, "round {round}: one batch per message");
        assert_eq!(
            parked.batches,
            K + 1,
            "round {round}: one batch per message"
        );
        // the driver's parked count (`Client::parked_batches`) is what the script makes happen
        assert_eq!(
            split.parked, 0,
            "round {round}: the reader keeps up, nothing parks"
        );
        assert_eq!(
            parked.parked, K,
            "round {round}: every batch after the first parks"
        );
        if round < WARM {
            continue;
        }
        // split − whole: the channel's node per added batch, less the whole answer's parks
        let added = split.batches - whole.batches;
        let channel = i128::from(split.stats.total_blocks) - i128::from(whole.stats.total_blocks);
        let expected =
            i128::from(added * DRIVER_MIN_BLOCKS_PER_BATCH) - i128::from(whole.parked * parking);
        // parked − split: the parking node per parked batch
        let parked_extra =
            i128::from(parked.stats.total_blocks) - i128::from(split.stats.total_blocks);
        let parks_expected = i128::from(parked.parked * parking);
        println!(
            "round {round}: whole {} blocks in {} batches ({} parked), split {} blocks in {} (0 \
             parked), parked {} blocks in {} ({} parked): {:.3} blocks per added batch, {:.3} \
             more per parked one",
            whole.stats.total_blocks,
            whole.batches,
            whole.parked,
            split.stats.total_blocks,
            split.batches,
            parked.stats.total_blocks,
            parked.batches,
            parked.parked,
            channel as f64 / added as f64,
            parked_extra as f64 / parked.parked as f64
        );
        let refill_blocks = refill();
        let refill = i128::from(refill_blocks);
        assert!(
            (expected - refill..=expected + refill).contains(&channel),
            "round {round}: {channel} blocks for {added} added batches: not {expected} \
             ({DRIVER_MIN_BLOCKS_PER_BATCH} per batch that finds the channel empty, a refill of \
             {refill_blocks} either way)"
        );
        assert!(
            (parks_expected - refill..=parks_expected + refill).contains(&parked_extra),
            "round {round}: {parked_extra} blocks more for {} parked batches: not \
             {parks_expected} ({DRIVER_MAX_BLOCKS_PER_BATCH} per batch that parks the connection, \
             {DRIVER_MIN_BLOCKS_PER_BATCH} per batch that does not, a refill of {refill_blocks} either \
             way)",
            parked.parked
        );
        measured += 1;
    }
    assert_eq!(measured, ROUNDS, "every measured round was checked");
    // the server lets go of its handle after its last answer, when it next runs
    rt.block_on(async {
        while Arc::strong_count(&client) > 1 {
            tokio::task::yield_now().await;
        }
    });
    let client = Arc::try_unwrap(client).unwrap_or_else(|_| panic!("the server holds the client"));
    drop(client);
    rt.block_on(server).unwrap();
    rt.block_on(driver).unwrap().unwrap();
}
