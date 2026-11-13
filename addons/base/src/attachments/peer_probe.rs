// SPDX-License-Identifier: MIT OR Apache-2.0
//! Owner-side peer evidence. Commit every transition before external filesystem work.
//! No transaction/connection is held across a peer wait. This API never clears contests.
//! Verified evidence admits nothing yet: no cleanup path calls [`admit`], and cleanup refuses
//! every shared local store whatever the evidence (note of 2026-11-12).
use super::{
    ownership,
    runtime::{blob_error, runtime},
};
use erp_sdk::audit::{AttachmentProbeRecorded, AuthEvent, ProbeOutcome};
use erp_sdk::blob::{
    Namespace,
    peer::{PEER_STEPS, PeerCleanup, PeerObservation, verify_transcript},
};
use erp_sdk::{Error, Result, Transaction, Uuid};
const MISSING: &str = "Attachment shared-filesystem peer evidence is missing or invalid; run erp attachments probe-store --peer on both nodes.";
fn missing() -> Error {
    Error::internal(MISSING)
}
fn decode<T>(r: std::result::Result<T, impl std::fmt::Display>) -> Result<T> {
    r.map_err(|e| Error::db(format!("Attachment peer state could not be decoded: {e}")))
}

/// A bounded participant binding. CLI callers collect host/mount provenance from the OS/store.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Participant {
    node: String,
    host: String,
    mount: String,
}
impl Participant {
    /// Validate identifiers; this is not host attestation or proof of a successful probe.
    /// # Errors
    /// `Validation` for empty, oversized, non-ASCII/control-bearing identifiers or a noncanonical digest.
    pub fn new(node: &str, host: &str, mount: &str) -> Result<Self> {
        if node.is_empty()
            || node.len() > 63
            || host.is_empty()
            || host.len() > 128
            || !node
                .bytes()
                .chain(host.bytes())
                .all(|b| b.is_ascii_graphic())
            || mount.len() != 64
            || !mount
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err(Error::validation(
                "Invalid attachment peer participant binding",
            ));
        }
        Ok(Self {
            node: node.into(),
            host: host.into(),
            mount: mount.into(),
        })
    }
}
struct State {
    store: i16,
    session: Namespace,
    a: Participant,
    b: Option<Participant>,
    left: Vec<u8>,
    right: Vec<u8>,
    status: String,
    live: bool,
    fingerprint: String,
}
impl State {
    fn slot(&self, who: &Participant) -> Result<u8> {
        if &self.a == who {
            Ok(0)
        } else if self.b.as_ref() == Some(who) {
            Ok(1)
        } else {
            Err(missing())
        }
    }
    fn active(&self) -> Result<()> {
        if self.live && matches!(self.status.as_str(), "pending" | "proved") {
            Ok(())
        } else {
            Err(missing())
        }
    }
}
async fn load(tx: &mut Transaction, key: &str, expected: Option<&Namespace>) -> Result<State> {
    let identity = ownership::identity(tx).await?;
    let rows=tx.query("SELECT p.store_id,p.session::text,p.node_a,p.host_a,p.mount_a,p.node_b,p.host_b,p.mount_b,p.a,p.b,p.status,clock_timestamp()<p.deadline,p.fingerprint,p.protocol=1 AND p.namespace=s.namespace AND p.fingerprint=s.fingerprint AND p.incarnation=$2::text::uuid FROM ir_blob_peer_probe p JOIN ir_blob_store s ON s.id=p.store_id WHERE s.store_key=$1 AND s.role='own' FOR UPDATE OF p", &[&key,&identity.incarnation().as_str()]).await?;
    let r = rows.first().ok_or_else(missing)?;
    if !decode::<bool>(r.try_get(13))? {
        return Err(missing());
    }
    let session = Namespace::parse(&decode::<String>(r.try_get(1))?).map_err(blob_error)?;
    if expected.is_some_and(|e| e != &session) {
        return Err(missing());
    }
    let node: Option<String> = decode(r.try_get(5))?;
    let host: Option<String> = decode(r.try_get(6))?;
    let mount: Option<String> = decode(r.try_get(7))?;
    let b = match (node, host, mount) {
        (None, None, None) => None,
        (Some(n), Some(h), Some(m)) => Some(Participant::new(&n, &h, &m)?),
        _ => return Err(missing()),
    };
    Ok(State {
        store: decode(r.try_get(0))?,
        session,
        a: Participant::new(
            &decode::<String>(r.try_get(2))?,
            &decode::<String>(r.try_get(3))?,
            &decode::<String>(r.try_get(4))?,
        )?,
        b,
        left: decode(r.try_get(8))?,
        right: decode(r.try_get(9))?,
        status: decode(r.try_get(10))?,
        live: decode(r.try_get(11))?,
        fingerprint: decode(r.try_get(12))?,
    })
}
fn audit(
    tx: &mut Transaction,
    store: i16,
    session: &Namespace,
    outcome: ProbeOutcome,
    participants: &[&str],
) -> Result<()> {
    tx.audit_auth(AuthEvent::AttachmentProbeRecorded(
        AttachmentProbeRecorded::new(store, Uuid::parse(session.as_str())?, outcome, participants)?,
    ))
}

/// Invalidate old admission and start one bounded owner-operated attempt. Commit before I/O.
/// # Errors
/// Invalid timeout/binding, missing registered store, physical database identity or audit/database failure.
pub async fn start(
    tx: &mut Transaction,
    key: &str,
    fingerprint: &str,
    who: &Participant,
    timeout: u32,
) -> Result<Namespace> {
    if !(30..=300).contains(&timeout) {
        return Err(Error::validation(
            "Attachment peer timeout must be between 30 and 300 seconds",
        ));
    }
    let identity = ownership::identity(tx).await?;
    let rows=tx.query("SELECT id,namespace::text FROM ir_blob_store WHERE store_key=$1 AND role='own' AND fingerprint=$2 FOR UPDATE", &[&key,&fingerprint]).await?;
    let row = rows.first().ok_or_else(missing)?;
    let store: i16 = decode(row.try_get(0))?;
    let ns: String = decode(row.try_get(1))?;
    let session = Namespace::random().map_err(blob_error)?;
    audit(tx, store, &session, ProbeOutcome::Started, &[&who.node])?;
    tx.execute("INSERT INTO ir_blob_peer_probe(store_id,session,namespace,fingerprint,incarnation,protocol,node_a,host_a,mount_a,status,deadline) VALUES($1,$2::text::uuid,$3::text::uuid,$4,$5::text::uuid,1,$6,$7,$8,'pending',clock_timestamp()+$9::int8*interval '1 second') ON CONFLICT(store_id) DO UPDATE SET session=excluded.session,namespace=excluded.namespace,fingerprint=excluded.fingerprint,incarnation=excluded.incarnation,protocol=1,node_a=excluded.node_a,host_a=excluded.host_a,mount_a=excluded.mount_a,node_b=NULL,host_b=NULL,mount_b=NULL,a='',b='',status='pending',deadline=excluded.deadline", &[&store,&session.as_str(),&ns,&fingerprint,&identity.incarnation().as_str(),&who.node,&who.host,&who.mount,&i64::from(timeout)]).await?;
    Ok(session)
}
/// Register the second node. A duplicate/restarted process must start a fresh attempt.
/// # Errors
/// Stale, expired, already joined or same-host/node attempt; database failures.
pub async fn join(
    tx: &mut Transaction,
    key: &str,
    session: &Namespace,
    who: &Participant,
) -> Result<()> {
    let s = load(tx, key, Some(session)).await?;
    s.active()?;
    if s.b.is_some() || s.a.node == who.node || s.a.host == who.host {
        return Err(Error::validation(
            "Attachment peer probe requires two distinct nodes on distinct hosts",
        ));
    }
    tx.execute(
        "UPDATE ir_blob_peer_probe SET node_b=$2,host_b=$3,mount_b=$4 WHERE store_id=$1",
        &[&s.store, &who.node, &who.host, &who.mount],
    )
    .await?;
    Ok(())
}
/// Check a barrier in a short transaction; commit/drop it before waiting or invoking the store.
/// # Errors
/// Wrong session/participant, expiry or invalid phase/state.
pub async fn ready(
    tx: &mut Transaction,
    key: &str,
    session: &Namespace,
    who: &Participant,
    phase: u8,
) -> Result<bool> {
    let s = load(tx, key, Some(session)).await?;
    s.slot(who)?;
    if phase == PEER_STEPS {
        return match s.status.as_str() {
            "proved" | "verified" => Ok(true),
            "pending" if s.live => Ok(false),
            _ => Err(missing()),
        };
    }
    s.active()?;
    if phase >= PEER_STEPS {
        return Err(missing());
    }
    Ok(s.b.is_some() && s.left.len() >= usize::from(phase) && s.right.len() >= usize::from(phase))
}
/// Persist one real local observation. Callers cannot construct observations from success flags.
/// # Errors
/// Replay, wrong binding/phase, failed transcript or database failure.
pub async fn record(
    tx: &mut Transaction,
    key: &str,
    who: &Participant,
    o: &PeerObservation,
) -> Result<()> {
    let mut s = load(tx, key, Some(o.session())).await?;
    s.active()?;
    let slot = s.slot(who)?;
    let (mine, peer) = if slot == 0 {
        (&mut s.left, &s.right)
    } else {
        (&mut s.right, &s.left)
    };
    if slot != o.slot()
        || o.mount() != who.mount
        || mine.len() != usize::from(o.phase())
        || peer.len() < usize::from(o.phase())
        || mine.len() >= usize::from(PEER_STEPS)
    {
        return Err(missing());
    }
    mine.push(o.value());
    let status =
        if s.left.len() == usize::from(PEER_STEPS) && s.right.len() == usize::from(PEER_STEPS) {
            verify_transcript(&s.left, &s.right).map_err(blob_error)?;
            "proved"
        } else {
            "pending"
        };
    tx.execute(
        "UPDATE ir_blob_peer_probe SET a=$2,b=$3,status=$4 WHERE store_id=$1",
        &[&s.store, &s.left, &s.right, &status],
    )
    .await?;
    Ok(())
}
/// Commit verified evidence and its audit event only after actual private-artifact cleanup.
/// # Errors
/// Incomplete/wrong participant, stale binding, timeout, invalid transcript or audit failure.
pub async fn finalize(
    tx: &mut Transaction,
    key: &str,
    who: &Participant,
    proof: PeerCleanup,
) -> Result<()> {
    let s = load(tx, key, Some(proof.session())).await?;
    s.active()?;
    if s.slot(who)? != 0 || s.status != "proved" || proof.mount() != who.mount {
        return Err(missing());
    }
    verify_transcript(&s.left, &s.right).map_err(blob_error)?;
    let peer = s.b.as_ref().ok_or_else(missing)?;
    audit(
        tx,
        s.store,
        &s.session,
        ProbeOutcome::Verified,
        &[&s.a.node, &peer.node],
    )?;
    tx.execute(
        "UPDATE ir_blob_peer_probe SET status='verified' WHERE store_id=$1",
        &[&s.store],
    )
    .await?;
    Ok(())
}
/// Record a failed/cancelled attempt without restoring any previous evidence.
/// # Errors
/// Wrong session/participant or audit/database failure.
pub async fn fail(
    tx: &mut Transaction,
    key: &str,
    session: &Namespace,
    who: &Participant,
) -> Result<()> {
    let s = load(tx, key, Some(session)).await?;
    s.slot(who)?;
    if s.status == "verified" {
        return Err(missing());
    }
    let participants = match &s.b {
        Some(peer) => vec![s.a.node.as_str(), peer.node.as_str()],
        None => vec![s.a.node.as_str()],
    };
    audit(tx, s.store, &s.session, ProbeOutcome::Failed, &participants)?;
    tx.execute(
        "UPDATE ir_blob_peer_probe SET status='failed' WHERE store_id=$1",
        &[&s.store],
    )
    .await?;
    Ok(())
}
/// Check pair-scoped evidence against current physical store binding. This is one prerequisite,
/// never an owner/contest/observation/hold check or permission to delete on its own.
/// # Errors
/// Missing/invalid evidence, changed physical binding or database/store failure.
pub async fn admit(tx: &mut Transaction, key: &str, who: &Participant) -> Result<()> {
    let s = load(tx, key, None).await?;
    s.slot(who)?;
    if s.status != "verified" {
        return Err(missing());
    }
    verify_transcript(&s.left, &s.right).map_err(blob_error)?;
    let rt = runtime(&tx.env())?;
    if rt.unshared_local_stores().iter().any(|store| store == key) {
        return Err(missing());
    }
    let store = rt.store(key).ok_or_else(missing)?;
    store
        .verify_fingerprint(&s.fingerprint)
        .await
        .map_err(|e| missing().with_source(e))?;
    if store.peer_binding().map_err(|e| missing().with_source(e))? != who.mount {
        return Err(missing());
    }
    Ok(())
}
/// Diagnostic state only; it cannot authorize maintenance.
/// # Errors
/// Missing/changed identity or malformed/database state.
pub async fn status(tx: &mut Transaction, key: &str) -> Result<String> {
    let s = load(tx, key, None).await?;
    Ok(if s.status == "pending" && !s.live {
        "expired".into()
    } else {
        s.status
    })
}

/// Check completion for this exact session and participant, never a replacement attempt.
/// # Errors
/// Missing, replaced, failed or expired attempt; database errors.
pub async fn completed(
    tx: &mut Transaction,
    key: &str,
    session: &Namespace,
    who: &Participant,
) -> Result<bool> {
    let s = load(tx, key, Some(session)).await?;
    s.slot(who)?;
    if s.status == "verified" {
        Ok(true)
    } else {
        s.active()?;
        Ok(false)
    }
}
