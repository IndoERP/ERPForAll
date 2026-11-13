//! Durable addon file receipts. All writes run under the owning lifecycle transaction.
use super::*;
use crate::artifact_identity::ArtifactSnapshot;

const IDENTITY_DDL: &str = "CREATE TABLE IF NOT EXISTS installed_addon_identity (
    name text PRIMARY KEY,
    content_hash text NOT NULL,
    installed_version text NOT NULL,
    source_kind text NOT NULL,
    recorded_at timestamptz NOT NULL DEFAULT clock_timestamp()
);
CREATE TABLE IF NOT EXISTS installed_addon_identity_audit (
    id bigserial PRIMARY KEY,
    operator text NOT NULL,
    note text NOT NULL,
    operation text NOT NULL,
    names text[] NOT NULL,
    content_hashes jsonb NOT NULL,
    created_at timestamptz NOT NULL DEFAULT clock_timestamp()
);
CREATE OR REPLACE FUNCTION installed_addon_identity_audit_insert_only() RETURNS trigger LANGUAGE plpgsql AS $audit$
BEGIN RAISE EXCEPTION 'Installed identity audit is insert-only'; END; $audit$;
DROP TRIGGER IF EXISTS installed_addon_identity_audit_insert_only ON installed_addon_identity_audit;
CREATE TRIGGER installed_addon_identity_audit_insert_only BEFORE UPDATE OR DELETE OR TRUNCATE ON installed_addon_identity_audit
FOR EACH STATEMENT EXECUTE FUNCTION installed_addon_identity_audit_insert_only();
REVOKE ALL ON installed_addon_identity, installed_addon_identity_audit FROM PUBLIC;
REVOKE ALL ON SEQUENCE installed_addon_identity_audit_id_seq FROM PUBLIC";
erp_orm::core_ddl!(
    INSTALLED_IDENTITY,
    owner = "erp-modules",
    name = "installed_addon_identity",
    purpose = "durable installed addon artifact identity and operator enrollment audit",
    tables = ["installed_addon_identity", "installed_addon_identity_audit"],
    sql = IDENTITY_DDL
);

pub(super) fn keep_linked() {
    std::hint::black_box(&INSTALLED_IDENTITY);
}

pub(super) fn valid_hash(value: &str) -> bool {
    let digest = value
        .strip_prefix("addon-v1:")
        .or_else(|| value.strip_prefix("package-v1:"));
    digest.is_some_and(|digest| {
        digest.len() == 64
            && digest
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    })
}

fn unknown(name: &ModuleName) -> Error {
    erp_fields::Error::validation(format!(
        "Addon '{name}' identity unknown, run identity-enroll first"
    ))
    .into()
}

fn mismatch(name: &ModuleName) -> Error {
    erp_fields::Error::validation(format!(
        "Addon '{name}' installed artifact identity does not match available content"
    ))
    .into()
}

fn row_error(error: tokio_postgres::Error) -> Error {
    erp_fields::Error::programming(format!("Invalid installed addon identity row: {error}")).into()
}

pub(super) async fn receipt(tx: &mut Transaction, name: &ModuleName) -> Result<String> {
    let present = tx
        .query(
            "SELECT pg_catalog.to_regclass('installed_addon_identity') IS NOT NULL",
            &[],
        )
        .await?;
    if !present[0].try_get::<_, bool>(0).map_err(row_error)? {
        return Err(unknown(name));
    }
    let rows = tx.query("SELECT content_hash FROM installed_addon_identity WHERE name OPERATOR(pg_catalog.=) $1", &[&name.as_str()]).await?;
    let row = rows.first().ok_or_else(|| unknown(name))?;
    let hash: String = row.try_get(0).map_err(row_error)?;
    if !valid_hash(&hash) {
        return Err(erp_fields::Error::validation(format!(
            "Addon '{name}' installed artifact identity is malformed"
        ))
        .into());
    }
    Ok(hash)
}

/// Capture bounded immutable file bytes before any consumer opens them independently.
pub(super) fn capture_entries(
    parts: &Parts,
    entries: &mut IndexMap<ModuleName, AddonEntry>,
) -> Result<BTreeMap<ModuleName, String>> {
    let mut hashes = BTreeMap::new();
    let mut total_bytes = 0usize;
    for (name, entry) in entries.iter_mut() {
        let hash =
            match entry.source_kind() {
                AddonSourceKind::DevFolder(path) => {
                    let snapshot = Arc::new(ArtifactSnapshot::capture(name.as_str(), path)?);
                    let bytes = snapshot
                        .files()
                        .values()
                        .try_fold(0usize, |total, bytes| total.checked_add(bytes.len()))
                        .ok_or_else(|| {
                            erp_fields::Error::validation("Addon snapshot bytes overflow")
                        })?;
                    total_bytes = total_bytes.checked_add(bytes).ok_or_else(|| {
                        erp_fields::Error::validation("Addon snapshot bytes overflow")
                    })?;
                    if total_bytes > 64 * 1024 * 1024 {
                        return Err(erp_fields::Error::validation(
                            "Lifecycle addon snapshots exceed 67108864 bytes",
                        )
                        .into());
                    }
                    let hash = snapshot.digest().to_owned();
                    if parts.code.contains_key(name.as_str()) {
                        match parts.native_artifact_identities.get(name) {
                            Some(linked) if linked == &hash => {}
                            Some(_) => return Err(erp_fields::Error::validation(format!(
                                "Addon '{name}' available files do not match linked native content"
                            ))
                            .into()),
                            None => {
                                return Err(erp_fields::Error::validation(format!(
                                    "Addon '{name}' linked native artifact identity is unknown"
                                ))
                                .into());
                            }
                        }
                    }
                    entry.attach_artifact_snapshot(snapshot)?;
                    hash
                }
                AddonSourceKind::Embedded => parts
                    .native_artifact_identities
                    .get(name)
                    .cloned()
                    .ok_or_else(|| {
                        erp_fields::Error::validation(format!(
                            "Addon '{name}' linked native artifact identity is unknown"
                        ))
                    })?,
                AddonSourceKind::Package { sha256: Some(hash) } => format!("package-v1:{hash}"),
                AddonSourceKind::Package { sha256: None } => {
                    return Err(erp_fields::Error::validation(format!(
                        "Addon '{name}' package artifact identity is unknown"
                    ))
                    .into());
                }
            };
        if !valid_hash(&hash) {
            return Err(erp_fields::Error::validation(format!(
                "Addon '{name}' available artifact identity is malformed"
            ))
            .into());
        }
        hashes.insert(name.clone(), hash);
    }
    Ok(hashes)
}

pub(super) fn entry_hashes(
    parts: &Parts,
    entries: &IndexMap<ModuleName, AddonEntry>,
) -> Result<BTreeMap<ModuleName, String>> {
    entries
        .iter()
        .map(|(name, entry)| {
            let hash = match entry.source_kind() {
                AddonSourceKind::DevFolder(_) => entry
                    .artifact_snapshot()
                    .map(|snapshot| snapshot.digest().to_owned())
                    .ok_or_else(|| {
                        erp_fields::Error::programming(format!(
                            "Addon '{name}' has no admitted artifact snapshot"
                        ))
                    })?,
                AddonSourceKind::Embedded => parts
                    .native_artifact_identities
                    .get(name)
                    .cloned()
                    .ok_or_else(|| unknown(name))?,
                AddonSourceKind::Package { sha256: Some(hash) } => format!("package-v1:{hash}"),
                AddonSourceKind::Package { sha256: None } => return Err(unknown(name)),
            };
            Ok((name.clone(), hash))
        })
        .collect()
}

fn source_kind(entry: &AddonEntry) -> &'static str {
    match entry.source_kind() {
        AddonSourceKind::DevFolder(_) => "folder",
        AddonSourceKind::Embedded => "embedded",
        AddonSourceKind::Package { .. } => "package",
    }
}

pub(super) async fn validate_sources(
    tx: &mut Transaction,
    names: &[ModuleName],
    entries: &IndexMap<ModuleName, AddonEntry>,
) -> Result<()> {
    for name in names {
        let entry = entries.get(name).ok_or_else(|| mismatch(name))?;
        let rows = tx
            .query(
                "SELECT source_kind FROM installed_addon_identity WHERE name=$1",
                &[&name.as_str()],
            )
            .await?;
        let old: String = rows
            .first()
            .ok_or_else(|| unknown(name))?
            .try_get(0)
            .map_err(row_error)?;
        if old != source_kind(entry) {
            return Err(mismatch(name));
        }
    }
    Ok(())
}

pub(super) fn verify_entries(entries: &IndexMap<ModuleName, AddonEntry>) -> Result<()> {
    for entry in entries.values() {
        if let Some(snapshot) = entry.artifact_snapshot() {
            snapshot.verify_unchanged()?;
        }
    }
    Ok(())
}

pub(super) async fn verify_entries_async(entries: &IndexMap<ModuleName, AddonEntry>) -> Result<()> {
    let entries = entries.clone();
    blocking(move || verify_entries(&entries)).await
}

/// A repeated forward publication can follow a file rollback without rolling receipts back.
/// Accept only a completely current set, retaining the exact original migration witnesses.
async fn validate_already_current_activation(
    tx: &mut Transaction,
    names: &[ModuleName],
    current: &BTreeMap<ModuleName, String>,
    previous: &BTreeMap<ModuleName, String>,
    entries: &IndexMap<ModuleName, AddonEntry>,
) -> Result<bool> {
    for name in names {
        let expected = current.get(name).ok_or_else(|| mismatch(name))?;
        if receipt(tx, name).await? != *expected {
            // This is a known unequal preimage, not a swallowed store/validation error.
            // The caller retains the ordinary strict previous-receipt validation.
            return Ok(false);
        }
    }
    validate_sources(tx, names, entries).await?;
    for name in names {
        let hash = current.get(name).ok_or_else(|| mismatch(name))?;
        // Package transitions have already been admitted against their actual stored archive;
        // previous release folder receipts do not describe an installed package (validate_names).
        if hash.starts_with("package-v1:") {
            continue;
        }
        let old = previous.get(name).ok_or_else(|| mismatch(name))?;
        if old != hash {
            require_forward_migration(tx, name, old, hash).await?;
        }
    }
    Ok(true)
}

async fn require_forward_migration(
    tx: &mut Transaction,
    name: &ModuleName,
    old: &str,
    current: &str,
) -> Result<()> {
    let proof = tx.query("SELECT EXISTS (SELECT 1 FROM installed_addon_identity_audit WHERE operation='forward-migrate' AND content_hashes->$1->>'previous'=$2 AND content_hashes->$1->>'current'=$3)", &[&name.as_str(), &old, &current]).await?;
    let proved: bool = proof[0].try_get(0).map_err(row_error)?;
    if !proved {
        return Err(erp_fields::Error::validation(format!(
            "Addon '{name}' has no committed forward migration for the candidate artifact"
        ))
        .into());
    }
    Ok(())
}

pub(super) async fn validate_names(
    tx: &mut Transaction,
    names: &[ModuleName],
    current: &BTreeMap<ModuleName, String>,
    previous: Option<&BTreeMap<ModuleName, String>>,
) -> Result<BTreeMap<ModuleName, String>> {
    let mut bound = BTreeMap::new();
    for name in names {
        let old = receipt(tx, name).await?;
        let expected = match previous {
            Some(_) if old.starts_with("package-v1:") => current.get(name),
            Some(previous) => previous.get(name),
            None => current.get(name),
        }
        .ok_or_else(|| mismatch(name))?;
        if &old != expected {
            return Err(mismatch(name));
        }
        if !current.contains_key(name) {
            return Err(mismatch(name));
        }
        bound.insert(name.clone(), old);
    }
    Ok(bound)
}

/// The old package bytes, row and receipt must agree before candidate effects begin.
pub(super) async fn validate_package_upgrade(
    tx: &mut Transaction,
    transition: &crate::wasm_lifecycle::PackageIdentityUpgrade,
) -> Result<()> {
    let name = &transition.name;
    if receipt(tx, name).await? != transition.previous {
        return Err(mismatch(name));
    }
    // Bound the transfer even if an operator weakened the table's size constraint.
    let rows = tx.query("SELECT sha256::text, substring(package_data FROM 1 FOR 67108865) FROM ir_module_package WHERE name=$1 ORDER BY id DESC LIMIT 1 FOR SHARE", &[&name.as_str()]).await?;
    let row = rows.first().ok_or_else(|| mismatch(name))?;
    let stored: String = row.try_get(0).map_err(row_error)?;
    let bytes: Vec<u8> = row.try_get(1).map_err(row_error)?;
    let package = crate::package::ErpAddonPackage::from_bytes(&bytes)?;
    if package.name() != name
        || package.archive_sha256() != stored.trim_end()
        || format!("package-v1:{}", package.archive_sha256()) != transition.previous
    {
        return Err(mismatch(name));
    }
    Ok(())
}

/// The package runtime writes its package/audit later in this same transaction.
pub(super) async fn commit_package_upgrade(
    tx: &mut Transaction,
    transition: &crate::wasm_lifecycle::PackageIdentityUpgrade,
    entry: &AddonEntry,
) -> Result<()> {
    if transition.previous == transition.current {
        return Ok(());
    }
    let changed = tx.execute("UPDATE installed_addon_identity SET content_hash=$2, installed_version=$3, recorded_at=clock_timestamp() WHERE name=$1 AND content_hash=$4 AND source_kind='package'", &[&transition.name.as_str(), &transition.current, &entry.manifest().version().to_string(), &transition.previous]).await?;
    if changed != 1 {
        return Err(mismatch(&transition.name));
    }
    Ok(())
}

pub(super) async fn insert_receipt(
    tx: &mut Transaction,
    entry: &AddonEntry,
    hash: &str,
) -> Result<()> {
    let source = source_kind(entry);
    tx.execute("INSERT INTO installed_addon_identity(name,content_hash,installed_version,source_kind) VALUES ($1,$2,$3,$4)", &[&entry.name().as_str(), &hash, &entry.manifest().version().to_string(), &source]).await?;
    Ok(())
}

pub(super) async fn remove_receipts(tx: &mut Transaction, names: &[ModuleName]) -> Result<()> {
    let names: Vec<_> = names.iter().map(ModuleName::as_str).collect();
    tx.execute(
        "DELETE FROM installed_addon_identity WHERE name OPERATOR(pg_catalog.=) ANY($1)",
        &[&names],
    )
    .await?;
    Ok(())
}

/// Successful candidate upgrades leave durable evidence in their own lifecycle transaction.
/// This records artifact preimages and candidates, not a database schema-version marker, and
/// deliberately does not change the installed receipt before forward activation succeeds.
pub(super) async fn record_forward_migrations(
    tx: &mut Transaction,
    upgraded: &[ModuleName],
    previous: &BTreeMap<ModuleName, String>,
    current: &BTreeMap<ModuleName, String>,
) -> Result<()> {
    let mut changes = BTreeMap::new();
    for name in upgraded {
        let old = previous.get(name).ok_or_else(|| unknown(name))?;
        let new = current.get(name).ok_or_else(|| mismatch(name))?;
        if old != new {
            changes.insert(
                name.as_str(),
                serde_json::json!({"previous": old, "current": new}),
            );
        }
    }
    if changes.is_empty() {
        return Ok(());
    }
    let names: Vec<_> = changes.keys().copied().collect();
    let hashes = serde_json::to_value(&changes).map_err(|error| {
        erp_fields::Error::programming(format!("Cannot encode forward migration evidence: {error}"))
    })?;
    tx.execute("INSERT INTO installed_addon_identity_audit(operator,note,operation,names,content_hashes) VALUES ('forward-release-migration','Candidate artifacts upgraded in lifecycle transaction','forward-migrate',$1,$2)", &[&names, &hashes]).await?;
    Ok(())
}

/// Resolve the complete installed preimage while the caller owns the lifecycle lock.
pub(super) async fn enrolled_receipts(
    tx: &mut Transaction,
    names: &[ModuleName],
) -> Result<BTreeMap<ModuleName, String>> {
    if names.is_empty() || names.len() > 64 {
        return Err(erp_fields::Error::validation(
            "Legacy identity requires between 1 and 64 installed addons",
        )
        .into());
    }
    let present = tx
        .query(
            "SELECT pg_catalog.to_regclass('installed_addon_identity') IS NOT NULL",
            &[],
        )
        .await?;
    let present: bool = present[0].try_get(0).map_err(row_error)?;
    let mut hashes = BTreeMap::new();
    for name in names {
        let missing = || {
            erp_fields::Error::validation(format!(
                "Addon '{name}' identity unknown, run erp module identity-enroll first"
            ))
        };
        if !present {
            return Err(missing().into());
        }
        let rows = tx.query("SELECT content_hash FROM installed_addon_identity WHERE name OPERATOR(pg_catalog.=) $1", &[&name.as_str()]).await?;
        let row = rows.first().ok_or_else(missing)?;
        let hash: String = row.try_get(0).map_err(row_error)?;
        if !valid_hash(&hash) {
            return Err(erp_fields::Error::validation(format!(
                "Addon '{name}' installed artifact identity is malformed"
            ))
            .into());
        }
        hashes.insert(name.clone(), hash);
    }
    Ok(hashes)
}

#[derive(Clone, Copy)]
enum IdentityWrite<'a> {
    Enroll(&'a [ModuleName]),
    Bootstrap(&'a BTreeMap<ModuleName, String>),
    Activate(&'a BTreeMap<ModuleName, String>),
    ActivateEnrolled,
}

fn operator_input(names: &[ModuleName], operator: &str, note: &str) -> Result<()> {
    if names.is_empty()
        || names.len() > 64
        || names.iter().collect::<BTreeSet<_>>().len() != names.len()
    {
        return Err(erp_fields::Error::validation(
            "Identity operation requires between 1 and 64 unique addon names",
        )
        .into());
    }
    if operator.trim().is_empty() || operator.len() > 256 || operator.chars().any(char::is_control)
    {
        return Err(erp_fields::Error::validation(
            "Identity operator must contain between 1 and 256 bytes without control characters",
        )
        .into());
    }
    if note.trim().is_empty()
        || note.len() > 4096
        || note.chars().any(|ch| ch.is_control() && ch != '\n')
    {
        return Err(erp_fields::Error::validation("Identity note must contain between 1 and 4096 bytes without control characters other than newline").into());
    }
    Ok(())
}

impl ModuleSystem {
    pub(super) async fn discover_admitted(
        &self,
    ) -> Result<(
        IndexMap<ModuleName, AddonEntry>,
        BTreeMap<ModuleName, String>,
    )> {
        let parts = Arc::clone(&self.parts);
        blocking(move || {
            let mut entries = parts.discover()?;
            let hashes = capture_entries(&parts, &mut entries)?;
            parts.check_code(&entries)?;
            Ok((entries, hashes))
        })
        .await
    }

    /// Explicit owner-operated enrollment of reviewed installed addon files, including legacy
    /// databases. Never call this from a request handler or as automatic mismatch recovery.
    /// # Errors
    /// Validation for invalid operator/note/names, non-installed modules or unattested files;
    /// database, source and lifecycle lock failures are preserved. Audit and receipts are atomic.
    pub async fn identity_enroll(
        &self,
        names: &[ModuleName],
        operator: &str,
        note: &str,
    ) -> Result<()> {
        operator_input(names, operator, note)?;
        self.identity_write(IdentityWrite::Enroll(names), operator, note)
            .await
    }

    /// Stamp candidate receipts after an operator has completed a successful forward release
    /// activation. `previous` is the verified previous release's per-addon file receipts.
    /// Never invoke this on restore or rollback. A repeat forward publication of the entirely
    /// current set validates sources and original migration proofs without rewriting receipts or
    /// audit rows. Unknown installed identity requires enrollment.
    /// # Errors
    /// Validation for missing/malformed/mismatched receipts, non-installed modules or invalid
    /// audit input; filesystem and database errors are preserved. A commit transport error may
    /// mean the transaction committed and must be reconciled by the operator before any retry.
    pub async fn identity_activate(
        &self,
        previous: &BTreeMap<ModuleName, String>,
        operator: &str,
        note: &str,
    ) -> Result<()> {
        self.identity_write(IdentityWrite::Activate(previous), operator, note)
            .await
    }

    /// Explicit legacy bootstrap of the complete installed set from admitted OLD files.
    /// `files` must be verified old-release folder receipts; packages use their admitted stored
    /// archives. No differing existing receipt is replaced. This method never loads hooks,
    /// migrates models or publishes a candidate identity. Use an enrollment-only composition
    /// without candidate native descriptors; normal native attestation is never bypassed.
    /// # Errors
    /// Invalid audit input, missing/changed old sources, differing enrollment, transient module
    /// state, lifecycle contention, database or atomic audit failure.
    pub async fn identity_enroll_legacy_installed(
        &self,
        files: &BTreeMap<ModuleName, String>,
        operator: &str,
        note: &str,
    ) -> Result<()> {
        self.identity_write(IdentityWrite::Bootstrap(files), operator, note)
            .await
    }

    /// Publish a forward candidate over a legacy release using enrolled installed preimages.
    /// The caller must verify that the previous release has no per-addon receipt field, and
    /// prove forward migration and successful activation just as for `identity_activate`.
    /// Receipts and migration proof are validated under lifecycle ownership.
    /// # Errors
    /// Missing/malformed receipts, missing migration proof, source mismatch or write failures.
    pub async fn identity_activate_enrolled(&self, operator: &str, note: &str) -> Result<()> {
        self.identity_write(IdentityWrite::ActivateEnrolled, operator, note)
            .await
    }

    async fn identity_write(
        &self,
        operation: IdentityWrite<'_>,
        operator: &str,
        note: &str,
    ) -> Result<()> {
        let mut optx = self.begin_operation(LockWait::Refuse).await?;
        let result = async {
            self.require_initialized(&mut optx.tx).await?;
            optx.tx.query(erp_sql::ddl::SCHEMA_LOCK, &[]).await?;
            erp_orm::core_ddl_run(&mut optx.tx, erp_orm::CorePhase::BeforeModels).await?;
            // ensure-only path: a deferred entry is completed (or refused) by the module operation
            // that creates its model table, never by an identity write
            let _deferred =
                erp_orm::core_ddl_run(&mut optx.tx, erp_orm::CorePhase::AfterModels).await?;
            let mut entries = self.discover().await?;
            let parts = Arc::clone(&self.parts);
            let (entries_out, hashes) = blocking(move || {
                let hashes = capture_entries(&parts, &mut entries)?;
                Ok((entries, hashes))
            }).await?;
            let entries = entries_out;
            // Package admission happened before this operation acquired ownership. A cooperating
            // node may have upgraded meanwhile: bind administration to the currently stored
            // archive under the same lifecycle transaction rather than enroll a stale index.
            for (name, hash) in &hashes {
                if hash.starts_with("package-v1:") {
                    let rows = optx.tx.query("SELECT sha256::text FROM ir_module_package WHERE name=$1 ORDER BY id DESC LIMIT 1", &[&name.as_str()]).await?;
                    let stored: String = rows.first().ok_or_else(|| mismatch(name))?.try_get(0).map_err(row_error)?;
                    if *hash != format!("package-v1:{}", stored.trim_end()) {
                        return Err(mismatch(name));
                    }
                }
            }
            let catalog = store::load(&mut optx.tx).await?;
            let activated_names: Vec<ModuleName> = catalog.rows().filter(|row| row.state() == ModuleState::Installed).map(|row| row.name().clone()).collect();
            let names = match operation {
                IdentityWrite::Enroll(names) => names,
                IdentityWrite::Bootstrap(_) | IdentityWrite::Activate(_) | IdentityWrite::ActivateEnrolled => activated_names.as_slice(),
            };
            operator_input(names, operator, note)?;
            let previous = match operation {
                IdentityWrite::Activate(hashes) => Some(hashes.clone()),
                IdentityWrite::ActivateEnrolled => Some(enrolled_receipts(&mut optx.tx, names).await?),
                IdentityWrite::Enroll(_) | IdentityWrite::Bootstrap(_) => None,
            };
            if let IdentityWrite::Bootstrap(files) = operation {
                if catalog.rows().any(|row| row.state().is_transient()) {
                    return Err(erp_fields::Error::validation("Legacy bootstrap requires a stable installed addon set with no pending operations").into());
                }
                for name in names {
                    let entry = entries.get(name).ok_or_else(|| mismatch(name))?;
                    match entry.source_kind() {
                        AddonSourceKind::DevFolder(_) if files.get(name) == hashes.get(name) && files.contains_key(name) => {}
                        AddonSourceKind::Package { sha256: Some(_) } => {}
                        AddonSourceKind::DevFolder(_) | AddonSourceKind::Embedded | AddonSourceKind::Package { sha256: None } => return Err(mismatch(name)),
                    }
                }
            }
            for name in names {
                if catalog.state(name.as_str()) != Some(ModuleState::Installed) {
                    return Err(erp_fields::Error::validation(format!("Identity enrollment requires installed addon '{name}'")).into());
                }
            }
            if let Some(previous) = previous.as_ref() {
                if validate_already_current_activation(&mut optx.tx, names, &hashes, previous, &entries).await? {
                    // Retain the final file recheck and transaction acknowledgement even though
                    // no receipt/audit row is written for this already-published candidate.
                    blocking(move || verify_entries(&entries)).await?;
                    return optx.commit().await;
                }
                validate_names(&mut optx.tx, names, &hashes, Some(previous)).await?;
                validate_sources(&mut optx.tx, names, &entries).await?;
            }
            let mut changed = BTreeMap::new();
            for name in names {
                let hash = hashes.get(name).ok_or_else(|| mismatch(name))?;
                let existing = optx.tx.query("SELECT content_hash, source_kind FROM installed_addon_identity WHERE name=$1", &[&name.as_str()]).await?;
                if matches!(operation, IdentityWrite::Bootstrap(_)) && let Some(row) = existing.first() {
                    let stored_source: String = row.try_get(1).map_err(row_error)?;
                    let entry = entries.get(name).ok_or_else(|| mismatch(name))?;
                    if stored_source != source_kind(entry) { return Err(mismatch(name)); }
                    let enrolled = receipt(&mut optx.tx, name).await?;
                    if enrolled != *hash {
                        return Err(erp_fields::Error::validation(format!("Legacy bootstrap refuses to replace enrolled identity for addon '{name}'")).into());
                    }
                }
                let identical = match existing.first() {
                    Some(row) => row.try_get::<_, String>(0).map_err(row_error)? == *hash,
                    None => false,
                };
                if !identical {
                    if previous.is_some() {
                        let old = receipt(&mut optx.tx, name).await?;
                        require_forward_migration(&mut optx.tx, name, &old, hash).await?;
                    }
                    let entry = entries.get(name).ok_or_else(|| mismatch(name))?;
                    // The addon key survives enrollment/activation. Replacing it with a
                    // DELETE would execute incoming FK delete actions on unrelated rows.
                    optx.tx.execute("INSERT INTO installed_addon_identity(name,content_hash,installed_version,source_kind) VALUES ($1,$2,$3,$4) ON CONFLICT(name) DO UPDATE SET content_hash=EXCLUDED.content_hash, installed_version=EXCLUDED.installed_version, source_kind=EXCLUDED.source_kind, recorded_at=clock_timestamp()", &[&name.as_str(), &hash, &entry.manifest().version().to_string(), &source_kind(entry)]).await?;
                    changed.insert(name.clone(), hash.clone());
                }
            }
            // Enrollment is always audited, even when the reviewed bytes were unchanged.
            // A forward activation with no changed hashes leaves every record as-is.
            if previous.is_none() || !changed.is_empty() {
                let names_text: Vec<_> = names.iter().map(ModuleName::as_str).collect();
                let audit_hashes: BTreeMap<_,_> = names.iter().map(|name| (name.as_str(), hashes.get(name).cloned().ok_or_else(|| mismatch(name)))).map(|(name, hash)| hash.map(|hash| (name,hash))).collect::<Result<_>>()?;
                let content_hashes = serde_json::to_value(audit_hashes).map_err(|error| erp_fields::Error::programming(format!("Cannot encode identity audit: {error}")))?;
                let operation = if previous.is_some() { "activate" } else { "enroll" };
                optx.tx.execute("INSERT INTO installed_addon_identity_audit(operator,note,operation,names,content_hashes) VALUES ($1,$2,$3,$4,$5)", &[&operator,&note,&operation,&names_text,&content_hashes]).await?;
            }
            blocking(move || verify_entries(&entries)).await?;
            optx.commit().await
        }.await;
        match result {
            Ok(()) => Ok(()),
            Err(error) => Err(rollback_after(&mut optx, error).await),
        }
    }
}
