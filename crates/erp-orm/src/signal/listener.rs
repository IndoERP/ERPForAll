//! The NOTIFY listener: one connection per (process, database) to the primary.

use std::sync::Arc;
use std::time::Duration;

use erp_fields::{Error, ErrorKind, Result};
use tokio::sync::{Notify, oneshot, watch};
use tokio::task::JoinHandle;
use tokio::time::MissedTickBehavior;
use tokio_postgres::tls::{MakeTlsConnect, TlsConnect};
use tokio_postgres::{AsyncMessage, Config, Socket};

use super::{Generations, READ_SQL, SIGNAL_CHANNEL, Signal, read_error};
use crate::Orm;

/// Timing of a listener.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct ListenerOptions {
    /// How often the generations are read even when no notification arrived (a lost notification
    /// is repaired within one interval), and how often the pool is read while the listener is
    /// down. Default 30 s.
    pub poll_interval: Duration,
    /// First reconnect delay; doubles up to [`backoff_max`](Self::backoff_max) with jitter.
    /// Default 200 ms.
    pub backoff_min: Duration,
    /// Longest reconnect delay. Default 10 s.
    pub backoff_max: Duration,
    /// Longest wait for one generation read (connection from the pool included) and the apply
    /// after it; a read that takes longer is abandoned and retried at the next wake-up.
    /// Default 10 s.
    pub read_timeout: Duration,
}

impl Default for ListenerOptions {
    fn default() -> Self {
        Self {
            poll_interval: Duration::from_secs(30),
            backoff_min: Duration::from_millis(200),
            backoff_max: Duration::from_secs(10),
            read_timeout: Duration::from_secs(10),
        }
    }
}

impl ListenerOptions {
    /// Set the poll interval (must be non-zero; checked when the listener starts).
    #[must_use]
    pub fn poll_interval(mut self, interval: Duration) -> Self {
        self.poll_interval = interval;
        self
    }

    /// Set the reconnect delays (`min <= max`, both non-zero; checked when the listener starts).
    #[must_use]
    pub fn backoff(mut self, min: Duration, max: Duration) -> Self {
        self.backoff_min = min;
        self.backoff_max = max;
        self
    }

    /// Set the read timeout (must be non-zero; checked when the listener starts).
    #[must_use]
    pub fn read_timeout(mut self, timeout: Duration) -> Self {
        self.read_timeout = timeout;
        self
    }

    fn check(&self) -> Result<()> {
        if self.poll_interval.is_zero()
            || self.backoff_min.is_zero()
            || self.read_timeout.is_zero()
            || self.backoff_min > self.backoff_max
        {
            return Err(Error::programming(
                "listener options: poll_interval, backoff_min and read_timeout must be non-zero \
                 and backoff_min <= backoff_max (zero never means unlimited or immediate)",
            ));
        }
        Ok(())
    }
}

/// Section `signal` of the configuration digest: every timing is a node
/// setting. A slower poll or a longer backoff on one replica delays only that replica's view of
/// the generations; it never makes two replicas disagree on what a generation means.
impl crate::node::SharedSettings for ListenerOptions {
    fn section(&self) -> crate::node::SectionName {
        crate::node::SectionName::new("signal")
    }

    fn describe(
        &self,
        out: &mut crate::node::SettingsWriter<'_>,
    ) -> std::result::Result<(), crate::node::NodeError> {
        let Self {
            poll_interval: _,
            backoff_min: _,
            backoff_max: _,
            read_timeout: _,
        } = self;
        out.node("signal.poll_interval")?;
        out.node("signal.backoff_min")?;
        out.node("signal.backoff_max")?;
        out.node("signal.read_timeout")
    }
}

/// The reconnect delay: doubles after every failed attempt up to the maximum, and starts over only
/// after a session got as far as `LISTEN` and the catch-up read (a server that accepts the
/// connection and then fails would otherwise be hammered at the minimum delay).
#[derive(Debug, Clone, Copy)]
struct Backoff {
    current: Duration,
    min: Duration,
    max: Duration,
}

impl Backoff {
    fn new(options: &ListenerOptions) -> Self {
        Self {
            current: options.backoff_min,
            min: options.backoff_min,
            max: options.backoff_max,
        }
    }

    /// A session got up (`LISTEN` and catch-up succeeded) before it ended.
    fn session_was_up(&mut self) {
        self.current = self.min;
    }

    /// The delay before the next attempt, then the one after is doubled.
    fn next_delay(&mut self) -> Duration {
        let delay = self.current;
        self.current = self.current.saturating_mul(2).min(self.max);
        delay
    }
}

/// A running listener. Dropping it stops the task; [`shutdown`](Self::shutdown) waits for it.
pub struct ListenerHandle {
    stop: watch::Sender<bool>,
    task: Option<JoinHandle<()>>,
    signal: Arc<Signal>,
}

impl ListenerHandle {
    /// True once the listener task ended: after [`shutdown`](Self::shutdown), or by itself when its
    /// [`Orm`] was superseded (a refresh replaced it; start a listener for the new `Orm`).
    pub fn is_finished(&self) -> bool {
        self.task.as_ref().is_none_or(JoinHandle::is_finished)
    }

    /// Stop the listener and wait until its task ended.
    pub async fn shutdown(mut self) {
        // a send error only means the task already ended
        self.stop.send_replace(true);
        if let Some(task) = self.task.take()
            && let Err(e) = task.await
        {
            tracing::error!(target: "erp::signal", error = %e, "signal listener task failed");
        }
    }
}

impl Drop for ListenerHandle {
    fn drop(&mut self) {
        // no listener is expected any more: routed reads stop waiting for it
        self.signal.set_listener_expected(false);
        self.stop.send_replace(true);
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

impl Orm {
    /// Start the listener of this database: `LISTEN` on the signal channel on its own connection
    /// (to the primary; `config` must not go through transaction pooling), a catch-up read after
    /// every (re)connect, a read on every wake-up and on every poll interval, and
    /// the signal's apply after each read (a refresh when something changed). While it is down it
    /// reads through the pool every interval and retries the connection with backoff; every read
    /// is bounded by [`ListenerOptions::read_timeout`] and gives way to a shutdown. The listener
    /// ends by itself when its `Orm` is superseded ([`ListenerHandle::is_finished`]). One per
    /// (process, database).
    ///
    /// # Errors
    ///
    /// `Programming` when the `Orm` has no [`Signal`] or the options are invalid.
    pub fn spawn_signal_listener<T>(
        &self,
        config: Config,
        tls: T,
        options: ListenerOptions,
    ) -> Result<ListenerHandle>
    where
        T: MakeTlsConnect<Socket> + Clone + Send + Sync + 'static,
        T::Stream: Send + 'static,
        T::TlsConnect: Send,
        <T::TlsConnect as TlsConnect<Socket>>::Future: Send,
    {
        let Some(signal) = self.shared.signal.clone() else {
            return Err(Error::programming(
                "spawn_signal_listener: this Orm has no Signal (OrmBuilder::signal)",
            ));
        };
        options.check()?;
        // from now on a down listener means this process may not know of changes: routed reads
        // use the primary until the catch-up read succeeded
        signal.set_listener_expected(true);
        let (stop, stop_rx) = watch::channel(false);
        let orm = self.clone();
        let task = tokio::spawn(run(orm, config, tls, options, stop_rx));
        Ok(ListenerHandle {
            stop,
            task: Some(task),
            signal,
        })
    }

    /// Read through the pool and apply (the listener is down).
    async fn poll_through_pool(&self, signal: &Signal) -> Result<()> {
        let conn = self.pool.get().await.map_err(|e| {
            Error::new(ErrorKind::Database, "could not get a database connection").with_source(e)
        })?;
        let read_epoch = signal.apply_epoch();
        let observed = Signal::read(&conn).await?;
        drop(conn);
        self.shared
            .apply_signal(&observed, read_epoch)
            .await
            .map(|_| ())
    }
}

enum Ended {
    Stopped,
    /// The `Orm` was replaced: this listener has nothing left to keep fresh.
    Superseded,
    /// The connection ended; `was_up` when `LISTEN` and the catch-up read had succeeded.
    Disconnected {
        why: String,
        was_up: bool,
    },
}

/// What one read-and-apply did.
enum Round {
    Done,
    Superseded,
}

async fn run<T>(
    orm: Orm,
    config: Config,
    tls: T,
    options: ListenerOptions,
    mut stop: watch::Receiver<bool>,
) where
    T: MakeTlsConnect<Socket> + Clone + Send + Sync + 'static,
    T::Stream: Send + 'static,
    T::TlsConnect: Send,
    <T::TlsConnect as TlsConnect<Socket>>::Future: Send,
{
    let Some(signal) = orm.shared.signal.clone() else {
        return;
    };
    let mut backoff = Backoff::new(&options);
    let mut first = true;
    loop {
        if *stop.borrow() || signal.is_superseded() {
            break;
        }
        if !first {
            signal.count_reconnect();
        }
        first = false;
        let mut delay = backoff.next_delay();
        match config.connect(tls.clone()).await {
            Ok((client, connection)) => {
                match serve(&orm, &signal, client, connection, options, &mut stop).await {
                    Ended::Stopped => break,
                    Ended::Superseded => {
                        tracing::info!(
                            target: "erp::signal",
                            "signal listener stops: its Orm was superseded"
                        );
                        break;
                    }
                    Ended::Disconnected { why, was_up } => {
                        signal.set_listener_up(false);
                        if was_up {
                            backoff.session_was_up();
                            delay = backoff.next_delay();
                        }
                        tracing::warn!(
                            target: "erp::signal",
                            reason = %why,
                            retry_in_ms = u64::try_from(delay.as_millis()).unwrap_or(u64::MAX),
                            "signal listener disconnected; reconnecting"
                        );
                    }
                }
            }
            Err(e) => {
                signal.set_listener_up(false);
                tracing::warn!(
                    target: "erp::signal",
                    error = %e,
                    retry_in_ms = u64::try_from(delay.as_millis()).unwrap_or(u64::MAX),
                    "signal listener could not connect; polling through the pool"
                );
            }
        }
        // down: keep the process correct by polling through the pool, then wait out the backoff
        signal.count_poll();
        let polled = tokio::select! {
            r = tokio::time::timeout(options.read_timeout, orm.poll_through_pool(&signal)) => r,
            _ = stop.changed() => break,
        };
        match polled {
            Ok(Ok(())) => {}
            Ok(Err(e)) if e.is_superseded() => {
                tracing::info!(target: "erp::signal", "signal listener stops: its Orm was superseded");
                break;
            }
            Ok(Err(e)) => {
                tracing::error!(target: "erp::signal", error = %e, "polling the generations failed");
            }
            Err(_) => {
                tracing::error!(
                    target: "erp::signal",
                    timeout_ms = u64::try_from(options.read_timeout.as_millis()).unwrap_or(u64::MAX),
                    "polling the generations through the pool timed out"
                );
            }
        }
        if signal.is_superseded() {
            tracing::info!(target: "erp::signal", "signal listener stops: its Orm was superseded");
            break;
        }
        let jittered = delay.saturating_add(Duration::from_millis(crate::transaction::jitter(
            // a delay too long for u64 milliseconds (only an absurd backoff_max) gets no jitter;
            // u64::MAX would overflow inside jitter
            u64::try_from(delay.as_millis() / 4).unwrap_or(0),
        )));
        tokio::select! {
            () = tokio::time::sleep(jittered) => {}
            _ = stop.changed() => {}
        }
    }
    signal.set_listener_up(false);
}

/// One connected session: `LISTEN`, catch up, then wake on notifications and the poll interval
/// until the connection ends or the listener is stopped.
async fn serve<S, Tl>(
    orm: &Orm,
    signal: &Arc<Signal>,
    client: tokio_postgres::Client,
    mut connection: tokio_postgres::Connection<S, Tl>,
    options: ListenerOptions,
    stop: &mut watch::Receiver<bool>,
) -> Ended
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
    Tl: tokio_postgres::tls::TlsStream + Unpin + Send + 'static,
{
    let woken = Arc::new(Notify::new());
    let (done_tx, mut done_rx) = oneshot::channel::<String>();
    let driver = {
        let woken = Arc::clone(&woken);
        let signal = Arc::clone(signal);
        tokio::spawn(async move {
            let why = loop {
                match std::future::poll_fn(|cx| connection.poll_message(cx)).await {
                    Some(Ok(AsyncMessage::Notification(notification))) => {
                        // the payload is deliberately never looked at
                        signal.count_notification();
                        if notification.channel() == SIGNAL_CHANNEL {
                            woken.notify_one();
                        } else {
                            signal.deliver(notification.channel());
                        }
                    }
                    Some(Ok(AsyncMessage::Notice(notice))) => {
                        tracing::debug!(target: "erp::signal", notice = %notice, "server notice");
                    }
                    Some(Ok(_)) => {}
                    Some(Err(e)) => break e.to_string(),
                    None => break "connection closed".to_owned(),
                }
            };
            // the receiver is gone when the session already ended
            let _ = done_tx.send(why);
        })
    };
    let mut was_up = false;
    let ended = session(
        orm,
        signal,
        &client,
        Wake {
            woken: &woken,
            done_rx: &mut done_rx,
            stop,
        },
        options,
        &mut was_up,
    )
    .await;
    driver.abort();
    match ended {
        Ended::Disconnected { why, .. } => Ended::Disconnected { why, was_up },
        other => other,
    }
}

/// What can end or wake a session.
struct Wake<'a> {
    woken: &'a Notify,
    done_rx: &'a mut oneshot::Receiver<String>,
    stop: &'a mut watch::Receiver<bool>,
}

async fn session(
    orm: &Orm,
    signal: &Arc<Signal>,
    client: &tokio_postgres::Client,
    wake: Wake<'_>,
    options: ListenerOptions,
    was_up: &mut bool,
) -> Ended {
    let Wake {
        woken,
        done_rx,
        stop,
    } = wake;
    let down = |why: String| Ended::Disconnected { why, was_up: false };
    // LISTEN first, then read: a change between the two is seen by the read, one after by the wake-up
    let listen = format!("LISTEN {SIGNAL_CHANNEL}");
    let started = tokio::select! {
        r = tokio::time::timeout(options.read_timeout, client.batch_execute(&listen)) => match r {
            Ok(r) => r.map_err(|e| e.to_string()),
            Err(_) => Err("LISTEN timed out".to_owned()),
        },
        why = &mut *done_rx => Err(why.unwrap_or_else(|_| "connection task ended".to_owned())),
        _ = stop.changed() => return Ended::Stopped,
    };
    if let Err(why) = started {
        return down(why);
    }
    let mut listening: Vec<String> = Vec::new();
    let mut seen_epoch = signal.subscription_epoch();
    if let Err(why) = sync_listens(client, signal, &mut listening, options).await {
        return down(why);
    }
    match read_and_apply(orm, signal, client, options, stop).await {
        Ok(Some(Round::Done)) => {}
        Ok(Some(Round::Superseded)) => return Ended::Superseded,
        Ok(None) => return Ended::Stopped,
        Err(why) => return down(why),
    }
    *was_up = true;
    signal.set_listener_up(true);
    // notifications sent while the connection was down are lost: every subscriber looks once
    signal.wake_subscribers();
    let mut tick = tokio::time::interval(options.poll_interval);
    tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
    tick.tick().await; // the first tick is immediate; the catch-up read above covered it
    loop {
        tokio::select! {
            () = woken.notified() => {}
            _ = tick.tick() => signal.count_poll(),
            why = &mut *done_rx => {
                return down(why.unwrap_or_else(|_| "connection task ended".to_owned()));
            }
            _ = stop.changed() => return Ended::Stopped,
        }
        if signal.subscription_epoch() != seen_epoch {
            seen_epoch = signal.subscription_epoch();
            if let Err(why) = sync_listens(client, signal, &mut listening, options).await {
                return down(why);
            }
            signal.wake_subscribers();
        }
        match read_and_apply(orm, signal, client, options, stop).await {
            Ok(Some(Round::Done)) => {}
            Ok(Some(Round::Superseded)) => return Ended::Superseded,
            Ok(None) => return Ended::Stopped,
            Err(why) => return down(why),
        }
    }
}

/// `LISTEN` on every channel with a subscriber that this session does not listen on yet.
async fn sync_listens(
    client: &tokio_postgres::Client,
    signal: &Signal,
    listening: &mut Vec<String>,
    options: ListenerOptions,
) -> std::result::Result<(), String> {
    for channel in signal.subscribed_channels() {
        if listening.contains(&channel) {
            continue;
        }
        let listen = format!("LISTEN {channel}");
        match tokio::time::timeout(options.read_timeout, client.batch_execute(&listen)).await {
            Ok(Ok(())) => listening.push(channel),
            Ok(Err(e)) => return Err(e.to_string()),
            Err(_) => return Err(format!("LISTEN {channel} timed out")),
        }
    }
    Ok(())
}

/// Read on the listener's connection and apply. A failed or timed-out read means the connection
/// is not usable (the session ends and reconnects); a failed apply is logged and retried at the
/// next wake-up; a superseded `Orm` ends the listener. `Ok(None)`: stopped meanwhile.
async fn read_and_apply(
    orm: &Orm,
    signal: &Signal,
    client: &tokio_postgres::Client,
    options: ListenerOptions,
    stop: &mut watch::Receiver<bool>,
) -> std::result::Result<Option<Round>, String> {
    let read_epoch = signal.apply_epoch();
    let messages = tokio::select! {
        r = tokio::time::timeout(options.read_timeout, client.simple_query(READ_SQL)) => match r {
            Ok(r) => r.map_err(|e| read_error(e).message().to_owned())?,
            Err(_) => return Err("the generation read timed out".to_owned()),
        },
        _ = stop.changed() => return Ok(None),
    };
    let observed = Generations::from_messages(&messages).map_err(|e| e.message().to_owned())?;
    match orm.shared.apply_signal(&observed, read_epoch).await {
        Ok(super::Applied::Superseded) => Ok(Some(Round::Superseded)),
        Ok(super::Applied::UpToDate | super::Applied::InPlace) => Ok(Some(Round::Done)),
        Err(e) if e.is_superseded() => Ok(Some(Round::Superseded)),
        Err(e) => {
            tracing::error!(target: "erp::signal", error = %e, "applying generations failed");
            Ok(Some(Round::Done))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Review item 6: a connection that succeeds but whose session never gets up does not reset the
    /// delay; a session that got up does.
    #[test]
    fn backoff_resets_only_after_a_session_was_up() {
        let options = ListenerOptions::default()
            .backoff(Duration::from_millis(100), Duration::from_millis(800));
        let mut backoff = Backoff::new(&options);
        let delays: Vec<u128> = (0..5).map(|_| backoff.next_delay().as_millis()).collect();
        assert_eq!(delays, [100, 200, 400, 800, 800]);
        backoff.session_was_up();
        assert_eq!(backoff.next_delay().as_millis(), 100);
        assert_eq!(backoff.next_delay().as_millis(), 200);
    }
}
