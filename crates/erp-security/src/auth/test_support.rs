//! Default-off integration regression seams. These forward to the exact production functions;
//! most add no alternate behavior, policy bypass, or fallback key. The named per-Auth
//! API-key verdict gate pauses only the cancellation regression at a known counted verdict.
//! `login_selection_auth` explicitly bypasses only the temporary rollout refusal for fixtures;
//! merely enabling this module never changes ordinary `Auth::new` admission.

use erp_fields::Result;
use erp_orm::Orm;

use super::{EndReason, SessionId, session};

/// Invoke the private post-login cleanup entry point with its production arguments.
///
/// # Errors
/// The unchanged errors from the production cleanup function.
pub async fn end_others_once(
    orm: &Orm,
    uid: i64,
    new: &str,
    keep: i64,
    reason: EndReason,
) -> Result<usize> {
    super::login::end_others_once(
        orm,
        uid,
        new,
        keep,
        reason,
        &super::token_plan::TokenPlan::base(),
    )
    .await
}

/// Issue fixture tokens with the production token encoder and HMAC, never a copied algorithm.
///
/// # Errors
/// The unchanged key/field errors from the production encoder.
pub fn token(
    key: &super::secret::PurposeKey,
    id: &SessionId,
    fields: &session::TokenFields<'_>,
) -> Result<[u8; 32]> {
    session::token(key.bytes(), id, fields)
}

/// The exact private SQL expression used by the session-check result column.
pub fn observation_stamp_sql() -> &'static str {
    session::OBSERVATION_STAMP_SQL
}

/// Forward SQL-produced boundary values to the production checked wire decoder.
///
/// # Errors
/// The production decoder's Database error for values outside its xid/flag range.
pub fn decode_observation_stamp(encoded: i64) -> Result<(i64, bool)> {
    let stamp = session::ObservationStamp::decode(encoded)?;
    Ok((stamp.version(), stamp.current()))
}

/// Install a one-use, per-Auth gate after a counted API-key refusal is decided, before rollback.
///
/// The reached signal and release wait are synchronous so a current-thread Tokio test can have
/// a separate OS thread abort the request at this exact boundary. Never enabled in default builds.
///
/// # Errors
/// Programming when a gate was already installed or its mutex was poisoned.
pub fn api_key_verdict_gate(
    auth: &super::Auth,
    reached: std::sync::mpsc::SyncSender<()>,
    release: std::sync::mpsc::Receiver<()>,
) -> Result<()> {
    auth.api_key_verdict_gate(reached, release)
}

/// Freeze one Auth's existing prefilter clock; uses its production bucket algorithm.
/// This default-off seam keeps logical admission tests independent of host scheduling/refill.
///
/// # Errors
/// Programming when this prefilter's clock was already frozen.
pub fn freeze_prefilter(auth: &super::Auth, now: std::time::Instant) -> Result<()> {
    auth.freeze_prefilter(now)
}

/// Validate the exact production scheduler fence inside a fixture transaction.
///
/// # Errors
/// The unchanged production fence errors for a stale/expired token or missing state.
pub async fn gc_fence(tx: &mut erp_orm::Transaction, token: i64) -> Result<()> {
    super::gc_schedule::fence(tx, token).await
}

/// Persist an idle batch's cursor with the exact production checkpoint operation.
///
/// # Errors
/// The unchanged production database errors, or Concurrency when the epoch is stale/expired.
pub async fn gc_checkpoint(
    tx: &mut erp_orm::Transaction,
    token: i64,
    next: super::gc::GcCursor,
) -> Result<()> {
    super::gc_schedule::checkpoint(tx, token, next).await
}

/// Construct an Auth for custom-selection integration fixtures only.
///
/// This explicit, default-off seam bypasses only the temporary LOGIN-ROLLOUT refusal.
/// It never authorizes a served deployment. Ordinary [`super::Auth::new`] still refuses
/// selection overrides even when `test-support` is compiled, and every other startup check
/// (including the SESSION composition refusal) executes unchanged.
///
/// # Errors
/// The unchanged model, schema, settings, pool and composition errors of `Auth::new`.
pub async fn login_selection_auth(
    orm: Orm,
    config: super::AuthConfig,
    pool: &'static super::HashPool,
) -> Result<super::Auth> {
    super::Auth::for_selection_tests(orm, config, pool).await
}
