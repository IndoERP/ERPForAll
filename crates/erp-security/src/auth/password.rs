//! Password hashes: our own codecs for the two stored forms
//!
//! - `$argon2id$v=19$m=<kib>,t=<t>,p=<p>$<salt>$<tag>` (PHC string, standard base64 without
//!   padding): what ERPForAll produces by default (16-byte salt, 32-byte tag, parallelism 1).
//!
//! A stored value is identified by its prefix, never by trying schemes; anything else (plaintext,
//! `$1$`, bcrypt, an empty string) is [`PasswordError::UnrecognisedFormat`] and never verifies.
//! Parsing is strict: one canonical spelling of each field, every length and cost bounded before
//! any work, so a crafted stored value cannot make a verify allocate or spin without bound.
//!
//! Parameters for **new** hashes are [`HashParams`], validated against the floors and hard bounds
//! of §3.1 (a value outside them is an error naming the parameter, never clamped).
//!
//! The functions here are CPU-bound and synchronous; callers run them on the hashing pool
//! ([`super::pool::HashPool`]), never on the async runtime.

use std::fmt;

use argon2::{Algorithm, Argon2, Params, Version};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD_NO_PAD;
use sha2::Sha512;
use subtle::ConstantTimeEq;
use zeroize::Zeroizing;

/// The hash scheme of new passwords (`password.hashing.scheme`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Scheme {
    /// Argon2id (RFC 9106), the default.
    Argon2id,
    Pbkdf2Sha512,
}

impl Scheme {
    /// The `password.hashing.scheme` value naming it.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Argon2id => "argon2id",
            Self::Pbkdf2Sha512 => "pbkdf2_sha512",
        }
    }

    /// The scheme a `password.hashing.scheme` value names.
    ///
    /// # Errors
    ///
    /// [`PasswordError::UnknownScheme`] for any other text.
    pub fn parse(value: &str) -> Result<Self, PasswordError> {
        match value {
            "argon2id" => Ok(Self::Argon2id),
            "pbkdf2_sha512" => Ok(Self::Pbkdf2Sha512),
            other => Err(PasswordError::UnknownScheme {
                value: bounded(other),
            }),
        }
    }
}

/// Floors and hard bounds of the hashing parameters.
pub mod limits {
    /// `password.hashing.argon2_memory_kib`: default and floor (19 MiB, OWASP's minimum for t = 2).
    pub const ARGON2_MEMORY_KIB_MIN: u32 = 19_456;
    /// `password.hashing.argon2_memory_kib`: hard bound (1 GiB).
    pub const ARGON2_MEMORY_KIB_MAX: u32 = 1_048_576;
    /// `password.hashing.argon2_time`: default and floor.
    pub const ARGON2_TIME_MIN: u32 = 2;
    /// `password.hashing.argon2_time`: hard bound.
    pub const ARGON2_TIME_MAX: u32 = 20;
    pub const PBKDF2_ROUNDS_MIN: u32 = 600_000;
    /// `password.hashing.rounds`: hard bound.
    pub const PBKDF2_ROUNDS_MAX: u32 = 10_000_000;
    /// Salt length of new hashes, bytes.
    pub const SALT_BYTES: usize = 16;
    /// Tag length of new Argon2id hashes, bytes.
    pub const ARGON2_TAG_BYTES: usize = 32;
    /// Tag length of PBKDF2-SHA512 hashes, bytes (the digest size).
    pub const PBKDF2_TAG_BYTES: usize = 64;
    /// Longest stored hash we parse, bytes (a passlib hash with a 1,024-byte salt is about 1,500).
    pub const STORED_MAX_BYTES: usize = 2_048;
    /// Stored Argon2 hashes: smallest memory we verify (the algorithm's minimum for p = 1 is 8).
    pub const STORED_ARGON2_MEMORY_KIB_MIN: u32 = 8;
    /// Stored Argon2 hashes: largest parallelism we verify.
    pub const STORED_ARGON2_LANES_MAX: u32 = 16;
    /// Stored salts: shortest and longest, bytes (passlib allows 0..=1024; Argon2 needs ≥ 8).
    pub const STORED_SALT_MIN: usize = 8;
    /// See [`STORED_SALT_MIN`].
    pub const STORED_SALT_MAX: usize = 1_024;
    /// Stored Argon2 tags: shortest and longest, bytes.
    pub const STORED_ARGON2_TAG_MIN: usize = 16;
    /// See [`STORED_ARGON2_TAG_MIN`].
    pub const STORED_ARGON2_TAG_MAX: usize = 64;
}

/// Parameters for new hashes, validated against the floors and hard bounds.
///
/// ```
/// use erp_security::auth::password::{HashParams, Scheme};
/// let p = HashParams::argon2id(19_456, 2)?;
/// assert_eq!((p.scheme(), p.memory_kib()), (Scheme::Argon2id, 19_456));
/// let e = HashParams::pbkdf2_sha512(1_000).unwrap_err();
/// assert_eq!(
///     e.to_string(),
///     "password.hashing.rounds = 1000 is outside 600000..=10000000"
/// );
/// # Ok::<(), erp_security::auth::password::PasswordError>(())
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct HashParams(Cost);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Cost {
    Argon2id { memory_kib: u32, time: u32 },
    Pbkdf2 { rounds: u32 },
}

impl HashParams {
    /// The defaults: Argon2id, 19 MiB, t = 2, p = 1.
    pub const DEFAULT: Self = Self(Cost::Argon2id {
        memory_kib: limits::ARGON2_MEMORY_KIB_MIN,
        time: limits::ARGON2_TIME_MIN,
    });

    /// Argon2id with `memory_kib` and `time` (parallelism is always 1).
    ///
    /// # Errors
    ///
    /// [`PasswordError::ParamOutOfRange`] naming the parameter.
    pub fn argon2id(memory_kib: u32, time: u32) -> Result<Self, PasswordError> {
        check_range(
            "password.hashing.argon2_memory_kib",
            memory_kib,
            limits::ARGON2_MEMORY_KIB_MIN,
            limits::ARGON2_MEMORY_KIB_MAX,
        )?;
        check_range(
            "password.hashing.argon2_time",
            time,
            limits::ARGON2_TIME_MIN,
            limits::ARGON2_TIME_MAX,
        )?;
        Ok(Self(Cost::Argon2id { memory_kib, time }))
    }

    /// PBKDF2-SHA512 with `rounds`.
    ///
    /// # Errors
    ///
    /// [`PasswordError::ParamOutOfRange`] naming `password.hashing.rounds`.
    pub fn pbkdf2_sha512(rounds: u32) -> Result<Self, PasswordError> {
        check_range(
            "password.hashing.rounds",
            rounds,
            limits::PBKDF2_ROUNDS_MIN,
            limits::PBKDF2_ROUNDS_MAX,
        )?;
        Ok(Self(Cost::Pbkdf2 { rounds }))
    }

    /// The scheme.
    pub const fn scheme(&self) -> Scheme {
        match self.0 {
            Cost::Argon2id { .. } => Scheme::Argon2id,
            Cost::Pbkdf2 { .. } => Scheme::Pbkdf2Sha512,
        }
    }

    /// Memory one hash with these parameters needs, KiB (0 for PBKDF2, which needs a few hundred
    /// bytes).
    pub const fn memory_kib(&self) -> u32 {
        match self.0 {
            Cost::Argon2id { memory_kib, .. } => memory_kib,
            Cost::Pbkdf2 { .. } => 0,
        }
    }

    /// These parameters with Argon2id's memory lowered to `reserved_kib` when above it (never below
    /// Argon2id's floor; PBKDF2 unchanged): the parameters a hashing pool reserving `reserved_kib`
    /// per permit can run (security review round 2 delta SD-1: the dummy verify and the denial
    /// cost floor use them, so the pool never refuses them).
    pub(crate) fn within_memory(self, reserved_kib: u32) -> Self {
        match self.0 {
            Cost::Argon2id { memory_kib, time } => Self(Cost::Argon2id {
                memory_kib: memory_kib.min(reserved_kib.max(limits::ARGON2_MEMORY_KIB_MIN)),
                time,
            }),
            Cost::Pbkdf2 { .. } => self,
        }
    }

    /// The stored-hash cost these parameters produce.
    fn stored(self) -> StoredCost {
        match self.0 {
            Cost::Argon2id { memory_kib, time } => StoredCost::Argon2id {
                memory_kib,
                time,
                lanes: 1,
            },
            Cost::Pbkdf2 { rounds } => StoredCost::Pbkdf2 { rounds },
        }
    }
}

fn check_range(param: &'static str, value: u32, min: u32, max: u32) -> Result<(), PasswordError> {
    if (min..=max).contains(&value) {
        Ok(())
    } else {
        Err(PasswordError::ParamOutOfRange {
            param,
            value: u64::from(value),
            min: u64::from(min),
            max: u64::from(max),
        })
    }
}

/// The cost of a stored hash, as parsed (may be below the floors of new hashes).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum StoredCost {
    Argon2id {
        memory_kib: u32,
        time: u32,
        lanes: u32,
    },
    Pbkdf2 {
        rounds: u32,
    },
}

/// How a stored hash's verify compares with a verify at the current parameters
/// ([`StoredHash::compare_cost`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum CostComparison {
    /// The same scheme, less work: the denial cost floor adds a verify at the current parameters.
    Cheaper,
    /// The same scheme, the same or more work.
    NotCheaper,
    /// Another scheme: the work does not compare, so the floor adds the verify too (lead decision 4
    /// is a floor, not an equality: such a denial may take longer than an unknown login's).
    Incomparable,
}

/// The work of one verify, per scheme.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Work {
    /// Memory (KiB, the total over all lanes) × passes: Argon2id fills `m` blocks `t` times
    /// whatever the parallelism, so lanes do not multiply (type design T2).
    Argon2id(u64),
    /// Rounds.
    Pbkdf2(u64),
}

impl StoredCost {
    fn work(self) -> Work {
        match self {
            Self::Argon2id {
                memory_kib, time, ..
            } => Work::Argon2id(u64::from(memory_kib) * u64::from(time)),
            Self::Pbkdf2 { rounds } => Work::Pbkdf2(u64::from(rounds)),
        }
    }
}

/// A stored password hash, parsed and bounded. Its `Debug` shows only the scheme and costs.
///
/// The decoded salt and tag are [`Zeroizing`]: every copy (a `Clone` included) wipes its bytes when
/// dropped, as does the half-decoded buffer of a hash that fails to parse (
/// closing the LD3 review SHOULD). The text the hash was parsed from is the caller's.
#[derive(Clone, PartialEq, Eq)]
pub struct StoredHash {
    cost: StoredCost,
    salt: Zeroizing<Vec<u8>>,
    tag: Zeroizing<Vec<u8>>,
}

impl fmt::Debug for StoredHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StoredHash")
            .field("cost", &self.cost)
            .finish_non_exhaustive()
    }
}

const ARGON2_PREFIX: &str = "$argon2id$v=19$";
const PBKDF2_PREFIX: &str = "$pbkdf2-sha512$";

impl StoredHash {
    /// Parse a stored `password` column value.
    ///
    /// # Errors
    ///
    /// [`PasswordError::UnrecognisedFormat`] when no known prefix matches (plaintext, another
    /// scheme, empty); [`PasswordError::Malformed`] when the prefix matches and the rest does not
    /// parse or is out of bounds.
    pub fn parse(stored: &str) -> Result<Self, PasswordError> {
        if stored.len() > limits::STORED_MAX_BYTES {
            return Err(malformed("longer than the longest hash we accept"));
        }
        if let Some(rest) = stored.strip_prefix(ARGON2_PREFIX) {
            Self::parse_argon2(rest)
        } else if let Some(rest) = stored.strip_prefix(PBKDF2_PREFIX) {
            Self::parse_pbkdf2(rest)
        } else {
            Err(PasswordError::UnrecognisedFormat)
        }
    }

    fn parse_argon2(rest: &str) -> Result<Self, PasswordError> {
        let mut parts = rest.split('$');
        let (Some(params), Some(salt), Some(tag), None) =
            (parts.next(), parts.next(), parts.next(), parts.next())
        else {
            return Err(malformed("argon2id: expected parameters, salt and tag"));
        };
        let mut fields = params.split(',');
        let (Some(m), Some(t), Some(p), None) =
            (fields.next(), fields.next(), fields.next(), fields.next())
        else {
            return Err(malformed("argon2id: expected exactly m, t and p"));
        };
        let memory_kib = decimal(m.strip_prefix("m="), "argon2id memory")?;
        let time = decimal(t.strip_prefix("t="), "argon2id time")?;
        let lanes = decimal(p.strip_prefix("p="), "argon2id parallelism")?;
        if !(limits::STORED_ARGON2_MEMORY_KIB_MIN..=limits::ARGON2_MEMORY_KIB_MAX)
            .contains(&memory_kib)
        {
            return Err(malformed("argon2id: memory outside 8..=1048576 KiB"));
        }
        if !(1..=limits::ARGON2_TIME_MAX).contains(&time) {
            return Err(malformed("argon2id: time outside 1..=20"));
        }
        if !(1..=limits::STORED_ARGON2_LANES_MAX).contains(&lanes) {
            return Err(malformed("argon2id: parallelism outside 1..=16"));
        }
        if memory_kib < 8 * lanes {
            return Err(malformed("argon2id: memory below 8 KiB per lane"));
        }
        let salt = decode_b64(salt, &STANDARD_NO_PAD, "salt")?;
        let tag = decode_b64(tag, &STANDARD_NO_PAD, "tag")?;
        check_len(
            salt.len(),
            limits::STORED_SALT_MIN,
            limits::STORED_SALT_MAX,
            "salt",
        )?;
        check_len(
            tag.len(),
            limits::STORED_ARGON2_TAG_MIN,
            limits::STORED_ARGON2_TAG_MAX,
            "tag",
        )?;
        Ok(Self {
            cost: StoredCost::Argon2id {
                memory_kib,
                time,
                lanes,
            },
            salt,
            tag,
        })
    }

    fn parse_pbkdf2(rest: &str) -> Result<Self, PasswordError> {
        let mut parts = rest.split('$');
        let (Some(rounds), Some(salt), Some(tag), None) =
            (parts.next(), parts.next(), parts.next(), parts.next())
        else {
            return Err(malformed("pbkdf2-sha512: expected rounds, salt and tag"));
        };
        let rounds = decimal(Some(rounds), "pbkdf2-sha512 rounds")?;
        if !(1..=limits::PBKDF2_ROUNDS_MAX).contains(&rounds) {
            return Err(malformed("pbkdf2-sha512: rounds outside 1..=10000000"));
        }
        let salt = decode_ab64(salt, "salt")?;
        let tag = decode_ab64(tag, "tag")?;
        check_len(
            salt.len(),
            limits::STORED_SALT_MIN,
            limits::STORED_SALT_MAX,
            "salt",
        )?;
        if tag.len() != limits::PBKDF2_TAG_BYTES {
            return Err(malformed("pbkdf2-sha512: the tag is not 64 bytes"));
        }
        Ok(Self {
            cost: StoredCost::Pbkdf2 { rounds },
            salt,
            tag,
        })
    }

    /// The scheme the hash was made with.
    pub fn scheme(&self) -> Scheme {
        match self.cost {
            StoredCost::Argon2id { .. } => Scheme::Argon2id,
            StoredCost::Pbkdf2 { .. } => Scheme::Pbkdf2Sha512,
        }
    }

    /// Memory one verify of this hash needs, KiB (0 for PBKDF2).
    pub fn memory_kib(&self) -> u32 {
        match self.cost {
            StoredCost::Argon2id { memory_kib, .. } => memory_kib,
            StoredCost::Pbkdf2 { .. } => 0,
        }
    }

    /// Whether `password` is the one this hash was made from (constant-time tag comparison).
    /// CPU-bound: run it on the hashing pool.
    ///
    /// # Errors
    ///
    /// [`PasswordError::Argon2`] when the algorithm refuses the stored parameters (they were
    /// bounded when parsed, so this only happens when memory cannot be allocated).
    pub fn verify(&self, password: &[u8]) -> Result<bool, PasswordError> {
        let mut computed = Zeroizing::new(vec![0u8; self.tag.len()]);
        derive(self.cost, password, &self.salt, &mut computed)?;
        Ok(bool::from(computed.as_slice().ct_eq(&self.tag)))
    }

    /// How verifying this hash compares with a verify at `current` (lead decision 4, the denial
    /// cost floor): the work of the same scheme ([`CostComparison::Cheaper`] or
    /// [`CostComparison::NotCheaper`]); [`CostComparison::Incomparable`] across schemes (PBKDF2
    /// rounds and Argon2id memory × passes measure different things).
    pub fn compare_cost(&self, current: &HashParams) -> CostComparison {
        match (self.cost.work(), current.stored().work()) {
            (Work::Argon2id(stored), Work::Argon2id(now))
            | (Work::Pbkdf2(stored), Work::Pbkdf2(now))
                if stored < now =>
            {
                CostComparison::Cheaper
            }
            (Work::Argon2id(_), Work::Argon2id(_)) | (Work::Pbkdf2(_), Work::Pbkdf2(_)) => {
                CostComparison::NotCheaper
            }
            (Work::Argon2id(_), Work::Pbkdf2(_)) | (Work::Pbkdf2(_), Work::Argon2id(_)) => {
                CostComparison::Incomparable
            }
        }
    }

    /// Whether a successful login should rehash: the scheme or a cost differs from `current`, or
    /// the salt or tag length is not the one new hashes use.
    pub fn needs_rehash(&self, current: &HashParams) -> bool {
        let tag_len = match current.0 {
            Cost::Argon2id { .. } => limits::ARGON2_TAG_BYTES,
            Cost::Pbkdf2 { .. } => limits::PBKDF2_TAG_BYTES,
        };
        self.cost != current.stored()
            || self.salt.len() != limits::SALT_BYTES
            || self.tag.len() != tag_len
    }
}

/// Hash `password` with `params` and a fresh 16-byte salt from the OS CSPRNG, as the stored text.
/// CPU-bound: run it on the hashing pool.
///
/// ```
/// use erp_security::auth::password::{HashParams, StoredHash, hash};
/// let stored = hash(b"pw", &HashParams::DEFAULT)?;
/// assert!(stored.starts_with("$argon2id$v=19$m=19456,t=2,p=1$"));
/// let parsed = StoredHash::parse(&stored)?;
/// assert!(parsed.verify(b"pw")?);
/// assert!(!parsed.verify(b"other")?);
/// assert!(!parsed.needs_rehash(&HashParams::DEFAULT));
/// # Ok::<(), erp_security::auth::password::PasswordError>(())
/// ```
///
/// # Errors
///
/// [`PasswordError::Random`] when the OS CSPRNG fails; [`PasswordError::Argon2`] when memory
/// cannot be allocated.
pub fn hash(password: &[u8], params: &HashParams) -> Result<String, PasswordError> {
    let mut salt = [0u8; limits::SALT_BYTES];
    getrandom::fill(&mut salt).map_err(PasswordError::Random)?;
    hash_with_salt(password, params, &salt)
}

/// [`hash`] with a given salt (tests and the dummy hash).
pub(crate) fn hash_with_salt(
    password: &[u8],
    params: &HashParams,
    salt: &[u8],
) -> Result<String, PasswordError> {
    let cost = params.stored();
    match cost {
        StoredCost::Argon2id {
            memory_kib, time, ..
        } => {
            let mut tag = Zeroizing::new([0u8; limits::ARGON2_TAG_BYTES]);
            derive(cost, password, salt, tag.as_mut_slice())?;
            Ok(format!(
                "{ARGON2_PREFIX}m={memory_kib},t={time},p=1${}${}",
                STANDARD_NO_PAD.encode(salt),
                STANDARD_NO_PAD.encode(tag.as_slice())
            ))
        }
        StoredCost::Pbkdf2 { rounds } => {
            let mut tag = Zeroizing::new([0u8; limits::PBKDF2_TAG_BYTES]);
            derive(cost, password, salt, tag.as_mut_slice())?;
            Ok(format!(
                "{PBKDF2_PREFIX}{rounds}${}${}",
                encode_ab64(salt),
                encode_ab64(tag.as_slice())
            ))
        }
    }
}

fn derive(
    cost: StoredCost,
    password: &[u8],
    salt: &[u8],
    out: &mut [u8],
) -> Result<(), PasswordError> {
    match cost {
        StoredCost::Argon2id {
            memory_kib,
            time,
            lanes,
        } => {
            let params = Params::new(memory_kib, time, lanes, Some(out.len()))
                .map_err(PasswordError::Argon2)?;
            Argon2::new(Algorithm::Argon2id, Version::V0x13, params)
                .hash_password_into(password, salt, out)
                .map_err(PasswordError::Argon2)
        }
        StoredCost::Pbkdf2 { rounds } => {
            pbkdf2::pbkdf2_hmac::<Sha512>(password, salt, rounds, out);
            Ok(())
        }
    }
}

/// A decimal number with no sign, no leading zero and at most 10 digits.
fn decimal(text: Option<&str>, what: &'static str) -> Result<u32, PasswordError> {
    let Some(text) = text else {
        return Err(PasswordError::Malformed { what });
    };
    let canonical = !text.is_empty()
        && text.len() <= 10
        && text.bytes().all(|b| b.is_ascii_digit())
        && !(text.len() > 1 && text.starts_with('0'));
    if !canonical {
        return Err(PasswordError::Malformed { what });
    }
    text.parse().map_err(|_| PasswordError::Malformed { what })
}

fn check_len(len: usize, min: usize, max: usize, what: &'static str) -> Result<(), PasswordError> {
    if (min..=max).contains(&len) {
        Ok(())
    } else {
        Err(PasswordError::Malformed { what })
    }
}

fn decode_b64(
    text: &str,
    engine: &base64::engine::GeneralPurpose,
    what: &'static str,
) -> Result<Zeroizing<Vec<u8>>, PasswordError> {
    // the bound was checked on the whole string; 3 bytes per 4 characters, rounded up. The buffer
    // is wiped on every path out, a failed decode included.
    let mut out = Zeroizing::new(vec![0u8; text.len().div_ceil(4) * 3]);
    let n = engine
        .decode_slice(text, &mut out)
        .map_err(|_| PasswordError::Malformed { what })?;
    out.truncate(n);
    Ok(out)
}

/// passlib's "adapted base64": the standard alphabet with `.` instead of `+`, no padding.
fn decode_ab64(text: &str, what: &'static str) -> Result<Zeroizing<Vec<u8>>, PasswordError> {
    if text.contains('+') {
        return Err(PasswordError::Malformed { what });
    }
    let standard = Zeroizing::new(text.replace('.', "+"));
    decode_b64(&standard, &STANDARD_NO_PAD, what)
}

fn encode_ab64(bytes: &[u8]) -> String {
    STANDARD_NO_PAD.encode(bytes).replace('+', ".")
}

fn malformed(what: &'static str) -> PasswordError {
    PasswordError::Malformed { what }
}

/// A user-supplied text in an error message: at most 64 characters, control characters escaped.
pub(crate) fn bounded(text: &str) -> String {
    let mut out: String = text.chars().take(64).flat_map(char::escape_debug).collect();
    if text.chars().count() > 64 {
        out.push('…');
    }
    out
}

/// Why a password could not be hashed or a stored hash not used.
#[derive(Debug)]
#[non_exhaustive]
pub enum PasswordError {
    /// The stored value has no known prefix (plaintext, another scheme, empty).
    UnrecognisedFormat,
    /// The stored value has a known prefix but does not parse or is out of bounds.
    Malformed {
        /// The part that is wrong.
        what: &'static str,
    },
    /// A hashing parameter for new hashes is outside its floor and hard bound.
    ParamOutOfRange {
        /// The `ir.config_parameter` key.
        param: &'static str,
        /// The value given.
        value: u64,
        /// The floor.
        min: u64,
        /// The hard bound.
        max: u64,
    },
    /// `password.hashing.scheme` names no scheme.
    UnknownScheme {
        /// The value, bounded for the message.
        value: String,
    },
    /// The OS CSPRNG failed.
    Random(getrandom::Error),
    /// The Argon2 implementation refused (memory could not be allocated).
    Argon2(argon2::Error),
}

impl fmt::Display for PasswordError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnrecognisedFormat => f.write_str("unrecognised password hash format"),
            Self::Malformed { what } => write!(f, "malformed password hash: {what}"),
            Self::ParamOutOfRange {
                param,
                value,
                min,
                max,
            } => write!(f, "{param} = {value} is outside {min}..={max}"),
            Self::UnknownScheme { value } => write!(
                f,
                "password.hashing.scheme = '{value}' is not a scheme: expected 'argon2id' or \
                 'pbkdf2_sha512'"
            ),
            Self::Random(e) => write!(f, "the OS random number generator failed: {e}"),
            Self::Argon2(e) => write!(f, "argon2id refused the parameters: {e}"),
        }
    }
}

impl std::error::Error for PasswordError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Argon2(e) => Some(e),
            Self::Random(e) => Some(e),
            _ => None,
        }
    }
}

impl From<PasswordError> for erp_fields::Error {
    fn from(e: PasswordError) -> Self {
        use erp_fields::ErrorKind;
        let kind = match &e {
            PasswordError::ParamOutOfRange { .. } | PasswordError::UnknownScheme { .. } => {
                ErrorKind::Validation
            }
            // a stored value we cannot use, or the machine refusing: never the user's input
            PasswordError::UnrecognisedFormat
            | PasswordError::Malformed { .. }
            | PasswordError::Random(_)
            | PasswordError::Argon2(_) => ErrorKind::Programming,
        };
        erp_fields::Error::new(kind, e.to_string()).with_source(e)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CHEAP: HashParams = HashParams(Cost::Pbkdf2 { rounds: 1_000 });

    #[test]
    fn pbkdf2_round_trip_uses_passlib_alphabet() -> Result<(), PasswordError> {
        // a salt whose standard base64 has '+' and '/' in it
        let salt = [
            0xfbu8, 0xef, 0xbe, 0xfb, 0xef, 0xbe, 0xfb, 0xef, 0xbe, 0xfb, 0xef, 0xbe,
        ];
        let stored = hash_with_salt(b"pw", &CHEAP, &salt)?;
        let salt_text = stored.split('$').nth(3).unwrap_or_default();
        assert!(
            salt_text.contains('.') && !salt_text.contains('+'),
            "{salt_text}"
        );
        let parsed = StoredHash::parse(&stored)?;
        assert!(parsed.verify(b"pw")?);
        assert!(!parsed.verify(b"pW")?);
        Ok(())
    }

    /// Lead decision 4, type design T2/T11: the cost comparison of stored hashes with the
    /// current parameters; lanes do not count as work.
    #[test]
    fn cost_comparison_with_the_current_parameters() -> Result<(), PasswordError> {
        let current = HashParams::DEFAULT;
        let same = StoredHash::parse(&hash_with_salt(b"pw", &current, &[7u8; 16])?)?;
        assert_eq!(same.compare_cost(&current), CostComparison::NotCheaper);
        let lighter = HashParams(Cost::Argon2id {
            memory_kib: limits::ARGON2_MEMORY_KIB_MIN / 2,
            time: limits::ARGON2_TIME_MIN,
        });
        let light = StoredHash::parse(&hash_with_salt(b"pw", &lighter, &[7u8; 16])?)?;
        assert_eq!(light.compare_cost(&current), CostComparison::Cheaper);
        assert_eq!(same.compare_cost(&lighter), CostComparison::NotCheaper);
        // m = 2432 KiB over 8 lanes, t = 2: an eighth of the default's work, whatever the lanes
        let lanes = StoredHash {
            cost: StoredCost::Argon2id {
                memory_kib: 2_432,
                time: 2,
                lanes: 8,
            },
            salt: Zeroizing::new(vec![7u8; 16]),
            tag: Zeroizing::new(vec![0u8; 32]),
        };
        assert_eq!(lanes.compare_cost(&current), CostComparison::Cheaper);
        let pbkdf2 = StoredHash::parse(&hash_with_salt(b"pw", &CHEAP, &[7u8; 16])?)?;
        assert_eq!(pbkdf2.compare_cost(&current), CostComparison::Incomparable);
        assert_eq!(
            pbkdf2.compare_cost(&HashParams::pbkdf2_sha512(600_000)?),
            CostComparison::Cheaper
        );
        assert_eq!(pbkdf2.compare_cost(&CHEAP), CostComparison::NotCheaper);
        Ok(())
    }

    /// The decoded salt and tag of a parsed hash (and of its clones) are wiped on drop. The
    /// assertion is on the field types: a `ZeroizeOnDrop` value wipes in its `Drop`, which is the
    /// property; the freed memory itself is not read (that would need `unsafe`).
    #[test]
    fn decoded_salt_and_tag_are_wiped_on_drop() -> Result<(), PasswordError> {
        fn wiped_on_drop<T: zeroize::ZeroizeOnDrop>(_: &T) {}
        for stored in [
            hash_with_salt(b"pw", &CHEAP, &[7u8; 16])?,
            hash_with_salt(b"pw", &HashParams::DEFAULT, &[7u8; 16])?,
        ] {
            let parsed = StoredHash::parse(&stored)?;
            let copy = parsed.clone();
            for hash in [&parsed, &copy] {
                wiped_on_drop(&hash.salt);
                wiped_on_drop(&hash.tag);
            }
            assert_eq!(parsed, copy);
            assert!(copy.verify(b"pw")?);
        }
        Ok(())
    }

    #[test]
    fn needs_rehash_on_scheme_and_cost() -> Result<(), PasswordError> {
        let stored = StoredHash::parse(&hash(b"pw", &CHEAP)?)?;
        assert!(stored.needs_rehash(&HashParams::DEFAULT));
        assert!(!stored.needs_rehash(&CHEAP));
        assert!(stored.needs_rehash(&HashParams::pbkdf2_sha512(600_000)?));
        Ok(())
    }

    /// Security review round 2 delta SD-1: parameters within a pool's reservation keep their
    /// memory up to it and their passes; PBKDF2 is unchanged.
    #[test]
    fn parameters_within_a_reservation() -> Result<(), PasswordError> {
        let min = limits::ARGON2_MEMORY_KIB_MIN;
        let big = HashParams::argon2id(2 * min, 3)?;
        assert_eq!(big.within_memory(min), HashParams::argon2id(min, 3)?);
        assert_eq!(big.within_memory(4 * min), big);
        assert_eq!(big.within_memory(0), HashParams::argon2id(min, 3)?);
        let pbkdf2 = HashParams::pbkdf2_sha512(600_000)?;
        assert_eq!(pbkdf2.within_memory(min), pbkdf2);
        Ok(())
    }

    #[test]
    fn strict_parsing() {
        let refused = [
            "",
            "pw",
            "$1$abc$def",
            "$argon2i$v=19$m=19456,t=2,p=1$c2FsdHNhbHQ$aGFzaGhhc2hoYXNoaGFzaA",
            "$argon2id$v=16$m=19456,t=2,p=1$c2FsdHNhbHQ$aGFzaGhhc2hoYXNoaGFzaA",
        ];
        for s in refused {
            assert!(
                matches!(StoredHash::parse(s), Err(PasswordError::UnrecognisedFormat)),
                "{s:?}"
            );
        }
        let malformed = [
            "$argon2id$v=19$m=19456,t=2$c2FsdHNhbHQ$aGFzaGhhc2hoYXNoaGFzaA",
            "$argon2id$v=19$t=2,m=19456,p=1$c2FsdHNhbHQ$aGFzaGhhc2hoYXNoaGFzaA",
            "$argon2id$v=19$m=019456,t=2,p=1$c2FsdHNhbHQ$aGFzaGhhc2hoYXNoaGFzaA",
            "$argon2id$v=19$m=19456,t=2,p=1$c2FsdHNhbHQ=$aGFzaGhhc2hoYXNoaGFzaA",
            "$argon2id$v=19$m=99999999,t=2,p=1$c2FsdHNhbHQ$aGFzaGhhc2hoYXNoaGFzaA",
            "$argon2id$v=19$m=19456,t=21,p=1$c2FsdHNhbHQ$aGFzaGhhc2hoYXNoaGFzaA",
            "$argon2id$v=19$m=19456,t=2,p=1$c2FsdA$aGFzaGhhc2hoYXNoaGFzaA",
            "$argon2id$v=19$m=19456,t=2,p=1,keyid=a$c2FsdHNhbHQ$aGFzaGhhc2hoYXNoaGFzaA",
            "$pbkdf2-sha512$0$c2FsdHNhbHQ$aGFzaA",
            "$pbkdf2-sha512$+600000$c2FsdHNhbHQ$aGFzaA",
            "$pbkdf2-sha512$600000$c2FsdHNhbHQ$aGFzaA",
            "$pbkdf2-sha512$99999999$c2FsdHNhbHQ$aGFzaA",
        ];
        for s in malformed {
            assert!(
                matches!(StoredHash::parse(s), Err(PasswordError::Malformed { .. })),
                "{s:?}"
            );
        }
        let huge = format!("$pbkdf2-sha512$1${}$x", "a".repeat(4_000));
        assert!(matches!(
            StoredHash::parse(&huge),
            Err(PasswordError::Malformed { .. })
        ));
    }

    #[test]
    fn floors_are_errors() {
        for (r, msg) in [
            (
                HashParams::argon2id(19_455, 2),
                "password.hashing.argon2_memory_kib = 19455 is outside 19456..=1048576",
            ),
            (
                HashParams::argon2id(19_456, 1),
                "password.hashing.argon2_time = 1 is outside 2..=20",
            ),
            (
                HashParams::pbkdf2_sha512(10_000_001),
                "password.hashing.rounds = 10000001 is outside 600000..=10000000",
            ),
        ] {
            let e = r.unwrap_err();
            assert_eq!(e.to_string(), msg);
            assert_eq!(
                erp_fields::Error::from(e).kind(),
                erp_fields::ErrorKind::Validation
            );
        }
    }

    #[test]
    fn bounded_escapes_and_truncates() {
        assert_eq!(bounded("a\nb"), "a\\nb");
        assert_eq!(bounded(&"é".repeat(70)), format!("{}…", "é".repeat(64)));
    }
}
