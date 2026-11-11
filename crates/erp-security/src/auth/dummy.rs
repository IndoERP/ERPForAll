//! The dummy verify (step 4, BUG-082): a login that has nothing to
//! verify (unknown or inactive login, empty password, empty or unrecognised stored hash) still
//! verifies a password against a hash made with the current scheme and parameters, so every
//! refusal costs the same work and the time of the answer does not tell which case it was.
//!
//! One dummy hash per parameter set and process, made from random input at first use and kept.
//! Making it is one hash with the same parameters, so the first use does that instead of a
//! verify: every use costs exactly one hash, the first included (rust review Low: no cold-cache
//! timing difference, no warm-up at `Auth::new` needed). The pool counts every use
//! ([`super::pool::HashPoolStats::dummy_verifies`]), which is how the tests assert the mechanism
//! (timing is not asserted).
//!
//! The denial cost floor (lead decision 4) uses it too: a refusal after verifying a stored hash
//! cheaper than the current parameters also runs a dummy verify at the current parameters
//! (`super::credential::verify_stored`), so no denial costs less than the current parameters'
//! work.
//!
//! The dummy is always built from parameters this node's pool can run ([`runnable`]): when the
//! database's parameters need more memory than a permit reserves (another node with a larger
//! `admission.hash_memory_kib` accepted them, or the scheme was switched over a larger stored
//! memory), the dummy uses the reservation instead, so the pool never refuses it and every unknown
//! or inactive login is still verified and counted (security review round 2 delta SD-1). The
//! service logs such settings once per generation (`AuthStats::settings_over_reservation`).

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock, PoisonError};

use erp_fields::Result;

use super::password::{self, HashParams, PasswordError, StoredHash};
use super::pool::PoolBinding;

fn cache() -> &'static Mutex<HashMap<HashParams, Arc<StoredHash>>> {
    static CACHE: OnceLock<Mutex<HashMap<HashParams, Arc<StoredHash>>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// `params` as `pool` can run them: Argon2id's memory at most what a permit reserves.
pub(crate) fn runnable(pool: &PoolBinding, params: &HashParams) -> HashParams {
    params.within_memory(pool.config().memory_kib())
}

/// Verify a random password against the dummy hash of `params` within `pool`'s reservation
/// ([`runnable`]) on `pool` (or make that hash, at first use: the same work).
///
/// # Errors
///
/// The pool's errors (an expired wait; never an over-reservation); [`PasswordError::Random`] when
/// the CSPRNG fails.
pub(crate) async fn verify(pool: &PoolBinding, params: &HashParams) -> Result<()> {
    let params = &runnable(pool, params);
    let cached = cache()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .get(params)
        .cloned();
    let Some(dummy) = cached else {
        let params_copy = *params;
        let made = pool
            .run(params.memory_kib(), move || make(&params_copy))
            .await??;
        cache()
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(*params, Arc::new(made));
        // counted once the work ran (an overloaded pool ran nothing)
        pool.count_dummy_verify();
        return Ok(());
    };
    let mut attempt = [0u8; 16];
    getrandom::fill(&mut attempt).map_err(PasswordError::Random)?;
    // the outcome is false (a random 16-byte password against a random one); it is computed, not
    // used: the work is the point
    let _matched = pool
        .run(dummy.memory_kib(), move || dummy.verify(&attempt))
        .await??;
    pool.count_dummy_verify();
    Ok(())
}

fn make(params: &HashParams) -> std::result::Result<StoredHash, PasswordError> {
    let mut input = [0u8; 32];
    getrandom::fill(&mut input).map_err(PasswordError::Random)?;
    StoredHash::parse(&password::hash(&input, params)?)
}
