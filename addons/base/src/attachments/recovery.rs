// SPDX-License-Identifier: MIT OR Apache-2.0
//! Owner-only, audited attachment recovery. No content keys are removed.
use super::{
    ownership,
    runtime::{blob_error, col_error, runtime, store_by_key},
};
use erp_sdk::blob::{
    AttachmentStore, ContestState, Namespace, OwnerIdentity, OwnerMarker, OwnerState, OwnerVersion,
};
use erp_sdk::{BeginOptions, Error, Isolation, Orm, Result, SUPERUSER_ID, Transaction};
use std::sync::Arc;

const CHANGED: &str = "Attachment ownership snapshot changed; inspect and confirm again.";
const IN_FLIGHT: &str = "An attachment ownership advancement is in flight; let the heartbeat settle it, then inspect and confirm again.";
const ACTIVE: &str =
    "Attachment ownership recovery is active; resume its audited operation before maintenance.";
fn decode<T>(r: std::result::Result<T, impl std::fmt::Display>) -> Result<T> {
    r.map_err(|e| col_error("attachment recovery", e))
}
fn version(etag: Option<String>, v: Option<String>) -> Result<Option<OwnerVersion>> {
    match (etag, v) {
        (Some(e), v) => Ok(Some(
            OwnerVersion::new(&e, v.as_deref()).map_err(blob_error)?,
        )),
        (None, None) => Ok(None),
        (None, Some(_)) => Err(Error::db("Attachment recovery version lacks ETag")),
    }
}
/// Operator decision reason; no arbitrary text or credential material enters the journal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    /// Restore the selected original database.
    RestorePrimary,
    /// Retire an isolated competing clone.
    RetireClone,
}
impl Reason {
    fn text(self) -> &'static str {
        match self {
            Self::RestorePrimary => "restore_primary",
            Self::RetireClone => "retire_clone",
        }
    }
}
/// An observed, configured namespace. Fields cannot be supplied independently by a caller.
#[derive(Clone)]
#[non_exhaustive]
pub struct Inspection {
    key: String,
    store_id: i16,
    ns: Namespace,
    fp: String,
    who: OwnerIdentity,
    store: Arc<dyn AttachmentStore>,
    owner: Option<OwnerState>,
    contest: Option<ContestState>,
}
impl std::fmt::Debug for Inspection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Inspection")
            .field("namespace", &self.ns)
            .finish_non_exhaustive()
    }
}
impl Inspection {
    /// Explicit confirmation binds database incarnation, namespace and exact observed tokens.
    pub fn confirmation(&self) -> String {
        use base64::Engine;
        let mut parts = vec![
            self.who.system_identifier().to_string(),
            self.who.database_oid().to_string(),
            self.who.incarnation().as_str().to_owned(),
            self.ns.as_str().to_owned(),
            self.fp.clone(),
        ];
        for v in [
            self.owner.as_ref().map(OwnerState::version),
            self.contest.as_ref().map(ContestState::version),
        ] {
            match v {
                Some(v) => {
                    parts.push(v.etag().to_owned());
                    parts.push(v.object_version().map_or_else(String::new, str::to_owned));
                }
                None => parts.push(String::new()),
            }
        }
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(parts.join("\0"))
    }
    /// Actual selected database identity, checked against its registered incarnation.
    pub fn identity(&self) -> &OwnerIdentity {
        &self.who
    }
    /// Observed owner, for the operator's decision; this is not a maintenance capability.
    pub fn owner_marker(&self) -> Option<&OwnerMarker> {
        self.owner.as_ref().map(OwnerState::marker)
    }
    /// Selected namespace, never a caller-supplied path override.
    pub fn namespace(&self) -> &Namespace {
        &self.ns
    }
}
async fn begin_tx(orm: &Orm) -> Result<Transaction> {
    orm.begin_opts(
        BeginOptions::new(SUPERUSER_ID)
            .isolation(Isolation::ReadCommitted)
            .on_primary()
            .operation_identity(erp_sdk::OperationIdentity::generate()?),
    )
    .await
}
/// Verify actual relation ownership, including search-path resolution; System Env alone is insufficient.
/// # Errors
/// Access when any required table is absent, resolves in the wrong schema or is not owned by current_user.
pub async fn require_owner(tx: &mut Transaction) -> Result<()> {
    for table in [
        "ir_blob_store",
        "ir_blob_owner_identity",
        "ir_blob_owner_state",
        "ir_blob_owner_claim",
        "ir_blob_owner_recovery",
        "erp_event.ir_audit_entry",
    ] {
        let rows=tx.query("SELECT n.nspname::text,pg_get_userbyid(c.relowner)=current_user FROM pg_class c JOIN pg_namespace n ON n.oid=c.relnamespace WHERE c.oid=to_regclass($1::text)",&[&table]).await?;
        let schema = if table.starts_with("erp_event.") {
            "erp_event"
        } else {
            "public"
        };
        let valid = match rows.first() {
            Some(r) => decode::<String>(r.try_get(0))? == schema && decode::<bool>(r.try_get(1))?,
            None => false,
        };
        if !valid {
            return Err(Error::access(format!(
                "Attachment ownership recovery requires the owner connection and the installed {table} table in {schema}."
            )));
        }
    }
    Ok(())
}
pub(super) async fn active_store(tx: &mut Transaction, id: i16) -> Result<bool> {
    let r=tx.query("SELECT EXISTS(SELECT 1 FROM ir_blob_owner_recovery WHERE store_id=$1 AND phase<>'completed')",&[&id]).await?;
    decode(r[0].try_get(0))
}
pub(super) async fn active_key(tx: &mut Transaction, key: &str) -> Result<bool> {
    let r=tx.query("SELECT EXISTS(SELECT 1 FROM ir_blob_owner_recovery r JOIN ir_blob_store s ON s.id=r.store_id WHERE s.store_key=$1 AND r.phase<>'completed')",&[&key]).await?;
    decode(r[0].try_get(0))
}
pub(super) async fn refuse_active(tx: &mut Transaction, key: &str) -> Result<()> {
    if active_key(tx, key).await? {
        return Err(active_refusal());
    }
    Ok(())
}
/// The error of a store whose operator recovery is still active.
pub(super) fn active_refusal() -> Error {
    Error::internal(ACTIVE)
}
async fn binding(tx: &mut Transaction, key: &str) -> Result<Inspection> {
    require_owner(tx).await?;
    let who = ownership::identity(tx).await?;
    let rt = runtime(&tx.env())?;
    let s = store_by_key(tx, &rt, key).await?;
    let fp = s
        .store
        .fingerprint()
        .ok_or_else(|| Error::programming("Attachment recovery requires a store fingerprint"))?;
    let rows = tx
        .query(
            "SELECT namespace::text,fingerprint FROM ir_blob_owner_state WHERE store_id=$1",
            &[&s.id],
        )
        .await?;
    let row = rows.first().ok_or_else(|| {
        Error::internal(
            "Attachment owner coordinator must be initialized before operator recovery.",
        )
    })?;
    let namespace: String = decode(row.try_get(0))?;
    let registered: String = decode(row.try_get(1))?;
    if namespace != s.namespace.as_str() || registered != fp {
        return Err(Error::internal(
            "Attachment ownership coordinator binding changed; maintenance admission is required.",
        ));
    }
    Ok(Inspection {
        key: key.to_owned(),
        store_id: s.id,
        ns: s.namespace,
        fp,
        who,
        store: s.store,
        owner: None,
        contest: None,
    })
}
/// Inspect without committing SQL changes or mutating ownership. Store opening is composition's job.
/// # Errors
/// Privilege, identity, binding, malformed marker and storage failures.
pub async fn inspect(orm: &Orm, key: &str) -> Result<Inspection> {
    let mut tx = begin_tx(orm).await?;
    let mut i = binding(&mut tx, key).await?;
    tx.rollback().await?;
    drop(tx);
    i.store
        .verify_fingerprint(&i.fp)
        .await
        .map_err(blob_error)?;
    i.owner = i.store.owner(&i.ns).await.map_err(blob_error)?;
    i.contest = i.store.contest_state(&i.ns).await.map_err(blob_error)?;
    Ok(i)
}
async fn audit(tx: &mut Transaction, id: i64, phase: &str) -> Result<()> {
    tx.audit_attachment_claim(phase, id).await
}
/// One persisted claim operation. Reloading uses its UUID, never newly invented intent.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct Claim {
    id: Namespace,
    key: String,
}
impl Claim {
    /// Durable recovery operation id for status/resume.
    pub fn id(&self) -> &Namespace {
        &self.id
    }
}
/// Whether a pending advancement may still be writing the store owner.
///
/// `observed_at` is the first preparation and every failing retry re-executes the same marker, so
/// age alone cannot prove that no write is in flight. The advancement counts as in flight while it
/// is younger than the `jobs.job_deadline_secs` catalogue hard bound on the database clock (a marker dated in the future is
/// young), or while any job of the ownership singleton holds a live lease: every destructive
/// attachment handler shares that singleton, and this is the liveness test of the queue's claim.
/// Once both have lapsed the claim proceeds; its higher generation supersedes the stale marker.
async fn in_flight(tx: &mut Transaction, orm: &Orm, pending: &[u8], now: u64) -> Result<bool> {
    let observed = OwnerMarker::decode(pending)
        .map_err(blob_error)?
        .observed_at();
    // Old markers have no admitted deadline. A lower current policy cannot prove they ended.
    // Wait the catalogue hard envelope, and still fence on any live singleton lease.
    orm.job_settings_snapshot().await?;
    let deadline = erp_sdk::jobs::Setting::JobDeadlineSecs.bounds().1;
    if observed > now || now - observed < deadline {
        return Ok(true);
    }
    let r = tx
        .query(
            "SELECT EXISTS(SELECT 1 FROM erp_job WHERE singleton_key=$1 AND state='running' AND lease_until > clock_timestamp())",
            &[&ownership::SINGLETON],
        )
        .await?;
    decode(r[0].try_get(0))
}
/// Commit a decision and its audit before any owner/contest mutation.
/// # Errors
/// Audit unavailability, changed inspection, privilege/binding errors or an active recovery.
pub async fn begin(orm: &Orm, i: &Inspection, reason: Reason) -> Result<Claim> {
    let mut tx = begin_tx(orm).await?;
    ownership::lock(&mut tx).await?;
    let current = binding(&mut tx, &i.key).await?;
    if current.who != i.who
        || current.store_id != i.store_id
        || current.ns != i.ns
        || current.fp != i.fp
    {
        return Err(Error::internal(CHANGED));
    }
    refuse_active(&mut tx, &i.key).await?;
    let r=tx.query("SELECT confirmed_marker,pending_marker,mode FROM ir_blob_owner_state WHERE store_id=$1 FOR UPDATE",&[&i.store_id]).await?;
    let row = r.first().ok_or_else(|| Error::internal(CHANGED))?;
    let confirmed: Option<Vec<u8>> = decode(row.try_get(0))?;
    let pending: Option<Vec<u8>> = decode(row.try_get(1))?;
    let mode: String = decode(row.try_get(2))?;
    // One database-clock reading: it ages the pending marker and dates the claim's marker.
    let t = tx
        .query(
            "SELECT floor(extract(epoch FROM clock_timestamp()))::int8",
            &[],
        )
        .await?;
    let seconds: i64 = decode(t[0].try_get(0))?;
    let now = u64::try_from(seconds).map_err(|_| Error::db("Invalid recovery time"))?;
    // A committed advancement outside the contest modes may still be writing the store owner; a
    // claim taken now would record a stale prior owner and fail on every resume.
    if let Some(bytes) = pending.as_deref()
        && mode != "contest_pending"
        && mode != "contested"
        && in_flight(&mut tx, orm, bytes, now).await?
    {
        return Err(Error::internal(IN_FLIGHT));
    }
    // Compared under the coordinator lock: no advancement of this database can prepare or
    // settle until this decision commits, after which `refuse_active` stops new ones.
    if i.store.owner(&i.ns).await.map_err(blob_error)? != i.owner
        || i.store.contest_state(&i.ns).await.map_err(blob_error)? != i.contest
    {
        return Err(Error::internal(CHANGED));
    }
    let mut g = i.owner.as_ref().map_or(0, |o| o.marker().generation());
    for b in [confirmed, pending].into_iter().flatten() {
        g = g.max(OwnerMarker::decode(&b).map_err(blob_error)?.generation());
    }
    let generation = g.checked_add(1).ok_or_else(|| {
        Error::internal("Attachment owner generation exhausted; recovery cannot advance it.")
    })?;
    let id = Namespace::random().map_err(blob_error)?;
    let marker =
        OwnerMarker::new(i.who.clone(), generation, id.clone(), now).map_err(blob_error)?;
    let owner = i.owner.as_ref();
    let contest = i.contest.as_ref();
    let rows=tx.query("INSERT INTO ir_blob_owner_claim(operation,store_id,namespace,fingerprint,chosen,previous_coordinator,prior_owner,prior_etag,prior_version,contest,contest_etag,contest_version,reason) VALUES($1::text::uuid,$2,$3::text::uuid,$4,$5,(SELECT to_jsonb(o) FROM ir_blob_owner_state o WHERE o.store_id=$2),$6,$7,$8,$9,$10,$11,$12) RETURNING id",&[&id.as_str(),&i.store_id,&i.ns.as_str(),&i.fp,&marker.encode(),&owner.map(|o|o.marker().encode()),&owner.map(|o|o.version().etag()),&owner.and_then(|o|o.version().object_version()),&contest.map(ContestState::encode),&contest.map(|c|c.version().etag()),&contest.and_then(|c|c.version().object_version()),&reason.text()]).await?;
    let dbid: i64 = decode(rows[0].try_get(0))?;
    tx.execute(
        "INSERT INTO ir_blob_owner_recovery(claim_id,store_id) VALUES($1,$2)",
        &[&dbid, &i.store_id],
    )
    .await?;
    tx.execute("UPDATE ir_blob_owner_state SET observation_id=NULL,observation_started_at=NULL,observation_not_before=NULL WHERE store_id=$1",&[&i.store_id]).await?;
    audit(&mut tx, dbid, "decision").await?;
    tx.commit().await.map_err(|e| Error::new(e.kind(),format!("{}; attachment recovery decision {} commit is unconfirmed; inspect this operation before retrying.", e.message(), id)).with_source(e))?;
    Ok(Claim {
        id,
        key: i.key.clone(),
    })
}
/// Load a committed operation and validate its live database/store binding.
/// # Errors
/// Unknown operation, privilege/binding/identity errors.
pub async fn resume(orm: &Orm, key: &str, id: &Namespace) -> Result<Claim> {
    let c = Claim {
        id: id.clone(),
        key: key.to_owned(),
    };
    c.load(orm).await?;
    Ok(c)
}
struct Loaded {
    id: i64,
    i: Inspection,
    marker: OwnerMarker,
    prior: Option<OwnerVersion>,
    contest: Option<ContestState>,
    phase: String,
    owner: Option<OwnerVersion>,
    prior_marker: Option<OwnerMarker>,
}
impl Claim {
    async fn load(&self, orm: &Orm) -> Result<Loaded> {
        let mut tx = begin_tx(orm).await?;
        ownership::lock(&mut tx).await?;
        let i = binding(&mut tx, &self.key).await?;
        let r=tx.query("SELECT c.id,c.namespace::text,c.fingerprint,c.chosen,c.prior_etag,c.prior_version,c.contest,c.contest_etag,c.contest_version,r.phase,r.owner_etag,r.owner_version,c.prior_owner FROM ir_blob_owner_claim c JOIN ir_blob_owner_recovery r ON r.claim_id=c.id AND r.store_id=c.store_id WHERE c.operation=$1::text::uuid AND c.store_id=$2",&[&self.id.as_str(),&i.store_id]).await?;
        let row = r
            .first()
            .ok_or_else(|| Error::user("Unknown attachment recovery operation for this store."))?;
        let ns: String = decode(row.try_get(1))?;
        let fp: String = decode(row.try_get(2))?;
        let bytes: Vec<u8> = decode(row.try_get(3))?;
        let marker = OwnerMarker::decode(&bytes).map_err(blob_error)?;
        if ns != i.ns.as_str()
            || fp != i.fp
            || marker.identity() != &i.who
            || marker.operation() != &self.id
        {
            return Err(Error::internal(CHANGED));
        }
        let cb: Option<Vec<u8>> = decode(row.try_get(6))?;
        let cv = version(decode(row.try_get(7))?, decode(row.try_get(8))?)?;
        let contest = match (cb, cv) {
            (Some(b), Some(v)) => Some(ContestState::decode(&b, v).map_err(blob_error)?),
            (None, None) => None,
            _ => return Err(Error::db("Invalid recovery contest tuple")),
        };
        let loaded = Loaded {
            id: decode(row.try_get(0))?,
            i,
            marker,
            prior: version(decode(row.try_get(4))?, decode(row.try_get(5))?)?,
            contest,
            phase: decode(row.try_get(9))?,
            owner: version(decode(row.try_get(10))?, decode(row.try_get(11))?)?,
            prior_marker: decode::<Option<Vec<u8>>>(row.try_get(12))?
                .as_deref()
                .map(OwnerMarker::decode)
                .transpose()
                .map_err(blob_error)?,
        };
        tx.rollback().await?;
        Ok(loaded)
    }
    /// Read durable phase. This does not retry external effects.
    /// # Errors
    /// As resume.
    pub async fn phase(&self, orm: &Orm) -> Result<String> {
        Ok(self.load(orm).await?.phase)
    }
    async fn phase_commit(
        &self,
        orm: &Orm,
        l: &Loaded,
        next: &str,
        owner: &OwnerState,
        receipt: Option<&ContestState>,
    ) -> Result<()> {
        let mut tx = begin_tx(orm).await?;
        ownership::lock(&mut tx).await?;
        require_owner(&mut tx).await?;
        let bound = binding(&mut tx, &self.key).await?;
        if bound.ns != l.i.ns || bound.fp != l.i.fp || bound.who != l.i.who {
            return Err(Error::internal(CHANGED));
        }
        let rows = tx
            .query(
                "SELECT phase FROM ir_blob_owner_recovery WHERE claim_id=$1 FOR UPDATE",
                &[&l.id],
            )
            .await?;
        let phase: String = decode(
            rows.first()
                .ok_or_else(|| Error::db("Missing recovery progress"))?
                .try_get(0),
        )?;
        if phase != l.phase {
            return Err(Error::internal(
                "Attachment recovery phase changed; resume the same operation.",
            ));
        }
        tx.execute("UPDATE ir_blob_owner_recovery SET phase=$2,owner_etag=$3,owner_version=$4,resolution_etag=$5,resolution_version=$6,updated_at=clock_timestamp() WHERE claim_id=$1",&[&l.id,&next,&owner.version().etag(),&owner.version().object_version(),&receipt.map(|r|r.version().etag()),&receipt.and_then(|r|r.version().object_version())]).await?;
        if next == "completed" {
            tx.execute("UPDATE ir_blob_owner_state SET mode='healthy',confirmed_marker=$2,confirmed_etag=$3,confirmed_version=$4,pending_marker=NULL,pending_etag=NULL,pending_version=NULL,pending_purpose=NULL,last_success_at=clock_timestamp(),probe_protocol=1,failure_code=NULL,observation_id=NULL,observation_started_at=NULL,observation_not_before=NULL WHERE store_id=$1",&[&l.i.store_id,&l.marker.encode(),&owner.version().etag(),&owner.version().object_version()]).await?;
        }
        audit(&mut tx, l.id, next).await?;
        tx.commit().await
    }
    /// Resume this exact decision through owner CAS, authorized resolution and audited completion.
    /// # Errors
    /// Incomplete operations retain their state. Inspect/resume the reported id; never delete markers.
    pub async fn run(&self, orm: &Orm) -> Result<()> {
        let result = self.run_inner(orm).await;
        result.map_err(|e|Error::new(e.kind(),format!("{}; attachment recovery {} is incomplete; inspect/resume this operation and preserve markers.",e.message(),self.id)).with_source(e))
    }
    async fn run_inner(&self, orm: &Orm) -> Result<()> {
        for _ in 0..4 {
            let l = self.load(orm).await?;
            if l.phase == "completed" {
                return Ok(());
            }
            l.i.store
                .verify_fingerprint(&l.i.fp)
                .await
                .map_err(blob_error)?;
            if l.phase == "decision" {
                l.i.store.probe_owner().await.map_err(blob_error)?;
            }
            let current = l.i.store.owner(&l.i.ns).await.map_err(blob_error)?;
            if l.phase == "decision" {
                if !current.as_ref().is_some_and(|o| o.marker() == &l.marker) {
                    if current.as_ref().map(OwnerState::version) != l.prior.as_ref()
                        || current.as_ref().map(OwnerState::marker) != l.prior_marker.as_ref()
                    {
                        return Err(Error::internal(CHANGED));
                    }
                    l.i.store
                        .put_owner(&l.i.ns, &l.marker, l.prior.as_ref())
                        .await
                        .map_err(blob_error)?;
                }
                let owner =
                    l.i.store
                        .owner(&l.i.ns)
                        .await
                        .map_err(blob_error)?
                        .ok_or_else(|| Error::internal(CHANGED))?;
                if owner.marker() != &l.marker {
                    return Err(Error::internal(CHANGED));
                }
                self.phase_commit(orm, &l, "owner", &owner, None).await?;
            } else {
                let owner = current.ok_or_else(|| Error::internal(CHANGED))?;
                if owner.marker() != &l.marker || Some(owner.version()) != l.owner.as_ref() {
                    return Err(Error::internal(CHANGED));
                }
                if l.phase == "owner" {
                    self.phase_commit(orm, &l, "resolution", &owner, None)
                        .await?;
                } else if l.phase == "resolution" {
                    let receipt =
                        l.i.store
                            .resolve_contested(&l.i.ns, &owner, l.contest.as_ref())
                            .await
                            .map_err(blob_error)?;
                    if l.i.store.owner(&l.i.ns).await.map_err(blob_error)?.as_ref() != Some(&owner)
                        || l.i
                            .store
                            .contest_state(&l.i.ns)
                            .await
                            .map_err(blob_error)?
                            .as_ref()
                            != Some(&receipt)
                    {
                        return Err(Error::internal(CHANGED));
                    }
                    self.phase_commit(orm, &l, "completed", &owner, Some(&receipt))
                        .await?;
                } else {
                    return Err(Error::db("Invalid recovery phase"));
                }
            }
        }
        Err(Error::internal(
            "Attachment recovery did not complete its bounded phase sequence.",
        ))
    }
}
