//! The attempt-local audit buffer (design `m3-audit` §6): empty and allocation-free until the
//! first record, frozen by the commit preamble, savepoint-aware.

use erp_fields::Result;

use super::elevation::{ElevationGrant, ElevationObservation, ElevationReason, GrantKey};
use super::{
    AuditCall, AuditError, AuditOrigin, AuthEvent, MAX_ELEVATION_ENTRIES, MAX_EVENTS, limit,
};
use crate::Env;
use crate::outbox::actor_details;

/// Who acted, as the row's attribution columns need it (names, ids and kinds only: never an API
/// key name, which `Actor::ApiKey` also carries).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct Attribution {
    pub(crate) principal_kind: &'static str,
    pub(crate) subject: i64,
    pub(crate) actor_kind: &'static str,
    pub(crate) actor_id: i64,
    pub(crate) delegating_user_id: Option<i64>,
}

impl Attribution {
    pub(crate) fn from_env(env: &Env) -> Result<Self> {
        let principal = env.principal();
        let (kind, actor_id, delegating) =
            actor_details(principal.kind(), principal.subject(), principal.actor())?;
        Ok(Self {
            principal_kind: principal.kind().as_str(),
            subject: principal.subject().get(),
            actor_kind: kind.as_str(),
            actor_id,
            delegating_user_id: delegating,
        })
    }
}

/// One deduplicated elevation of the attempt.
#[derive(Debug, Clone)]
pub(crate) struct ElevationEntry {
    pub(crate) reason: ElevationReason,
    pub(crate) key: GrantKey,
    /// Sequence of the first block that opened it (scoped grants only).
    pub(crate) block_seq: Option<u32>,
    pub(crate) original: Attribution,
    pub(crate) effective_subject: i64,
    /// Every use, however deep it ran.
    pub(crate) count: u32,
    /// Uses that ran under a savepoint still open, by depth (ascending, merged).
    uses: Vec<(usize, u32)>,
    /// Uses that ran under a savepoint that rolled back. The evidence is kept: the elevation was
    /// used even though what it did was undone.
    pub(crate) rolled_back: u32,
}

impl ElevationEntry {
    fn matches(&self, o: &ElevationObservation) -> bool {
        self.reason == o.reason
            && self.key == o.grant.key()
            && self.original == o.original
            && self.effective_subject == o.effective_subject.get()
    }

    fn add_use(&mut self, depth: usize) -> Result<()> {
        self.count = self
            .count
            .checked_add(1)
            .ok_or_else(|| limit("elevation use count overflow"))?;
        match self.uses.last_mut() {
            Some((d, n)) if *d == depth => *n += 1,
            _ => {
                self.uses.push((depth, 1));
                self.uses.sort_unstable_by_key(|(d, _)| *d);
            }
        }
        Ok(())
    }

    /// `effective` while some use ran outside a rolled-back savepoint.
    pub(crate) fn state(&self) -> &'static str {
        if self.rolled_back == self.count {
            "savepoint_rolled_back"
        } else {
            "effective"
        }
    }
}

/// What a failed attempt hands to an independent attempt row.
///
/// Opaque: take it with
/// [`Transaction::take_audit_attempt`](crate::Transaction::take_audit_attempt) before the rollback
/// and hand it to [`AuditAttempt`](super::AuditAttempt).
#[derive(Debug, Default)]
pub struct AttemptEvidence {
    pub(crate) env: Option<Env>,
    pub(crate) call: Option<AuditCall>,
    pub(crate) origin: Option<AuditOrigin>,
    pub(crate) elevations: Vec<ElevationEntry>,
}

impl AttemptEvidence {
    /// Evidence for a call that never reached the transaction's buffer (a read, or a failure
    /// before the summary was set): the caller's environment and the call summary.
    pub fn for_call(env: Env, call: AuditCall) -> Self {
        Self {
            env: Some(env),
            call: Some(call),
            origin: None,
            elevations: Vec::new(),
        }
    }

    /// Fill the call summary when the buffer had none.
    #[must_use]
    pub fn with_call(mut self, call: AuditCall) -> Self {
        self.call.get_or_insert(call);
        self
    }

    /// Fill the environment when the buffer had none.
    #[must_use]
    pub fn with_env(mut self, env: Env) -> Self {
        self.env.get_or_insert(env);
        self
    }

    /// Whether an elevation was observed in the attempt (a read that elevated is audited).
    pub fn has_elevation(&self) -> bool {
        !self.elevations.is_empty()
    }

    /// Whether the buffer had anything: an environment, a summary or an elevation.
    pub(crate) fn has_content(&self) -> bool {
        self.env.is_some() || self.call.is_some() || !self.elevations.is_empty()
    }
}

/// The audit content of one transaction attempt.
#[derive(Debug, Default)]
pub(crate) struct AuditBuffer {
    env: Option<Env>,
    origin: Option<AuditOrigin>,
    call: Option<AuditCall>,
    auth: Vec<(usize, AuthEvent)>,
    elevations: Vec<ElevationEntry>,
    frozen: bool,
    /// What the buffer held when a rollback or a reset cleared it, until the next record or take:
    /// the evidence of an attempt that failed after its content was cleared (a failed commit).
    stash: Option<AttemptEvidence>,
}

impl AuditBuffer {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    fn check_open(&self) -> Result<()> {
        if self.frozen {
            Err(AuditError::Frozen.into())
        } else {
            Ok(())
        }
    }

    fn bind(&mut self, env: &Env) {
        // recording starts a new attempt: older cleared content is no longer its evidence
        self.stash = None;
        if self.env.is_none() {
            self.env = Some(env.clone());
        }
    }

    /// A row is written for this attempt: a call summary, an auth event or an elevation exists.
    pub(crate) fn wants_row(&self) -> bool {
        self.call.is_some() || !self.auth.is_empty() || !self.elevations.is_empty()
    }

    pub(crate) fn set_call(&mut self, call: AuditCall, env: &Env) -> Result<()> {
        self.check_open()?;
        if self.call.is_some() {
            return Err(AuditError::HeaderTwice.into());
        }
        self.bind(env);
        self.call = Some(call);
        Ok(())
    }

    pub(crate) fn set_origin(&mut self, origin: AuditOrigin) -> Result<()> {
        self.check_open()?;
        self.origin = Some(origin);
        Ok(())
    }

    pub(crate) fn push_auth(&mut self, depth: usize, event: AuthEvent, env: &Env) -> Result<()> {
        self.check_open()?;
        if self.auth.len() + self.elevations.len() >= MAX_EVENTS {
            return Err(limit(format!("more than {MAX_EVENTS} events in one row")).into());
        }
        self.bind(env);
        self.auth.push((depth, event));
        Ok(())
    }

    pub(crate) fn observe(
        &mut self,
        depth: usize,
        observation: ElevationObservation,
        env: &Env,
    ) -> Result<()> {
        self.check_open()?;
        if let Some(entry) = self.elevations.iter_mut().find(|e| e.matches(&observation)) {
            return entry.add_use(depth);
        }
        if self.elevations.len() >= MAX_ELEVATION_ENTRIES {
            return Err(limit(format!(
                "more than {MAX_ELEVATION_ENTRIES} distinct elevations in one transaction"
            ))
            .into());
        }
        if self.auth.len() + self.elevations.len() >= MAX_EVENTS {
            return Err(limit(format!("more than {MAX_EVENTS} events in one row")).into());
        }
        self.bind(env);
        let block_seq = match &observation.grant {
            ElevationGrant::Scoped { block, .. } => Some(block.seq()),
            ElevationGrant::Full { .. } => None,
        };
        let mut entry = ElevationEntry {
            reason: observation.reason,
            key: observation.grant.key(),
            block_seq,
            original: observation.original.clone(),
            effective_subject: observation.effective_subject.get(),
            count: 0,
            uses: Vec::new(),
            rolled_back: 0,
        };
        entry.add_use(depth)?;
        self.elevations.push(entry);
        Ok(())
    }

    /// `ROLLBACK TO` the savepoint at `depth`: auth events recorded deeper describe changes that
    /// did not happen and go; elevation uses recorded deeper stay as evidence, marked rolled back.
    pub(crate) fn rolled_back_to(&mut self, depth: usize) {
        self.auth.retain(|(d, _)| *d <= depth);
        for entry in &mut self.elevations {
            let mut moved = 0u32;
            entry.uses.retain(|(d, n)| {
                if *d > depth {
                    moved = moved.saturating_add(*n);
                    false
                } else {
                    true
                }
            });
            entry.rolled_back = entry.rolled_back.saturating_add(moved);
        }
    }

    pub(crate) fn released_to(&mut self, depth: usize) {
        for (d, _) in &mut self.auth {
            *d = (*d).min(depth);
        }
        for entry in &mut self.elevations {
            for (d, _) in &mut entry.uses {
                *d = (*d).min(depth);
            }
            entry.uses.sort_unstable_by_key(|(d, _)| *d);
            let mut merged: Vec<(usize, u32)> = Vec::with_capacity(entry.uses.len());
            for (d, n) in entry.uses.drain(..) {
                match merged.last_mut() {
                    Some((ld, ln)) if *ld == d => *ln = ln.saturating_add(n),
                    _ => merged.push((d, n)),
                }
            }
            entry.uses = merged;
        }
    }

    pub(crate) fn freeze(&mut self) -> Result<()> {
        if self.frozen {
            return Err(AuditError::Frozen.into());
        }
        self.frozen = true;
        Ok(())
    }

    pub(crate) fn clear(&mut self) {
        let previous = self.take_live();
        self.stash = previous.has_content().then_some(previous);
    }

    fn take_live(&mut self) -> AttemptEvidence {
        let mut taken = std::mem::take(self);
        AttemptEvidence {
            env: taken.env.take(),
            call: taken.call.take(),
            origin: taken.origin.take(),
            elevations: std::mem::take(&mut taken.elevations),
        }
    }

    /// Move the content a failed attempt keeps for its independent row out of the buffer. Auth
    /// events are dropped: they describe a change that did not happen.
    pub(crate) fn take_attempt(&mut self) -> AttemptEvidence {
        let stash = self.stash.take();
        let live = self.take_live();
        if live.has_content() {
            live
        } else {
            stash.unwrap_or(live)
        }
    }

    pub(crate) fn origin(&self) -> AuditOrigin {
        self.origin.unwrap_or(AuditOrigin::Internal)
    }
    pub(crate) fn call(&self) -> Option<&AuditCall> {
        self.call.as_ref()
    }
    pub(crate) fn env(&self) -> Option<&Env> {
        self.env.as_ref()
    }
    pub(crate) fn auth(&self) -> &[(usize, AuthEvent)] {
        &self.auth
    }
    pub(crate) fn elevations(&self) -> &[ElevationEntry] {
        &self.elevations
    }
}
