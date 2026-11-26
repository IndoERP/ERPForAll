// SPDX-License-Identifier: MIT OR Apache-2.0
//! Notify-only saved thread subscriptions. A message payload is always read by normal RPC.
use super::*;
use futures_util::{SinkExt, StreamExt};
use hyper_util::rt::TokioIo;
use std::sync::{OnceLock, atomic::AtomicU64};
use tokio::time::Instant;
use tokio_tungstenite::{
    WebSocketStream,
    tungstenite::{
        handshake::server::create_response_with_body,
        protocol::{CloseFrame, Message, Role, WebSocketConfig},
    },
};
use tokio_util::io::InspectReader;
type ByteObserver = Box<dyn FnMut(&[u8]) + Send + Sync>;
type WebSocket = WebSocketStream<InspectReader<TokioIo<hyper::upgrade::Upgraded>, ByteObserver>>;
use erp_security::auth::SessionSecret;
use tokio::sync::watch;

pub(super) fn capacity(limits: &crate::startup_limits::StartupLimits) -> usize {
    let count = limits.ws_inflight_bytes() / limits.ws_socket_bytes();
    count
        .min(limits.max_connections())
        .min(Semaphore::MAX_PERMITS as u64) as usize
}

/// Native addon admission and visible sequence for a single record, under a fresh browser transaction.
pub trait WebThreadFeed: Send + Sync {
    /// A changed sequence wakes this reader; neither IDs nor record values are returned over the wire.
    fn visible_window<'a>(
        &'a self,
        tx: &'a mut erp_orm::Transaction,
        model: &'a str,
        id: i64,
    ) -> erp_orm::BoxFuture<'a, erp_fields::Result<Vec<i64>>>;
}

/// One PostgreSQL LISTEN subscription and one coalescing fan-out task per active ORM generation.
#[derive(Default)]
pub(super) struct WakeHub(Mutex<Option<(u64, watch::Receiver<()>)>>);
impl WakeHub {
    async fn subscribe(&self, auth: &Auth) -> HttpResult<watch::Receiver<()>> {
        let mut current = self.0.lock().await;
        if let Some((id, rx)) = &*current
            && *id == auth.orm().id()
        {
            return Ok(rx.clone());
        }
        let signal = auth.orm().signal().ok_or_else(busy)?;
        let wake = signal
            .subscribe(&erp_orm::NotifyChannel::new("erp_thread_changed").map_err(auth_error)?)
            .map_err(auth_error)?;
        let mut security = signal.subscribe_security_changes();
        let (tx, rx) = watch::channel(());
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    () = tx.closed() => break,
                    () = wake.notified() => { tx.send_replace(()); }
                    result = security.changed() => {
                        if result.is_err() { break; }
                        tx.send_replace(());
                    }
                }
            }
        });
        *current = Some((auth.orm().id(), rx.clone()));
        Ok(rx)
    }
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Subscription {
    #[serde(rename = "type")]
    kind: String,
    csrf: String,
    model: String,
    id: i64,
    company: i64,
}
impl Subscription {
    fn parse(text: &str) -> Option<Self> {
        let sub: Self = serde_json::from_str(text).ok()?;
        if sub.kind != "subscribe"
            || sub.id <= 0
            || sub.company <= 0
            || sub.model.is_empty()
            || !sub
                .model
                .bytes()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'.' || c == b'_')
        {
            return None;
        }
        Some(sub)
    }
}

impl WebApp {
    /// Register the payload-free channel before starting the ORM generation's signal listener.
    /// # Errors
    /// Refuses a missing signal service or exhausted channel registry on a composed mail node.
    pub async fn prepare_thread_notifications(&self, auth: &Auth) -> std::io::Result<()> {
        if self.thread_feed.is_some() && auth.orm().registry().model("mail.thread").is_some() {
            self.thread_wakes.subscribe(auth).await.map_err(|_| {
                std::io::Error::other("Could not register thread notification channel")
            })?;
        }
        Ok(())
    }

    /// Compose a native, access-checked source for saved-record notification windows.
    pub fn with_thread_feed(mut self, feed: Arc<dyn WebThreadFeed>) -> Self {
        self.thread_feed = Some(feed);
        self
    }

    pub(super) async fn thread_socket(
        &self,
        parts: axum::http::request::Parts,
        auth: Arc<Auth>,
    ) -> HttpResult<Response> {
        let headers = &parts.headers;
        if parts.method != Method::GET || parts.uri.query().is_some() {
            return Err(bad("Unexpected websocket request"));
        }
        if headers.contains_key(header::AUTHORIZATION)
            || one(headers, "origin")? != Some(self.config.origin())
        {
            return Err(response(
                StatusCode::FORBIDDEN,
                "Open the socket from the signed-in application.",
            ));
        }
        let secret = self
            .session(headers)?
            .ok_or_else(|| response(StatusCode::UNAUTHORIZED, "Sign in to open the socket."))?;
        let cookies = values(headers, "cookie")?;
        let preference = boundary::cookie(&cookies, "prefs")
            .map_err(|_| bad("The thread subscription needs a valid signed browser preference."))?
            .ok_or_else(|| {
                bad("The thread subscription needs a valid signed browser preference.")
            })?;
        let signer = auth.browser_tokens().await.map_err(auth_error)?;
        let prefs = signer.verify(preference).map_err(csrf_refusal)?;
        let feed = self.thread_feed.clone().ok_or_else(|| {
            response(
                StatusCode::NOT_FOUND,
                "Thread notifications are unavailable.",
            )
        })?;
        let slot = Arc::clone(&self.socket_slots)
            .try_acquire_owned()
            .map_err(|_| busy())?;
        let mut request = Request::from_parts(parts, ());
        let handshake = create_response_with_body(&request, Body::empty)
            .map_err(|_| bad("A valid websocket upgrade is required."))?;
        let upgrade = request
            .extensions_mut()
            .remove::<hyper::upgrade::OnUpgrade>()
            .ok_or_else(|| bad("A valid websocket upgrade is required."))?;
        let mut tx = auth
            .begin_session(
                &secret,
                CredentialOptions::new().read_only("thread socket admission"),
            )
            .await
            .map_err(auth_error)?
            .tx;
        let env = tx.env();
        if env.is_superuser()
            || env.principal().kind() != erp_orm::PrincipalKind::User
            || env.principal().actor().is_some()
            || !tx
                .has_group(&env, "base.group_user")
                .await
                .map_err(auth_error)?
        {
            return Err(response(
                StatusCode::FORBIDDEN,
                "An internal user session is required.",
            ));
        }
        if env.registry().model("mail.thread").is_none() {
            return Err(response(
                StatusCode::NOT_FOUND,
                "Thread notifications are unavailable.",
            ));
        }
        tx.rollback().await.map_err(auth_error)?;
        drop(tx);
        let wake = self.thread_wakes.subscribe(&auth).await?;
        let limits = self.config.startup_limits();
        let io = Duration::from_secs(limits.ws_message_read_secs());
        let max = usize::try_from(limits.ws_max_message_bytes()).map_err(|_| internal())?;
        let read = usize::try_from(limits.ws_read_buffer_bytes()).map_err(|_| internal())?;
        let session = SocketSession {
            auth,
            secret,
            signer,
            prefs,
            feed,
            draining: Arc::clone(&self.draining),
            admission: self.admission.clone(),
            io,
            min_rate: limits.ws_min_bytes_per_sec(),
            heartbeat: io.min(Duration::from_secs(
                limits
                    .ws_max_message_bytes()
                    .div_ceil(limits.ws_min_bytes_per_sec()),
            )) / 2,
        };
        let age = self.config.connection_limits().max_age();
        let config = WebSocketConfig::default()
            .max_message_size(Some(max))
            .max_frame_size(Some(max))
            .read_buffer_size(read)
            .write_buffer_size(0)
            .max_write_buffer_size(max);
        tokio::spawn(async move {
            let _slot = slot;
            if let Ok(Ok(raw)) = tokio::time::timeout(io, upgrade).await {
                // Inspect the upgraded stream, including bytes Hyper may already have buffered.
                let received = Arc::new(InputProgress::default());
                let observed = received.clone();
                let count: ByteObserver = Box::new(move |chunk| {
                    observed.observe(chunk.len());
                });
                let socket = WebSocketStream::from_raw_socket(
                    InspectReader::new(TokioIo::new(raw), count),
                    Role::Server,
                    Some(config),
                )
                .await;
                let _ended = tokio::time::timeout(age, session.run(socket, wake, &received)).await;
            }
        });
        Ok(handshake)
    }
}

#[derive(Default)]
struct InputProgress {
    bytes: AtomicU64,
    first: OnceLock<Instant>,
    started: tokio::sync::Notify,
}
impl InputProgress {
    fn observe(&self, length: usize) {
        if length == 0 {
            return;
        }
        self.bytes.fetch_add(length as u64, Ordering::Relaxed);
        if self.first.set(Instant::now()).is_ok() {
            self.started.notify_one();
        }
    }
    // Rates are measured per second starting at the first received byte. The initial second
    // admits short control envelopes; subsequent seconds must be earned by actual wire bytes.
    fn deadline(&self, rate: u64) -> Option<Instant> {
        self.first.get().map(|start| {
            *start + Duration::from_secs(self.bytes.load(Ordering::Relaxed).div_ceil(rate))
        })
    }
    async fn slow(&self, rate: u64) {
        self.started.notified().await;
        while let Some(until) = self.deadline(rate) {
            if Instant::now() >= until {
                return;
            }
            tokio::time::sleep_until(until).await;
        }
    }
}

struct SocketSession {
    auth: Arc<Auth>,
    secret: SessionSecret,
    signer: Arc<PrefsSigner>,
    prefs: SignedPrefs,
    feed: Arc<dyn WebThreadFeed>,
    draining: Arc<AtomicBool>,
    admission: Option<erp_orm::node::NodeAdmission>,
    io: Duration,
    min_rate: u64,
    heartbeat: Duration,
}

#[derive(Default)]
struct SocketCounts {
    wakeups: u64,
    heartbeats: u64,
    resyncs: u64,
    refused: bool,
}

impl Drop for SocketCounts {
    fn drop(&mut self) {
        tracing::debug!(
            wakeups = self.wakeups,
            heartbeats = self.heartbeats,
            resyncs = self.resyncs,
            refused = self.refused,
            "thread socket delivery totals"
        );
    }
}

impl SocketSession {
    async fn window(&self, sub: &Subscription) -> erp_fields::Result<Vec<i64>> {
        if self.draining.load(Ordering::Acquire)
            || self.admission.as_ref().is_some_and(|a| !a.is_serving())
            || self
                .auth
                .orm()
                .signal()
                .is_some_and(erp_orm::Signal::is_superseded)
        {
            return Err(erp_fields::Error::internal(
                "Thread notifications are reconnecting.",
            ));
        }
        let context = erp_orm::Context::from_pairs([(
            "allowed_company_ids",
            erp_fields::Value::list([erp_fields::Value::Int(sub.company)]),
        )]);
        let mut tx = self
            .auth
            .begin_session(
                &self.secret,
                CredentialOptions::new()
                    .context(context)
                    .read_only("thread socket observation"),
            )
            .await?
            .tx;
        let env = tx.env();
        if env.is_superuser()
            || env.principal().kind() != erp_orm::PrincipalKind::User
            || env.principal().actor().is_some()
            || !tx.has_group(&env, "base.group_user").await?
        {
            return Err(erp_fields::Error::access(
                "Thread subscription is unavailable.",
            ));
        }
        let ids = self
            .feed
            .visible_window(&mut tx, &sub.model, sub.id)
            .await?;
        tx.rollback().await?;
        Ok(ids)
    }

    async fn checked_window(&self, socket: &mut WebSocket, sub: &Subscription) -> Option<Vec<i64>> {
        match tokio::time::timeout(self.io, self.window(sub)).await {
            Ok(Ok(window)) => Some(window),
            Ok(Err(error)) if matches!(error.kind().http_status(), 401 | 403 | 404 | 422) => {
                refuse(socket, self.io).await;
                None
            }
            _ => {
                let _sent = send(
                    socket,
                    self.io,
                    Message::Close(Some(CloseFrame {
                        code: 1011.into(),
                        reason: "Reconnect to the thread".into(),
                    })),
                )
                .await;
                None
            }
        }
    }
    async fn run(
        self,
        mut socket: WebSocket,
        mut wake: watch::Receiver<()>,
        received: &InputProgress,
    ) {
        let frame = tokio::select! {
            frame = tokio::time::timeout(self.io, socket.next()) => frame.ok().flatten(),
            () = received.slow(self.min_rate) => None,
        };
        let raw = match frame {
            Some(Ok(Message::Text(raw)))
                if received
                    .deadline(self.min_rate)
                    .is_some_and(|until| Instant::now() < until) =>
            {
                raw
            }
            Some(Err(tokio_tungstenite::tungstenite::Error::Capacity(_))) => {
                too_large(&mut socket, self.io).await;
                return;
            }
            _ => {
                refuse(&mut socket, self.io).await;
                return;
            }
        };
        // The only inbound application frame is one complete masked text subscription.
        // Any additional bytes cannot be justified by an authorized Pong. Counting all wire
        // bytes prevents a fragmented payload from surviving an otherwise healthy heartbeat.
        let length = raw.len() as u64;
        let mut accepted = length
            + if length < 126 {
                6
            } else if length <= 65535 {
                8
            } else {
                14
            };
        if received.bytes.load(Ordering::Relaxed) != accepted {
            refuse(&mut socket, self.io).await;
            return;
        }
        let Some(sub) = Subscription::parse(&raw) else {
            refuse(&mut socket, self.io).await;
            return;
        };
        if !now().is_ok_and(|current| {
            self.signer
                .verify_csrf(&self.prefs, &sub.csrf, current)
                .is_ok()
        }) {
            refuse(&mut socket, self.io).await;
            return;
        }
        let Some(mut visible) = self.checked_window(&mut socket, &sub).await else {
            return;
        };
        let resync = || Message::Text(r#"{"type":"resync"}"#.into());
        if !send(&mut socket, self.io, resync()).await {
            return;
        }
        let mut counts = SocketCounts::default();
        let mut check = tokio::time::interval(self.heartbeat);
        let mut awaiting_pong = false;
        check.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        check.tick().await;
        loop {
            let heartbeat = tokio::select! {
                changed = wake.changed() => { if changed.is_err() {return;} false },
                _ = check.tick() => true,
                msg = socket.next() => match msg {
                    Some(Ok(Message::Pong(data))) if awaiting_pong && data.is_empty() => {
                        accepted += 6; // RFC 6455 masked, empty control frame.
                        if received.bytes.load(Ordering::Relaxed) != accepted {
                            refuse(&mut socket, self.io).await;
                            return;
                        }
                        awaiting_pong = false;
                        continue;
                    },
                    Some(Err(tokio_tungstenite::tungstenite::Error::Capacity(_))) => {
                        too_large(&mut socket, self.io).await; return;
                    },
                    Some(Ok(Message::Close(_))) | None | Some(Err(_)) => return,
                    _ => {refuse(&mut socket,self.io).await;return;}
                }
            };
            if heartbeat && awaiting_pong {
                refuse(&mut socket, self.io).await;
                return;
            }
            if heartbeat {
                counts.heartbeats += 1;
            } else {
                counts.wakeups += 1;
            }
            let Some(next) = self.checked_window(&mut socket, &sub).await else {
                counts.refused = true;
                return;
            };
            if next != visible {
                visible = next;
                if !send(&mut socket, self.io, resync()).await {
                    return;
                }
                counts.resyncs += 1;
            }
            if heartbeat {
                awaiting_pong = true;
                if !send(&mut socket, self.io, Message::Ping(bytes::Bytes::new())).await {
                    return;
                }
            }
        }
    }
}
async fn send(socket: &mut WebSocket, limit: Duration, frame: Message) -> bool {
    matches!(
        tokio::time::timeout(limit, socket.send(frame)).await,
        Ok(Ok(()))
    )
}
async fn refuse(socket: &mut WebSocket, limit: Duration) {
    let _sent = send(
        socket,
        limit,
        Message::Close(Some(CloseFrame {
            code: 1008.into(),
            reason: "Subscription unavailable".into(),
        })),
    )
    .await;
}

async fn too_large(socket: &mut WebSocket, limit: Duration) {
    let _sent = send(
        socket,
        limit,
        Message::Close(Some(CloseFrame {
            code: 1009.into(),
            reason: "Frame exceeds the configured message limit".into(),
        })),
    )
    .await;
}
