//! Configured server-to-server deliveries. A signed provider can submit data to one
//! native write target. Its session-independent ERP credential is resolved on every attempt.
use super::{
    BearerCredential, Body, Duration, HeaderValue, HttpResult, IpAddr, KeyScope, Method, Response,
    StatusCode, WebApp, header, one, response,
};
use crate::config::{ConfigError, read_secret};
use axum::http::request::Parts;
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use erp_rpc::{CorrelationId, IdempotencyKey, Route, RpcCall, RpcService};
use erp_security::auth::{Auth, CredentialOptions};
use hmac::{Hmac, KeyInit, Mac};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};
use zeroize::Zeroizing;

const MAX_BINDINGS: usize = 32;
const MAX_BODY_BYTES: usize = 64 * 1024;
const CLOCK_WINDOW_SECS: u64 = 300;
const BODY_DEADLINE: Duration = Duration::from_secs(5);
const CALLBACK_DEADLINE: Duration = Duration::from_secs(15);

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawCallback {
    signing_key_file: PathBuf,
    api_key_file: PathBuf,
    model: String,
    method: String,
}

pub(crate) struct Callbacks(BTreeMap<String, Binding>);

struct Binding {
    signing_key: Zeroizing<[u8; 32]>,
    api_key: Zeroizing<String>,
    route: Route,
}

impl Callbacks {
    pub(crate) fn load(
        raw: BTreeMap<String, RawCallback>,
        resolve: impl Fn(&Path) -> PathBuf,
    ) -> Result<Self, ConfigError> {
        if raw.len() > MAX_BINDINGS {
            return Err(ConfigError::Callbacks);
        }
        let mut bindings = BTreeMap::new();
        for (name, files) in raw {
            if name.is_empty()
                || name.len() > 63
                || !name
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
            {
                return Err(ConfigError::Callbacks);
            }
            let route =
                Route::parse_json2(&format!("/erp/json/2/{}/{}", files.model, files.method))
                    .map_err(|_| ConfigError::Callbacks)?;
            let raw_key = read_secret(&resolve(&files.signing_key_file))
                .map_err(|_| ConfigError::Callbacks)?;
            let mut signing_key = Zeroizing::new([0; 32]);
            let written = URL_SAFE_NO_PAD
                .decode_slice(raw_key.as_bytes(), &mut *signing_key)
                .map_err(|_| ConfigError::Callbacks)?;
            if written != signing_key.len() {
                return Err(ConfigError::Callbacks);
            }
            let api_key =
                read_secret(&resolve(&files.api_key_file)).map_err(|_| ConfigError::Callbacks)?;
            if !api_key.bytes().all(|byte| byte.is_ascii_graphic()) {
                return Err(ConfigError::Callbacks);
            }
            bindings.insert(
                name,
                Binding {
                    signing_key,
                    api_key,
                    route,
                },
            );
        }
        Ok(Self(bindings))
    }
}

fn single_header<'a>(parts: &'a Parts, name: &'static str) -> Option<&'a str> {
    one(&parts.headers, name).ok().flatten()
}

fn refused() -> Response {
    response(StatusCode::UNAUTHORIZED, "Callback authentication failed.")
}

fn unavailable() -> Response {
    response(
        StatusCode::SERVICE_UNAVAILABLE,
        "Callback temporarily unavailable.",
    )
}

fn is_fresh(timestamp: &str, now: u64) -> bool {
    !timestamp.is_empty()
        && timestamp.len() <= 16
        && timestamp.bytes().all(|byte| byte.is_ascii_digit())
        && timestamp
            .parse::<u64>()
            .is_ok_and(|seconds| now.abs_diff(seconds) <= CLOCK_WINDOW_SECS)
}

struct SignedDelivery<'a> {
    origin: &'a str,
    database: &'a str,
    name: &'a str,
    timestamp: &'a str,
    event: &'a IdempotencyKey,
}

impl SignedDelivery<'_> {
    fn verify(&self, binding: &Binding, body: &[u8], signature: &str) -> bool {
        let Some(encoded) = signature.strip_prefix("v1=") else {
            return false;
        };
        // A SHA-256 MAC encodes as exactly 43 unpadded base64url bytes.
        if encoded.len() != 43 {
            return false;
        }
        let mut tag = [0; 32];
        if URL_SAFE_NO_PAD.decode_slice(encoded.as_bytes(), &mut tag) != Ok(tag.len()) {
            return false;
        }
        let Ok(mut mac) = Hmac::<Sha256>::new_from_slice(&*binding.signing_key) else {
            return false;
        };
        mac.update(b"erp-callback-v1\n");
        for value in [
            self.origin,
            self.database,
            self.name,
            binding.route.model(),
            binding.route.method(),
            self.timestamp,
            self.event.as_str(),
        ] {
            mac.update(value.as_bytes());
            mac.update(b"\n");
        }
        mac.update(body);
        mac.verify_slice(&tag).is_ok()
    }

    fn idempotency_key(&self) -> Result<IdempotencyKey, erp_rpc::RpcFailure> {
        // Independent of the signing key and delivery time, including across node restarts.
        // Scope by binding, so a provider event can be used independently on another route.
        let mut digest = Sha256::new();
        for value in [
            b"erp-callback-id:v1".as_slice(),
            self.name.as_bytes(),
            self.event.as_str().as_bytes(),
        ] {
            digest.update(value);
            digest.update(b"\n");
        }
        let mut bytes = [0; 16];
        bytes.copy_from_slice(&digest.finalize()[..16]);
        bytes[6] = (bytes[6] & 0x0f) | 0x80;
        bytes[8] = (bytes[8] & 0x3f) | 0x80;
        IdempotencyKey::parse(&erp_fields::Uuid::from_bytes(bytes).to_string())
    }
}

impl WebApp {
    pub(super) async fn provider_callback(
        &self,
        name: &str,
        parts: &Parts,
        body: Body,
        auth: Arc<Auth>,
        rpc: Option<Arc<RpcService>>,
        client: IpAddr,
    ) -> HttpResult<Response> {
        let binding = self
            .config
            .callbacks()
            .0
            .get(name)
            .ok_or_else(|| response(StatusCode::NOT_FOUND, "Not found"))?;
        if parts.method != Method::POST {
            let mut result = response(StatusCode::METHOD_NOT_ALLOWED, "Callback requires POST.");
            result
                .headers_mut()
                .insert(header::ALLOW, HeaderValue::from_static("POST"));
            return Err(result);
        }
        if parts.uri.query().is_some()
            || [
                header::AUTHORIZATION,
                header::COOKIE,
                header::ORIGIN,
                header::CONTENT_ENCODING,
            ]
            .iter()
            .any(|name| parts.headers.contains_key(name))
        {
            return Err(response(
                StatusCode::BAD_REQUEST,
                "Invalid callback request.",
            ));
        }
        if !single_header(parts, "content-type")
            .is_some_and(|value| value.eq_ignore_ascii_case("application/json"))
        {
            return Err(response(
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                "Callback requires application/json.",
            ));
        }
        let timestamp = single_header(parts, "x-erp-callback-timestamp").ok_or_else(refused)?;
        let signature = single_header(parts, "x-erp-callback-signature").ok_or_else(refused)?;
        let event = single_header(parts, "idempotency-key").ok_or_else(refused)?;
        let event = IdempotencyKey::parse(event).map_err(|_| refused())?;
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| unavailable())?;
        if !is_fresh(timestamp, now.as_secs()) {
            return Err(refused());
        }
        let bytes = tokio::time::timeout(BODY_DEADLINE, axum::body::to_bytes(body, MAX_BODY_BYTES))
            .await
            .map_err(|_| {
                response(
                    StatusCode::REQUEST_TIMEOUT,
                    "Callback body deadline exceeded.",
                )
            })?
            .map_err(|_| {
                response(
                    StatusCode::PAYLOAD_TOO_LARGE,
                    "Callback body could not be read within its size limit.",
                )
            })?;
        let delivery = SignedDelivery {
            origin: self.config.origin(),
            database: self.config.database(),
            name,
            timestamp,
            event: &event,
        };
        if !delivery.verify(binding, &bytes, signature) {
            return Err(refused());
        }
        let rpc = rpc.ok_or_else(unavailable)?;
        let key = delivery.idempotency_key().map_err(|_| unavailable())?;
        let credential = Arc::new(BearerCredential {
            audience: KeyScope::Rpc,
            auth,
            secret: binding.api_key.clone(),
            session: None,
            client,
            drop_cookie: false.into(),
            last_use: Default::default(),
        });
        let mut tx =
            tokio::time::timeout(CALLBACK_DEADLINE, credential.open(CredentialOptions::new()))
                .await
                .map_err(|_| unavailable())?
                .map_err(|_| unavailable())?;
        let principal = tx.env().principal().clone();
        tx.rollback().await.map_err(|_| unavailable())?;
        let mut call = RpcCall::new(
            binding.route.clone(),
            bytes.to_vec(),
            erp_rpc::Caller::Principal(principal),
        )
        .signed_callback()
        .credential(credential)
        .idempotency_key(key)
        .client_address(client)
        .deadline(CALLBACK_DEADLINE);
        if let Some(id) = parts.extensions.get::<CorrelationId>() {
            call = call.correlation(id.clone());
        }
        let reply = rpc.call(call).await;
        let status = StatusCode::from_u16(reply.status()).map_err(|_| unavailable())?;
        let mut result = match status {
            StatusCode::OK => response(StatusCode::NO_CONTENT, ""),
            StatusCode::CONFLICT => response(
                status,
                "Callback conflicts with an existing delivery or policy.",
            ),
            StatusCode::BAD_REQUEST | StatusCode::UNPROCESSABLE_ENTITY => {
                response(status, "Callback payload was rejected.")
            }
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN | StatusCode::NOT_FOUND => {
                response(StatusCode::FORBIDDEN, "Callback is not permitted.")
            }
            _ => unavailable(),
        };
        if let Some(seconds) = reply.retry_after()
            && let Ok(value) = HeaderValue::from_str(&seconds.to_string())
        {
            result.headers_mut().insert(header::RETRY_AFTER, value);
        }
        Ok(result)
    }
}
