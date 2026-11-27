// SPDX-License-Identifier: MIT OR Apache-2.0
//! Fixed or rotating verification keys for a single pinned issuer.
use super::*;
use std::sync::Arc;
use std::time::{Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::{Mutex, RwLock};

/// An operator-pinned source of ID token signing keys.
pub enum SigningKeys<'a> {
    /// A bounded, locally loaded JSON Web Key Set.
    File(&'a [u8]),
    /// A pinned HTTPS URL on the issuer's origin and the maximum key lifetime in seconds.
    Remote { url: &'a str, ttl_seconds: u64 },
}

/// Operator-visible state. Contains no key material, request URLs, tokens or transport errors.
#[derive(serde::Serialize)]
pub struct KeyDiagnostics {
    /// Configured source, file or remote.
    pub mode: &'static str,
    /// Number of supported verification keys in the last accepted set.
    pub key_count: usize,
    /// Most recent successful remote fetch.
    pub last_success_unix: Option<i64>,
    /// Most recent remote request.
    pub last_attempt_unix: Option<i64>,
    /// Stable key status code for the operator view.
    pub status: &'static str,
}

pub(super) enum KeyStore {
    File(Arc<[SigningKey]>),
    Remote(Box<RemoteKeys>),
}

const COOLDOWN: Duration = Duration::from_secs(10);

pub(super) struct RemoteKeys {
    url: Url,
    ttl: Duration,
    cache: RwLock<Cache>,
    refresh: Mutex<()>,
}

#[derive(Default)]
struct Cache {
    keys: Arc<[SigningKey]>,
    success: Option<Instant>,
    last_failure: Option<Instant>,
    unknown_refresh: Option<Instant>,
    last_success_unix: Option<i64>,
    last_attempt_unix: Option<i64>,
}

impl Cache {
    fn fresh(&self, ttl: Duration) -> bool {
        self.success.is_some_and(|time| time.elapsed() < ttl)
    }

    /// None means that a network refresh can change the answer.
    fn cached(&self, kid: &str, ttl: Duration) -> Option<Result<Arc<[SigningKey]>>> {
        let fresh = self.fresh(ttl);
        if fresh && self.keys.iter().any(|key| key.id() == kid) {
            return Some(Ok(self.keys.clone()));
        }
        let cooling = |time: Option<Instant>| time.is_some_and(|time| time.elapsed() < COOLDOWN);
        if cooling(self.last_failure) || (fresh && cooling(self.unknown_refresh)) {
            return Some(Err(denied()));
        }
        None
    }
}

fn unix_now() -> Option<i64> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()?
        .as_secs()
        .try_into()
        .ok()
}

impl KeyStore {
    pub(super) fn new(source: &SigningKeys<'_>, issuer: &Url) -> Result<Self> {
        match source {
            SigningKeys::File(bytes) => Ok(Self::File(parse_keys(bytes)?.into())),
            SigningKeys::Remote { url, ttl_seconds } => {
                if !valid_text(url, 2048) {
                    return Err(invalid_config());
                }
                let url = endpoint(url)?;
                if url.scheme() != "https"
                    || url.origin() != issuer.origin()
                    || !(30..=3600).contains(ttl_seconds)
                {
                    return Err(invalid_config());
                }
                Ok(Self::Remote(Box::new(RemoteKeys::new(
                    url,
                    Duration::from_secs(*ttl_seconds),
                ))))
            }
        }
    }

    pub(super) async fn select(
        &self,
        http: &reqwest::Client,
        kid: &str,
    ) -> Result<Arc<[SigningKey]>> {
        match self {
            Self::File(keys) => Ok(keys.clone()),
            Self::Remote(remote) => remote.select(http, kid).await,
        }
    }

    pub(super) async fn diagnostics(&self) -> KeyDiagnostics {
        match self {
            Self::File(keys) => KeyDiagnostics {
                mode: "file",
                key_count: keys.len(),
                last_success_unix: None,
                last_attempt_unix: None,
                status: "ready",
            },
            Self::Remote(remote) => {
                let state = remote.cache.read().await;
                let status = if state.success.is_some() && !state.fresh(remote.ttl) {
                    "expired"
                } else if state.last_failure.is_some() {
                    "refresh_failed"
                } else if state.success.is_some() {
                    "ready"
                } else {
                    "not_loaded"
                };
                KeyDiagnostics {
                    mode: "remote",
                    key_count: state.keys.len(),
                    last_success_unix: state.last_success_unix,
                    last_attempt_unix: state.last_attempt_unix,
                    status,
                }
            }
        }
    }
}

impl RemoteKeys {
    fn new(url: Url, ttl: Duration) -> Self {
        Self {
            url,
            ttl,
            cache: RwLock::new(Cache::default()),
            refresh: Mutex::new(()),
        }
    }

    async fn select(&self, http: &reqwest::Client, kid: &str) -> Result<Arc<[SigningKey]>> {
        if let Some(result) = self.cache.read().await.cached(kid, self.ttl) {
            return result;
        }
        let _refresh = self.refresh.lock().await;
        let unknown_refresh = {
            let mut state = self.cache.write().await;
            if let Some(result) = state.cached(kid, self.ttl) {
                return result;
            }
            state.last_attempt_unix = unix_now();
            state.fresh(self.ttl)
        };
        // Known, unexpired keys remain available to other requests during a refresh.
        let loaded = self.fetch(http).await;
        let mut state = self.cache.write().await;
        if unknown_refresh {
            state.unknown_refresh = Some(Instant::now());
        }
        let keys = match loaded {
            Ok(keys) => keys,
            Err(error) => {
                state.last_failure = Some(Instant::now());
                return Err(error);
            }
        };
        state.last_failure = None;
        state.success = Some(Instant::now());
        state.last_success_unix = unix_now();
        state.keys = keys.into();
        if !state.keys.iter().any(|key| key.id() == kid) {
            state.unknown_refresh = state.success;
            return Err(denied());
        }
        Ok(state.keys.clone())
    }

    async fn fetch(&self, http: &reqwest::Client) -> Result<Vec<SigningKey>> {
        let mut response = http
            .get(self.url.clone())
            .send()
            .await
            .map_err(|_| denied())?;
        if !response.status().is_success()
            || response
                .content_length()
                .is_some_and(|n| n > MAX_RESPONSE as u64)
        {
            return Err(denied());
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|_| denied())? {
            if bytes.len().saturating_add(chunk.len()) > MAX_RESPONSE {
                return Err(denied());
            }
            bytes.extend_from_slice(&chunk);
        }
        parse_keys(&bytes).map_err(|_| denied())
    }
}

#[cfg(test)]
mod tests;
