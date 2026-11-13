//! Adopted: the same hash and index.
//! Modified: the KDF runs on the hashing pool, a bounded per-process memo keyed by
//! `SHA-256(key)` skips repeat KDF work, and the memo is never authority. Every request binds
//! the verified row id and stored hash into the opening statement, which re-reads user
//! activity, route audience and expiry, so revocation, rotation, expiry and deactivation are
//! seen by the next request in every process whatever the memo holds.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

use erp_fields::{Error, Result};
use sha2::{Digest, Sha256};
use zeroize::Zeroizing;

use super::password::{Scheme, StoredHash};
use super::pool::PoolBinding;

/// Memo capacity (design note `m3-auth.md` §4.4). Oldest entries leave first.
pub(super) const MEMO_CAPACITY: usize = 10_000;
/// Rows sharing one eight-hex index which may be verified for one request.
const MAX_CANDIDATES: usize = 8;

/// The index-based candidate lookup: active user, exact route audience, unexpired. An address
/// in cooldown gets no rows, so no hashing work is spent on it; the opening statement then
/// reports the cooldown itself.
pub(super) const CANDIDATES_SQL: &str = "SELECT k.id::int8, k.legacy_hash::text \
 FROM res_users_apikeys k JOIN res_users u ON u.id = k.user_id \
 WHERE u.active AND k.legacy_index = $1::text AND k.legacy_hash IS NOT NULL \
   AND k.scope = $2::text \
   AND (k.expiration_date IS NULL OR k.expiration_date >= now() AT TIME ZONE 'utc') \
   AND NOT EXISTS (SELECT FROM erp_login_throttle t WHERE t.kind = 'address' \
        AND t.key_hash = decode($3::text, 'hex') AND t.failures >= $4::int8 \
        AND extract(epoch FROM now() - t.last_failure_at)::float8 < $5::float8) \
 ORDER BY k.id LIMIT 9";

/// A verified row: the opening statement admits only this id with exactly this stored hash.
#[derive(Clone)]
pub(super) struct LegacyMatch {
    pub id: i64,
    pub hash: Arc<str>,
}

struct Entry {
    generation: u64,
    found: LegacyMatch,
}

/// Bounded verification memo. Holds no key text, only its SHA-256.
#[derive(Default)]
pub(super) struct LegacyMemo {
    inner: Mutex<MemoInner>,
}

#[derive(Default)]
struct MemoInner {
    map: HashMap<[u8; 32], Entry>,
    order: VecDeque<[u8; 32]>,
}

fn fingerprint(raw: &str) -> [u8; 32] {
    Sha256::digest(raw.as_bytes()).into()
}

impl LegacyMemo {
    /// The remembered verification, only while the row it names is still a current candidate
    /// with the same stored hash; anything else must be verified again.
    pub(super) fn get(
        &self,
        raw: &str,
        generation: u64,
        rows: &[(i64, Arc<str>)],
    ) -> Result<Option<LegacyMatch>> {
        let inner = self
            .inner
            .lock()
            .map_err(|_| Error::programming("legacy API key memo was poisoned"))?;
        Ok(inner
            .map
            .get(&fingerprint(raw))
            .filter(|entry| entry.generation == generation)
            .filter(|entry| {
                rows.iter()
                    .any(|(id, hash)| *id == entry.found.id && **hash == *entry.found.hash)
            })
            .map(|entry| entry.found.clone()))
    }

    pub(super) fn put(&self, raw: &str, generation: u64, found: LegacyMatch) -> Result<()> {
        let mut inner = self
            .inner
            .lock()
            .map_err(|_| Error::programming("legacy API key memo was poisoned"))?;
        let key = fingerprint(raw);
        if inner.map.insert(key, Entry { generation, found }).is_none() {
            inner.order.push_back(key);
            while inner.order.len() > MEMO_CAPACITY {
                if let Some(oldest) = inner.order.pop_front() {
                    inner.map.remove(&oldest);
                }
            }
        }
        Ok(())
    }
}

/// A loud refusal caused by stored legacy rows (more than [`MAX_CANDIDATES`] on one index, or a
/// corrupt hash when no other candidate matched). The attempt is still counted for the address
/// like a native miss, then [`LoudRefusal::error`] is returned instead of the ordinary refusal.
#[derive(Clone)]
pub(super) struct LoudRefusal(Arc<str>);

impl LoudRefusal {
    /// The `Db` error the caller receives (row id only, no key text).
    pub(super) fn error(&self) -> Error {
        Error::db(self.0.to_string())
    }
}

/// Verify `raw` against each candidate on the hashing pool; the first match wins. The inner
/// `Err` is a [`LoudRefusal`] of the stored rows.
///
/// # Errors
/// Pool errors, and a hash the pool could not run.
pub(super) async fn verify_candidates(
    pool: &PoolBinding,
    raw: &str,
    rows: Vec<(i64, Arc<str>)>,
) -> Result<std::result::Result<Option<LegacyMatch>, LoudRefusal>> {
    if rows.len() > MAX_CANDIDATES {
        return Ok(Err(LoudRefusal(Arc::from(
            "Too many imported API key rows share one index; re-issue native keys",
        ))));
    }
    let mut corrupt = None;
    for (id, hash) in rows {
        // A corrupt row must not lock out a valid key sharing its index: it is logged, the
        // other candidates are still checked, and a refusal then fails loudly instead.
        let Some(parsed) = StoredHash::parse(&hash)
            .ok()
            .filter(|parsed| parsed.scheme() == Scheme::Pbkdf2Sha512)
        else {
            tracing::error!(target: "erp::auth", "invalid stored legacy API key hash on key {id}");
            corrupt.get_or_insert(id);
            continue;
        };
        let secret = Zeroizing::new(raw.as_bytes().to_vec());
        let memory = parsed.memory_kib();
        let ok = pool
            .run(memory, move || parsed.verify(&secret))
            .await?
            .map_err(|_| Error::db(format!("Legacy API key hash of key {id} could not be run")))?;
        if ok {
            return Ok(Ok(Some(LegacyMatch { id, hash })));
        }
    }
    Ok(match corrupt {
        Some(id) => Err(LoudRefusal(Arc::from(format!(
            "Invalid stored legacy API key hash on key {id}"
        )))),
        None => Ok(None),
    })
}
