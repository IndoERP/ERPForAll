//! Shared resident ownership, byte admission and reclaimable LRU entries.
use super::negotiation::{self, Encoding};
use super::{AssetSource, PublicationError, PublicationErrorKind, hex, refused};
use axum::{
    body::Body,
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::Response,
};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, VecDeque},
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicU64, Ordering},
    },
};

// Conservative reservation for fixed resident bookkeeping and bounded key/header allocations.
// Admission accounting is not an RSS measurement.
const METADATA_BYTES: u64 = 8192;
#[derive(Debug)]
struct Budget {
    limit: u64,
    used: AtomicU64,
}
#[derive(Debug)]
pub(super) struct Reservation {
    budget: Arc<Budget>,
    bytes: u64,
}
impl Drop for Reservation {
    fn drop(&mut self) {
        self.budget.used.fetch_sub(self.bytes, Ordering::AcqRel);
    }
}
#[derive(Debug, Default)]
struct Cache {
    entries: BTreeMap<String, Arc<ResidentAsset>>,
    order: VecDeque<String>,
}
/// Explicit per-process resident allowance shared by tenant generations and release assets.
/// Snapshots, HTTP bodies and detached response header clones retain their reservation until
/// the final owner of shared resident storage is dropped.
#[derive(Debug)]
#[non_exhaustive]
pub struct AssetStore {
    budget: Arc<Budget>,
    cache: Mutex<Cache>,
}
impl AssetStore {
    /// Create an empty store. The host supplies its validated assets.memory_bytes setting.
    ///
    /// # Examples
    /// ```
    /// let store = erp_server::assets::AssetStore::new(16 * 1024 * 1024)?;
    /// assert_eq!(store.used_bytes(), 0);
    /// # Ok::<(), erp_server::assets::PublicationError>(())
    /// ```
    /// # Errors
    /// Zero is refused; it never disables admission.
    pub fn new(limit: u64) -> Result<Arc<Self>, PublicationError> {
        if limit == 0 {
            return Err(refused(
                PublicationErrorKind::Limit,
                "assets.memory_bytes must be positive",
            ));
        }
        Ok(Arc::new(Self {
            budget: Arc::new(Budget {
                limit,
                used: AtomicU64::new(0),
            }),
            cache: Mutex::new(Cache::default()),
        }))
    }
    /// Configured logical resident allowance in bytes.
    pub fn limit(&self) -> u64 {
        self.budget.limit
    }
    /// Current charged residency, including externally owned entries after cache eviction.
    pub fn used_bytes(&self) -> u64 {
        self.budget.used.load(Ordering::Acquire)
    }
    /// Discard cache-only entries. Active snapshots and bodies keep their entries resident;
    /// detached response header clones retain their charge even after entry eviction.
    /// # Errors
    /// A poisoned accounting lock.
    pub fn clear_idle(&self) -> Result<(), PublicationError> {
        let mut cache = self.cache.lock().map_err(|_| poisoned())?;
        cache
            .entries
            .retain(|_, value| Arc::strong_count(value) > 1);
        let keys: std::collections::BTreeSet<_> = cache.entries.keys().cloned().collect();
        cache.order.retain(|key| keys.contains(key));
        Ok(())
    }
    pub(super) fn touch(&self, key: &str) -> Result<(), PublicationError> {
        let mut cache = self.cache.lock().map_err(|_| poisoned())?;
        if !cache.entries.contains_key(key) {
            return Err(refused(
                PublicationErrorKind::Cache,
                "resident content missing from cache index",
            ));
        }
        cache.order.retain(|old| old != key);
        cache.order.push_back(key.to_owned());
        Ok(())
    }
    // Batch preflight guarantees a failed generation cannot leave a partially admitted candidate.
    pub(super) fn admit(
        &self,
        sources: BTreeMap<String, AssetSource>,
        metadata_bytes: u64,
    ) -> Result<(BTreeMap<String, Arc<ResidentAsset>>, Reservation), PublicationError> {
        let mut prepared = Vec::with_capacity(sources.len());
        for (name, source) in sources {
            prepared.push((name, Prepared::new(source)?));
        }
        let mut cache = self.cache.lock().map_err(|_| poisoned())?;
        let mut missing = BTreeMap::new();
        for (_, entry) in &prepared {
            if let Some(old) = cache.entries.get(&entry.key) {
                if old.source.bytes != entry.source.bytes
                    || old.source.gzip != entry.source.gzip
                    || old.source.brotli != entry.source.brotli
                {
                    return Err(refused(
                        PublicationErrorKind::Cache,
                        "content digest conflicts with resident bytes",
                    ));
                }
            } else {
                missing.insert(entry.key.clone(), entry.cost);
            }
        }
        let needed = missing.values().try_fold(metadata_bytes, |total, cost| {
            total.checked_add(*cost).ok_or_else(limit)
        })?;
        if needed > self.budget.limit {
            return Err(limit());
        }
        let mut attempts = cache.order.len();
        while self.used_bytes() > self.budget.limit - needed {
            if attempts == 0 {
                return Err(limit());
            }
            attempts -= 1;
            let key = cache.order.pop_front().ok_or_else(|| {
                refused(
                    PublicationErrorKind::Cache,
                    "resident LRU accounting inconsistent",
                )
            })?;
            let reclaimable = cache
                .entries
                .get(&key)
                .is_some_and(|entry| Arc::strong_count(entry) == 1)
                && !prepared.iter().any(|(_, candidate)| candidate.key == key);
            if reclaimable {
                cache.entries.remove(&key);
            } else {
                cache.order.push_back(key);
            }
        }
        self.budget.used.fetch_add(metadata_bytes, Ordering::AcqRel);
        let metadata = Reservation {
            budget: Arc::clone(&self.budget),
            bytes: metadata_bytes,
        };
        let mut result = BTreeMap::new();
        // Activate and stage every new entry before modifying the cache. An internal activation
        // failure drops all staged owners and metadata, retaining no partial candidate.
        let mut staged: BTreeMap<String, Arc<ResidentAsset>> = BTreeMap::new();
        for (name, prepared) in prepared {
            let asset = if let Some(old) = cache.entries.get(&prepared.key) {
                Arc::clone(old)
            } else if let Some(old) = staged.get(&prepared.key) {
                Arc::clone(old)
            } else {
                self.budget.used.fetch_add(prepared.cost, Ordering::AcqRel);
                let reservation = Reservation {
                    budget: Arc::clone(&self.budget),
                    bytes: prepared.cost,
                };
                // Prepared owns a fresh private pending cell. Treat any violated activation
                // invariant as an accounting error; the rejected reservation refunds its charge.
                if prepared.charge.set(reservation).is_err() {
                    return Err(refused(
                        PublicationErrorKind::Cache,
                        "resident header reservation already activated",
                    ));
                }
                let asset = Arc::new(ResidentAsset {
                    key: prepared.key.clone(),
                    source: prepared.source,
                    headers: prepared.headers,
                    _reservation: prepared.charge,
                });
                staged.insert(prepared.key, Arc::clone(&asset));
                asset
            };
            result.insert(name, asset);
        }
        for (key, asset) in staged {
            cache.entries.insert(key, asset);
        }
        for asset in result.values() {
            cache.order.retain(|old| old != &asset.key);
            cache.order.push_back(asset.key.clone());
        }
        Ok((result, metadata))
    }
}
fn poisoned() -> PublicationError {
    refused(
        PublicationErrorKind::Cache,
        "resident asset store lock poisoned",
    )
}
fn limit() -> PublicationError {
    refused(
        PublicationErrorKind::Limit,
        "assets.memory_bytes admission limit exceeded",
    )
}
struct Prepared {
    key: String,
    source: AssetSource,
    headers: [HeaderMap; 3],
    cost: u64,
    // Headers may be prepared for an existing cache hit or a refused batch. Their token is
    // activated only when this candidate becomes a newly charged resident entry.
    charge: Arc<OnceLock<Reservation>>,
}
impl Prepared {
    fn new(source: AssetSource) -> Result<Self, PublicationError> {
        let forms = [
            Some(source.bytes.as_ref()),
            source.gzip.as_deref(),
            source.brotli.as_deref(),
        ];
        let mut cost = METADATA_BYTES;
        let mut key = source.path.clone();
        let mut headers = [HeaderMap::new(), HeaderMap::new(), HeaderMap::new()];
        let charge = Arc::new(OnceLock::new());
        for (index, form) in forms.into_iter().enumerate() {
            let Some(bytes) = form else {
                key.push_str(":none");
                continue;
            };
            cost = cost.checked_add(bytes.len() as u64).ok_or_else(limit)?;
            let digest = hex(&Sha256::digest(bytes));
            key.push(':');
            key.push_str(&digest);
            let values = &mut headers[index];
            values.insert(
                header::CONTENT_TYPE,
                HeaderValue::from_str(&source.mime).map_err(|_| {
                    refused(PublicationErrorKind::Header, "invalid asset media header")
                })?,
            );
            values.insert(
                header::CACHE_CONTROL,
                HeaderValue::from_static("private, max-age=31536000, immutable"),
            );
            values.insert(
                header::ETAG,
                HeaderValue::from_str(&format!("\"{digest}\""))
                    .map_err(|_| refused(PublicationErrorKind::Header, "invalid asset ETag"))?,
            );
            values.insert(header::VARY, HeaderValue::from_static("Accept-Encoding"));
            values.insert(
                header::X_CONTENT_TYPE_OPTIONS,
                HeaderValue::from_static("nosniff"),
            );
            if index == 1 {
                values.insert(header::CONTENT_ENCODING, HeaderValue::from_static("gzip"));
            }
            if index == 2 {
                values.insert(header::CONTENT_ENCODING, HeaderValue::from_static("br"));
            }
            if source.mime == "image/svg+xml" {
                values.insert(
                    header::CONTENT_SECURITY_POLICY,
                    HeaderValue::from_static(
                        "default-src 'none'; style-src 'unsafe-inline'; sandbox",
                    ),
                );
            }
        }
        // Each cloned HeaderValue shares this owner rather than retaining an uncharged alias
        // of a resident header allocation. Conversion is fallible only during preparation,
        // before cache changes or budget activation; responses clone the admitted values.
        for values in &mut headers {
            for value in values.values_mut() {
                let sensitive = value.is_sensitive();
                let owner = HeaderOwner {
                    value: value.clone(),
                    _charge: Arc::clone(&charge),
                };
                let mut leased = HeaderValue::from_maybe_shared(bytes::Bytes::from_owner(owner))
                    .map_err(|_| {
                        refused(
                            PublicationErrorKind::Header,
                            "invalid admitted asset header",
                        )
                    })?;
                leased.set_sensitive(sensitive);
                *value = leased;
            }
        }
        Ok(Self {
            key,
            source,
            headers,
            cost,
            charge,
        })
    }
}
struct HeaderOwner {
    // Release the original header byte allocation before the shared reservation owner.
    value: HeaderValue,
    _charge: Arc<OnceLock<Reservation>>,
}
impl AsRef<[u8]> for HeaderOwner {
    fn as_ref(&self) -> &[u8] {
        self.value.as_bytes()
    }
}
#[derive(Debug)]
pub(super) struct ResidentAsset {
    pub(super) key: String,
    source: AssetSource,
    headers: [HeaderMap; 3],
    // No cycle: the cell owns only Budget, while headers and this final field own the cell.
    _reservation: Arc<OnceLock<Reservation>>,
}
impl ResidentAsset {
    pub(super) fn response(
        self: &Arc<Self>,
        validator: Option<&str>,
        accept: Option<&str>,
    ) -> Result<Response, PublicationError> {
        let Some(encoding) = negotiation::select(
            accept,
            [
                true,
                self.source.gzip.is_some(),
                self.source.brotli.is_some(),
            ],
        )?
        else {
            let mut result = Response::new(Body::empty());
            *result.status_mut() = StatusCode::NOT_ACCEPTABLE;
            result
                .headers_mut()
                .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
            result
                .headers_mut()
                .insert(header::VARY, HeaderValue::from_static("Accept-Encoding"));
            result.headers_mut().insert(
                header::X_CONTENT_TYPE_OPTIONS,
                HeaderValue::from_static("nosniff"),
            );
            return Ok(result);
        };
        let headers = &self.headers[encoding.index()];
        let tag = headers
            .get(header::ETAG)
            .ok_or_else(|| refused(PublicationErrorKind::Header, "missing admitted asset ETag"))?
            .to_str()
            .map_err(|_| refused(PublicationErrorKind::Header, "invalid admitted asset ETag"))?;
        let matched = validator.is_some_and(|value| {
            value.split(',').any(|item| {
                let item = item.trim();
                item == "*" || item == tag || item.strip_prefix("W/") == Some(tag)
            })
        });
        let mut response = Response::new(if matched {
            Body::empty()
        } else {
            let bytes = match encoding {
                Encoding::Identity => Arc::clone(&self.source.bytes),
                Encoding::Gzip => Arc::clone(self.source.gzip.as_ref().ok_or_else(|| {
                    refused(
                        PublicationErrorKind::Representation,
                        "selected gzip form is missing",
                    )
                })?),
                Encoding::Brotli => Arc::clone(self.source.brotli.as_ref().ok_or_else(|| {
                    refused(
                        PublicationErrorKind::Representation,
                        "selected Brotli form is missing",
                    )
                })?),
            };
            Body::from(bytes::Bytes::from_owner(RepresentationBody {
                _asset: Arc::clone(self),
                bytes,
            }))
        });
        if matched {
            *response.status_mut() = StatusCode::NOT_MODIFIED;
        }
        *response.headers_mut() = headers.clone();
        Ok(response)
    }
}
struct RepresentationBody {
    bytes: Arc<[u8]>,
    // Keep the reservation alive until the final body-owned byte reference has dropped.
    _asset: Arc<ResidentAsset>,
}
impl AsRef<[u8]> for RepresentationBody {
    fn as_ref(&self) -> &[u8] {
        &self.bytes
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[allow(clippy::unwrap_used, reason = "tests")]
    fn source(value: &[u8]) -> BTreeMap<String, AssetSource> {
        let hash = hex(&Sha256::digest(value));
        [(
            format!("{hash}.js"),
            AssetSource::new(
                &format!("{hash}.js"),
                "text/javascript; charset=utf-8",
                value.to_vec(),
            )
            .unwrap(),
        )]
        .into_iter()
        .collect()
    }
    #[test]
    #[allow(clippy::unwrap_used, reason = "tests")]
    fn pressure_evicts_the_least_recently_used_unpinned_content() {
        let store = AssetStore::new(2 * (METADATA_BYTES + 4)).unwrap();
        let (a, _a_metadata) = store.admit(source(b"aaaa"), 0).unwrap();
        let (b, _b_metadata) = store.admit(source(b"bbbb"), 0).unwrap();
        let a_key = a.values().next().unwrap().key.clone();
        let b_key = b.values().next().unwrap().key.clone();
        store.touch(&a_key).unwrap();
        drop(a);
        drop(b);
        let (_c, _c_metadata) = store.admit(source(b"cccc"), 0).unwrap();
        let cache = store.cache.lock().unwrap();
        assert!(cache.entries.contains_key(&a_key));
        assert!(!cache.entries.contains_key(&b_key));
    }
}
