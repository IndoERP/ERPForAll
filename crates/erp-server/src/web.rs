//! Bounded browser transport using the existing authentication service.
#![allow(
    clippy::result_large_err,
    reason = "HTTP helpers propagate an already-rendered response with headers and body; boxing would add an allocation to every refusal"
)]
use crate::{boundary, config::ServerConfig, redirect::RedirectPolicy, routes};
use axum::{
    Router,
    body::{Body, to_bytes},
    http::{HeaderMap, HeaderValue, Method, Request, StatusCode, header},
    response::{IntoResponse, Response},
};
use erp_security::auth::{
    ApiKeyCooldown, ApiKeyLastUse, ApiKeyRefusal, ApiKeyRequest, Auth, ClientAddr,
    CredentialOptions, LoginError, LoginRequest, SessionSecret,
    apikey_store::KeyScope,
    browser::{PrefsError, PrefsSigner, SignedPrefs},
};
use erp_views::wire::LoginPage;
use std::{
    borrow::Cow,
    collections::BTreeMap,
    error::Error as _,
    net::IpAddr,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::{
    net::TcpListener,
    sync::{Mutex, RwLock, Semaphore},
};

type HttpResult<T> = Result<T, Response>;

#[derive(Clone)]
struct Services {
    auth: Arc<Auth>,
    rpc: Option<Arc<erp_rpc::RpcService>>,
    assets: Option<AssetAdmission>,
}
impl Services {
    fn generation(&self) -> u64 {
        self.auth.orm().signal().map_or(0, |signal| {
            signal.applied().get(&erp_orm::SignalKey::Registry)
        })
    }
}

/// Result of tenant asset compatibility admission. A failed database remains addressable with
/// a clear maintenance response; no older-runtime bytes are substituted.
#[derive(Clone)]
#[non_exhaustive]
pub enum AssetAdmission {
    /// A complete committed generation for this process runtime.
    Published(Arc<crate::assets::PublishedAssets>),
    /// Startup assembly/admission failed. Web requests answer a fixed 503; the detail is for the
    /// operator log only (it names addons, asset paths and compiler messages).
    Broken(Arc<str>),
}

/// A fully admitted generation produced by native lifecycle composition.
#[non_exhaustive]
pub struct Publication {
    auth: Arc<Auth>,
    rpc: Arc<erp_rpc::RpcService>,
    mutation: MutationResult,
    assets: Option<AssetAdmission>,
}
impl Publication {
    /// A committed native generation; assets remain the prebuilt, admitted release bundle.
    pub fn new(auth: Arc<Auth>, rpc: Arc<erp_rpc::RpcService>, installed: Vec<String>) -> Self {
        Self {
            auth,
            rpc,
            mutation: MutationResult::Install(installed),
            assets: None,
        }
    }
    /// A committed upgrade with actual upgraded addons and newly installed dependencies.
    pub fn upgrade(
        auth: Arc<Auth>,
        rpc: Arc<erp_rpc::RpcService>,
        upgraded: Vec<String>,
        installed: Vec<String>,
    ) -> Self {
        let mut publication = Self::new(auth, rpc, Vec::new());
        publication.mutation = MutationResult::Upgrade {
            upgraded,
            installed,
        };
        publication
    }
    /// A committed "load demo data" run: the addons whose demo data loaded and the addons whose
    /// demo data failed (rolled back alone), each failure as `(addon, error text)`.
    pub fn force_demo(
        auth: Arc<Auth>,
        rpc: Arc<erp_rpc::RpcService>,
        loaded: Vec<String>,
        failed: Vec<(String, String)>,
    ) -> Self {
        let mut publication = Self::new(auth, rpc, Vec::new());
        publication.mutation = MutationResult::ForceDemo { loaded, failed };
        publication
    }
    /// A committed uninstall with the actual removed addons.
    pub fn uninstall(
        auth: Arc<Auth>,
        rpc: Arc<erp_rpc::RpcService>,
        uninstalled: Vec<String>,
    ) -> Self {
        let mut publication = Self::new(auth, rpc, Vec::new());
        publication.mutation = MutationResult::Uninstall(uninstalled);
        publication
    }
    /// Bind the tenant's committed assets or explicit broken-assets state to this publication.
    pub fn with_assets(mut self, admission: AssetAdmission) -> Self {
        self.assets = Some(admission);
        self
    }
}
enum MutationResult {
    Install(Vec<String>),
    Upgrade {
        upgraded: Vec<String>,
        installed: Vec<String>,
    },
    Uninstall(Vec<String>),
    ForceDemo {
        loaded: Vec<String>,
        failed: Vec<(String, String)>,
    },
}

/// Post-commit boundary that failed before a lifecycle generation became usable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum PublicationStage {
    /// Completion hooks after the engine commit.
    PostCommit,
    /// Runtime-role grant publication and readiness.
    RuntimeGrants,
    /// Auth and RPC reconstruction.
    Recomposition,
    /// Admitted runtime assets.
    Assets,
    /// Final atomic service publication.
    Publication,
}
impl PublicationStage {
    fn key(self) -> &'static str {
        match self {
            Self::PostCommit => "post_commit",
            Self::RuntimeGrants => "runtime_grants",
            Self::Recomposition => "recomposition",
            Self::Assets => "assets",
            Self::Publication => "publication",
        }
    }
    fn description(self) -> &'static str {
        match self {
            Self::PostCommit => "post-commit completion",
            Self::RuntimeGrants => "runtime grant publication",
            Self::Recomposition => "runtime recomposition",
            Self::Assets => "asset publication",
            Self::Publication => "service publication",
        }
    }
}

/// A lifecycle failure retains engine commit certainty independently of diagnostic text.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum LifecycleFailure {
    /// Ordinary refusal or known rollback, reported through the existing RPC error policy.
    #[error("{0}")]
    Refused(#[source] erp_fields::Error),
    /// The engine did not receive an authoritative commit acknowledgement. Never replay.
    #[error("lifecycle commit outcome is unknown: {0}")]
    Unknown(#[source] erp_fields::Error),
    /// The engine committed; recovery must complete this publication stage.
    #[error("lifecycle committed but {stage:?} failed: {error}")]
    Committed {
        /// Failed post-commit publication boundary.
        stage: PublicationStage,
        /// Private diagnostic source, never exposed by outcome responses.
        #[source]
        error: erp_fields::Error,
    },
}

/// Read-only current-generation admission bound to this configured tenant. Implementations may
/// fetch committed bytes on change; they must never collect sources, compile or publish.
pub trait AssetAuthority: Send + Sync {
    /// Verify the durable runtime/key before serving cached bytes.
    ///
    /// The server logs a refusal and answers a fixed 503 text; the error detail never reaches
    /// the client. A persistent refusal should be cheap to repeat: an implementation that
    /// reloads assets should remember a refused reload rather than retry it per request.
    /// # Errors
    /// Missing/incompatible/corrupt publication or database failure returns maintenance.
    fn admit<'a>(
        &'a self,
    ) -> erp_orm::BoxFuture<'a, erp_fields::Result<Arc<crate::assets::PublishedAssets>>>;
}

/// Trusted composition adapter for's sealed native installation flow.
pub trait WebLifecycle: Send + Sync {
    /// Rebuild a superseded runtime from durable primary module state. No install/upgrade or
    /// business write may be started by this refresh; pending lifecycle work returns maintenance.
    fn refresh<'a>(
        &'a self,
        auth: &'a Arc<Auth>,
    ) -> erp_orm::BoxFuture<'a, erp_fields::Result<Publication>>;
    /// Recheck this credential and system authority in the lifecycle-owned primary transaction,
    /// install only linked native addons, and return a complete generation after durable commit.
    fn install<'a>(
        &'a self,
        auth: &'a Arc<Auth>,
        secret: &'a SessionSecret,
        modules: Vec<String>,
    ) -> erp_orm::BoxFuture<'a, erp_fields::Result<Publication>>;
    /// Upgrade only admitted native addons, rechecking primary session authority under the
    /// lifecycle fence; return operation-specific results after complete publication admission.
    fn upgrade<'a>(
        &'a self,
        auth: &'a Arc<Auth>,
        secret: &'a SessionSecret,
        modules: Vec<String>,
    ) -> erp_orm::BoxFuture<'a, Result<Publication, LifecycleFailure>>;
    /// Make the database a demo database: rechecks primary session authority under
    /// the lifecycle fence and loads the demo data of every installed addon. The default refuses,
    /// so an adapter that does not implement it fails loudly instead of reporting success.
    fn force_demo<'a>(
        &'a self,
        auth: &'a Arc<Auth>,
        secret: &'a SessionSecret,
    ) -> erp_orm::BoxFuture<'a, Result<Publication, LifecycleFailure>> {
        let _ = (auth, secret);
        Box::pin(async {
            Err(LifecycleFailure::Refused(erp_fields::Error::programming(
                "This lifecycle adapter does not support loading demo data",
            )))
        })
    }
    /// Confirm exactly the reviewed uninstall impact. Unsupported native execution is a typed
    /// refusal, never an empty successful publication.
    fn uninstall<'a>(
        &'a self,
        auth: &'a Arc<Auth>,
        secret: &'a SessionSecret,
        modules: Vec<String>,
        impact: String,
    ) -> erp_orm::BoxFuture<'a, Result<Publication, LifecycleFailure>>;
}

mod content;
mod password_change;
mod pivot_export;
pub use content::{
    ContentLookup, ContentOpenFailure, ContentOpener, ContentRange, ContentReader, FoundContent,
    WebAttachmentContent,
};
pub use pivot_export::{PivotWorkbook, WebPivotExport};

/// Native composition supplies current nonsecret upload policy without coupling HTTP to an addon.
pub trait WebAttachmentPolicy: Send + Sync {
    /// Load policy inside the authenticated shell transaction and its registry generation.
    ///
    /// # Errors
    /// Preserve provider database, validation and runtime configuration errors for the caller.
    fn load<'a>(
        &'a self,
        tx: &'a mut erp_orm::Transaction,
    ) -> erp_orm::BoxFuture<'a, erp_fields::Result<erp_views::wire::AttachmentUploadLimits>>;
}

#[derive(Debug)]
struct BrowserCredential {
    auth: Arc<Auth>,
    secret: SessionSecret,
    client: IpAddr,
}
impl erp_rpc::RpcCredential for BrowserCredential {
    fn password<'a>(
        &'a self,
        orm: &'a erp_orm::Orm,
        call: erp_rpc::PasswordCall<'a>,
    ) -> erp_orm::BoxFuture<'a, std::result::Result<erp_rpc::PasswordReply, erp_rpc::RpcFailure>>
    {
        Box::pin(password_change::execute(self, orm, call))
    }
    fn begin<'a>(
        &'a self,
        orm: &'a erp_orm::Orm,
        options: CredentialOptions,
    ) -> erp_orm::BoxFuture<'a, erp_fields::Result<erp_orm::Transaction>> {
        Box::pin(async move {
            if !Arc::ptr_eq(orm.registry(), self.auth.orm().registry()) {
                return Err(erp_fields::Error::programming(
                    "RPC credential belongs to a different registry generation",
                ));
            }
            Ok(self.auth.begin_session(&self.secret, options).await?.tx)
        })
    }
}
// Request-local only: no successful credential or authority is cached across requests.
struct BearerCredential {
    auth: Arc<Auth>,
    secret: zeroize::Zeroizing<String>,
    session: Option<SessionSecret>,
    client: IpAddr,
    drop_cookie: AtomicBool,
    /// The key's first due last-use ticket, kept until this request-local credential is dropped,
    /// after the RPC call returned and the response was built: the write then starts after every
    /// request transaction has ended, so it cannot make the request fail to serialize against
    /// its own key row. `open` runs once as the preflight and again in each RPC attempt; later
    /// tickets for the same key are discarded unfired.
    last_use: std::sync::Mutex<Option<ApiKeyLastUse>>,
}
impl std::fmt::Debug for BearerCredential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BearerCredential").finish_non_exhaustive()
    }
}
impl BearerCredential {
    async fn open(&self, options: CredentialOptions) -> erp_fields::Result<erp_orm::Transaction> {
        let mut request = ApiKeyRequest::new(&self.secret, KeyScope::Rpc, self.client);
        if let Some(session) = &self.session {
            request = request.session(session);
        }
        match self.auth.begin_api_key(request, options).await {
            Ok(begun) => {
                self.drop_cookie
                    .fetch_or(begun.drop_cookie, Ordering::Relaxed);
                if let Some(ticket) = begun.last_use {
                    let mut slot = self.last_use.lock().map_err(|_| {
                        erp_fields::Error::programming(
                            "the API key last-use ticket of this request was poisoned",
                        )
                    })?;
                    // Keep the first ticket; dropping an earlier one would launch the write
                    // while this attempt's transaction is open.
                    match slot.as_ref() {
                        None => *slot = Some(ticket),
                        Some(_) => ticket.discard(),
                    }
                }
                Ok(begun.tx)
            }
            Err(error) => {
                if invalid_api_cookie(&error) {
                    self.drop_cookie.store(true, Ordering::Relaxed);
                }
                Err(error)
            }
        }
    }
}
impl erp_rpc::RpcCredential for BearerCredential {
    fn begin<'a>(
        &'a self,
        orm: &'a erp_orm::Orm,
        options: CredentialOptions,
    ) -> erp_orm::BoxFuture<'a, erp_fields::Result<erp_orm::Transaction>> {
        Box::pin(async move {
            if !Arc::ptr_eq(orm.registry(), self.auth.orm().registry()) {
                return Err(erp_fields::Error::programming(
                    "RPC credential belongs to a different registry generation",
                ));
            }
            self.open(options).await
        })
    }
    fn failure(&self, error: erp_fields::Error) -> erp_rpc::RpcFailure {
        bearer_failure(error)
    }
}

/// One database's browser application. Authentication remains owned by `Auth`.
pub struct WebApp {
    assets: crate::assets::AssetBundle,
    asset_store: Arc<crate::assets::AssetStore>,
    expected_asset_runtime: Option<String>,
    asset_authority: Option<Arc<dyn AssetAuthority>>,
    config: Arc<ServerConfig>,
    services: RwLock<Services>,
    lifecycle: Option<Arc<dyn WebLifecycle>>,
    attachment_content: Option<Arc<dyn WebAttachmentContent>>,
    attachment_policy: Option<Arc<dyn WebAttachmentPolicy>>,
    pivot_export: Option<Arc<dyn WebPivotExport>>,
    refreshing: Mutex<()>,
    requests: Arc<Semaphore>,
    queue: Arc<Semaphore>,
    report_limiter: crate::client_error::ReportLimiter,
    admission: Option<erp_orm::node::NodeAdmission>,
    login_available: AtomicBool,
    draining: AtomicBool,
    shutdown_at: std::sync::OnceLock<tokio::time::Instant>,
}
impl WebApp {
    /// Compose the configured boundary with the database authentication service.
    /// # Errors
    /// Compiled assets must all pass manifest and digest admission.
    pub fn new(
        config: Arc<ServerConfig>,
        auth: Arc<Auth>,
    ) -> Result<Self, crate::assets::AssetError> {
        let store = crate::assets::AssetStore::new(config.startup_limits().assets_memory_bytes())?;
        Self::new_with_store(config, auth, store)
    }
    /// Compose a tenant using the process's explicit shared resident allowance.
    /// # Errors
    /// Store/configuration disagreement or release asset admission failure.
    pub fn new_with_store(
        config: Arc<ServerConfig>,
        auth: Arc<Auth>,
        store: Arc<crate::assets::AssetStore>,
    ) -> Result<Self, crate::assets::AssetError> {
        if store.limit() != config.startup_limits().assets_memory_bytes() {
            return Err(crate::assets::AssetError::Admission(
                crate::assets::refused(
                    crate::assets::PublicationErrorKind::Limit,
                    "asset store differs from configured assets.memory_bytes",
                ),
            ));
        }
        let assets = crate::assets::AssetBundle::load(config.assets_manifest(), &store)?;
        Ok(Self {
            assets,
            asset_store: store,
            expected_asset_runtime: None,
            asset_authority: None,
            config,
            services: RwLock::new(Services {
                auth,
                rpc: None,
                assets: None,
            }),
            lifecycle: None,
            attachment_content: None,
            attachment_policy: None,
            pivot_export: None,
            refreshing: Mutex::new(()),
            requests: Arc::new(Semaphore::new(crate::startup_limits::RPC_CONCURRENCY)),
            queue: Arc::new(Semaphore::new(256)),
            report_limiter: crate::client_error::ReportLimiter::default(),
            admission: None,
            login_available: AtomicBool::new(false),
            draining: AtomicBool::new(false),
            shutdown_at: std::sync::OnceLock::new(),
        })
    }
    /// Supply the native RPC service. The service must use this application's same ORM lineage.
    pub fn with_rpc(mut self, rpc: Arc<erp_rpc::RpcService>) -> Self {
        self.services.get_mut().rpc = Some(rpc);
        self
    }

    /// Supply the upload policy used in authenticated bootstrap data.
    ///
    /// ```no_run
    /// fn compose(app: erp_server::web::WebApp,
    ///     policy: std::sync::Arc<dyn erp_server::web::WebAttachmentPolicy>) -> erp_server::web::WebApp {
    ///     app.with_attachment_policy(policy)
    /// }
    /// ```
    pub fn with_attachment_policy(mut self, provider: Arc<dyn WebAttachmentPolicy>) -> Self {
        self.attachment_policy = Some(provider);
        self
    }
    /// Supply startup compatibility admission for this configured database.
    /// # Errors
    /// A generation built for another runtime is refused before any route can serve it.
    pub fn with_assets(
        mut self,
        runtime: &str,
        admission: AssetAdmission,
    ) -> Result<Self, crate::assets::PublicationError> {
        if let AssetAdmission::Published(assets) = &admission
            && assets.runtime() != runtime
        {
            return Err(crate::assets::PublicationError::runtime_mismatch());
        }
        if let AssetAdmission::Published(assets) = &admission {
            assets.check_store(&self.asset_store)?;
        }
        self.expected_asset_runtime = Some(runtime.to_owned());
        self.services.get_mut().assets = Some(admission);
        Ok(self)
    }
    /// Bind current durable generation checks for this configured database.
    pub fn with_asset_authority(mut self, authority: Arc<dyn AssetAuthority>) -> Self {
        self.asset_authority = Some(authority);
        self
    }
    /// Supply native lifecycle composition; no request subprocess or uploaded code is accepted.
    pub fn with_lifecycle(mut self, lifecycle: Arc<dyn WebLifecycle>) -> Self {
        self.lifecycle = Some(lifecycle);
        self
    }
    /// Bind an accepted transport peer to the HTTP router (never supplied by a header).
    pub fn router(self: &Arc<Self>, peer: IpAddr) -> Router {
        let app = Arc::clone(self);
        Router::new().fallback(move |request: Request<Body>| {
            let app = Arc::clone(&app);
            async move { app.handle(peer, request).await }
        })
    }
    /// Attach the production membership. Every request checks its monotonic lease fence.
    /// Isolated in-process applications may omit membership; the native CLI always supplies it.
    pub fn with_admission(mut self, admission: erp_orm::node::NodeAdmission) -> Self {
        self.admission = Some(admission);
        self
    }
    /// Publish authentication-collection health from the native maintenance supervisor.
    ///
    /// Unknown health starts closed. Only a successful collection or a validated, current shared
    /// schedule may enable login. False refuses both login methods and HTTP readiness; this never
    /// overrides the separate membership lease, registry-publication or shutdown admission checks.
    ///
    /// ```
    /// # fn maintenance_failed(app: &erp_server::web::WebApp) {
    /// app.set_login_available(false);
    /// # }
    /// ```
    pub fn set_login_available(&self, available: bool) {
        self.login_available.store(available, Ordering::Release);
    }
    /// The current published authentication service, for supervised maintenance work.
    pub async fn current_auth(&self) -> Arc<Auth> {
        Arc::clone(&self.services.read().await.auth)
    }
    /// Refresh a superseded generation without replaying any request or business write.
    /// Returns false while publication is unavailable; request admission retains maintenance.
    pub async fn refresh_generation(&self) -> bool {
        self.refresh_services().await
    }
    /// Serve bounded HTTP/1 connections until the listener or supervision fails.
    /// # Errors
    /// Listener or connection-task supervision failure.
    pub async fn serve(self: Arc<Self>, listener: TcpListener) -> std::io::Result<()> {
        self.serve_until(listener, std::future::pending()).await
    }
    /// Absolute deadline of an HTTP shutdown already in progress.
    /// The native supervisor uses this same deadline for final membership departure.
    pub fn shutdown_deadline(&self) -> Option<tokio::time::Instant> {
        self.shutdown_at.get().copied()
    }
    /// Stop accepting on shutdown, drain active requests, then abort and reap overdue tasks.
    /// Descriptor, socket-buffer or memory exhaustion on `accept()` pauses new connections for
    /// [`crate::conn::ACCEPT_PAUSE`]; aborted handshakes are skipped (see [`crate::conn::accept_failure`]).
    /// # Errors
    /// Listener, task-supervision or supplied shutdown failure. Cleanup still runs on failure.
    pub async fn serve_until(
        self: Arc<Self>,
        listener: TcpListener,
        shutdown: impl std::future::Future<Output = std::io::Result<()>>,
    ) -> std::io::Result<()> {
        let max_connections = usize::try_from(self.config.startup_limits().max_connections())
            .map_err(|_| {
                std::io::Error::other("HTTP connection limit does not fit this platform")
            })?;
        let connections = Arc::new(Semaphore::new(max_connections));
        let mut tasks = tokio::task::JoinSet::new();
        let (stop, _) = tokio::sync::watch::channel(false);
        tokio::pin!(shutdown);
        // Set while new connections are paused after descriptor or buffer exhaustion.
        let mut paused_until: Option<tokio::time::Instant> = None;
        // Pauses since the last accepted connection: one warning per exhaustion episode, not per pause.
        let mut pauses: u64 = 0;
        let result = loop {
            tokio::select! {
                biased;
                result = &mut shutdown => break result,
                () = tokio::time::sleep_until(paused_until.unwrap_or_else(tokio::time::Instant::now)), if paused_until.is_some() => {
                    paused_until = None;
                }
                done = tasks.join_next(), if !tasks.is_empty() => {
                    match done {
                        Some(Ok(())) => {},
                        Some(Err(_)) => break Err(std::io::Error::other("HTTP connection task failed")),
                        None => break Err(std::io::Error::other("HTTP supervision lost connection task")),
                    }
                }
                accepted = listener.accept(), if paused_until.is_none() => {
                    let (socket, peer) = match accepted {
                        Ok(pair) => pair,
                        Err(error) => match crate::conn::accept_failure(&error) {
                            crate::conn::AcceptFailure::Skip => continue,
                            crate::conn::AcceptFailure::Pause => {
                                if pauses == 0 {
                                    tracing::warn!(error = %error, "HTTP accept paused: out of descriptors, socket buffers or memory");
                                }
                                pauses = pauses.saturating_add(1);
                                paused_until = Some(tokio::time::Instant::now() + crate::conn::ACCEPT_PAUSE);
                                continue;
                            }
                            crate::conn::AcceptFailure::Fatal => break Err(error),
                        },
                    };
                    if pauses > 0 {
                        tracing::warn!(pauses, "HTTP accept resumed after exhaustion");
                        pauses = 0;
                    }
                    let permit = match Arc::clone(&connections).try_acquire_owned() {
                        Ok(permit) => permit,
                        Err(_) => { drop(socket); continue; }
                    };
                    let router = self.router(peer.ip());
                    let limits = self.config.connection_limits();
                    let drain = self.config.shutdown_deadline();
                    let mut stopped = stop.subscribe();
                    tasks.spawn(async move {
                        let _permit = permit;
                        crate::conn::serve_draining(socket, router, limits, async {
                            // A closed sender also means the owner stopped serving.
                            if !*stopped.borrow() { let _closed = stopped.changed().await; }
                        }, drain).await;
                    });
                }
            }
        };
        let deadline = *self
            .shutdown_at
            .get_or_init(|| tokio::time::Instant::now() + self.config.shutdown_deadline());
        self.draining.store(true, Ordering::Release);
        drop(listener);
        stop.send_replace(true);
        let mut task_failed = false;
        let drained = tokio::time::timeout_at(deadline, async {
            while let Some(done) = tasks.join_next().await {
                if done.is_err() {
                    task_failed = true;
                }
            }
        })
        .await
        .is_ok();
        if !drained {
            tracing::warn!("HTTP server drain deadline expired; aborting remaining connections");
            tasks.abort_all();
            while let Some(done) = tasks.join_next().await {
                if done.is_err_and(|error| !error.is_cancelled()) {
                    task_failed = true;
                }
            }
        }
        result?;
        if task_failed {
            return Err(std::io::Error::other(
                "HTTP connection task failed while draining",
            ));
        }
        Ok(())
    }
    async fn ready(&self) -> bool {
        !self.draining.load(Ordering::Acquire)
            && self.login_available.load(Ordering::Acquire)
            && self
                .admission
                .as_ref()
                .is_none_or(erp_orm::node::NodeAdmission::is_serving)
            && !self
                .services
                .read()
                .await
                .auth
                .orm()
                .signal()
                .is_some_and(erp_orm::Signal::is_superseded)
    }
    /// Give the request an id, serve it, return the id in `X-Request-Id` and write the refusals
    /// the RPC layer did not already log.
    async fn handle(self: Arc<Self>, peer: IpAddr, request: Request<Body>) -> Response {
        // Every answer on the content route, the substitutes (busy, deadline, refresh) included,
        // leaves with the attachment class (threat model §13).
        let content_route =
            canonical_path(request.uri().path()).starts_with(content::CONTENT_PREFIX);
        let mut response = Box::pin(self.handle_identified(peer, request)).await;
        if content_route {
            content::apply_attachment_class(&mut response);
        }
        response
    }
    async fn handle_identified(
        self: Arc<Self>,
        peer: IpAddr,
        mut request: Request<Body>,
    ) -> Response {
        let id = match request_id(request.headers()) {
            Ok(id) => id,
            Err(response) => return finish_response(response, false),
        };
        request.extensions_mut().insert(id.clone());
        let method = request.method().clone();
        let path = canonical_path(request.uri().path()).into_owned();
        // The whole request runs inside its trace, so a log line written anywhere below it
        // (an authentication failure, a guest addon) can name the request.
        let trace = erp_orm::trace::RequestTrace::new(Some(id.as_str()));
        // Boxed: the request future is large and a debug build overflowed a 2 MiB thread stack
        // with one more inline layer around it.
        let mut response =
            erp_orm::trace::with_request_trace(trace, Box::pin(self.handle_request(peer, request)))
                .await;
        match HeaderValue::from_str(id.as_str()) {
            Ok(value) => {
                response.headers_mut().insert("x-request-id", value);
            }
            Err(_) => return finish_response(internal(), false), // a UUID is always a valid header value
        }
        if response.extensions().get::<RpcLogged>().is_none() {
            log_web_refusal(&method, &path, &response, id.as_str());
        }
        response
    }
    async fn handle_request(self: Arc<Self>, peer: IpAddr, request: Request<Body>) -> Response {
        if request.method() == Method::GET && request.uri().path() == routes::HEALTH_READY {
            return if self.ready().await {
                response(StatusCode::OK, "ready")
            } else {
                busy()
            };
        }
        if self.draining.load(Ordering::Acquire)
            || self
                .admission
                .as_ref()
                .is_some_and(|admission| !admission.is_serving())
        {
            return finish_response(busy(), false);
        }
        let classified_path = canonical_path(request.uri().path());
        if classified_path == routes::LOGIN
            && matches!(*request.method(), Method::GET | Method::POST)
            && !self.login_available.load(Ordering::Acquire)
        {
            return finish_response(busy(), true);
        }
        let navigation = !classified_path.starts_with(routes::JSON2_PREFIX)
            && LifecycleOperation::from_path(&classified_path).is_none()
            && !classified_path.starts_with(routes::ASSETS_PREFIX)
            // attachment errors keep their class and stay plain text, never the HTML page
            && !classified_path.starts_with(content::CONTENT_PREFIX)
            && (request.method() == Method::GET || accepts_html(request.headers()));
        // The queue admission and execution permit precede body buffering.
        let queued = match Arc::clone(&self.queue).try_acquire_owned() {
            Ok(permit) => permit,
            Err(_) => return finish_response(busy(), navigation),
        };
        let permit = match tokio::time::timeout(
            Duration::from_secs(5),
            Arc::clone(&self.requests).acquire_owned(),
        )
        .await
        {
            Ok(Ok(permit)) => permit,
            Ok(Err(_)) | Err(_) => return finish_response(busy(), navigation),
        };
        drop(queued);
        // A cancelled lifecycle future may have sent COMMIT without receiving acknowledgement.
        // Keep the operation classification outside the future so its timeout cannot invite replay.
        let deadline_operation = LifecycleOperation::from_path(&classified_path);
        // The deadline includes refresh and the single safe retry, not just dispatch.
        let response = match tokio::time::timeout(
            Duration::from_secs(120),
            self.dispatch_and_refresh(peer, request, navigation),
        )
        .await
        {
            Ok(response) => response,
            Err(_) => lifecycle_deadline_response(deadline_operation),
        };
        drop(permit);
        finish_response(response, navigation)
    }
    async fn dispatch_and_refresh(
        &self,
        peer: IpAddr,
        request: Request<Body>,
        navigation: bool,
    ) -> Response {
        let (parts, body) = request.into_parts();
        // Never retain or replay a write body. Retry navigation only, through all the same
        // proxy, cookie and authorization checks, using the newly published service pair.
        let retry = (navigation && parts.method == Method::GET).then(|| parts.clone());
        let services = self.services.read().await.clone();
        let request_auth = Arc::clone(&services.auth);
        let mut response = match self
            .dispatch(peer, Request::from_parts(parts, body), services)
            .await
        {
            Ok(response) | Err(response) => response,
        };
        // Commit truth is terminal for this write. In particular, unknown commits must not
        // cause automatic recomposition, and superseded-generation handling cannot erase the
        // structured recovery response. A subsequent explicit safe navigation may refresh.
        if response.extensions().get::<LifecycleResponse>().is_some() {
            return response;
        }
        // Inspect the generation that served this request, even when a peer has already
        // replaced the application's current pair while this request was in flight.
        let superseded = request_auth
            .orm()
            .signal()
            .is_some_and(erp_orm::Signal::is_superseded);
        if superseded {
            let refreshed = self.refresh_services().await;
            if refreshed
                && response.status() == StatusCode::SERVICE_UNAVAILABLE
                && let Some(parts) = retry
            {
                let services = self.services.read().await.clone();
                response = match self
                    .dispatch(peer, Request::from_parts(parts, Body::empty()), services)
                    .await
                {
                    Ok(response) | Err(response) => response,
                };
            } else if response.status().is_server_error() {
                response = superseded_failure(&response);
            }
        }
        response
    }
    async fn refresh_services(&self) -> bool {
        // A request may have failed on an old service pair while a peer was rebuilding.
        // Recheck after acquiring the gate; share the published result rather than rebuild it.
        let _refreshing = self.refreshing.lock().await;
        let services = self.services.read().await.clone();
        if !services
            .auth
            .orm()
            .signal()
            .is_some_and(erp_orm::Signal::is_superseded)
        {
            return true;
        }
        let Some(lifecycle) = &self.lifecycle else {
            return false;
        };
        match tokio::time::timeout(Duration::from_secs(30), lifecycle.refresh(&services.auth)).await
        {
            Ok(Ok(publication)) => match self.publish(publication).await {
                Ok(()) => true,
                Err(result) => {
                    tracing::error!(status=%result.status(), "asset generation publication refused; node remains in maintenance");
                    false
                }
            },
            Ok(Err(error)) => {
                tracing::error!(kind=?error.kind(), "registry refresh failed; node remains in maintenance");
                false
            }
            Err(_) => {
                tracing::error!("registry refresh deadline expired; node remains in maintenance");
                false
            }
        }
    }
    async fn dispatch(
        &self,
        peer: IpAddr,
        request: Request<Body>,
        services: Services,
    ) -> HttpResult<Response> {
        let auth = services.auth;
        let (parts, body) = request.into_parts();
        let headers = &parts.headers;
        let tokens = values(headers, "x-erp-proxy-token")?;
        let forwarded = [
            "forwarded",
            "x-forwarded-for",
            "x-forwarded-host",
            "x-forwarded-proto",
        ]
        .iter()
        .any(|key| headers.contains_key(*key));
        self.config
            .proxy()
            .authorize(peer, &tokens, forwarded)
            .map_err(|_| response(StatusCode::FORBIDDEN, "untrusted proxy request"))?;
        // Trusted proxy headers are still bounded and unambiguous. Origin/cookies always use config.
        if headers.contains_key("forwarded") {
            return Err(bad(
                "unsupported Forwarded header; use the configured proxy headers",
            ));
        }
        let expected_proto = if self.config.origin().starts_with("https:") {
            "https"
        } else {
            "http"
        };
        if one(headers, "x-forwarded-proto")?.is_some_and(|proto| proto != expected_proto) {
            return Err(bad("proxy scheme mismatch"));
        }
        if let Some(host) = one(headers, "x-forwarded-host")? {
            let expected = self
                .config
                .origin()
                .split_once("://")
                .ok_or_else(internal)?
                .1;
            if host != expected {
                return Err(bad("proxy host mismatch"));
            }
        }
        let client = match one(headers, "x-forwarded-for")? {
            Some(value) => value
                .parse()
                .map_err(|_| bad("invalid proxy client address"))?,
            None => peer, // an authenticated direct proxy probe has no forwarded client identity
        };
        let incoming_path = parts.uri.path();
        let canonical = canonical_path(incoming_path);
        let path = canonical.as_ref();
        // GET bookmarks/assets migrate to the native namespace. Unsafe aliases are
        // dispatched directly once below, retaining all origin/body/auth/CSRF gates.
        if parts.method == Method::GET
            && path != incoming_path
            && path != routes::ERP
            && !path.starts_with(routes::ACTION_PREFIX)
            && !path.starts_with(content::CONTENT_PREFIX)
        {
            let destination = match parts.uri.query() {
                Some(query) => format!("{path}?{query}"),
                None => path.to_owned(),
            };
            return redirect(&self.redirect(Some(&destination))?);
        }
        if parts.method == Method::GET
            && let Some(name) = path.strip_prefix(routes::ASSETS_PREFIX)
        {
            if let Some(admission) = &self.current_asset_admission().await? {
                match admission {
                    AssetAdmission::Broken(_) => {
                        return Err(response(
                            StatusCode::SERVICE_UNAVAILABLE,
                            ASSETS_UNAVAILABLE,
                        ));
                    }
                    AssetAdmission::Published(assets) => {
                        if self.expected_asset_runtime.as_deref() != Some(assets.runtime()) {
                            return Err(response(
                                StatusCode::SERVICE_UNAVAILABLE,
                                "Asset runtime is incompatible",
                            ));
                        }
                        // The release client files are admitted independently from the configured
                        // operator manifest. Tenant addon hashes never fall back to that set.
                        if !name.starts_with("client.") {
                            return assets
                                .response(
                                    name,
                                    one(headers, "if-none-match")?,
                                    one(headers, "accept-encoding")?,
                                )
                                .map_err(|error| bad(error.to_string()))?
                                .ok_or_else(|| response(StatusCode::NOT_FOUND, "Not found"));
                        }
                    }
                }
            }
            return self
                .assets
                .response(
                    name,
                    one(headers, "if-none-match")?,
                    one(headers, "accept-encoding")?,
                )
                .map_err(|error| bad(error.to_string()))?
                .ok_or_else(|| response(StatusCode::NOT_FOUND, "Not found"));
        }
        if parts.method == Method::GET && path.contains(routes::STATIC_SEGMENT) {
            match &self.current_asset_admission().await? {
                Some(AssetAdmission::Published(assets)) => {
                    let url = assets
                        .fixed_path(path)
                        .ok_or_else(|| response(StatusCode::NOT_FOUND, "Not found"))?;
                    let name = url
                        .strip_prefix(routes::ASSETS_PREFIX)
                        .ok_or_else(internal)?;
                    let mut result = assets
                        .response(
                            name,
                            one(headers, "if-none-match")?,
                            one(headers, "accept-encoding")?,
                        )
                        .map_err(|error| bad(error.to_string()))?
                        .ok_or_else(|| response(StatusCode::NOT_FOUND, "Not found"))?;
                    if matches!(result.status(), StatusCode::OK | StatusCode::NOT_MODIFIED) {
                        result
                            .headers_mut()
                            .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
                    }
                    return Ok(result);
                }
                Some(AssetAdmission::Broken(_)) => {
                    return Err(response(
                        StatusCode::SERVICE_UNAVAILABLE,
                        ASSETS_UNAVAILABLE,
                    ));
                }
                None => return Err(response(StatusCode::NOT_FOUND, "Not found")),
            }
        }
        if let Some(rest) = path.strip_prefix(content::CONTENT_PREFIX) {
            return self.attachment_content(&parts, rest, &auth).await;
        }
        if path == routes::PIVOT_EXPORT {
            return self.pivot_export(&parts, body, &auth).await;
        }
        if path == routes::CLIENT_ERROR {
            return self
                .client_error(&parts.method, headers, parts.extensions.get(), body, &auth)
                .await;
        }
        if path.starts_with(routes::JSON2_PREFIX) || LifecycleOperation::from_path(path).is_some() {
            if parts.method != Method::POST {
                return Err(response(StatusCode::METHOD_NOT_ALLOWED, "POST required"));
            }
            if one(headers, "content-type")? != Some("application/json") {
                return Err(response(
                    StatusCode::UNSUPPORTED_MEDIA_TYPE,
                    "expected application/json",
                ));
            }
            let operation = LifecycleOperation::from_path(path);
            if matches!(
                operation,
                Some(
                    LifecycleOperation::Upgrade
                        | LifecycleOperation::Uninstall
                        | LifecycleOperation::ForceDemo
                )
            ) && headers.contains_key(header::AUTHORIZATION)
            {
                return Err(response(
                    StatusCode::FORBIDDEN,
                    "Lifecycle writes require only a primary browser session",
                ));
            }
            let json2 = path.starts_with(routes::JSON2_PREFIX);
            // An ambiguous/non-ASCII header remains a failed key attempt; its text is never
            // retained, and it still receives cookie precedence and address throttling.
            let bearer = if json2 && headers.contains_key(header::AUTHORIZATION) {
                // Charge external requests once, before any credential/settings SQL. The
                // execution adapter rechecks authority without charging retries again.
                let address = erp_security::auth::throttle::address_key(client);
                if !auth.prefilter().admit(&address) {
                    return Err(self
                        .render_bearer_failure(
                            erp_rpc::InfraError::RateLimited { retry_after: 1 }.into(),
                        )
                        .await);
                }
                let raw = match values(headers, "authorization") {
                    Ok(values) => match boundary::bearer(&values) {
                        Ok(Some(raw)) => raw,
                        Ok(None) | Err(_) => "", // Presented malformed credentials cannot use a cookie.
                    },
                    Err(_) => "", // Invalid bytes are an invalid credential, never a diagnostic.
                };
                if one(headers, "origin")?.is_some_and(|origin| origin != self.config.origin()) {
                    return Err(response(
                        StatusCode::FORBIDDEN,
                        boundary::BoundaryError::SameOrigin.to_string(),
                    ));
                }
                Some(Arc::new(BearerCredential {
                    auth: Arc::clone(&auth),
                    secret: zeroize::Zeroizing::new(raw.to_owned()),
                    session: self.session(headers)?,
                    client,
                    drop_cookie: AtomicBool::new(false),
                    last_use: std::sync::Mutex::new(None),
                }))
            } else {
                None
            };
            let (mut request_tx, secret) = if let Some(credential) = &bearer {
                match credential.open(CredentialOptions::new()).await {
                    Ok(tx) => (tx, None),
                    Err(error) => {
                        let mut result = self.bearer_error(error).await;
                        if credential.drop_cookie.load(Ordering::Relaxed) {
                            self.set_cookie(&mut result, "session_id", "", 0)?;
                        }
                        return Err(result);
                    }
                }
            } else {
                let secret = self.session(headers)?;
                if json2 && secret.is_none() {
                    return Err(self.unauthorized("User not authenticated, use an API Key with a Bearer Authorization header.").await);
                }
                boundary::same_origin_fetch(
                    one(headers, "sec-fetch-site")?,
                    one(headers, "origin")?,
                    self.config.origin(),
                )
                .map_err(|error| response(StatusCode::FORBIDDEN, error.to_string()))?;
                let secret = secret
                    .ok_or_else(|| response(StatusCode::UNAUTHORIZED, "user is not connected"))?;
                let begun = auth
                    .begin_session(&secret, CredentialOptions::new())
                    .await
                    .map_err(auth_error)?;
                (begun.tx, Some(secret))
            };
            let principal = request_tx.env().principal().clone();
            request_tx.rollback().await.map_err(auth_error)?;
            // A Transaction retains its pooled connection after rollback for reuse. RPC or
            // lifecycle execution must be able to acquire that slot after this preflight.
            drop(request_tx);
            let bytes = to_bytes(body, 1024 * 1024).await.map_err(|_| {
                response(
                    StatusCode::PAYLOAD_TOO_LARGE,
                    "JSON body exceeds 1048576 bytes",
                )
            })?;
            if let Some(operation) = operation {
                let lifecycle = self.lifecycle.as_ref().ok_or_else(|| {
                    response(
                        StatusCode::SERVICE_UNAVAILABLE,
                        if operation == LifecycleOperation::Install {
                            "native installation composition is unavailable"
                        } else {
                            "native lifecycle composition is unavailable"
                        },
                    )
                })?;
                let secret = secret.as_ref().ok_or_else(internal)?;
                let publication = match operation {
                    LifecycleOperation::Install => {
                        let modules = install_names(&bytes)?;
                        match lifecycle.install(&auth, secret, modules).await {
                            Ok(publication) => publication,
                            Err(error) => {
                                if let Some(response) = install_outcome_response(&error) {
                                    return Err(response);
                                }
                                return Err(self.rpc_error(error).await);
                            }
                        }
                    }
                    LifecycleOperation::Upgrade
                    | LifecycleOperation::Uninstall
                    | LifecycleOperation::ForceDemo => {
                        let outcome = match operation {
                            LifecycleOperation::Upgrade => {
                                let request = lifecycle_request(operation, &bytes)
                                    .map_err(|error| bad(error.message()))?;
                                lifecycle.upgrade(&auth, secret, request.modules).await
                            }
                            LifecycleOperation::Uninstall => {
                                let request = lifecycle_request(operation, &bytes)
                                    .map_err(|error| bad(error.message()))?;
                                lifecycle
                                    .uninstall(
                                        &auth,
                                        secret,
                                        request.modules,
                                        request.impact.ok_or_else(internal)?,
                                    )
                                    .await
                            }
                            LifecycleOperation::ForceDemo => {
                                force_demo_confirmed(&bytes)
                                    .map_err(|error| bad(error.message()))?;
                                lifecycle.force_demo(&auth, secret).await
                            }
                            LifecycleOperation::Install => return Err(internal()),
                        };
                        match outcome {
                            Ok(publication) => publication,
                            Err(LifecycleFailure::Refused(error)) => {
                                return Err(self.rpc_error(error).await);
                            }
                            Err(LifecycleFailure::Unknown(error)) => {
                                tracing::error!(operation = operation.name(), error = %error, "lifecycle outcome requires reconciliation");
                                return Err(lifecycle_outcome_response(operation, None));
                            }
                            Err(LifecycleFailure::Committed { stage, error }) => {
                                tracing::error!(operation = operation.name(), stage = stage.key(), error = %error, "lifecycle publication requires recovery");
                                return Err(lifecycle_outcome_response(operation, Some(stage)));
                            }
                        }
                    }
                };
                let mut result = match (&publication.mutation, operation) {
                    (MutationResult::Install(installed), LifecycleOperation::Install) => {
                        serde_json::json!({"installed":installed})
                    }
                    (
                        MutationResult::Upgrade {
                            upgraded,
                            installed,
                        },
                        LifecycleOperation::Upgrade,
                    ) => serde_json::json!({"upgraded":upgraded,"installed":installed}),
                    (MutationResult::Uninstall(uninstalled), LifecycleOperation::Uninstall) => {
                        serde_json::json!({"uninstalled":uninstalled})
                    }
                    (
                        MutationResult::ForceDemo { loaded, failed },
                        LifecycleOperation::ForceDemo,
                    ) => serde_json::json!({
                        "loaded":loaded,
                        "failed":failed.iter().map(|(module, error)| {
                            serde_json::json!({"module":module,"error":error})
                        }).collect::<Vec<_>>(),
                    }),
                    _ => {
                        return Err(lifecycle_outcome_response(
                            operation,
                            Some(PublicationStage::Publication),
                        ));
                    }
                };
                // Every mutation, install included, decides publication and asset usability
                // under one lock and never reports success for a committed-but-unusable state.
                let registry_version = match self.publish_mutation(publication).await {
                    Ok(version) => version,
                    Err(stage) => {
                        return Err(lifecycle_outcome_response(operation, Some(stage)));
                    }
                };
                result["registry_version"] = serde_json::Value::String(registry_version);
                result["asset_version"] =
                    serde_json::Value::String(self.assets.version().to_owned());
                return json_response(StatusCode::OK, &result);
            }
            let rpc = services.rpc.ok_or_else(|| {
                response(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "RPC composition is unavailable",
                )
            })?;
            let credential: Arc<dyn erp_rpc::RpcCredential> = match &bearer {
                Some(credential) => credential.clone(),
                None => Arc::new(BrowserCredential {
                    auth: Arc::clone(&auth),
                    secret: secret.ok_or_else(internal)?,
                    client,
                }),
            };
            let mut call = erp_rpc::RpcCall::json2(
                path,
                bytes.to_vec(),
                erp_rpc::Caller::Principal(principal),
            )
            .credential(credential);
            if let Some(id) = parts.extensions.get::<erp_rpc::CorrelationId>() {
                call = call.correlation(id.clone());
            }
            if let Some(key) = one(headers, "idempotency-key")? {
                let key = erp_rpc::IdempotencyKey::from_headers(&[key])
                    .map_err(|error| bad(error.message()))?;
                if let Some(key) = key {
                    call = call.idempotency_key(key);
                }
            }
            let reply = rpc.call(call).await;
            let mut result = Response::new(Body::from(reply.body().to_vec()));
            // the RPC layer logged this call's failure itself
            result.extensions_mut().insert(RpcLogged);
            *result.status_mut() = StatusCode::from_u16(reply.status()).map_err(|_| internal())?;
            result.headers_mut().insert(
                header::CONTENT_TYPE,
                HeaderValue::from_static("application/json"),
            );
            if reply.replayed() {
                result
                    .headers_mut()
                    .insert("idempotent-replayed", HeaderValue::from_static("true"));
            }
            if let Some(seconds) = reply.retry_after() {
                result.headers_mut().insert(
                    header::RETRY_AFTER,
                    HeaderValue::from_str(&seconds.to_string()).map_err(|_| internal())?,
                );
            }
            if let Some(credential) = &bearer {
                if result.status() == StatusCode::UNAUTHORIZED {
                    result
                        .headers_mut()
                        .insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("bearer"));
                }
                if credential.drop_cookie.load(Ordering::Relaxed) {
                    self.set_cookie(&mut result, "session_id", "", 0)?;
                }
            }
            return Ok(result);
        }
        if let Some(response) = brand_route(&parts.method, path) {
            return Ok(response);
        }
        match (&parts.method, path) {
            (&Method::GET, routes::ROOT) => redirect("/erp"),
            (&Method::GET, routes::LOGIN) => {
                let fields = form(parts.uri.query().unwrap_or(""), &["redirect", "db"])?;
                if fields
                    .get("db")
                    .is_some_and(|db| db != self.config.database())
                {
                    return Err(bad("Database not found."));
                }
                let redirect = self.redirect(fields.get("redirect").map(String::as_str))?;
                self.login_page(headers, redirect, None).await
            }
            (&Method::POST, routes::LOGIN) => {
                self.form_origin(headers)?;
                let fields = read_form(
                    headers,
                    body,
                    &["db", "login", "password", "redirect", "csrf_token"],
                )
                .await?;
                self.verify_csrf(headers, required(&fields, "csrf_token")?)
                    .await?;
                if required(&fields, "db")? != self.config.database() {
                    return Err(bad("Database not found."));
                }
                let destination = self.redirect(fields.get("redirect").map(String::as_str))?;
                let mut login = LoginRequest::new(
                    required(&fields, "login")?,
                    required(&fields, "password")?,
                    ClientAddr::Known(client),
                );
                if let Some(agent) = one(headers, "user-agent")? {
                    login = login.user_agent(agent);
                }
                match auth.authenticate(login).await {
                    Ok(login) => {
                        let mut begun = auth
                            .begin_session(login.session().secret(), CredentialOptions::new())
                            .await
                            .map_err(auth_error)?;
                        let env = begun.tx.env();
                        let internal_user = begun
                            .tx
                            .has_group(&env, "base.group_user")
                            .await
                            .map_err(auth_error)?;
                        begun.tx.rollback().await.map_err(auth_error)?;
                        let destination = if internal_user {
                            destination.as_str()
                        } else {
                            "/erp/login_successful"
                        };
                        let mut result = redirect(destination)?;
                        let value = format!(
                            "{}.{}",
                            self.config.database(),
                            login.session().secret().to_cookie().as_str()
                        );
                        self.set_cookie(
                            &mut result,
                            "session_id",
                            &value,
                            login.session().lifetime().as_secs(),
                        )?;
                        Ok(result)
                    }
                    Err(LoginError::Denied) => {
                        self.login_page(
                            headers,
                            destination,
                            Some("Wrong login/password".to_owned()),
                        )
                        .await
                    }
                    Err(LoginError::Cooldown) => Err(response(
                        StatusCode::TOO_MANY_REQUESTS,
                        "Too many login attempts; try again later.",
                    )),
                    Err(LoginError::Overloaded { .. }) => Err(busy()),
                    Err(LoginError::Failed(error)) => Err(auth_error(error)),
                    Err(_) => Err(internal()),
                }
            }
            (&Method::POST, routes::LOGOUT) => {
                self.form_origin(headers)?;
                let fields = read_form(headers, body, &["csrf_token"]).await?;
                self.verify_csrf(headers, required(&fields, "csrf_token")?)
                    .await?;
                // Only after the origin and token checks passed: a refused request clears nothing.
                // From here every outcome carries the cleanup, a failed revocation included; that
                // one keeps its auth failure status and never claims a sign-out.
                let cleanup = LogoutCleanup::new(self.cookie_header("session_id", "", 0)?);
                match self.revoke_session(headers, &auth).await {
                    Ok(()) => {
                        let mut result = redirect("/erp/login")?;
                        cleanup.apply(&mut result);
                        Ok(result)
                    }
                    Err(mut refused) => {
                        cleanup.apply(&mut refused);
                        Err(refused)
                    }
                }
            }
            (&Method::GET, routes::LOGIN_SUCCESS) => {
                let secret = self
                    .session(headers)?
                    .ok_or_else(|| response(StatusCode::UNAUTHORIZED, "user is not connected"))?;
                let mut begun = auth
                    .begin_session(&secret, CredentialOptions::new())
                    .await
                    .map_err(auth_error)?;
                begun.tx.rollback().await.map_err(auth_error)?;
                self.login_success_page().await
            }
            (&Method::GET, path)
                if path == routes::ERP || path.starts_with(routes::ACTION_PREFIX) =>
            {
                let requested = parts
                    .uri
                    .path_and_query()
                    .map_or(path, |value| value.as_str());
                let policy =
                    RedirectPolicy::new(self.config.origin(), crate::redirect::SHELL_VIEW_TYPES)
                        .map_err(|_| internal())?;
                let canonical = policy.shell(requested).map_err(|error| {
                    let mut refused = bad("invalid shell URL state");
                    // The variant's fixed text for the log line, never the refused URL.
                    refused
                        .extensions_mut()
                        .insert(RefusalReason(error.reason()));
                    refused
                })?;
                if canonical != requested {
                    return redirect(&canonical);
                }
                let cookies = values(headers, "cookie")?;
                let cookie = boundary::cookie(&cookies, "prefs")
                    .map_err(|_| bad("ambiguous or malformed authentication cookie"))?;
                let Some(secret) = self.session(headers)? else {
                    return self.session_login(&canonical);
                };
                let mut context = erp_orm::Context::new();
                if let Some(query) = parts.uri.query() {
                    for (key, value) in url::form_urlencoded::parse(query.as_bytes()) {
                        if key == "cids" {
                            let ids = value
                                .split(',')
                                .map(|value| {
                                    value
                                        .parse::<i64>()
                                        .map(erp_fields::Value::Int)
                                        .map_err(|_| bad("invalid company identity"))
                                })
                                .collect::<HttpResult<Vec<_>>>()?;
                            context.set("allowed_company_ids", erp_fields::Value::list(ids));
                        }
                    }
                }
                let signer = auth.browser_tokens().await.map_err(auth_error)?;
                let verified = cookie.map(|value| (value, signer.verify(value)));
                let (prefs, prefs_cookie) = match verified {
                    Some((value, Ok(prefs))) => (prefs, value),
                    None | Some((_, Err(PrefsError::Cookie))) => {
                        return self.session_login(&canonical);
                    }
                    Some((_, Err(_))) => return Err(internal()),
                };
                let mut begun = match auth
                    .begin_session(
                        &secret,
                        CredentialOptions::new()
                            .context(context)
                            .read_only("web.bootstrap"),
                    )
                    .await
                {
                    Ok(begun) => begun,
                    Err(error) if error.kind() == erp_fields::ErrorKind::SessionExpired => {
                        return self.session_login(&canonical);
                    }
                    Err(error) => return Err(auth_error(error)),
                };
                // The shell access guard runs before any bootstrap work, from the
                // principal just checked against the primary; an error resolving it fails closed.
                let env = begun.tx.env();
                let internal_user = begun
                    .tx
                    .has_group(&env, "base.group_user")
                    .await
                    .map_err(auth_error)?;
                if !internal_user {
                    begun.tx.rollback().await.map_err(auth_error)?;
                    if !accepts_html(headers) {
                        return Err(response(
                            StatusCode::FORBIDDEN,
                            "The web client requires an internal user",
                        ));
                    }
                    // Rendered in place, never a redirect loop through the login page.
                    return self.login_success_page().await;
                }
                let document = self.bootstrap(&mut begun.tx, &signer, &prefs).await?;
                begun.tx.rollback().await.map_err(auth_error)?;
                let mut result = self.document("erp-bootstrap", &document).await?;
                // Sliding expiry: the verified value is sent again, never re-minted.
                self.set_cookie(&mut result, "prefs", prefs_cookie, PREFS_LIFETIME_SECS)?;
                Ok(result)
            }
            _ => Err(response(StatusCode::NOT_FOUND, "Not found")),
        }
    }
    /// `POST /erp/client-error`: a bounded report from the signed-in
    /// browser, logged at `warn`, never stored or echoed.
    async fn client_error(
        &self,
        method: &Method,
        headers: &HeaderMap,
        request_id: Option<&erp_rpc::CorrelationId>,
        body: Body,
        auth: &Arc<Auth>,
    ) -> HttpResult<Response> {
        use crate::client_error::{ClientReport, LimitRefusal, MAX_BODY_BYTES};
        if method != Method::POST {
            return Err(response(StatusCode::METHOD_NOT_ALLOWED, "POST required"));
        }
        boundary::same_origin_fetch(
            one(headers, "sec-fetch-site")?,
            one(headers, "origin")?,
            self.config.origin(),
        )
        .map_err(|error| response(StatusCode::FORBIDDEN, error.to_string()))?;
        if one(headers, "content-type")? != Some("application/json") {
            return Err(response(
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                "expected application/json",
            ));
        }
        let secret = self
            .session(headers)?
            .ok_or_else(|| response(StatusCode::UNAUTHORIZED, "user is not connected"))?;
        let mut begun = auth
            .begin_session(&secret, CredentialOptions::new())
            .await
            .map_err(auth_error)?;
        let uid = begun.tx.env().principal().subject().get();
        let session = *begun.session.id().bytes();
        begun.tx.rollback().await.map_err(auth_error)?;
        let bytes = to_bytes(body, MAX_BODY_BYTES).await.map_err(|_| {
            response(
                StatusCode::PAYLOAD_TOO_LARGE,
                format!("report body exceeds {MAX_BODY_BYTES} bytes"),
            )
        })?;
        let report = ClientReport::parse(&bytes).map_err(bad)?;
        match self
            .report_limiter
            .admit(session, std::time::Instant::now())
        {
            Ok(()) => {}
            Err(LimitRefusal::SessionRate | LimitRefusal::Full) => {
                let mut refused = response(
                    StatusCode::TOO_MANY_REQUESTS,
                    "Too many error reports; try again later.",
                );
                refused
                    .headers_mut()
                    .insert(header::RETRY_AFTER, HeaderValue::from_static("60"));
                return Err(refused);
            }
        }
        report.log(uid, request_id.map_or("-", erp_rpc::CorrelationId::as_str));
        Ok(response(StatusCode::NO_CONTENT, ""))
    }
    /// Revoke the browser's session for a logout whose origin and token were verified. No session
    /// cookie, or one the server no longer knows, is an idempotent success. A malformed or
    /// duplicated cookie is refused without any revocation: an ambiguous envelope may hide a live
    /// session, so the answer must not suggest a sign-out that did not happen.
    async fn revoke_session(&self, headers: &HeaderMap, auth: &Arc<Auth>) -> HttpResult<()> {
        let Some(secret) = self.session(headers)? else {
            return Ok(());
        };
        match auth.begin_session(&secret, CredentialOptions::new()).await {
            Ok(mut begun) => {
                let id = begun.session.id();
                begun.tx.rollback().await.map_err(auth_error)?;
                // `false` (the row was already gone) is the same idempotent success.
                auth.logout(id).await.map_err(auth_error)?;
                Ok(())
            }
            // idempotent logout of an expired session
            Err(error) if error.kind() == erp_fields::ErrorKind::SessionExpired => Ok(()),
            Err(error) => Err(auth_error(error)),
        }
    }
    fn session_login(&self, destination: &str) -> HttpResult<Response> {
        let query = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("redirect", destination)
            .finish();
        let mut result = redirect(&format!("/erp/login?{query}"))?;
        // GET only expires this browser's cookies; it never revokes a database session.
        self.set_cookie(&mut result, "session_id", "", 0)?;
        self.set_cookie(&mut result, "prefs", "", 0)?;
        Ok(result)
    }
    fn redirect(&self, input: Option<&str>) -> HttpResult<String> {
        let local = RedirectPolicy::new(self.config.origin(), crate::redirect::SHELL_VIEW_TYPES)
            .map_err(|_| internal())?
            .normalize(input.unwrap_or("/erp"))
            .map_err(|_| bad("unsafe redirect"))?;
        // Normalize only the path. Query and fragment are separately preserved
        // by RedirectPolicy; a fragment must not hide a legacy path from mapping.
        // No suffix delimiter means the entire validated local string is its path.
        let boundary = local.find(['?', '#']).unwrap_or(local.len());
        let (path, suffix) = local.split_at(boundary);
        Ok(format!("{}{suffix}", canonical_path(path)))
    }

    /// Publish the result of a committed upgrade or uninstall and return the registry version
    /// the node now serves. The decision and the usability check happen under one write lock:
    /// a mutation never reports success while the services that stay published (the candidate,
    /// or a newer retained installation when the candidate is discarded as older) have
    /// incomplete assets. A broken candidate is not published here.
    async fn publish_mutation(&self, publication: Publication) -> Result<String, PublicationStage> {
        let candidate = Services {
            auth: publication.auth,
            rpc: Some(publication.rpc),
            assets: publication.assets,
        };
        if let Some(AssetAdmission::Published(assets)) = &candidate.assets {
            assets
                .check_store(&self.asset_store)
                .map_err(|_| PublicationStage::Publication)?;
        }
        if let Some(expected) = &self.expected_asset_runtime {
            match &candidate.assets {
                Some(AssetAdmission::Published(assets)) if assets.runtime() == expected => {}
                Some(AssetAdmission::Broken(_)) => {}
                _ => {
                    tracing::error!(
                        "lifecycle publication refused: candidate assets are missing or incompatible"
                    );
                    return Err(PublicationStage::Publication);
                }
            }
        }
        let mut current = self.services.write().await;
        if candidate.generation() >= current.generation() {
            if matches!(candidate.assets, Some(AssetAdmission::Broken(_))) {
                tracing::error!("lifecycle publication refused: candidate assets are incomplete");
                return Err(PublicationStage::Assets);
            }
            *current = candidate;
        } else if matches!(current.assets, Some(AssetAdmission::Broken(_))) {
            tracing::error!(
                "lifecycle publication refused: a newer retained installation has incomplete assets"
            );
            return Err(PublicationStage::Assets);
        }
        Ok(current.auth.orm().registry().generation().to_string())
    }

    async fn publish(&self, publication: Publication) -> HttpResult<()> {
        let candidate = Services {
            auth: publication.auth,
            rpc: Some(publication.rpc),
            assets: publication.assets,
        };
        if let Some(AssetAdmission::Published(assets)) = &candidate.assets {
            assets
                .check_store(&self.asset_store)
                .map_err(|_| response(StatusCode::SERVICE_UNAVAILABLE, ASSETS_UNAVAILABLE))?;
        }
        if let Some(expected) = &self.expected_asset_runtime {
            match &candidate.assets {
                Some(AssetAdmission::Published(assets)) if assets.runtime() == expected => {}
                Some(AssetAdmission::Broken(_)) => {}
                _ => {
                    return Err(response(
                        StatusCode::SERVICE_UNAVAILABLE,
                        "Asset publication is missing or incompatible",
                    ));
                }
            }
        }
        let mut current = self.services.write().await;
        // A slower refresh must never overwrite a newer already-published installation.
        if candidate.generation() >= current.generation() {
            *current = candidate;
        }
        Ok(())
    }

    async fn unauthorized(&self, message: &str) -> Response {
        self.render_bearer_failure(erp_rpc::ProtocolError::Unauthorized(message.to_owned()).into())
            .await
    }
    async fn bearer_error(&self, error: erp_fields::Error) -> Response {
        self.render_bearer_failure(bearer_failure(error)).await
    }
    async fn render_bearer_failure(&self, failure: erp_rpc::RpcFailure) -> Response {
        let services = self.services.read().await;
        let Some(rpc) = services.rpc.as_ref() else {
            return internal();
        };
        let reply = rpc.reject(failure);
        let retry_after = reply.retry_after();
        let mut result = rpc_response(reply);
        result.extensions_mut().insert(RpcLogged);
        if result.status() == StatusCode::UNAUTHORIZED {
            result
                .headers_mut()
                .insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("bearer"));
        }
        if let Some(seconds) = retry_after {
            match HeaderValue::from_str(&seconds.to_string()) {
                Ok(value) => {
                    result.headers_mut().insert(header::RETRY_AFTER, value);
                }
                Err(_) => return internal(),
            }
        }
        result
    }
    async fn rpc_error(&self, error: erp_fields::Error) -> Response {
        let services = self.services.read().await;
        let Some(rpc) = services.rpc.as_ref() else {
            return internal();
        };
        rpc_response(rpc.reject(error.into()))
    }

    async fn document(&self, id: &str, value: &impl serde::Serialize) -> HttpResult<Response> {
        let json = serde_json::to_string(value)
            .map_err(|_| internal())?
            .replace('&', "\\u0026")
            .replace('<', "\\u003c")
            .replace('>', "\\u003e")
            .replace('\u{2028}', "\\u2028")
            .replace('\u{2029}', "\\u2029");
        let js = self.assets.path("client.js").ok_or_else(internal)?;
        let css = self.assets.path("client.css").ok_or_else(internal)?;
        let mut result = response(
            StatusCode::OK,
            format!(
                "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width,initial-scale=1\"><title>ERPForAll</title><link rel=\"icon\" type=\"image/svg+xml\" href=\"/erp/brand/icon.svg\"><link rel=\"stylesheet\" href=\"/erp/assets/{css}\"></head><body><main id=\"erp-root\"></main><script type=\"application/json\" id=\"{id}\">{json}</script><script type=\"module\" src=\"/erp/assets/{js}\"></script></body></html>"
            ),
        );
        result.headers_mut().insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("text/html; charset=utf-8"),
        );
        self.apply_asset_document(result, PageClass::Shell).await
    }

    /// The fixed login-success document (`login` class): the sealed `/erp/login_successful`
    /// page, and what a signed-in non-internal user opening the shell as HTML receives.
    /// No bootstrap, menus or back-office data.
    async fn login_success_page(&self) -> HttpResult<Response> {
        let mut result = response(StatusCode::OK, LOGIN_SUCCESS_PAGE);
        result.headers_mut().insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("text/html; charset=utf-8"),
        );
        self.apply_asset_document(result, PageClass::Login).await
    }

    async fn current_asset_admission(&self) -> HttpResult<Option<AssetAdmission>> {
        let admission = self.services.read().await.assets.clone();
        if matches!(admission, Some(AssetAdmission::Published(_)))
            && let Some(authority) = &self.asset_authority
        {
            // The refusal detail names keys, limits and database errors: log it, never send it.
            let assets = authority.admit().await.map_err(|error| {
                tracing::warn!(%error, "asset authority refused admission; answering 503");
                response(StatusCode::SERVICE_UNAVAILABLE, ASSETS_UNAVAILABLE)
            })?;
            if self.expected_asset_runtime.as_deref() != Some(assets.runtime()) {
                return Err(response(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "Asset runtime is incompatible",
                ));
            }
            assets
                .check_store(&self.asset_store)
                .map_err(|_| response(StatusCode::SERVICE_UNAVAILABLE, ASSETS_UNAVAILABLE))?;
            return Ok(Some(AssetAdmission::Published(assets)));
        }
        Ok(admission)
    }

    async fn apply_asset_document(
        &self,
        mut result: Response,
        class: PageClass,
    ) -> HttpResult<Response> {
        let admission = self.current_asset_admission().await?;
        let Some(admission) = &admission else {
            // No published generation (a transport fixture, a host without tenant assets): the
            // page carries no import map, so it gets the class policy without a nonce. Never a
            // page without a policy (threat model §13, T-AST.9).
            let csp = crate::assets::static_shell_csp(self.config.origin())
                .map_err(|error| response(StatusCode::SERVICE_UNAVAILABLE, error.to_string()))?;
            return class_headers(result, &csp, class);
        };
        let assets = match admission {
            AssetAdmission::Published(assets) => assets,
            AssetAdmission::Broken(_) => {
                return Err(response(
                    StatusCode::SERVICE_UNAVAILABLE,
                    ASSETS_UNAVAILABLE,
                ));
            }
        };
        if self.expected_asset_runtime.as_deref() != Some(assets.runtime()) {
            return Err(response(
                StatusCode::SERVICE_UNAVAILABLE,
                "Asset runtime is incompatible",
            ));
        }
        let shell = assets
            .shell(self.config.origin())
            .map_err(|error| response(StatusCode::SERVICE_UNAVAILABLE, error.to_string()))?;
        let body = to_bytes(
            std::mem::replace(result.body_mut(), Body::empty()),
            2 * 1024 * 1024,
        )
        .await
        .map_err(|_| internal())?;
        let html = std::str::from_utf8(&body)
            .map_err(|_| internal())?
            .replacen("</head>", &format!("{}</head>", shell.import_map_html()), 1);
        *result.body_mut() = Body::from(html);
        class_headers(result, shell.csp(), class)
    }

    async fn bootstrap(
        &self,
        tx: &mut erp_orm::Transaction,
        signer: &PrefsSigner,
        prefs: &SignedPrefs,
    ) -> HttpResult<erp_views::wire::Bootstrap> {
        // The caller checked the shell access guard (internal user) before this.
        let env = tx.env();
        let ids: Vec<i64> = env
            .principal()
            .resolution()
            .all_companies()
            .iter()
            .map(|id| id.get())
            .collect();
        if ids.is_empty() {
            return Err(response(
                StatusCode::FORBIDDEN,
                "The user has no allowed company",
            ));
        }
        env.records("res.company")
            .map_err(auth_error)?
            .browse(ids.iter().copied())
            .map_err(auth_error)?
            .check_access(tx, erp_orm::Operation::Read)
            .await
            .map_err(auth_error)?;
        let rows = tx
            .query(
                "SELECT id,name,parent_id FROM res_company WHERE id=ANY($1) ORDER BY name,id",
                &[&ids],
            )
            .await
            .map_err(auth_error)?;
        let mut companies = Vec::new();
        for row in rows {
            let id: i64 = row.try_get(0).map_err(|_| internal())?;
            let name: String = row.try_get(1).map_err(|_| internal())?;
            let parent_id: Option<i64> = row.try_get(2).map_err(|_| internal())?;
            // a parent the user may not use is not named: its id would leak
            let parent_id = parent_id.filter(|parent| ids.contains(parent));
            companies.push(serde_json::json!({"id":id,"name":name,"parent_id":parent_id}));
        }
        let context = erp_security::auth::user_context(tx, env.user_id())
            .await
            .map_err(auth_error)?
            .ok_or_else(internal)?;
        let user = tx
            .query("SELECT name FROM res_users WHERE id=$1", &[&env.uid()])
            .await
            .map_err(auth_error)?;
        let name: String = user
            .first()
            .ok_or_else(internal)?
            .try_get(0)
            .map_err(|_| internal())?;
        let language = tx.query("SELECT decimal_point,thousands_sep,grouping,date_format,time_format FROM res_lang WHERE code=$1 AND active", &[&context.lang()]).await.map_err(auth_error)?;
        let language = language.first().ok_or_else(|| {
            response(
                StatusCode::SERVICE_UNAVAILABLE,
                "The user's language is not installed",
            )
        })?;
        let grouping: String = language.try_get(2).map_err(|_| internal())?;
        let grouping: Vec<u8> = serde_json::from_str(&grouping).map_err(|_| internal())?;
        let active: Vec<i64> = env
            .companies()
            .map_err(auth_error)?
            .iter()
            .map(|id| id.get())
            .collect();
        let current = env.company_id().map_err(auth_error)?.ok_or_else(internal)?;
        let attachment_limits = match &self.attachment_policy {
            Some(provider) => Some(provider.load(tx).await.map_err(auth_error)?),
            None => None, // No provider composed: bootstrap explicitly carries no policy snapshot.
        };
        let document = serde_json::json!({"wire_major":1,"registry_version":env.registry().generation().to_string(),
            "attachment_limits":attachment_limits,"rpc_body_bytes":self.config.startup_limits().rpc_body_bytes(),
            "asset_version":self.assets.version(),"identity":{"uid":env.uid(),"name":name,"internal":true},
            "companies":companies,"current_company_id":current,"allowed_company_ids":ids,"active_company_ids":active,
            "locale":{"lang":context.lang(),"tz":context.tz().unwrap_or("UTC"),
                "decimal_point":language.try_get::<_,String>(0).map_err(|_| internal())?,
                "thousands_sep":language.try_get::<_,String>(1).map_err(|_| internal())?,"grouping":grouping,
                "date_format":language.try_get::<_,String>(3).map_err(|_| internal())?,
                "time_format":language.try_get::<_,String>(4).map_err(|_| internal())?},
            "csrf":{"logout":signer.csrf(prefs,now()?).map_err(|_| internal())?},
            "renderers":[{"key":"web.list","schema_major":1},{"key":"web.form","schema_major":1}]});
        serde_json::from_value(document).map_err(|_| internal())
    }
    fn form_origin(&self, headers: &HeaderMap) -> HttpResult<()> {
        if one(headers, "sec-fetch-site")?
            .is_some_and(|site| !matches!(site, "same-origin" | "none"))
            || one(headers, "origin")?.is_some_and(|origin| origin != self.config.origin())
        {
            return Err(response(StatusCode::FORBIDDEN, "cross-origin form refused"));
        }
        Ok(())
    }
    async fn verify_csrf(&self, headers: &HeaderMap, token: &str) -> HttpResult<()> {
        let cookies = values(headers, "cookie")?;
        let cookie = boundary::cookie(&cookies, "prefs")
            .map_err(|_| bad("Session expired (invalid CSRF token)"))?
            .ok_or_else(|| bad("Session expired (invalid CSRF token)"))?;
        let services = self.services.read().await.clone();
        let auth = services.auth;
        let signer = auth.browser_tokens().await.map_err(auth_error)?;
        let prefs = signer.verify(cookie).map_err(csrf_refusal)?;
        signer
            .verify_csrf(&prefs, token, now()?)
            .map_err(csrf_refusal)
    }
    async fn login_page(
        &self,
        headers: &HeaderMap,
        destination: String,
        error: Option<String>,
    ) -> HttpResult<Response> {
        let services = self.services.read().await.clone();
        let auth = services.auth;
        let signer = auth.browser_tokens().await.map_err(auth_error)?;
        let cookies = values(headers, "cookie")?;
        let existing = boundary::cookie(&cookies, "prefs")
            .map_err(|_| bad("ambiguous or malformed authentication cookie"))?;
        let (cookie, prefs) = match existing.map(|cookie| signer.verify(cookie)) {
            Some(Ok(prefs)) => (None, prefs),
            // Anonymous preferences are replaceable on GET; invalid tokens never authenticate POST.
            Some(Err(_)) | None => {
                let cookie = signer.issue().map_err(|_| internal())?;
                let prefs = signer.verify(&cookie).map_err(|_| internal())?;
                (Some(cookie), prefs)
            }
        };
        let dto = LoginPage::new(
            signer.csrf(&prefs, now()?).map_err(|_| internal())?,
            self.config.database().to_owned(),
            destination,
            error,
        );
        let json = serde_json::to_string(&dto)
            .map_err(|_| internal())?
            .replace('&', "\\u0026")
            .replace('<', "\\u003c")
            .replace('>', "\\u003e")
            .replace('\u{2028}', "\\u2028")
            .replace('\u{2029}', "\\u2029");
        let js = self.assets.path("client.js").ok_or_else(internal)?;
        let css = self.assets.path("client.css").ok_or_else(internal)?;
        let html = format!(
            "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width,initial-scale=1\"><title>ERPForAll</title><link rel=\"icon\" type=\"image/svg+xml\" href=\"/erp/brand/icon.svg\"><link rel=\"stylesheet\" href=\"/erp/assets/{css}\"></head><body><main id=\"erp-root\"></main><script type=\"application/json\" id=\"erp-login\">{json}</script><script type=\"module\" src=\"/erp/assets/{js}\"></script></body></html>"
        );
        let mut result = response(StatusCode::OK, html);
        result.headers_mut().insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("text/html; charset=utf-8"),
        );
        if let Some(cookie) = cookie {
            self.set_cookie(&mut result, "prefs", &cookie, PREFS_LIFETIME_SECS)?;
        }
        self.apply_asset_document(result, PageClass::Login).await
    }
    fn session(&self, headers: &HeaderMap) -> HttpResult<Option<SessionSecret>> {
        let cookies = values(headers, "cookie")?;
        let value = boundary::cookie(&cookies, "session_id")
            .map_err(|_| bad("ambiguous or malformed authentication cookie"))?;
        let native = one(headers, "x-erp-database")?;
        let legacy = one(headers, "x-odoo-database")?;
        let header = match (native, legacy) {
            (Some(_), Some(_)) => {
                return Err(bad("Ambiguous database selectors; use X-ERP-Database."));
            }
            (Some(value), None) | (None, Some(value)) => Some(value),
            (None, None) => None,
        };
        let Some(value) = value else {
            boundary::database(self.config.database(), header, None)
                .map_err(|_| bad("Database not found."))?;
            return Ok(None);
        };
        let (database, secret) = value
            .split_once('.')
            .ok_or_else(|| bad("ambiguous or malformed authentication cookie"))?;
        boundary::database(self.config.database(), header, Some(database))
            .map_err(|error| bad(error.to_string()))?;
        SessionSecret::from_cookie(secret)
            .map(Some)
            .ok_or_else(|| bad("ambiguous or malformed authentication cookie"))
    }
    fn set_cookie(
        &self,
        response: &mut Response,
        name: &str,
        value: &str,
        lifetime: u64,
    ) -> HttpResult<()> {
        let cookie = self.cookie_header(name, value, lifetime)?;
        response.headers_mut().append(header::SET_COOKIE, cookie);
        Ok(())
    }
    fn cookie_header(&self, name: &str, value: &str, lifetime: u64) -> HttpResult<HeaderValue> {
        let secure = if self.config.origin().starts_with("https:") {
            "; Secure"
        } else {
            ""
        };
        let cookie =
            format!("{name}={value}; Path=/; HttpOnly; SameSite=Lax; Max-Age={lifetime}{secure}");
        HeaderValue::from_str(&cookie).map_err(|_| internal())
    }
}

// Legacy input aliases are isolated here; all generated URLs use /erp.
/// Marks a response whose failure the RPC layer already logged.
#[derive(Clone)]
struct RpcLogged;

/// The browser cleanup of a verified logout (T-SHELL.5): the expired `session_id` cookie and
/// `Clear-Site-Data`. It rides on the response as an extension so a later replacement of a 5xx
/// (`superseded_failure`) can carry exactly these headers over, and nothing else.
#[derive(Clone)]
struct LogoutCleanup {
    expired_session: HeaderValue,
}

impl LogoutCleanup {
    fn new(expired_session: HeaderValue) -> Self {
        Self { expired_session }
    }
    fn apply(&self, response: &mut Response) {
        let headers = response.headers_mut();
        headers.append(header::SET_COOKIE, self.expired_session.clone());
        headers.insert(
            "clear-site-data",
            HeaderValue::from_static(LOGOUT_CLEAR_SITE_DATA),
        );
        response.extensions_mut().insert(self.clone());
    }
}

/// The answer that replaces a 5xx served by a superseded generation: `busy()`, plus the cleanup
/// of a verified logout when the replaced response carried one. Other headers of the replaced
/// response (any other `set-cookie` among them) are dropped.
fn superseded_failure(replaced: &Response) -> Response {
    let mut result = busy();
    if let Some(cleanup) = replaced.extensions().get::<LogoutCleanup>() {
        cleanup.apply(&mut result);
    }
    result
}

/// The request's id. An incoming `X-Request-Id` is kept only when it is exactly one header holding
/// a UUID (RFC 9562 text form, any version); anything else (several values, other text, other
/// lengths) is replaced by a fresh random v4 UUID, never refused: the id is a label to follow a
/// request through the logs, not an input the server acts on.
fn request_id(headers: &HeaderMap) -> HttpResult<erp_rpc::CorrelationId> {
    let mut sent = headers.get_all("x-request-id").iter();
    if let (Some(value), None) = (sent.next(), sent.next())
        // specified: a malformed id is replaced, not an error
        && let Some(id) = value
            .to_str()
            .ok()
            .and_then(|text| erp_rpc::CorrelationId::parse(text).ok())
    {
        return Ok(id);
    }
    fresh_correlation_id().map_err(|_| internal())
}

/// A fresh random v4 UUID (RFC 9562) from OS randomness.
fn fresh_correlation_id() -> Result<erp_rpc::CorrelationId, getrandom::Error> {
    let mut bytes = [0_u8; 16];
    getrandom::fill(&mut bytes)?;
    bytes[6] = bytes[6] & 0x0f | 0x40;
    bytes[8] = bytes[8] & 0x3f | 0x80;
    Ok(erp_rpc::CorrelationId::from_u128(u128::from_be_bytes(
        bytes,
    )))
}

/// The server-generated reference shown on a rendered error page ("Reference: …"), logged as
/// `error_id`. It is never the client's `X-Request-Id`, so a page cannot be made to show a
/// value the requester chose (T-SHELL.7).
#[derive(Clone)]
struct ErrorReference(erp_rpc::CorrelationId);

/// The fixed text of the refusal's typed cause (a `BoundaryError` variant), logged as
/// `reason`. Never request input: no URL, query, header or cookie value.
#[derive(Clone, Copy)]
struct RefusalReason(&'static str);

/// One log line for a refusal that did not come through the RPC layer (a CSRF, media type, proxy
/// or session refusal; a server error). Method and path only: no query, header, cookie or body.
/// `reason` is the fixed text of a typed refusal ([`RefusalReason`]) and `error_id` the reference
/// the rendered error page shows ([`ErrorReference`]); each is written only when present.
/// Unknown paths and methods (scanners) are `info`, which the default `warn` filter drops.
fn log_web_refusal(method: &Method, path: &str, response: &Response, request_id: &str) {
    let code = response.status().as_u16();
    let path: String = path.chars().take(200).collect();
    let reason = response
        .extensions()
        .get::<RefusalReason>()
        .map(|reason| reason.0);
    let error_id = response
        .extensions()
        .get::<ErrorReference>()
        .map(|reference| reference.0.as_str());
    if code >= 500 {
        tracing::error!(target: "erp::http", %method, path, status = code, request_id, reason, error_id, "http request failed");
    } else if code == 404 || code == 405 {
        tracing::info!(target: "erp::http", %method, path, status = code, request_id, reason, error_id, "http request not found");
    } else if code >= 400 {
        tracing::warn!(target: "erp::http", %method, path, status = code, request_id, reason, error_id, "http request refused");
    }
}

fn canonical_path(path: &str) -> Cow<'_, str> {
    for (legacy, native) in routes::LEGACY_EXACT {
        if path == *legacy {
            return Cow::Borrowed(*native);
        }
    }
    for (legacy, native) in routes::LEGACY_PREFIX {
        if let Some(suffix) = path.strip_prefix(*legacy) {
            return Cow::Owned(format!("{native}{suffix}"));
        }
    }
    Cow::Borrowed(path)
}

/// Lifetime of the anonymous preferences cookie, renewed by every verified shell load.
const PREFS_LIFETIME_SECS: u64 = 48 * 60 * 60;

/// What a valid sign-out clears in the browser for this origin (T-SHELL.5).
const LOGOUT_CLEAR_SITE_DATA: &str = "\"cache\", \"storage\"";

// Fixed document for a signed-in user without web client access; nothing is interpolated.
const LOGIN_SUCCESS_PAGE: &str = "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width,initial-scale=1\"><title>Login successful · ERPForAll</title><link rel=\"icon\" type=\"image/svg+xml\" href=\"/erp/brand/icon.svg\"></head><body><main><h1>Login successful.</h1></main></body></html>";

/// Header class of a served HTML document (threat model §13). Both classes send
/// `same-origin`: a page that hosts a form must not send `no-referrer`, because a browser then
/// puts `Origin: null` on that form's POST and the same-origin check of the login refuses it
/// (release cf9b245a6, 2026-11-12: nobody could sign in). Local redirects keep `no-referrer`.
#[derive(Debug, Clone, Copy)]
enum PageClass {
    Shell,
    Login,
}

impl PageClass {
    fn referrer_policy(self) -> HeaderValue {
        match self {
            Self::Shell => HeaderValue::from_static("same-origin"),
            Self::Login => HeaderValue::from_static("same-origin"),
        }
    }
}

/// The `shell`/`login` class headers of a served document (threat model §13) with policy `csp`.
fn class_headers(mut result: Response, csp: &str, class: PageClass) -> HttpResult<Response> {
    let headers = result.headers_mut();
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_str(csp).map_err(|_| internal())?,
    );
    headers.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    headers.insert(header::REFERRER_POLICY, class.referrer_policy());
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    Ok(result)
}

// The error page's only styling, an inline `<style>` element allowed by the hash of these exact
// bytes (`ERROR_CSP`), never `'unsafe-inline'`. The page has no `style=` attributes. A separate
// stylesheet is not used: `busy()` answers before the brand route while a node drains.
const ERROR_STYLE: &str = r#":root{font-family:system-ui,-apple-system,BlinkMacSystemFont,"Segoe UI",sans-serif;color:#28343c;background:#f5f6f8;color-scheme:light}*{box-sizing:border-box}body{margin:0}header{padding:12px 24px;background:#fff;border-bottom:1px solid #dce0e4}header img{display:block;height:28px;width:auto}main{min-height:calc(100dvh - 60px);display:grid;place-items:center;padding:24px}.card{width:100%;max-width:520px;background:white;border:1px solid #dce1e5;border-radius:12px;padding:40px;box-shadow:0 3px 14px #23303b0a}.code{color:#714b67;font-size:14px;font-weight:650;letter-spacing:.08em;margin:0 0 16px}h1{font-size:28px;line-height:1.25;margin:0 0 14px}p{line-height:1.6;color:#596570;margin:0 0 28px}nav{display:flex;gap:12px;flex-wrap:wrap}a{display:inline-flex;align-items:center;justify-content:center;min-height:44px;border:1px solid #cbd2d8;border-radius:6px;padding:10px 18px;color:#34424b;font-weight:600;text-decoration:none}.primary{background:#007e84;border-color:#007e84;color:white}a:hover{filter:brightness(.94)}a:focus-visible{outline:3px solid #218c99;outline-offset:3px}@media(max-width:480px){main{padding:16px}.card{padding:28px 24px}h1{font-size:25px}nav a{flex:1}}.ref{margin:24px 0 0;font-size:13px;line-height:1.5}.ref code{font-family:ui-monospace,SFMono-Regular,Menlo,monospace;user-select:all}"#;

// Fixed template owned by this transport; no request or diagnostic text is interpolated. The
// reference is a fresh server-generated UUID (`ErrorReference`), never request input.
const ERROR_PAGE: &str = r#"<!doctype html>
<html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1"><title>{{TITLE}} · ERPForAll</title>
<style>{{STYLE}}</style></head><body><header><img src="/erp/brand/logo.svg" alt="ERPForAll" width="105" height="28"></header><main><section class="card" aria-labelledby="error-title"><p class="code">ERROR {{STATUS}}</p><h1 id="error-title">{{TITLE}}</h1><p>{{MESSAGE}}</p><nav aria-label="Recovery options"><a class="primary" href="/erp/login">Sign in again</a><a href="/erp">Go to home</a></nav><p class="ref">Reference: <code>{{REFERENCE}}</code></p></section></main></body></html>
"#;

/// The error page policy (threat model §13 `error` class, moved toward it): the style is allowed
/// by its sha256, the logo by `img-src 'self'`. If the header cannot be built, the page goes out
/// unstyled under a stricter policy and the failure is logged; it never falls back to
/// `'unsafe-inline'`.
static ERROR_CSP: std::sync::LazyLock<HeaderValue> = std::sync::LazyLock::new(|| {
    use base64::Engine as _;
    use sha2::Digest as _;
    let hash = base64::engine::general_purpose::STANDARD
        .encode(sha2::Sha256::digest(ERROR_STYLE.as_bytes()));
    let policy = format!(
        "default-src 'none'; style-src 'sha256-{hash}'; img-src 'self'; base-uri 'none'; frame-ancestors 'none'; form-action 'self'"
    );
    HeaderValue::from_str(&policy).unwrap_or_else(|_| {
        tracing::error!(target: "erp::http", "error page policy could not be built; serving it unstyled");
        HeaderValue::from_static(
            "default-src 'none'; img-src 'self'; base-uri 'none'; frame-ancestors 'none'; form-action 'self'",
        )
    })
});

// Render only navigation failures. JSON/asset protocol bodies and every HTTP status remain intact.
fn finish_response(mut result: Response, navigation: bool) -> Response {
    let status = result.status();
    if navigation
        && result.extensions().get::<LifecycleResponse>().is_none()
        && (status.is_client_error() || status.is_server_error())
    {
        let (title, message) = match status {
            StatusCode::BAD_REQUEST => (
                "Request could not be processed",
                "Reload the page and try again. If the problem continues, sign in again.",
            ),
            StatusCode::UNAUTHORIZED => (
                "Your session has expired",
                "Sign in again to continue working.",
            ),
            StatusCode::FORBIDDEN => (
                "Access is restricted",
                "Your account does not have access to this page. Contact your administrator if you need access.",
            ),
            StatusCode::NOT_FOUND => (
                "Page not found",
                "This page may have moved or the address may be incorrect. You can return to the application home.",
            ),
            StatusCode::TOO_MANY_REQUESTS => (
                "Please wait a moment",
                "Too many requests were received. Wait briefly before trying again.",
            ),
            StatusCode::SERVICE_UNAVAILABLE => (
                "Temporarily unavailable",
                "The application is busy or updating. Please try again shortly.",
            ),
            _ => (
                "Something went wrong",
                "The application could not complete this request. Try again or contact your administrator.",
            ),
        };
        // A fresh id, never the client's X-Request-Id. Without OS randomness the page shows no
        // reference and the failure is logged; no other value stands in for it.
        let reference = match fresh_correlation_id() {
            Ok(reference) => Some(reference),
            Err(_) => {
                tracing::error!(target: "erp::http", "OS randomness unavailable; error page rendered without a reference");
                None
            }
        };
        let shown = reference
            .as_ref()
            .map_or("unavailable", erp_rpc::CorrelationId::as_str);
        let html = ERROR_PAGE
            .replace("{{STYLE}}", ERROR_STYLE)
            .replace("{{TITLE}}", title)
            .replace("{{STATUS}}", status.as_str())
            .replace("{{MESSAGE}}", message)
            .replace("{{REFERENCE}}", shown);
        *result.body_mut() = Body::from(html);
        result.headers_mut().remove(header::CONTENT_LENGTH);
        result.headers_mut().insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("text/html; charset=utf-8"),
        );
        result
            .headers_mut()
            .insert(header::CONTENT_SECURITY_POLICY, ERROR_CSP.clone());
        if let Some(reference) = reference {
            result.extensions_mut().insert(ErrorReference(reference));
        }
    }
    if !result.headers().contains_key(header::CACHE_CONTROL) {
        result
            .headers_mut()
            .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    }
    result.headers_mut().insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    // A page or redirect that chose the stricter class (login, redirect) keeps it; any other
    // value a handler set is replaced, so no response leaves with a weaker policy.
    let mut chosen = result.headers().get_all(header::REFERRER_POLICY).iter();
    let strict = chosen
        .next()
        .is_some_and(|value| value.as_bytes() == b"no-referrer")
        && chosen.next().is_none();
    if !strict {
        result.headers_mut().insert(
            header::REFERRER_POLICY,
            HeaderValue::from_static("same-origin"),
        );
    }
    result
}

const BRAND_ICON: &[u8] = include_bytes!("../brand/erpforall-icon.svg");
const BRAND_LOGO: &[u8] = include_bytes!("../brand/erpforall-logo.svg");
const BRAND_LOGO_DARK: &[u8] = include_bytes!("../brand/erpforall-logo-dark.svg");

// Brand images compiled into the binary, served anonymously like the favicon browsers probe;
// none opens an application session.
fn brand_route(method: &Method, path: &str) -> Option<Response> {
    if *method != Method::GET {
        return None;
    }
    match path {
        routes::FAVICON | routes::BRAND_ICON_PATH => Some(brand_response(BRAND_ICON)),
        routes::BRAND_LOGO_PATH => Some(brand_response(BRAND_LOGO)),
        routes::BRAND_LOGO_DARK_PATH => Some(brand_response(BRAND_LOGO_DARK)),
        _ => None,
    }
}

fn brand_response(svg: &'static [u8]) -> Response {
    let mut response = Response::new(Body::from(svg));
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("image/svg+xml"),
    );
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("public, max-age=86400"),
    );
    // An SVG opened directly is a document: it may draw, never run script or load anything.
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static("default-src 'none'; style-src 'unsafe-inline'"),
    );
    response
}

/// Whether the request asks for HTML (an `Accept` item starting with `text/html`), as a browser
/// navigation does. A header value that is not visible ASCII counts as not asking for HTML.
fn accepts_html(headers: &HeaderMap) -> bool {
    headers.get_all(header::ACCEPT).iter().any(|value| {
        value.to_str().is_ok_and(|value| {
            value
                .split(',')
                .any(|item| item.trim().starts_with("text/html"))
        })
    })
}

fn values<'a>(headers: &'a HeaderMap, name: &str) -> HttpResult<Vec<&'a str>> {
    headers
        .get_all(name)
        .iter()
        .map(|value| value.to_str().map_err(|_| bad("invalid HTTP header")))
        .collect()
}
fn one<'a>(headers: &'a HeaderMap, name: &str) -> HttpResult<Option<&'a str>> {
    let values = values(headers, name)?;
    match values.as_slice() {
        [] => Ok(None),
        [value] => Ok(Some(*value)),
        _ => Err(bad("duplicate HTTP header")),
    }
}
fn form(input: &str, allowed: &[&str]) -> HttpResult<BTreeMap<String, String>> {
    let mut values = BTreeMap::new();
    for (key, value) in url::form_urlencoded::parse(input.as_bytes()) {
        if !allowed.contains(&key.as_ref())
            || values
                .insert(key.into_owned(), value.into_owned())
                .is_some()
        {
            return Err(bad("unknown or duplicate form field"));
        }
    }
    Ok(values)
}
async fn read_form(
    headers: &HeaderMap,
    body: Body,
    allowed: &[&str],
) -> HttpResult<BTreeMap<String, String>> {
    if one(headers, "content-type")? != Some("application/x-www-form-urlencoded") {
        return Err(response(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "expected application/x-www-form-urlencoded",
        ));
    }
    let bytes = to_bytes(body, 16 * 1024).await.map_err(|_| {
        response(
            StatusCode::PAYLOAD_TOO_LARGE,
            "authentication body exceeds 16384 bytes",
        )
    })?;
    let raw = zeroize::Zeroizing::new(bytes.to_vec());
    form(
        std::str::from_utf8(&raw).map_err(|_| bad("form is not UTF-8"))?,
        allowed,
    )
}
fn required<'a>(fields: &'a BTreeMap<String, String>, key: &str) -> HttpResult<&'a str> {
    fields
        .get(key)
        .map(String::as_str)
        .ok_or_else(|| bad("missing form field"))
}
fn now() -> HttpResult<u64> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|time| time.as_secs())
        .map_err(|_| internal())
}
/// Public 503 text for a broken asset admission. The admission detail names addons, asset paths and
/// compiler messages, so it stays in the server log (written where the admission is composed).
const ASSETS_UNAVAILABLE: &str =
    "UI assets are unavailable; the operator must check the server log";

fn response(status: StatusCode, text: impl Into<String>) -> Response {
    (status, text.into()).into_response()
}
fn bad(text: impl Into<String>) -> Response {
    response(StatusCode::BAD_REQUEST, text)
}
fn internal() -> Response {
    response(StatusCode::INTERNAL_SERVER_ERROR, "Internal server error")
}
fn invalid_api_cookie(error: &erp_fields::Error) -> bool {
    if let Some(cooldown) = error
        .source()
        .and_then(|source| source.downcast_ref::<ApiKeyCooldown>())
    {
        return cooldown.drop_cookie();
    }
    error
        .source()
        .and_then(|source| source.downcast_ref::<ApiKeyRefusal>())
        .is_some_and(ApiKeyRefusal::drop_cookie)
}
fn bearer_failure(error: erp_fields::Error) -> erp_rpc::RpcFailure {
    if error.condition() == Some(erp_fields::Condition::Overloaded) {
        // An admission/settings wait expired before authentication. This hint does not
        // promise when capacity will become free; the next attempt must pass admission.
        return erp_rpc::InfraError::Overloaded { retry_after: 1 }.into();
    }
    if let Some(cooldown) = error
        .source()
        .and_then(|source| source.downcast_ref::<ApiKeyCooldown>())
    {
        let duration = cooldown.retry_after();
        let seconds = duration
            .as_secs()
            .saturating_add(u64::from(duration.subsec_nanos() != 0));
        // HTTP Retry-After is bounded by the protocol's u32 representation; a longer
        // configured duration saturates the hint, never the enforced cooldown.
        let retry_after = match u32::try_from(seconds) {
            Ok(value) => value.max(1),
            Err(_) => u32::MAX,
        };
        return erp_rpc::InfraError::RateLimited { retry_after }.into();
    }
    if error.kind() == erp_fields::ErrorKind::AccessDenied && error.message() == "Invalid apikey" {
        return erp_rpc::ProtocolError::Unauthorized(error.message().to_owned()).into();
    }
    error.into()
}
fn auth_error(error: erp_fields::Error) -> Response {
    if error.is_superseded() {
        return response(
            StatusCode::SERVICE_UNAVAILABLE,
            "The application is updating; retry shortly.",
        );
    }
    if matches!(
        error.kind(),
        erp_fields::ErrorKind::Access
            | erp_fields::ErrorKind::AccessDenied
            | erp_fields::ErrorKind::Validation
    ) {
        return response(
            if error.kind() == erp_fields::ErrorKind::Validation {
                StatusCode::BAD_REQUEST
            } else {
                StatusCode::FORBIDDEN
            },
            error.to_string(),
        );
    }
    if error.kind() == erp_fields::ErrorKind::SessionExpired {
        return response(StatusCode::UNAUTHORIZED, error.to_string());
    }
    tracing::error!(
        kind = ?error.kind(),
        sqlstate = error.sqlstate(),
        reason = ?error.message(),
        request_id = erp_orm::trace::current_request_id().as_deref(),
        "authentication operation failed"
    );
    internal()
}
/// The answer to a failed form-token check. A bad cookie or token is the client's 400; a key or
/// entropy failure is the server's: a plain 500 whose body names neither the token nor the key,
/// with the cause in the log (its text names no secret).
fn csrf_refusal(error: PrefsError) -> Response {
    match error {
        PrefsError::Cookie | PrefsError::Csrf => bad("Session expired (invalid CSRF token)"),
        _ => {
            tracing::error!(
                target: "erp::http",
                reason = %error,
                request_id = erp_orm::trace::current_request_id().as_deref(),
                "browser form token check failed on the server side"
            );
            internal()
        }
    }
}
fn busy() -> Response {
    let mut result = response(StatusCode::SERVICE_UNAVAILABLE, "Server busy; retry later.");
    result
        .headers_mut()
        .insert(header::RETRY_AFTER, HeaderValue::from_static("5"));
    result
}
/// A local `303` (threat model §13 `redirect` class): no referrer leaves with it, nothing caches it.
fn redirect(location: &str) -> HttpResult<Response> {
    let mut result = response(StatusCode::SEE_OTHER, "");
    let headers = result.headers_mut();
    headers.insert(
        header::LOCATION,
        HeaderValue::from_str(location).map_err(|_| bad("unsafe redirect"))?,
    );
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    Ok(result)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LifecycleOperation {
    Install,
    Upgrade,
    Uninstall,
    ForceDemo,
}
impl LifecycleOperation {
    fn from_path(path: &str) -> Option<Self> {
        match path {
            routes::MODULE_INSTALL => Some(Self::Install),
            routes::MODULE_UPGRADE => Some(Self::Upgrade),
            routes::MODULE_UNINSTALL => Some(Self::Uninstall),
            routes::MODULE_FORCE_DEMO => Some(Self::ForceDemo),
            _ => None, // Exact canonical paths alone own lifecycle writes.
        }
    }
    fn name(self) -> &'static str {
        match self {
            Self::Install => "Install",
            Self::Upgrade => "Upgrade",
            Self::Uninstall => "Uninstall",
            Self::ForceDemo => "Load demo data",
        }
    }
}
#[derive(Debug)]
struct LifecycleRequest {
    modules: Vec<String>,
    impact: Option<String>,
}
fn lifecycle_request(
    operation: LifecycleOperation,
    bytes: &[u8],
) -> erp_fields::Result<LifecycleRequest> {
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Modules {
        modules: Vec<String>,
    }
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Uninstall {
        modules: Vec<String>,
        impact: String,
    }
    let invalid = || {
        erp_fields::Error::validation(match operation {
            LifecycleOperation::Install => {
                "Invalid install request: expected only modules as a string array"
            }
            LifecycleOperation::Upgrade => {
                "Invalid upgrade request: expected only modules as a string array"
            }
            LifecycleOperation::Uninstall => {
                "Invalid uninstall request: expected only modules as a string array and impact as a string"
            }
            LifecycleOperation::ForceDemo => "Invalid demo request: expected only confirm=true",
        })
    };
    let request = match operation {
        LifecycleOperation::Install | LifecycleOperation::Upgrade => {
            let parsed: Modules = serde_json::from_slice(bytes).map_err(|_| invalid())?;
            LifecycleRequest {
                modules: parsed.modules,
                impact: None,
            }
        }
        // its body is checked by `force_demo_confirmed`; no module names travel with it
        LifecycleOperation::ForceDemo => return Err(invalid()),
        LifecycleOperation::Uninstall => {
            let parsed: Uninstall = serde_json::from_slice(bytes).map_err(|_| invalid())?;
            let valid = parsed
                .impact
                .strip_prefix("uninstall-v1:")
                .is_some_and(|digest| {
                    digest.len() == 64
                        && digest
                            .bytes()
                            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
                });
            if !valid {
                return Err(erp_fields::Error::validation(
                    "Invalid uninstall impact fingerprint",
                ));
            }
            LifecycleRequest {
                modules: parsed.modules,
                impact: Some(parsed.impact),
            }
        }
    };
    if request.modules.is_empty() || request.modules.len() > 64 {
        return Err(erp_fields::Error::validation(format!(
            "{} requires between 1 and 64 addon names",
            operation.name()
        )));
    }
    let mut unique = std::collections::BTreeSet::new();
    for name in &request.modules {
        if name.is_empty()
            || name.len() > 256
            || !name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
            || !unique.insert(name)
        {
            return Err(erp_fields::Error::validation(format!(
                "{} contains an invalid or duplicate addon name",
                operation.name()
            )));
        }
    }
    Ok(request)
}
/// The body of "load demo data": exactly `{"confirm":true}`, the typed form of the irreversible
/// confirmation; anything else is a validation error.
fn force_demo_confirmed(bytes: &[u8]) -> erp_fields::Result<()> {
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Confirm {
        confirm: bool,
    }
    match serde_json::from_slice::<Confirm>(bytes) {
        Ok(Confirm { confirm: true }) => Ok(()),
        _ => Err(erp_fields::Error::validation(
            "Loading demo data is irreversible: send {\"confirm\":true} to confirm",
        )),
    }
}
#[derive(Clone)]
struct LifecycleResponse;
// Preserve the public install adapter signature while retaining a typed immediate source.
// Diagnostic strings never select outcome: only this exact trusted source bridge does.
fn install_outcome_response(error: &erp_fields::Error) -> Option<Response> {
    let failure = error.source()?.downcast_ref::<LifecycleFailure>()?;
    match failure {
        LifecycleFailure::Unknown(_) => Some(lifecycle_outcome_response(
            LifecycleOperation::Install,
            None,
        )),
        LifecycleFailure::Committed { stage, .. } => Some(lifecycle_outcome_response(
            LifecycleOperation::Install,
            Some(*stage),
        )),
        LifecycleFailure::Refused(_) => None,
    }
}
fn lifecycle_deadline_response(operation: Option<LifecycleOperation>) -> Response {
    match operation {
        // A cancelled lifecycle future may have sent COMMIT without an acknowledgement: unknown.
        Some(operation) => lifecycle_outcome_response(operation, None),
        None => busy(),
    }
}
fn lifecycle_outcome_response(
    operation: LifecycleOperation,
    stage: Option<PublicationStage>,
) -> Response {
    let message = match stage {
        Some(stage) => format!(
            "{} committed but {} is incomplete; operator recovery is required",
            operation.name(),
            stage.description()
        ),
        None => format!(
            "{} outcome is unknown; reconcile addon state before any retry",
            operation.name()
        ),
    };
    let value = serde_json::json!({
        "name":if stage.is_some() { "erp.lifecycle.committed" } else { "erp.lifecycle.unknown" },
        "message":message,"outcome":if stage.is_some() {"committed"} else {"unknown"},
        "stage":stage.map(PublicationStage::key),
    });
    let mut result = match json_response(StatusCode::SERVICE_UNAVAILABLE, &value) {
        Ok(result) => result,
        Err(result) => result,
    };
    result.extensions_mut().insert(LifecycleResponse);
    result
}

fn install_names(bytes: &[u8]) -> HttpResult<Vec<String>> {
    lifecycle_request(LifecycleOperation::Install, bytes)
        .map(|request| request.modules)
        .map_err(|error| bad(error.message()))
}

fn json_response(status: StatusCode, value: &impl serde::Serialize) -> HttpResult<Response> {
    let mut result = Response::new(Body::from(
        serde_json::to_vec(value).map_err(|_| internal())?,
    ));
    *result.status_mut() = status;
    result.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    Ok(result)
}
fn rpc_response(reply: erp_rpc::RpcResponse) -> Response {
    let mut result = Response::new(Body::from(reply.body().to_vec()));
    match StatusCode::from_u16(reply.status()) {
        Ok(status) => *result.status_mut() = status,
        Err(_) => return internal(),
    }
    result.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    /// confirmation)
    #[test]
    #[allow(clippy::unwrap_used, reason = "test assertions")]
    fn force_demo_route_and_body_are_closed() {
        assert_eq!(
            LifecycleOperation::from_path("/erp/module/force-demo"),
            Some(LifecycleOperation::ForceDemo)
        );
        for path in [
            "/web/module/force-demo",
            "/erp/module/force-demo/",
            "/erp/module/force_demo",
        ] {
            assert_eq!(LifecycleOperation::from_path(path), None, "{path}");
        }
        force_demo_confirmed(br#"{"confirm":true}"#).unwrap();
        for body in [
            &br#"{}"#[..],
            br#"{"confirm":false}"#,
            br#"{"confirm":"yes"}"#,
            br#"{"confirm":true,"modules":["base"]}"#,
            b"",
            b"[]",
        ] {
            let error = force_demo_confirmed(body).unwrap_err();
            assert_eq!(error.kind(), erp_fields::ErrorKind::Validation);
            assert!(error.message().contains("irreversible"));
        }
    }

    #[tokio::test]
    #[allow(clippy::unwrap_used, reason = "test assertions")]
    async fn lifecycle_paths_and_requests_are_closed() {
        assert_eq!(
            LifecycleOperation::from_path("/erp/module/upgrade"),
            Some(LifecycleOperation::Upgrade)
        );
        assert_eq!(
            LifecycleOperation::from_path("/erp/module/uninstall"),
            Some(LifecycleOperation::Uninstall)
        );
        for path in [
            "/web/module/upgrade",
            "/erp/module/upgrade/",
            "/erp/module/upgrade/extra",
        ] {
            assert_eq!(LifecycleOperation::from_path(path), None);
        }
        let request =
            lifecycle_request(LifecycleOperation::Upgrade, br#"{"modules":["notes"]}"#).unwrap();
        assert_eq!(request.modules, vec!["notes"]);
        assert_eq!(request.impact, None);
        for body in [
            br#"{"modules":["notes"],"impact":"x"}"#.as_slice(),
            br#"{"modules":null}"#,
            br#"{"modules":["notes"],"unknown":true}"#,
        ] {
            let error = lifecycle_request(LifecycleOperation::Upgrade, body).unwrap_err();
            assert_eq!(error.kind(), erp_fields::ErrorKind::Validation);
            assert_eq!(
                error.message(),
                "Invalid upgrade request: expected only modules as a string array"
            );
        }
        let error = lifecycle_request(
            LifecycleOperation::Uninstall,
            br#"{"modules":["notes"],"impact":"bad"}"#,
        )
        .unwrap_err();
        assert_eq!(error.kind(), erp_fields::ErrorKind::Validation);
        assert_eq!(error.message(), "Invalid uninstall impact fingerprint");
    }

    #[test]
    fn canonical_path_keeps_its_legacy_mappings() {
        for (from, to) in [
            ("/odoo", "/erp"),
            ("/web", "/erp"),
            ("/web/login", "/erp/login"),
            ("/web/login_successful", "/erp/login_successful"),
            ("/web/session/logout", "/erp/session/logout"),
            ("/web/module/install", "/erp/module/install"),
            ("/odoo/action-12", "/erp/action-12"),
            ("/web/assets/a.js", "/erp/assets/a.js"),
            ("/json/2/res.partner/read", "/erp/json/2/res.partner/read"),
            ("/web/dataset/call_kw/x", "/erp/dataset/call_kw/x"),
            ("/erp/login", "/erp/login"),
            ("/web/content/3", "/erp/content/3"),
        ] {
            assert_eq!(canonical_path(from), to, "{from}");
        }
    }

    #[test]
    fn every_served_legacy_alias_lands_on_a_served_route() {
        for (legacy, native) in routes::LEGACY_EXACT.iter().chain(routes::LEGACY_PREFIX) {
            if routes::KNOWN_UNSERVED_ALIAS_TARGETS.contains(native) {
                continue;
            }
            let sample = if routes::LEGACY_EXACT.contains(&(*legacy, *native)) {
                (*legacy).to_owned()
            } else {
                format!("{legacy}sample")
            };
            assert!(routes::served(&canonical_path(&sample)), "{sample}");
        }
        for path in [
            "/web/login",
            "/odoo/action-5",
            "/web/assets/x.js",
            "/json/2/res.partner/read",
        ] {
            assert!(routes::served(&canonical_path(path)), "{path}");
        }
    }

    #[tokio::test]
    #[allow(clippy::unwrap_used, reason = "test assertions")]
    async fn lifecycle_install_admission_keeps_its_existing_contract() {
        assert_eq!(
            LifecycleOperation::from_path(&canonical_path("/web/module/install")),
            Some(LifecycleOperation::Install)
        );
        assert_eq!(
            install_names(br#"{"modules":["notes"]}"#).unwrap(),
            vec!["notes"]
        );
        for (body, message) in [
            (
                br#"{"modules":[],"unknown":true}"#.as_slice(),
                "Invalid install request: expected only modules as a string array",
            ),
            (
                br#"{"modules":[]}"#,
                "Install requires between 1 and 64 addon names",
            ),
            (
                br#"{"modules":["notes","notes"]}"#,
                "Install contains an invalid or duplicate addon name",
            ),
        ] {
            let error = install_names(body).unwrap_err();
            assert_eq!(error.status(), StatusCode::BAD_REQUEST);
            assert_eq!(
                to_bytes(error.into_body(), 4096).await.unwrap().as_ref(),
                message.as_bytes()
            );
        }
    }

    #[test]
    #[allow(clippy::unwrap_used, reason = "test assertions")]
    fn lifecycle_request_bounds_are_exact() {
        for modules in [
            vec![],
            vec!["notes".to_owned(), "notes".to_owned()],
            vec!["bad-name".to_owned()],
            vec!["a".repeat(257)],
        ] {
            let bytes = serde_json::to_vec(&serde_json::json!({"modules":modules})).unwrap();
            let error = lifecycle_request(LifecycleOperation::Upgrade, &bytes).unwrap_err();
            assert_eq!(error.kind(), erp_fields::ErrorKind::Validation);
            assert_eq!(
                error.message(),
                if modules.is_empty() {
                    "Upgrade requires between 1 and 64 addon names"
                } else {
                    "Upgrade contains an invalid or duplicate addon name"
                }
            );
        }
        let modules: Vec<_> = (0..64).map(|n| format!("addon_{n}")).collect();
        let bytes = serde_json::to_vec(&serde_json::json!({"modules":modules})).unwrap();
        assert_eq!(
            lifecycle_request(LifecycleOperation::Upgrade, &bytes)
                .unwrap()
                .modules
                .len(),
            64
        );
        let modules: Vec<_> = (0..65).map(|n| format!("addon_{n}")).collect();
        let bytes = serde_json::to_vec(&serde_json::json!({"modules":modules})).unwrap();
        let error = lifecycle_request(LifecycleOperation::Upgrade, &bytes).unwrap_err();
        assert_eq!(error.kind(), erp_fields::ErrorKind::Validation);
        assert_eq!(
            error.message(),
            "Upgrade requires between 1 and 64 addon names"
        );
        let fingerprint = format!("uninstall-v1:{}", "a".repeat(64));
        let bytes =
            serde_json::to_vec(&serde_json::json!({"modules":["notes"],"impact":fingerprint}))
                .unwrap();
        assert_eq!(
            lifecycle_request(LifecycleOperation::Uninstall, &bytes)
                .unwrap()
                .impact,
            Some(fingerprint)
        );
        for impact in [
            format!("uninstall-v1:{}", "A".repeat(64)),
            format!("uninstall-v1:{}", "a".repeat(63)),
            format!("uninstall-v1:{}", "a".repeat(65)),
        ] {
            let bytes =
                serde_json::to_vec(&serde_json::json!({"modules":["notes"],"impact":impact}))
                    .unwrap();
            let error = lifecycle_request(LifecycleOperation::Uninstall, &bytes).unwrap_err();
            assert_eq!(error.kind(), erp_fields::ErrorKind::Validation);
            assert_eq!(error.message(), "Invalid uninstall impact fingerprint");
        }
    }

    #[tokio::test]
    #[allow(clippy::unwrap_used, reason = "test assertions")]
    async fn lifecycle_failure_responses_preserve_commit_truth() {
        let result = lifecycle_outcome_response(
            LifecycleOperation::Upgrade,
            Some(PublicationStage::RuntimeGrants),
        );
        assert_eq!(result.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert!(result.extensions().get::<LifecycleResponse>().is_some());
        let body: serde_json::Value =
            serde_json::from_slice(&to_bytes(result.into_body(), 4096).await.unwrap()).unwrap();
        assert_eq!(body["name"], "erp.lifecycle.committed");
        assert_eq!(body["outcome"], "committed");
        assert_eq!(body["stage"], "runtime_grants");
        assert_eq!(
            body["message"],
            "Upgrade committed but runtime grant publication is incomplete; operator recovery is required"
        );
        let result = lifecycle_outcome_response(LifecycleOperation::Uninstall, None);
        assert_eq!(result.status(), StatusCode::SERVICE_UNAVAILABLE);
        let body: serde_json::Value =
            serde_json::from_slice(&to_bytes(result.into_body(), 4096).await.unwrap()).unwrap();
        assert_eq!(body["outcome"], "unknown");
        assert_eq!(body["stage"], serde_json::Value::Null);
        assert_eq!(
            body["message"],
            "Uninstall outcome is unknown; reconcile addon state before any retry"
        );
    }

    #[tokio::test]
    #[allow(clippy::unwrap_used, reason = "test assertions")]
    async fn lifecycle_install_failure_bridge_requires_a_typed_immediate_source() {
        let ordinary = erp_fields::Error::validation("lifecycle commit outcome is unknown");
        assert!(install_outcome_response(&ordinary).is_none());
        let refusal = erp_fields::Error::validation("ordinary refusal").with_source(
            LifecycleFailure::Refused(erp_fields::Error::validation("not installed")),
        );
        assert!(install_outcome_response(&refusal).is_none());
        let unknown = erp_fields::Error::programming("private outer diagnostic").with_source(
            LifecycleFailure::Unknown(erp_fields::Error::programming("private source")),
        );
        let response = install_outcome_response(&unknown).unwrap();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        let body: serde_json::Value =
            serde_json::from_slice(&to_bytes(response.into_body(), 4096).await.unwrap()).unwrap();
        assert_eq!(
            body,
            serde_json::json!({"name":"erp.lifecycle.unknown","message":"Install outcome is unknown; reconcile addon state before any retry","outcome":"unknown","stage":null})
        );
        let committed = erp_fields::Error::programming("private outer diagnostic").with_source(
            LifecycleFailure::Committed {
                stage: PublicationStage::PostCommit,
                error: erp_fields::Error::programming("private source"),
            },
        );
        let response = install_outcome_response(&committed).unwrap();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        let body: serde_json::Value =
            serde_json::from_slice(&to_bytes(response.into_body(), 4096).await.unwrap()).unwrap();
        assert_eq!(
            body,
            serde_json::json!({"name":"erp.lifecycle.committed","message":"Install committed but post-commit completion is incomplete; operator recovery is required","outcome":"committed","stage":"post_commit"})
        );
    }

    #[tokio::test]
    #[allow(clippy::unwrap_used, reason = "test assertions")]
    async fn lifecycle_deadlines_keep_recovery_json_even_for_html_acceptance() {
        for operation in [
            LifecycleOperation::Install,
            LifecycleOperation::Upgrade,
            LifecycleOperation::Uninstall,
        ] {
            let result = finish_response(lifecycle_deadline_response(Some(operation)), true);
            assert_eq!(result.status(), StatusCode::SERVICE_UNAVAILABLE);
            assert_eq!(result.headers()[header::CONTENT_TYPE], "application/json");
            assert!(result.extensions().get::<LifecycleResponse>().is_some());
            let body: serde_json::Value =
                serde_json::from_slice(&to_bytes(result.into_body(), 4096).await.unwrap()).unwrap();
            assert_eq!(body["name"], "erp.lifecycle.unknown");
            assert_eq!(body["outcome"], "unknown");
            assert_eq!(body["stage"], serde_json::Value::Null);
            assert_eq!(
                body["message"],
                format!(
                    "{} outcome is unknown; reconcile addon state before any retry",
                    operation.name()
                )
            );
        }
        for (stage, key, description) in [
            (
                PublicationStage::PostCommit,
                "post_commit",
                "post-commit completion",
            ),
            (
                PublicationStage::RuntimeGrants,
                "runtime_grants",
                "runtime grant publication",
            ),
            (
                PublicationStage::Recomposition,
                "recomposition",
                "runtime recomposition",
            ),
            (PublicationStage::Assets, "assets", "asset publication"),
            (
                PublicationStage::Publication,
                "publication",
                "service publication",
            ),
        ] {
            let result = finish_response(
                lifecycle_outcome_response(LifecycleOperation::Upgrade, Some(stage)),
                true,
            );
            assert_eq!(result.headers()[header::CONTENT_TYPE], "application/json");
            assert!(result.extensions().get::<LifecycleResponse>().is_some());
            let body: serde_json::Value =
                serde_json::from_slice(&to_bytes(result.into_body(), 4096).await.unwrap()).unwrap();
            assert_eq!(body["stage"], key);
            assert_eq!(
                body["message"],
                format!(
                    "Upgrade committed but {description} is incomplete; operator recovery is required"
                )
            );
        }
    }

    #[test]
    fn bearer_admission_overload_keeps_the_retryable_transport_status() {
        let error = erp_fields::Error::new(
            erp_fields::ErrorKind::ResourceExhausted,
            "admission wait expired",
        )
        .with_condition(erp_fields::Condition::Overloaded);
        let failure = bearer_failure(error);
        assert_eq!(failure.status(), 503);
        assert_eq!(failure.name(), "erp.overloaded");
        assert_eq!(failure.retry_after(), Some(1));
        assert_eq!(failure.message(), "Server busy; retry after 1 seconds.");
    }

    #[tokio::test]
    #[allow(clippy::unwrap_used, reason = "test assertions")]
    async fn navigation_errors_are_safe_documents_without_changing_protocol_failures() {
        let mut raw = response(
            StatusCode::SERVICE_UNAVAILABLE,
            "private diagnostic <script>bad()</script>",
        );
        raw.headers_mut()
            .insert(header::RETRY_AFTER, HeaderValue::from_static("5"));
        let page = finish_response(raw, true);
        assert_eq!(page.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(page.headers()[header::RETRY_AFTER], "5");
        assert_eq!(page.headers()[header::CACHE_CONTROL], "no-store");
        assert!(
            page.headers()[header::CONTENT_TYPE]
                .to_str()
                .unwrap()
                .starts_with("text/html")
        );
        let body = to_bytes(page.into_body(), 16_384).await.unwrap();
        let html = std::str::from_utf8(&body).unwrap();
        assert!(html.contains("Temporarily unavailable"));
        assert!(!html.contains("private diagnostic"));
        assert!(!html.contains("<script>"));
        let api = finish_response(
            json_response(
                StatusCode::UNAUTHORIZED,
                &serde_json::json!({"error":"expired"}),
            )
            .unwrap(),
            false,
        );
        assert_eq!(api.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(api.headers()[header::CONTENT_TYPE], "application/json");
        assert_eq!(
            to_bytes(api.into_body(), 4096).await.unwrap().as_ref(),
            b"{\"error\":\"expired\"}"
        );
    }

    /// it serves, never `'unsafe-inline'`, and shows its own server-generated v4 reference)
    #[tokio::test]
    #[allow(clippy::unwrap_used, reason = "test assertions")]
    async fn error_pages_hash_their_style_and_show_a_fresh_reference() {
        use base64::Engine as _;
        use sha2::Digest as _;
        let mut references = Vec::new();
        for _ in 0..2 {
            let page = finish_response(response(StatusCode::FORBIDDEN, "private detail"), true);
            assert_eq!(page.status(), StatusCode::FORBIDDEN);
            let stored = page
                .extensions()
                .get::<ErrorReference>()
                .unwrap()
                .0
                .as_str()
                .to_owned();
            let csp = page.headers()[header::CONTENT_SECURITY_POLICY]
                .to_str()
                .unwrap()
                .to_owned();
            let body = to_bytes(page.into_body(), 65_536).await.unwrap();
            let html = String::from_utf8(body.to_vec()).unwrap();
            assert_eq!(html.matches("<style>").count(), 1, "{html}");
            assert!(!html.contains("style="), "no style attribute: {html}");
            let style = html
                .split("<style>")
                .nth(1)
                .unwrap()
                .split("</style>")
                .next()
                .unwrap();
            assert!(!style.is_empty());
            let hash = base64::engine::general_purpose::STANDARD
                .encode(sha2::Sha256::digest(style.as_bytes()));
            assert_eq!(
                csp,
                format!(
                    "default-src 'none'; style-src 'sha256-{hash}'; img-src 'self'; base-uri 'none'; frame-ancestors 'none'; form-action 'self'"
                )
            );
            assert!(!csp.contains("unsafe-inline"), "{csp}");
            let shown = html
                .split("Reference: <code>")
                .nth(1)
                .unwrap()
                .split("</code>")
                .next()
                .unwrap();
            erp_rpc::CorrelationId::parse(shown).unwrap();
            assert_eq!(shown.as_bytes()[14], b'4', "a v4 id: {shown}");
            assert_eq!(shown, stored);
            assert!(!html.contains("private detail"));
            references.push(stored);
        }
        assert_ne!(
            references[0], references[1],
            "each page has its own reference"
        );
        // A protocol response is not rendered and carries no reference.
        let api = finish_response(response(StatusCode::FORBIDDEN, "denied"), false);
        assert!(api.extensions().get::<ErrorReference>().is_none());
        assert!(!api.headers().contains_key(header::CONTENT_SECURITY_POLICY));
    }

    /// Threat model §13: the final response keeps `no-referrer`, defaults to `same-origin`, and
    /// replaces a weaker value set by a handler.
    #[test]
    #[allow(clippy::unwrap_used, reason = "test assertions")]
    fn final_referrer_policy_is_never_weaker_than_same_origin() {
        let plain = finish_response(response(StatusCode::OK, "ok"), false);
        assert_eq!(plain.headers()[header::REFERRER_POLICY], "same-origin");

        let local = finish_response(redirect("/erp/login").unwrap(), false);
        assert_eq!(local.headers()[header::REFERRER_POLICY], "no-referrer");

        for weaker in [
            "unsafe-url",
            "origin-when-cross-origin",
            "no-referrer, unsafe-url",
        ] {
            let mut raw = response(StatusCode::OK, "ok");
            raw.headers_mut()
                .insert(header::REFERRER_POLICY, HeaderValue::from_static(weaker));
            let finished = finish_response(raw, false);
            let values: Vec<_> = finished
                .headers()
                .get_all(header::REFERRER_POLICY)
                .iter()
                .map(HeaderValue::as_bytes)
                .collect();
            assert_eq!(
                values,
                [b"same-origin".as_slice()],
                "handler value {weaker}"
            );
        }

        // A second header after the strict one would win in the browser; both are replaced.
        let mut raw = redirect("/erp/login").unwrap();
        raw.headers_mut().append(
            header::REFERRER_POLICY,
            HeaderValue::from_static("unsafe-url"),
        );
        let finished = finish_response(raw, false);
        let values: Vec<_> = finished
            .headers()
            .get_all(header::REFERRER_POLICY)
            .iter()
            .map(HeaderValue::as_bytes)
            .collect();
        assert_eq!(values, [b"same-origin".as_slice()]);
    }

    /// bad cookie or token keeps the typed 400)
    #[tokio::test]
    #[allow(clippy::unwrap_used, reason = "test assertions")]
    async fn csrf_key_failure_is_a_server_error() {
        let refused = csrf_refusal(PrefsError::Key);
        assert_eq!(refused.status(), StatusCode::INTERNAL_SERVER_ERROR);
        let body = to_bytes(refused.into_body(), 4096).await.unwrap();
        assert_eq!(body.as_ref(), b"Internal server error");
        let text = std::str::from_utf8(&body).unwrap().to_ascii_lowercase();
        assert!(!text.contains("csrf") && !text.contains("key"), "{text}");
        for client in [PrefsError::Cookie, PrefsError::Csrf] {
            let refused = csrf_refusal(client);
            assert_eq!(refused.status(), StatusCode::BAD_REQUEST);
            assert_eq!(
                to_bytes(refused.into_body(), 4096).await.unwrap().as_ref(),
                b"Session expired (invalid CSRF token)"
            );
        }
    }

    /// busy answer keeps exactly its cleanup headers; no other cookie is carried over)
    #[test]
    #[allow(clippy::unwrap_used, reason = "test assertions")]
    fn busy_keeps_logout_cleanup_headers() {
        let expired =
            HeaderValue::from_static("session_id=; Path=/; HttpOnly; SameSite=Lax; Max-Age=0");
        let mut failed = internal();
        failed.headers_mut().append(
            header::SET_COOKIE,
            HeaderValue::from_static("other=1; Path=/"),
        );
        LogoutCleanup::new(expired.clone()).apply(&mut failed);
        let replaced = superseded_failure(&failed);
        assert_eq!(replaced.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(replaced.headers()[header::RETRY_AFTER], "5");
        let cookies: Vec<_> = replaced
            .headers()
            .get_all(header::SET_COOKIE)
            .iter()
            .collect();
        assert_eq!(cookies, [&expired]);
        assert_eq!(
            replaced.headers()["clear-site-data"],
            LOGOUT_CLEAR_SITE_DATA
        );
        assert!(replaced.extensions().get::<LogoutCleanup>().is_some());

        // An unmarked failure is replaced by the plain busy answer, its cookies dropped.
        let mut other = internal();
        other.headers_mut().append(
            header::SET_COOKIE,
            HeaderValue::from_static("other=1; Path=/"),
        );
        let replaced = superseded_failure(&other);
        assert_eq!(replaced.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert!(!replaced.headers().contains_key(header::SET_COOKIE));
        assert!(!replaced.headers().contains_key("clear-site-data"));
    }

    /// the hook's text)
    #[tokio::test]
    #[allow(clippy::unwrap_used, reason = "test assertions")]
    async fn a_failed_cooldown_hook_is_a_plain_internal_error() {
        let error =
            erp_fields::Error::programming("the login cooldown policy could not be evaluated");
        let page = auth_error(error);
        assert_eq!(page.status(), StatusCode::INTERNAL_SERVER_ERROR);
        let body = to_bytes(page.into_body(), 4096).await.unwrap();
        assert_eq!(body.as_ref(), b"Internal server error");
    }

    #[tokio::test]
    #[allow(clippy::unwrap_used, reason = "test assertions")]
    async fn brand_images_are_the_compiled_svgs() {
        for (path, svg) in [
            ("/favicon.ico", BRAND_ICON),
            ("/erp/brand/icon.svg", BRAND_ICON),
            ("/erp/brand/logo.svg", BRAND_LOGO),
            ("/erp/brand/logo-dark.svg", BRAND_LOGO_DARK),
        ] {
            let response = brand_route(&Method::GET, path).unwrap();
            assert_eq!(response.status(), StatusCode::OK, "{path}");
            assert_eq!(
                response.headers()[header::CONTENT_TYPE],
                "image/svg+xml",
                "{path}"
            );
            assert!(
                response.headers()[header::CONTENT_SECURITY_POLICY]
                    .to_str()
                    .unwrap()
                    .starts_with("default-src 'none'"),
                "{path}"
            );
            let body = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
            assert_eq!(body.as_ref(), svg, "{path}");
            assert!(body.starts_with(b"<svg"), "{path}");
        }
        assert!(brand_route(&Method::POST, "/favicon.ico").is_none());
        assert!(brand_route(&Method::GET, "/erp/brand/other.svg").is_none());
        // The error page shows the logo from the same origin, the only image its CSP allows.
        assert!(ERROR_PAGE.contains(r#"<header><img src="/erp/brand/logo.svg""#));
    }
}
