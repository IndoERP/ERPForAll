// SPDX-License-Identifier: MIT OR Apache-2.0
//! Bounded store-side records's owner fence. No database authority lives here.

use crate::{AttachmentStore, BlobError, Namespace};

/// Maximum bytes accepted from a store-side ownership record.
pub const OWNER_MARKER_MAX_BYTES: usize = 256;
const RECORD_BYTES: usize = 108;
const MAGIC: &[u8; 8] = b"ERPOWN01";

pub(crate) fn invalid(reason: &'static str) -> BlobError {
    BlobError::InvalidOwnerMarker { reason }
}

/// Database identity: PostgreSQL cluster identifier, database OID and database incarnation.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct OwnerIdentity {
    system_identifier: u64,
    database_oid: u32,
    incarnation: Namespace,
}

impl OwnerIdentity {
    /// Build an identity obtained by the operator-side composition.
    ///
    /// # Errors
    /// `InvalidOwnerMarker` for zero cluster/database identifiers.
    pub fn new(
        system_identifier: u64,
        database_oid: u32,
        incarnation: Namespace,
    ) -> Result<Self, BlobError> {
        if system_identifier == 0 || database_oid == 0 {
            return Err(invalid("database identity must be positive"));
        }
        Ok(Self {
            system_identifier,
            database_oid,
            incarnation,
        })
    }

    /// PostgreSQL system identifier, kept as an integer without JSON number conversion.
    pub fn system_identifier(&self) -> u64 {
        self.system_identifier
    }
    /// PostgreSQL database OID.
    pub fn database_oid(&self) -> u32 {
        self.database_oid
    }
    /// Operator-managed database incarnation UUID.
    pub fn incarnation(&self) -> &Namespace {
        &self.incarnation
    }
}

/// One intended owner advancement, also the losing operation recorded in `_contested`.
///
/// The coordinator must persist this exact operation before the put. Neither a matching
/// identity alone nor this record alone is permission to delete content.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct OwnerMarker {
    identity: OwnerIdentity,
    generation: u64,
    operation: Namespace,
    observed_at: u64,
}

impl OwnerMarker {
    /// Build a record; `observed_at` is Unix seconds supplied by the coordinator.
    ///
    /// # Errors
    /// `InvalidOwnerMarker` for generation zero.
    pub fn new(
        identity: OwnerIdentity,
        generation: u64,
        operation: Namespace,
        observed_at: u64,
    ) -> Result<Self, BlobError> {
        if generation == 0 {
            return Err(invalid("generation must be positive"));
        }
        Ok(Self {
            identity,
            generation,
            operation,
            observed_at,
        })
    }

    /// The database that attempted the advancement.
    pub fn identity(&self) -> &OwnerIdentity {
        &self.identity
    }
    /// The positive generation.
    pub fn generation(&self) -> u64 {
        self.generation
    }
    /// The unique, durably recorded advancement operation.
    pub fn operation(&self) -> &Namespace {
        &self.operation
    }
    /// Unix seconds at which the operation was prepared.
    pub fn observed_at(&self) -> u64 {
        self.observed_at
    }

    /// Encode a fixed-length v1 record. Integers are little endian; UUIDs are canonical ASCII.
    pub fn encode(&self) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(RECORD_BYTES);
        bytes.extend_from_slice(MAGIC);
        bytes.extend_from_slice(&self.identity.system_identifier.to_le_bytes());
        bytes.extend_from_slice(&self.identity.database_oid.to_le_bytes());
        bytes.extend_from_slice(self.identity.incarnation.as_str().as_bytes());
        bytes.extend_from_slice(&self.generation.to_le_bytes());
        bytes.extend_from_slice(self.operation.as_str().as_bytes());
        bytes.extend_from_slice(&self.observed_at.to_le_bytes());
        bytes
    }

    /// Parse exactly one canonical record; unknown versions, extra bytes and invalid UUIDs fail.
    ///
    /// # Errors
    /// `InvalidOwnerMarker`; errors never echo the untrusted bytes.
    pub fn decode(bytes: &[u8]) -> Result<Self, BlobError> {
        let bad = || invalid("expected bounded canonical v1 record");
        if bytes.len() != RECORD_BYTES || bytes.get(..8) != Some(MAGIC.as_slice()) {
            return Err(bad());
        }
        let system_identifier = u64::from_le_bytes(
            bytes
                .get(8..16)
                .ok_or_else(bad)?
                .try_into()
                .map_err(|_| bad())?,
        );
        let database_oid = u32::from_le_bytes(
            bytes
                .get(16..20)
                .ok_or_else(bad)?
                .try_into()
                .map_err(|_| bad())?,
        );
        let uuid = |start, end| {
            let text =
                std::str::from_utf8(bytes.get(start..end).ok_or_else(bad)?).map_err(|_| bad())?;
            Namespace::parse(text).map_err(|_| bad())
        };
        let identity = OwnerIdentity::new(system_identifier, database_oid, uuid(20, 56)?)?;
        let generation = u64::from_le_bytes(
            bytes
                .get(56..64)
                .ok_or_else(bad)?
                .try_into()
                .map_err(|_| bad())?,
        );
        let operation = uuid(64, 100)?;
        let observed_at = u64::from_le_bytes(
            bytes
                .get(100..108)
                .ok_or_else(bad)?
                .try_into()
                .map_err(|_| bad())?,
        );
        Self::new(identity, generation, operation, observed_at)
    }
}

/// Opaque backend concurrency token. Preserve both values through durable recovery.
///
/// The ETag is a single exact token, never a wildcard or a list of HTTP preconditions.
#[derive(Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct OwnerVersion {
    etag: String,
    object_version: Option<String>,
}

impl std::fmt::Debug for OwnerVersion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OwnerVersion").finish_non_exhaustive()
    }
}

impl OwnerVersion {
    /// Restore a token from backend metadata or persisted coordinator state.
    ///
    /// # Errors
    /// `InvalidOwnerMarker` for empty, oversized or unsafe tokens.
    pub fn new(etag: &str, object_version: Option<&str>) -> Result<Self, BlobError> {
        let opaque =
            |s: &str| !s.is_empty() && s.len() <= 1024 && s.bytes().all(|b| b.is_ascii_graphic());
        if !opaque(etag)
            || etag == "*"
            || etag.contains(',')
            || etag.starts_with("W/")
            || object_version.is_some_and(|v| !opaque(v))
        {
            return Err(invalid("expected one bounded opaque version token"));
        }
        Ok(Self {
            etag: etag.to_owned(),
            object_version: object_version.map(str::to_owned),
        })
    }
    /// Exact ETag for conditional writes and current-object checks.
    pub fn etag(&self) -> &str {
        &self.etag
    }
    /// Backend version identifier, when supplied by the store.
    pub fn object_version(&self) -> Option<&str> {
        self.object_version.as_deref()
    }
}

/// An owner record and the version from the same atomic store read.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct OwnerState {
    marker: OwnerMarker,
    version: OwnerVersion,
}

impl OwnerState {
    pub(crate) fn new(marker: OwnerMarker, version: OwnerVersion) -> Self {
        Self { marker, version }
    }
    /// The owner advancement record.
    pub fn marker(&self) -> &OwnerMarker {
        &self.marker
    }
    /// The concurrency token of that record.
    pub fn version(&self) -> &OwnerVersion {
        &self.version
    }
}

/// Versioned contest or operator resolution receipt.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct ContestState {
    marker: OwnerMarker,
    version: OwnerVersion,
    resolved: bool,
}
impl ContestState {
    pub(crate) fn new(marker: OwnerMarker, version: OwnerVersion, resolved: bool) -> Self {
        Self {
            marker,
            version,
            resolved,
        }
    }
    /// Losing marker, or the exact owner operation chosen by recovery.
    pub fn marker(&self) -> &OwnerMarker {
        &self.marker
    }
    /// Exact concurrency token, including resolved-to-active transitions.
    pub fn version(&self) -> &OwnerVersion {
        &self.version
    }
    /// Whether this is an operator resolution receipt rather than an active contest.
    pub fn resolved(&self) -> bool {
        self.resolved
    }
    /// Bounded canonical record; legacy active records retain their v1 representation.
    pub fn encode(&self) -> Vec<u8> {
        contest_bytes(&self.marker, self.resolved)
    }
    /// Decode a bounded record and its atomically read version.
    /// # Errors
    /// InvalidOwnerMarker for unknown, malformed or oversized formats.
    pub fn decode(bytes: &[u8], version: OwnerVersion) -> Result<Self, BlobError> {
        let (marker, resolved) = if bytes.len() == RECORD_BYTES {
            (OwnerMarker::decode(bytes)?, false)
        } else if bytes.len() == RECORD_BYTES + 8 && bytes.get(..8) == Some(b"ERPRSL01") {
            (OwnerMarker::decode(&bytes[8..])?, true)
        } else {
            return Err(invalid("expected bounded contest or resolution record"));
        };
        Ok(Self::new(marker, version, resolved))
    }
}
pub(crate) fn contest_bytes(marker: &OwnerMarker, resolved: bool) -> Vec<u8> {
    if resolved {
        let mut b = b"ERPRSL01".to_vec();
        b.extend(marker.encode());
        b
    } else {
        marker.encode()
    }
}

pub(crate) fn owner_path(ns: &Namespace) -> String {
    format!("{ns}/_owner")
}
pub(crate) fn contested_path(ns: &Namespace) -> String {
    format!("{ns}/_contested")
}

/// Exercise the real operations against a fresh namespace, never an application's owner.
pub(crate) async fn probe_contract(
    store: &dyn AttachmentStore,
    ns: &Namespace,
) -> Result<(), BlobError> {
    let missing = |capability| BlobError::OwnerCapabilityMissing { capability };
    let first = OwnerMarker::new(
        OwnerIdentity::new(1, 1, ns.clone())?,
        1,
        Namespace::random()?,
        0,
    )?;
    let next = OwnerMarker::new(first.identity().clone(), 2, Namespace::random()?, 0)?;
    let version = store.put_owner(ns, &first, None).await?;
    match store.put_owner(ns, &next, None).await {
        Err(BlobError::OwnerConflict) => {}
        Err(error) => return Err(error),
        Ok(_) => return Err(missing("create-only owner put")),
    }
    let found = store
        .owner(ns)
        .await?
        .ok_or_else(|| missing("owner read-after-write"))?;
    if found.marker() != &first || found.version() != &version {
        return Err(missing("create-only owner put"));
    }
    store.check_owner(ns, &version).await?;
    let newer = store.put_owner(ns, &next, Some(&version)).await?;
    if newer == version {
        return Err(missing("distinct owner versions"));
    }
    match store.put_owner(ns, &first, Some(&version)).await {
        Err(BlobError::OwnerConflict) => {}
        Err(error) => return Err(error),
        Ok(_) => return Err(missing("If-Match owner put")),
    }
    let found = store
        .owner(ns)
        .await?
        .ok_or_else(|| missing("owner read-after-write"))?;
    if found.marker() != &next || found.version() != &newer {
        return Err(missing("If-Match owner put"));
    }
    match store.check_owner(ns, &version).await {
        Err(BlobError::OwnerConflict) => {}
        Err(error) => return Err(error),
        Ok(()) => return Err(missing("conditional owner HEAD")),
    }
    store.check_owner(ns, &newer).await?;
    store.create_contested(ns, &first).await?;
    store.create_contested(ns, &next).await?;
    if store.contested(ns).await? != Some(first.clone()) {
        return Err(missing("create-only contested put"));
    }
    let active = store
        .contest_state(ns)
        .await?
        .ok_or_else(|| missing("versioned contest read"))?;
    let chosen = store
        .owner(ns)
        .await?
        .ok_or_else(|| missing("current owner read"))?;
    let receipt = store.resolve_contested(ns, &chosen, Some(&active)).await?;
    if !receipt.resolved()
        || receipt.marker() != chosen.marker()
        || store.contested(ns).await?.is_some()
    {
        return Err(missing("conditional contest resolution"));
    }
    store.create_contested(ns, &first).await?;
    match store.resolve_contested(ns, &chosen, Some(&active)).await {
        Err(BlobError::OwnerConflict) => {}
        Err(e) => return Err(e),
        Ok(_) => return Err(missing("stale contest resolution refusal")),
    }
    Ok(())
}

pub(crate) fn finish_probe(
    probe: Result<(), BlobError>,
    cleanup: Result<(), BlobError>,
) -> Result<(), BlobError> {
    match (probe, cleanup) {
        (result, Ok(())) => result,
        (Ok(()), Err(cleanup)) => Err(BlobError::OwnerProbeCleanup {
            probe: None,
            cleanup: Box::new(cleanup),
        }),
        (Err(probe), Err(cleanup)) => Err(BlobError::OwnerProbeCleanup {
            probe: Some(Box::new(probe)),
            cleanup: Box::new(cleanup),
        }),
    }
}
