//! The login throttle (§4.1;/027).
//!
//! Neither is stored in clear:
//! the row key is `HMAC-SHA256(K, kind ‖ 0x00 ‖ value)` under a purpose key `K` of
//! `database.secret` ([`ThrottleHasher`]; security review round 2 S4: a password typed into the
//! login field is no fast unsalted hash in the table, its backups or its replicas, and an IPv4
//! address key is no longer reversible by trying 2^32 values). A cooldown in force refuses before
//! any hash and does not count; a failure counts on both keys in one statement; a success clears
//! neither (the counters decay with time and collection: a success from one account must not
//! reset the counter of a whole address, security review Low 1, nor give an attacker spreading
//! guesses a fresh burst on the login, round 2 N4; trusted pairs keep the real user in).
//! Rotating `database.secret` changes `K` and so resets every counter (accepted).
//!
//! A third kind, **trusted**, records the last success of an (address, login) pair
//! ([`ThrottleKey::Trusted`]): a pair that logged in within [`TRUSTED_FOR`] is not refused by the
//! per-login cooldown when it presents the correct password (lead decision 3), so nobody can keep
//! a known login locked out from elsewhere. Its `failures` column is always 1 and
//! `last_failure_at` holds the time of the success.
//!
//! New login rows are bounded per address: while the address key has
//! [`UNKNOWN_LOGIN_ROWS_PER_ADDRESS`] failures or more, a failure from it adds no new login row
//! (it still counts on the address and on an existing login row). The rule is the same whether
//! the login names a user or not (security review round 2 S1: an exemption for known logins told
//! which logins exist through the per-login cooldown). A trusted pair's key starts with
//! [`TRUSTED_PREFIX_BYTES`] bytes that depend on the login alone, so the success statement keeps
//! at most [`TRUSTED_PAIRS_PER_LOGIN`] pairs per login (round 2 S2).
//!
//! [`Prefilter`] runs before the database: a per-process token bucket per address, in a table
//! of fixed capacity. Across R replicas it admits R × burst (documented, `scale-out.md` §2).

use std::collections::hash_map::RandomState;
use std::hash::BuildHasher;
use std::net::IpAddr;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use hmac::{Hmac, KeyInit, Mac};
use sha2::Sha256;

use super::secret::{DatabaseSecret, SecretError, THROTTLE_KEY_INFO};

/// The kind of a throttle key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ThrottleKind {
    /// The client address.
    Address,
    /// The login as presented.
    Login,
    /// An (address, login) pair that logged in ([`ThrottleKey::Trusted`]).
    Trusted,
    /// The verified user principal (UID).
    Principal,
    /// An (address, principal UID) pair.
    PrincipalTrusted,
}

impl ThrottleKind {
    /// The `kind` column value.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Address => "address",
            Self::Login => "login",
            Self::Trusted => "trusted",
            Self::Principal => "principal",
            Self::PrincipalTrusted => "principal_trusted",
        }
    }
}

/// A throttle row's key, typed by kind (type design T10: a swapped or mis-kinded key does not
/// compile).
#[derive(Debug, Clone, Copy)]
pub enum ThrottleKey<'a> {
    /// The client address.
    Address(&'a AddressKey),
    /// The login as presented.
    Login(&'a str),
    /// An (address, login) pair.
    Trusted(&'a AddressKey, &'a str),
    /// The verified user principal UID.
    Principal(erp_orm::UserId),
    /// An (address, principal UID) pair.
    PrincipalTrusted(&'a AddressKey, erp_orm::UserId),
}

impl ThrottleKey<'_> {
    /// The kind of the row.
    pub const fn kind(&self) -> ThrottleKind {
        match self {
            Self::Address(_) => ThrottleKind::Address,
            Self::Login(_) => ThrottleKind::Login,
            Self::Trusted(..) => ThrottleKind::Trusted,
            Self::Principal(_) => ThrottleKind::Principal,
            Self::PrincipalTrusted(..) => ThrottleKind::PrincipalTrusted,
        }
    }
}

/// The leading bytes of a trusted pair's key that depend on the login alone (the rest depends on
/// the pair): the success statement finds a login's pairs by this prefix through the primary key.
pub const TRUSTED_PREFIX_BYTES: usize = 8;

/// Trusted pairs one login keeps at most (security review round 2 S2): a success past it drops
/// the pairs that logged in longest ago, in the same statement.
pub const TRUSTED_PAIRS_PER_LOGIN: u32 = 16;

/// The keyed hash of throttle rows: HMAC-SHA256 under the purpose key [`THROTTLE_KEY_INFO`] of
/// `database.secret` (security review round 2 S4). Holds the keyed state, never printed, never
/// cloned out of this module, and wiped when dropped (the `zeroize` features of `hmac` and
/// `sha2`; security review round 2 delta N-d), as is the copy each key's HMAC starts from.
pub struct ThrottleHasher(Hmac<Sha256>);

impl std::fmt::Debug for ThrottleHasher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ThrottleHasher(<redacted>)")
    }
}

impl ThrottleHasher {
    /// The hasher of the database whose secret is `secret`.
    ///
    /// # Errors
    ///
    /// [`SecretError::Derivation`] when HKDF or HMAC refuses the key (not reachable for a 32-byte
    /// key).
    pub fn derive(secret: &DatabaseSecret) -> Result<Self, SecretError> {
        let key = secret.purpose_key(THROTTLE_KEY_INFO)?;
        let mac = <Hmac<Sha256> as KeyInit>::new_from_slice(key.bytes())
            .map_err(|_| SecretError::Derivation)?;
        Ok(Self(mac))
    }

    fn mac(&self, kind: &str, parts: &[&[u8]]) -> [u8; 32] {
        let mut mac = self.0.clone();
        mac.update(kind.as_bytes());
        for part in parts {
            mac.update(&[0u8]);
            mac.update(part);
        }
        mac.finalize().into_bytes().into()
    }

    /// The row key of `key`: `HMAC(K, kind ‖ 0x00 ‖ value)`; for a pair, the first
    /// [`TRUSTED_PREFIX_BYTES`] bytes of `HMAC(K, "trusted-login" ‖ 0x00 ‖ login)` then the rest of
    /// `HMAC(K, "trusted" ‖ 0x00 ‖ address key ‖ 0x00 ‖ login)` (an address key never holds a 0
    /// byte, so no two pairs share an encoding).
    pub fn key_hash(&self, key: ThrottleKey<'_>) -> [u8; 32] {
        match key {
            ThrottleKey::Address(address) => self.mac("address", &[address.0.as_bytes()]),
            ThrottleKey::Login(login) => self.mac("login", &[login.as_bytes()]),
            ThrottleKey::Trusted(address, login) => {
                let mut key = self.mac("trusted", &[address.0.as_bytes(), login.as_bytes()]);
                key[..TRUSTED_PREFIX_BYTES].copy_from_slice(&self.trusted_prefix(login));
                key
            }
            ThrottleKey::Principal(uid) => self.mac("principal", &[&uid.get().to_be_bytes()]),
            ThrottleKey::PrincipalTrusted(address, uid) => {
                let mut key = self.mac(
                    "principal_trusted",
                    &[address.0.as_bytes(), &uid.get().to_be_bytes()],
                );
                key[..TRUSTED_PREFIX_BYTES].copy_from_slice(&self.trusted_principal_prefix(uid));
                key
            }
        }
    }

    fn trusted_principal_prefix(&self, uid: erp_orm::UserId) -> [u8; TRUSTED_PREFIX_BYTES] {
        let full = self.mac("trusted-principal", &[&uid.get().to_be_bytes()]);
        let mut prefix = [0u8; TRUSTED_PREFIX_BYTES];
        prefix.copy_from_slice(&full[..TRUSTED_PREFIX_BYTES]);
        prefix
    }

    /// The smallest and the largest key a trusted pair of `uid` can have in the primary key.
    pub fn trusted_principal_range(&self, uid: erp_orm::UserId) -> ([u8; 32], [u8; 32]) {
        let prefix = self.trusted_principal_prefix(uid);
        let (mut low, mut high) = ([0u8; 32], [0xffu8; 32]);
        low[..TRUSTED_PREFIX_BYTES].copy_from_slice(&prefix);
        high[..TRUSTED_PREFIX_BYTES].copy_from_slice(&prefix);
        (low, high)
    }

    fn trusted_prefix(&self, login: &str) -> [u8; TRUSTED_PREFIX_BYTES] {
        let full = self.mac("trusted-login", &[login.as_bytes()]);
        let mut prefix = [0u8; TRUSTED_PREFIX_BYTES];
        prefix.copy_from_slice(&full[..TRUSTED_PREFIX_BYTES]);
        prefix
    }

    /// The smallest and the largest key a trusted pair of `login` can have (its prefix, then all
    /// zeros or all ones): the range of its pairs in the primary key.
    pub fn trusted_range(&self, login: &str) -> ([u8; 32], [u8; 32]) {
        let prefix = self.trusted_prefix(login);
        let (mut low, mut high) = ([0u8; 32], [0xffu8; 32]);
        low[..TRUSTED_PREFIX_BYTES].copy_from_slice(&prefix);
        high[..TRUSTED_PREFIX_BYTES].copy_from_slice(&prefix);
        (low, high)
    }
}

/// How long a successful login trusts its (address, login) pair against the per-login cooldown
/// (lead decision 3): 30 days.
pub const TRUSTED_FOR: Duration = Duration::from_secs(30 * 86_400);

/// Failures of an address after which a failure from it adds no new login row (security review
/// M5: a random-login spray cannot grow the table without bound; round 2 S1: the same for every
/// login, so the rule tells nothing about which logins exist).
pub const UNKNOWN_LOGIN_ROWS_PER_ADDRESS: i32 = 32;

/// The address key of a client: made only by [`address_key`].
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct AddressKey(String);

impl AddressKey {
    /// The key text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for AddressKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// The address key: IPv4 as is, an IPv4-mapped IPv6 address as its IPv4 form, other IPv6
/// addresses as their /64 network (`2001:db8:1:2::/64`).
///
/// ```
/// use erp_security::auth::throttle::address_key;
/// assert_eq!(address_key("192.0.2.7".parse()?).as_str(), "192.0.2.7");
/// assert_eq!(address_key("2001:db8:1:2:aa:bb:cc:dd".parse()?).as_str(), "2001:db8:1:2::/64");
/// assert_eq!(address_key("::ffff:192.0.2.7".parse()?).as_str(), "192.0.2.7");
/// # Ok::<(), std::net::AddrParseError>(())
/// ```
pub fn address_key(addr: IpAddr) -> AddressKey {
    AddressKey(address_text(addr))
}

fn address_text(addr: IpAddr) -> String {
    match addr {
        IpAddr::V4(v4) => v4.to_string(),
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => v4.to_string(),
            None => {
                let s = v6.segments();
                let net = std::net::Ipv6Addr::new(s[0], s[1], s[2], s[3], 0, 0, 0, 0);
                format!("{net}/64")
            }
        },
    }
}

/// The failure upsert: one row per key, `ON CONFLICT` adds one (bounded below `i32::MAX`), in key
/// order (`address` before `login`, so two failures never lock in opposite orders).
/// `$1`: address key hash or NULL, `$2`: login key hash, `$3`:
/// [`UNKNOWN_LOGIN_ROWS_PER_ADDRESS`]. A login row is written only when it exists already, when
/// there is no address, or while the address has fewer than `$3` failures, whether the login
/// names a user or not (round 2 S1).
pub const FAILURE_SQL: &str = "\
INSERT INTO erp_login_throttle AS t (kind, key_hash, failures, last_failure_at)
SELECT k.kind, k.key_hash, 1, now()
  FROM (VALUES ('address', $1::bytea), ('login', $2::bytea)) AS k (kind, key_hash)
 WHERE k.key_hash IS NOT NULL
   AND (k.kind = 'address' OR $1::bytea IS NULL
        OR EXISTS (SELECT 1 FROM erp_login_throttle l
                    WHERE l.kind = 'login' AND l.key_hash = $2::bytea)
        OR COALESCE((SELECT a.failures FROM erp_login_throttle a
                      WHERE a.kind = 'address' AND a.key_hash = $1::bytea), 0) < $3::int4)
 ORDER BY k.kind
ON CONFLICT (kind, key_hash) DO UPDATE
   SET failures = LEAST(t.failures, 2147483646) + 1, last_failure_at = now()";

/// Atomic failure accounting for custom selection. `$4` is the verified principal
/// key or NULL for an unknown/inactive candidate. Trust is never written on failure.
pub const FAILURE_SELECTION_SQL: &str = "\
INSERT INTO erp_login_throttle AS t (kind, key_hash, failures, last_failure_at)
SELECT k.kind, k.key_hash, 1, now()
  FROM (VALUES ('address', $1::bytea), ('login', $2::bytea), ('principal', $4::bytea)) AS k (kind, key_hash)
 WHERE k.key_hash IS NOT NULL
   AND (k.kind IN ('address', 'principal') OR $1::bytea IS NULL
        OR EXISTS (SELECT 1 FROM erp_login_throttle l
                    WHERE l.kind = 'login' AND l.key_hash = $2::bytea)
        OR COALESCE((SELECT a.failures FROM erp_login_throttle a
                      WHERE a.kind = 'address' AND a.key_hash = $1::bytea), 0) < $3::int4)
 ORDER BY k.kind
ON CONFLICT (kind, key_hash) DO UPDATE
   SET failures = LEAST(t.failures, 2147483646) + 1, last_failure_at = now()";

/// A per-process token bucket per address with a fixed number of slots ('s rule for
/// bounded tables). A new key takes a free slot of its probe window, else a slot whose bucket has
/// refilled to full (which loses nothing), else the slot of the window used least recently: that
/// address gets a full bucket again when it returns (security review M5: a table filled by many
/// addresses degrades to a smaller window per address, it never refuses everyone new).
#[derive(Debug)]
pub struct Prefilter {
    slots: Mutex<Vec<Slot>>,
    burst: f64,
    rate: f64,
    hasher: RandomState,
    refused: AtomicU64,
    evicted: AtomicU64,
    #[cfg(feature = "test-support")]
    fixed_now: std::sync::OnceLock<Instant>,
}

#[derive(Debug, Clone, Copy)]
struct Slot {
    key: u64,
    tokens: f64,
    at: Instant,
    used: bool,
}

/// Slots probed for one key.
const PROBE: usize = 8;

impl Prefilter {
    /// A table of `entries` slots, `burst` tokens per address refilled at `rate_per_sec`.
    pub fn new(entries: usize, burst: u32, rate_per_sec: f64) -> Self {
        let now = Instant::now();
        Self {
            slots: Mutex::new(vec![
                Slot {
                    key: 0,
                    tokens: 0.0,
                    at: now,
                    used: false,
                };
                entries.max(PROBE)
            ]),
            burst: f64::from(burst),
            rate: rate_per_sec,
            hasher: RandomState::new(),
            refused: AtomicU64::new(0),
            evicted: AtomicU64::new(0),
            #[cfg(feature = "test-support")]
            fixed_now: std::sync::OnceLock::new(),
        }
    }

    /// Take one token for `address`; `false` when its bucket is empty.
    pub fn admit(&self, address: &AddressKey) -> bool {
        #[cfg(feature = "test-support")]
        // Only an explicitly installed test seam fixes time; absent means the real clock.
        let now = self.fixed_now.get().copied().unwrap_or_else(Instant::now);
        #[cfg(not(feature = "test-support"))]
        let now = Instant::now();
        self.admit_at(address.as_str(), now)
    }

    /// Fix this prefilter's clock for deterministic integration contracts, default-off.
    #[cfg(feature = "test-support")]
    pub(crate) fn freeze_at(&self, now: Instant) -> erp_fields::Result<()> {
        self.fixed_now
            .set(now)
            .map_err(|_| erp_fields::Error::programming("prefilter test clock was already frozen"))
    }

    pub(crate) fn admit_at(&self, address: &str, now: Instant) -> bool {
        let key = self.hasher.hash_one(address);
        let mut slots = self
            .slots
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let len = slots.len();
        // the table holds at least PROBE slots, so `len` is not 0, and the remainder is below
        // `len`, so it fits a usize
        let start = (key % (len as u64)) as usize;
        let mut free: Option<usize> = None;
        // seeded with the first slot of the window, so there always is one
        let mut least_recent = (start, slots[start].at);
        for i in 0..PROBE {
            let idx = (start + i) % len;
            let slot = slots[idx];
            if slot.used && slot.key == key {
                let tokens = self.refill(slot, now);
                if tokens >= 1.0 {
                    slots[idx] = Slot {
                        tokens: tokens - 1.0,
                        at: now,
                        ..slot
                    };
                    return true;
                }
                slots[idx] = Slot {
                    tokens,
                    at: now,
                    ..slot
                };
                self.refused.fetch_add(1, Ordering::Relaxed);
                return false;
            }
            if free.is_none() && (!slot.used || self.refill(slot, now) >= self.burst) {
                free = Some(idx);
            }
            if slot.at < least_recent.1 {
                least_recent = (idx, slot.at);
            }
        }
        let idx = match free {
            Some(idx) => idx,
            None => {
                let evicted = self.evicted.fetch_add(1, Ordering::Relaxed) + 1;
                // the first eviction and every 1,024th: a spray over many addresses shows
                if evicted == 1 || evicted.is_multiple_of(1_024) {
                    tracing::info!(
                        target: "erp::auth",
                        "login pre-filter full: {evicted} buckets taken over from other addresses \
                         so far (throttle.table_entries)"
                    );
                }
                least_recent.0
            }
        };
        slots[idx] = Slot {
            key,
            tokens: self.burst - 1.0,
            at: now,
            used: true,
        };
        true
    }

    fn refill(&self, slot: Slot, now: Instant) -> f64 {
        let elapsed = now.saturating_duration_since(slot.at).as_secs_f64();
        (slot.tokens + elapsed * self.rate).min(self.burst)
    }

    /// Attempts refused so far (an empty bucket).
    pub fn refused(&self) -> u64 {
        self.refused.load(Ordering::Relaxed)
    }

    /// Buckets taken over from another address because the probe window had no free slot.
    pub fn evicted(&self) -> u64 {
        self.evicted.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "tests")]
mod tests {
    use std::time::Duration;

    use super::*;

    fn hasher(secret: &str) -> ThrottleHasher {
        ThrottleHasher::derive(&DatabaseSecret::from_stored(secret).unwrap()).unwrap()
    }

    const SECRET: &str = "0123456789abcdef0123456789abcdef";

    #[test]
    fn keys_differ_by_kind_and_value() {
        let h = hasher(SECRET);
        let x = AddressKey("x".to_owned());
        assert_ne!(
            h.key_hash(ThrottleKey::Address(&x)),
            h.key_hash(ThrottleKey::Login("x"))
        );
        assert_ne!(
            h.key_hash(ThrottleKey::Login("a")),
            h.key_hash(ThrottleKey::Login("A"))
        );
    }

    /// Round 2 S4: the key is keyed by the database's secret and is not the plain SHA-256 of
    /// the row's encoding; it never prints.
    #[test]
    fn keys_are_keyed_by_the_secret() {
        use sha2::Digest;
        let (one, two) = (hasher(SECRET), hasher("0123456789abcdef0123456789abcdeF"));
        let login = ThrottleKey::Login("bob");
        assert_ne!(one.key_hash(login), two.key_hash(login));
        assert_eq!(one.key_hash(login), one.key_hash(login));
        let plain: [u8; 32] = sha2::Sha256::digest(b"login\0bob").into();
        assert_ne!(one.key_hash(login), plain);
        assert_eq!(format!("{one:?}"), "ThrottleHasher(<redacted>)");
    }

    /// Round 2 S2: a login's pairs share its prefix and fall inside its range; another login's
    /// do not.
    #[test]
    fn trusted_pairs_of_a_login_share_its_range() -> Result<(), std::net::AddrParseError> {
        let h = hasher(SECRET);
        let (one, two) = (
            address_key("192.0.2.1".parse()?),
            address_key("192.0.2.2".parse()?),
        );
        let (low, high) = h.trusted_range("bob");
        for address in [&one, &two] {
            let key = h.key_hash(ThrottleKey::Trusted(address, "bob"));
            assert!(low <= key && key <= high);
        }
        let other = h.key_hash(ThrottleKey::Trusted(&one, "ann"));
        assert!(!(low <= other && other <= high));
        Ok(())
    }

    #[test]
    fn burst_then_refill() {
        let p = Prefilter::new(64, 5, 1.0);
        let t0 = Instant::now();
        for _ in 0..5 {
            assert!(p.admit_at("192.0.2.1", t0));
        }
        assert!(!p.admit_at("192.0.2.1", t0));
        assert!(
            p.admit_at("192.0.2.2", t0),
            "another address has its own bucket"
        );
        assert!(p.admit_at("192.0.2.1", t0 + Duration::from_millis(1_001)));
        assert_eq!((p.refused(), p.evicted()), (1, 0));
    }

    /// Security review M5: a full table never refuses a new address; the least recently used
    /// bucket of the window goes, and its address starts with a full bucket when it returns.
    #[test]
    fn a_full_table_evicts_the_least_recent_bucket() {
        // 8 slots: every new key probes all of them
        let p = Prefilter::new(8, 5, 1.0);
        let t0 = Instant::now();
        // every key spends its whole burst, 10.0.0.0 first
        for i in 0..8u64 {
            for _ in 0..5 {
                assert!(p.admit_at(&format!("10.0.0.{i}"), t0 + Duration::from_millis(i)));
            }
        }
        assert!(p.admit_at("10.0.1.1", t0 + Duration::from_millis(10)));
        assert_eq!((p.refused(), p.evicted()), (0, 1));
        // 10.0.1.1 has its own bucket now: 4 tokens left
        for _ in 0..4 {
            assert!(p.admit_at("10.0.1.1", t0 + Duration::from_millis(10)));
        }
        assert!(!p.admit_at("10.0.1.1", t0 + Duration::from_millis(10)));
        // 10.0.0.0 was the least recent: it lost its empty bucket and gets a full one back
        // (taking the slot of the next least recent, 10.0.0.1)
        assert!(p.admit_at("10.0.0.0", t0 + Duration::from_millis(11)));
        assert_eq!((p.refused(), p.evicted()), (1, 2));
        // 10.0.0.2 kept its empty bucket
        assert!(!p.admit_at("10.0.0.2", t0 + Duration::from_millis(12)));
    }

    #[test]
    fn trusted_keys_differ_by_address_and_login() -> Result<(), std::net::AddrParseError> {
        let one = address_key("192.0.2.1".parse()?);
        let two = address_key("192.0.2.2".parse()?);
        let h = hasher(SECRET);
        let pair = |a, l| h.key_hash(ThrottleKey::Trusted(a, l));
        assert_ne!(pair(&one, "bob"), pair(&two, "bob"));
        assert_ne!(pair(&one, "bob"), pair(&one, "bob2"));
        assert_ne!(pair(&one, "bob"), h.key_hash(ThrottleKey::Login("bob")));
        assert_ne!(pair(&one, "bob"), h.key_hash(ThrottleKey::Address(&one)));
        Ok(())
    }

    /// Security review round 2 delta N-d: the keyed state the hasher holds, and every per-key
    /// copy `mac` makes of it, wipe themselves when dropped. The HMAC's state is two SHA-256 cores
    /// (the inner and the outer keyed state) and a block buffer; with the `zeroize` features of
    /// `hmac` and `sha2` they are `ZeroizeOnDrop` (this does not compile without them).
    #[test]
    fn the_keyed_state_is_wiped_when_dropped() {
        use hmac::digest::block_api::{Buffer, EagerHash};
        use hmac::digest::zeroize::ZeroizeOnDrop;
        fn wiped_on_drop<T: ZeroizeOnDrop>() {}
        wiped_on_drop::<<Sha256 as EagerHash>::Core>();
        wiped_on_drop::<Buffer<<Sha256 as EagerHash>::Core>>();
        wiped_on_drop::<Sha256>();
    }

    /// Security review round 3 R3-e: the hasher's own field is the `Hmac<Sha256>` whose parts the
    /// test above pins as wiped on drop (a switch to another MAC type stops this compiling, so
    /// the zeroize check is made again for the new type). `Hmac` itself has no `ZeroizeOnDrop`
    /// impl in `hmac` 0.13: its two SHA-256 cores and its block buffer wipe themselves.
    #[test]
    fn the_hasher_holds_the_pinned_mac_type() {
        fn field(hasher: &ThrottleHasher) -> &Hmac<Sha256> {
            &hasher.0
        }
        let h = hasher(SECRET);
        let mac = field(&h).clone().finalize().into_bytes();
        assert_eq!(mac.len(), 32);
    }
}
