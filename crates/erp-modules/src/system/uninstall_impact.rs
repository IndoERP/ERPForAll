//! Bounded, schema-only confirmed uninstall. One immutable plan governs digest and effects.
use super::*;
use serde::Serialize;
use sha2::{Digest as _, Sha256};

/// Aggregate admitted identities across dropped rows, schema objects and metadata deletions.
pub const MAX_UNINSTALL_TARGETS: usize = 10_000;
/// Maximum serialized public impact bytes.
pub const MAX_UNINSTALL_IMPACT_BYTES: usize = 65_536;
/// SQL deadline, lowered further by an existing shorter lifecycle deadline.
pub const UNINSTALL_STATEMENT_DEADLINE_MS: u64 = 5_000;
const MAX_LOCKED_TABLES: usize = 512;
const MAX_PLANNING_BYTES: i64 = 1_048_576;
/// Hard byte ceiling for serialized private canonical plan material.
pub const MAX_UNINSTALL_CANONICAL_BYTES: usize = 8 * 1024 * 1024;

/// A configured effect provider the confirmed uninstall planner cannot describe.
///
/// This diagnostic is negative-only: absence of a blocker never establishes authority,
/// artifact identity, schema safety or a confirmed impact. It performs no callbacks or SQL.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum UninstallCompositionBlocker {
    /// Opening would read generations through an unstabilized Signal source.
    ConfiguredSignal,
    /// Package lifecycle callbacks have no exact confirmed effect plan yet.
    PackageRuntime,
    /// A custom loader may purge records through undescribed callbacks.
    CustomDataLoader,
    /// Registered host services must not be silently skipped by the executor.
    LifecycleServices,
}
impl UninstallCompositionBlocker {
    /// Fixed, bounded public diagnostic naming the prerequisite task.
    pub const fn message(self) -> &'static str {
        match self {
            Self::ConfiguredSignal => {
                "Native lifecycle opening with configured Signal is unsupported until a later release"
            }
            Self::PackageRuntime => {
                "Uninstall impact cannot describe package runtime lifecycle callbacks; unsupported until a later release"
            }
            Self::CustomDataLoader => {
                "Uninstall impact cannot describe custom data loader purge callbacks; unsupported until a later release"
            }
            Self::LifecycleServices => {
                "Uninstall impact cannot describe registered lifecycle services; unsupported until a later release"
            }
        }
    }
}

/// An acknowledgment of a versioned impact, never an authorization credential.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct ImpactFingerprint(String);
impl ImpactFingerprint {
    /// Canonical versioned SHA-256 encoding.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A complete model table that the confirmed operation drops.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[non_exhaustive]
pub struct DroppedModelImpact {
    model: String,
    table: String,
    row_count: String,
}
impl DroppedModelImpact {
    /// Technical model name.
    pub fn model(&self) -> &str {
        &self.model
    }
    /// Unqualified physical table name, resolved in the locked schema.
    pub fn table(&self) -> &str {
        &self.table
    }
    /// Exact SQL COUNT, represented as a decimal string.
    pub fn row_count(&self) -> &str {
        &self.row_count
    }
}

/// Public metadata effect, containing counts only, without record identities or values.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[non_exhaustive]
pub struct MetadataImpact {
    table: String,
    kind: &'static str,
    row_count: String,
}

impl MetadataImpact {
    /// Physical table affected by the metadata operation.
    pub fn table(&self) -> &str {
        &self.table
    }
    /// Explicit operation kind (delete, schema-object removal or signal bump).
    pub fn kind(&self) -> &str {
        self.kind
    }
    /// Exact deletion/object count, or the declared one-row signal effect, as decimal text.
    pub fn row_count(&self) -> &str {
        &self.row_count
    }
}

/// Complete supported impact. Unsupported effects return an error instead of partial output.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[non_exhaustive]
pub struct UninstallImpact {
    version: u32,
    requested: Vec<ModuleName>,
    resolved: Vec<ModuleName>,
    dropped_models: Vec<DroppedModelImpact>,
    metadata: Vec<MetadataImpact>,
    #[serde(serialize_with = "serialize_count")]
    target_count: usize,
    fingerprint: ImpactFingerprint,
}
impl UninstallImpact {
    /// Sorted unique explicit request.
    pub fn requested(&self) -> &[ModuleName] {
        &self.requested
    }
    /// Sorted reverse dependency closure.
    pub fn resolved(&self) -> &[ModuleName] {
        &self.resolved
    }
    /// Complete model removals with exact counts.
    pub fn dropped_models(&self) -> &[DroppedModelImpact] {
        &self.dropped_models
    }
    /// Described metadata and signal effects, without record identities or values.
    pub fn metadata(&self) -> &[MetadataImpact] {
        &self.metadata
    }
    /// Aggregate bounded identity cardinality (including schema objects and metadata).
    pub fn target_count(&self) -> usize {
        self.target_count
    }
    /// Confirmation acknowledgment to submit unchanged with the same request.
    pub fn fingerprint(&self) -> &ImpactFingerprint {
        &self.fingerprint
    }
}

#[derive(Debug, Serialize)]
struct DeleteEffect {
    table: String,
    ids: Vec<i64>,
    count: i64,
}
#[derive(Debug, Serialize)]
struct DropEffect {
    table: String,
    oid: String,
    schema_objects: Vec<String>,
    schema_object_count: i64,
    ids: Vec<i64>,
    count: i64,
}
struct Plan {
    impact: UninstallImpact,
    schema: String,
    drops: Vec<DropEffect>,
    deletes: Vec<DeleteEffect>,
    remaining: Orm,
    entries: IndexMap<ModuleName, AddonEntry>,
}

fn serialize_count<S: serde::Serializer>(
    count: &usize,
    serializer: S,
) -> std::result::Result<S::Ok, S::Error> {
    serializer.serialize_str(&count.to_string())
}

fn unsupported(effect: impl std::fmt::Display) -> Error {
    erp_fields::Error::validation(format!(
        "Uninstall impact cannot describe {effect}; unsupported until a later release"
    ))
    .into()
}
fn shape(e: tokio_postgres::Error) -> Error {
    erp_fields::Error::programming(format!("Invalid uninstall impact query result: {e}")).into()
}
struct CappedWriter {
    bytes: Vec<u8>,
    limit: usize,
    exceeded: bool,
}
impl std::io::Write for CappedWriter {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        let Some(next_len) = self
            .bytes
            .len()
            .checked_add(data.len())
            .filter(|length| *length <= self.limit)
        else {
            self.exceeded = true;
            return Err(std::io::Error::other(
                "uninstall canonical byte limit exceeded",
            ));
        };
        if next_len > self.bytes.capacity() {
            let desired = self
                .bytes
                .capacity()
                .saturating_mul(2)
                .max(next_len)
                .min(self.limit);
            self.bytes
                .try_reserve_exact(desired - self.bytes.len())
                .map_err(std::io::Error::other)?;
        }
        self.bytes.extend_from_slice(data);
        Ok(data.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
fn json<T: Serialize + ?Sized>(value: &T) -> Result<Vec<u8>> {
    json_with_limit(value, MAX_UNINSTALL_CANONICAL_BYTES)
}
fn json_with_limit<T: Serialize + ?Sized>(value: &T, limit: usize) -> Result<Vec<u8>> {
    let mut writer = CappedWriter {
        bytes: Vec::new(),
        limit,
        exceeded: false,
    };
    match serde_json::to_writer(&mut writer, value) {
        Ok(()) => Ok(writer.bytes),
        Err(_) if writer.exceeded => Err(erp_fields::Error::validation(format!(
            "Uninstall impact exceeds {limit} canonical bytes"
        ))
        .into()),
        Err(error) => Err(erp_fields::Error::programming(format!(
            "Cannot encode uninstall impact: {error}"
        ))
        .with_source(error)
        .into()),
    }
}

fn admit_bytes(bytes: usize) -> Result<()> {
    if bytes > MAX_UNINSTALL_IMPACT_BYTES {
        return Err(erp_fields::Error::validation(format!(
            "Uninstall impact exceeds {MAX_UNINSTALL_IMPACT_BYTES} response bytes"
        ))
        .into());
    }
    Ok(())
}
fn admit_targets(total: usize, additional: usize) -> Result<usize> {
    match total.checked_add(additional) {
        Some(count) if count <= MAX_UNINSTALL_TARGETS => Ok(count),
        _ => Err(erp_fields::Error::validation(format!(
            "Uninstall impact exceeds {MAX_UNINSTALL_TARGETS} target identities"
        ))
        .into()),
    }
}
fn validate_request(names: &[ModuleName]) -> Result<()> {
    if names.is_empty() || names.len() > 64 {
        return Err(erp_fields::Error::validation(
            "Uninstall requires between 1 and 64 addon names",
        )
        .into());
    }
    let mut seen = BTreeSet::new();
    if names.iter().any(|name| !seen.insert(name)) {
        return Err(erp_fields::Error::validation("Uninstall requires unique addon names").into());
    }
    Ok(())
}
fn validate_fingerprint(raw: &str) -> Result<()> {
    let valid = raw.strip_prefix("uninstall-v1:").is_some_and(|digest| {
        digest.len() == 64
            && digest
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    });
    if !valid {
        return Err(erp_fields::Error::validation("Invalid uninstall impact fingerprint").into());
    }
    Ok(())
}
fn qualified(schema: &str, table: &str) -> String {
    format!("{}.{}", erp_sql::quote(schema), erp_sql::quote(table))
}

/// Preview protects relation definitions while allowing ordinary DML; confirmation fences
/// membership through comparison, effects and commit.
///
/// # Lock-fence invariant
///
/// Confirmation runs at READ COMMITTED (a REPEATABLE READ snapshot taken before the lock
/// wait would miss a writer that committed during it). That is sound only because
/// confirmation holds ACCESS EXCLUSIVE on **every ordinary table of the current schema**
/// before it counts or hashes anything: no writer can commit between the planner's
/// statements and the drop. Foreign-schema foreign keys and cascades are refused, and the
/// drop runs without CASCADE. Narrowing the fence (locking fewer tables) invalidates this
/// reasoning and needs a new decision.
#[derive(Clone, Copy)]
enum PlanningMode {
    Preview,
    Confirmation,
}
impl PlanningMode {
    fn relation_lock(self) -> &'static str {
        match self {
            Self::Preview => "ACCESS SHARE",
            Self::Confirmation => "ACCESS EXCLUSIVE",
        }
    }
}

impl ModuleSystem {
    /// Return the first configured provider with undescribed confirmed-uninstall effects.
    ///
    /// Reads immutable configuration only, without SQL, discovery, callbacks or authority.
    /// Precedence is Signal, package runtime, custom loader, then host lifecycle services.
    /// `None` means further transaction-bound planning is required, never that uninstall
    /// is supported. Callers still need fresh authorization and a valid confirmed impact.
    pub fn uninstall_composition_blocker(&self) -> Option<UninstallCompositionBlocker> {
        if self.root.signal().is_some() {
            Some(UninstallCompositionBlocker::ConfiguredSignal)
        } else if self.parts.package_runtime.is_some() {
            Some(UninstallCompositionBlocker::PackageRuntime)
        } else if !self.parts.schema_only_loader {
            Some(UninstallCompositionBlocker::CustomDataLoader)
        } else if !self.parts.lifecycle_hooks.is_empty() {
            Some(UninstallCompositionBlocker::LifecycleServices)
        } else {
            None
        }
    }

    /// Compute a bounded uninstall impact under lifecycle, schema and membership locks.
    /// No catalog, record or audit write is performed. The trusted checker is revalidated
    /// after membership locks, so a revocation while waiting cannot authorize the preview.
    /// This native engine seam has no HTTP exposure; installed artifact attestation is
    /// additionally required by ENGINE.IDENTITY before browser exposure.
    ///
    /// ```no_run
    /// # async fn confirm(system: &erp_modules::ModuleSystem,
    /// # names: &[erp_modules::ModuleName],
    /// # checker: &dyn erp_modules::install::InstallAuthorizer)
    /// # -> Result<erp_modules::OperationOutcome, erp_modules::Error> {
    /// let impact = system.preview_uninstall_authorized(names, checker).await?;
    /// system.uninstall_authorized(names, impact.fingerprint().as_str(), checker).await
    /// # }
    /// ```
    ///
    /// # Errors
    /// Validation for unsupported effects, invalid inputs or limits; authority, database,
    /// lock and deadline errors retain their kinds. A missing durable signal table refuses.
    pub async fn preview_uninstall_authorized(
        &self,
        names: &[ModuleName],
        authorizer: &dyn crate::install::InstallAuthorizer,
    ) -> Result<UninstallImpact> {
        validate_request(names)?;
        let mut optx = begin_operation_with_catalog_preflight(
            &self.root,
            self.parts.timeouts,
            LockWait::Refuse,
            false,
            true,
            erp_orm::Isolation::RepeatableRead,
        )
        .await?;
        let result = async {
            require_catalog_precedence(&mut optx.tx).await?;
            self.require_initialized(&mut optx.tx).await?;
            let checked = authorizer.authorize(&mut optx.tx).await?;
            require_catalog_precedence(&mut optx.tx).await?;
            optx.verify_intact().await?;
            let plan = self
                .uninstall_plan(
                    &mut optx,
                    names,
                    authorizer,
                    &checked,
                    PlanningMode::Preview,
                )
                .await?;
            Ok(plan.impact)
        }
        .await;
        match result {
            Ok(impact) => {
                optx.rollback().await?;
                Ok(impact)
            }
            Err(error) => {
                let error = rollback_after(&mut optx, error).await;
                Err(optx.limits.classify(error))
            }
        }
    }

    /// Execute exactly the freshly locked shared plan after comparing its acknowledgment.
    /// Confirmation is actor/session/database/generation/content and target-identity bound.
    ///
    /// # Errors
    /// Conflict when impact changed; all preview refusals remain errors. Every database effect
    /// and the audit insertion commit atomically. Post-commit failures are reported in outcome.
    pub async fn uninstall_authorized(
        &self,
        names: &[ModuleName],
        fingerprint: &str,
        authorizer: &dyn crate::install::InstallAuthorizer,
    ) -> Result<OperationOutcome> {
        validate_request(names)?;
        validate_fingerprint(fingerprint)?;
        let mut optx = begin_operation_with_catalog_preflight(
            &self.root,
            self.parts.timeouts,
            LockWait::Refuse,
            false,
            true,
            erp_orm::Isolation::ReadCommitted,
        )
        .await?;
        let result: std::result::Result<_, Failure> = async {
            require_catalog_precedence(&mut optx.tx).await?;
            self.require_initialized(&mut optx.tx).await?;
            let checked = authorizer.authorize(&mut optx.tx).await?;
            require_catalog_precedence(&mut optx.tx).await?;
            optx.verify_intact().await?;
            let plan = self.uninstall_plan(&mut optx, names, authorizer, &checked, PlanningMode::Confirmation).await?;
            if plan.impact.fingerprint.as_str() != fingerprint {
                return Err(Failure::Refused(erp_fields::Error::conflict(
                    "Uninstall impact changed; preview again before confirming",
                ).into()));
            }
            optx.tx.operation_bind_actor(&optx.control, checked.principal.clone())?;
            // No hook, purge callback, SQL CASCADE or registry reconciliation is allowed here.
            // These exact identities were counted, hashed and locked by the planner.
            for deletion in &plan.deletes {
                let sql = format!("DELETE FROM {} WHERE id OPERATOR(pg_catalog.=) ANY($1)", qualified(&plan.schema, &deletion.table));
                let changed = optx.tx.execute(&sql, &[&deletion.ids]).await?;
                if changed != deletion.count as u64 {
                    return Err(Failure::Refused(erp_fields::Error::conflict("Uninstall impact deletion membership changed").into()));
                }
            }
            if !plan.drops.is_empty() {
                let tables: Vec<_> = plan.drops.iter().map(|drop| qualified(&plan.schema, &drop.table)).collect();
                optx.tx.batch(&format!("DROP TABLE {}", tables.join(", "))).await?;
            }
            installed_identity::remove_receipts(&mut optx.tx, &plan.impact.resolved).await?;
            installed_identity::verify_entries_async(&plan.entries).await?;
            let resolved: Vec<_> = plan.impact.resolved.iter().map(ModuleName::as_str).collect();
            optx.tx.execute("UPDATE ir_module_module SET state='uninstalled', latest_version=NULL, demo=false, write_uid=1, write_date=(now() AT TIME ZONE 'UTC') WHERE name OPERATOR(pg_catalog.=) ANY($1)", &[&resolved]).await?;
            optx.tx.operation_switch_orm(&optx.control, &plan.remaining).await?;
            // Always bump durable state, even when this process has no signal listener.
            optx.tx.signal_bump(&SignalKey::Registry, SelfApplied::No).await?;
            crate::install::record_uninstall_audit(&mut optx.tx, &checked, &plan.impact.requested, &plan.impact.resolved, fingerprint).await?;
            let mut run = Run::new(self, IndexMap::new(), false);
            run.uninstalled = plan.impact.resolved;
            optx.commit().await?;
            Ok((plan.remaining, run))
        }.await;
        // This executor never persists transient scheduling. A preflight or execution error
        // must not reset an unrelated pending operation in a second transaction.
        let result = result.map_err(|failure| match failure {
            Failure::Refused(error) | Failure::Failed(error) => Failure::Refused(error),
        });
        self.finish(optx, result, AfterCommit::RunPostCommit).await
    }

    async fn uninstall_plan(
        &self,
        optx: &mut OperationTx,
        names: &[ModuleName],
        authorizer: &dyn crate::install::InstallAuthorizer,
        authorization: &crate::install::InstallAuthorization,
        mode: PlanningMode,
    ) -> Result<Plan> {
        let deadline = lowered(
            millis(optx.limits.statement),
            Duration::from_millis(UNINSTALL_STATEMENT_DEADLINE_MS),
        );
        set_timeouts(&mut optx.tx, millis(optx.limits.lock), millis(deadline)).await?;
        optx.limits.statement = deadline;
        optx.tx.query(erp_sql::ddl::SCHEMA_LOCK, &[]).await?;
        let schema = current_schema(&mut optx.tx).await?;
        // Broad first delivery: every ordinary table of the current schema participates in
        // the membership fence, including metadata, external IDs, FK sources and authority.
        // No per-table lock is acquired before the two advisory locks.
        let table_rows = optx.tx.query("SELECT c.relname::text, c.oid::text, c.relkind::text, c.relrowsecurity OR c.relforcerowsecurity FROM pg_class c JOIN pg_namespace n ON n.oid=c.relnamespace WHERE n.nspname=$1 AND c.relkind IN ('r','p','f') ORDER BY c.relname LIMIT 513", &[&schema]).await?;
        if table_rows.len() > MAX_LOCKED_TABLES {
            return Err(unsupported("more than 512 schema membership tables"));
        }
        let mut tables = BTreeMap::new();
        for row in table_rows {
            let name: String = row.try_get(0).map_err(shape)?;
            let oid: String = row.try_get(1).map_err(shape)?;
            let kind: String = row.try_get(2).map_err(shape)?;
            if row.try_get::<_, bool>(3).map_err(shape)? {
                return Err(unsupported(format!(
                    "row-security membership source '{name}'"
                )));
            }
            if kind != "r" {
                return Err(unsupported(format!(
                    "partitioned or foreign table '{name}'"
                )));
            }
            tables.insert(name, oid);
        }
        if !tables.contains_key("erp_signal_generation") {
            return Err(erp_fields::Error::validation("Uninstall impact requires durable registry generation; run Transaction::install_signal before preview").into());
        }
        if !tables.contains_key("installed_addon_identity") {
            return Err(erp_fields::Error::validation(
                "Installed addon identity schema is missing, run identity-enroll first",
            )
            .into());
        }
        if !tables.contains_key("erp_module_lifecycle_audit") {
            return Err(erp_fields::Error::validation(
                "Uninstall impact requires lifecycle audit schema initialization before preview",
            )
            .into());
        }
        let locks: Vec<_> = tables
            .keys()
            .map(|table| qualified(&schema, table))
            .collect();
        optx.tx
            .batch(&format!(
                "LOCK TABLE {} IN {} MODE",
                locks.join(", "),
                mode.relation_lock()
            ))
            .await?;
        let checked = authorizer.authorize(&mut optx.tx).await?;
        require_catalog_precedence(&mut optx.tx).await?;
        optx.verify_intact().await?;
        if checked.session_id != authorization.session_id
            || checked.principal.subject() != authorization.principal.subject()
        {
            return Err(erp_fields::Error::conflict(
                "Uninstall impact authority changed while waiting for membership locks",
            )
            .into());
        }
        // Raw catalog helpers use unqualified core names. Refuse shadowed sources rather
        // than read one relation and lock/delete another through a changed search_path.
        if current_schema(&mut optx.tx).await? != schema {
            return Err(unsupported("search_path changed by the trusted authorizer"));
        }
        let locked_names: Vec<_> = tables.keys().map(String::as_str).collect();
        let resolved = optx.tx.query("SELECT name, pg_catalog.to_regclass(pg_catalog.quote_ident(name))::oid::text FROM pg_catalog.unnest($1::text[]) AS name ORDER BY name", &[&locked_names]).await?;
        for row in resolved {
            let table: String = row.try_get(0).map_err(shape)?;
            let oid: Option<String> = row.try_get(1).map_err(shape)?;
            if oid.as_ref() != tables.get(&table) {
                return Err(unsupported(format!(
                    "shadowed schema membership source '{table}'"
                )));
            }
        }
        admit_native_lookup(&mut optx.tx).await?;
        admit_native_column_types(&mut optx.tx, &schema).await?;
        admit_native_index_storage(&mut optx.tx, &schema).await?;
        let scan_tables = vec![
            "ir_module_module",
            "ir_module_module_dependency",
            "ir_module_module_exclusion",
            "ir_module_category",
            "ir_model_data",
            "ir_module_post_commit",
            "erp_signal_generation",
            "erp_module_lifecycle_audit",
            "installed_addon_identity",
            "ir_model",
            "ir_model_fields",
            "ir_model_relation",
            "ir_ui_view",
        ];
        admit_index_expressions(&mut optx.tx, &schema, &scan_tables).await?;
        admit_relation_expression_dependencies(&mut optx.tx, &schema).await?;
        let native_expressions = admit_native_write_expressions(&mut optx.tx, &schema).await?;
        if let Some(blocker) = self.uninstall_composition_blocker() {
            return Err(erp_fields::Error::validation(blocker.message()).into());
        }
        // store::load and canonical catalog snapshots are bounded before fetching them.
        for source in [
            "ir_module_module",
            "ir_module_module_dependency",
            "ir_module_module_exclusion",
            "ir_module_category",
        ] {
            admit_metadata_source(&mut optx.tx, &schema, source).await?;
        }
        let (entries, content_identities) = self.discover_admitted().await?;
        let (mut catalog, _) = self.load_catalog(&mut optx.tx, &entries).await?;
        if catalog.rows().any(|row| row.state().is_transient()) {
            return Err(erp_fields::Error::validation(
                "Pending module operations must be resolved before human lifecycle operation",
            )
            .into());
        }
        for name in names {
            if catalog.state(name.as_str()) != Some(ModuleState::Installed) {
                return Err(erp_fields::Error::validation(format!(
                    "Uninstall requires installed addon '{name}'"
                ))
                .into());
            }
        }
        let installed: Vec<_> = catalog
            .rows()
            .filter(|row| row.state() == ModuleState::Installed)
            .map(|row| row.name().clone())
            .collect();
        // Check protection before Catalog's narrower direct-root guard to keep Validation.
        let mut protected = BTreeMap::new();
        for root in ["base", "web"] {
            if catalog.state(root).is_some() {
                for dependency in catalog.upstream_closure(&ModuleName::new(root)?) {
                    protected.insert(dependency, root);
                }
            }
        }
        for name in names {
            if let Some(root) = protected.get(name) {
                return Err(erp_fields::Error::validation(format!(
                    "Uninstall cannot remove addon '{name}' required by protected addon '{root}'"
                ))
                .into());
            }
        }
        catalog.button_uninstall(names)?;
        let mut resolved: Vec<_> = catalog
            .rows()
            .filter(|row| row.state() == ModuleState::ToRemove)
            .map(|row| row.name().clone())
            .collect();
        resolved.sort();
        for name in &resolved {
            if let Some(root) = protected.get(name) {
                return Err(erp_fields::Error::validation(format!(
                    "Uninstall cannot remove addon '{name}' required by protected addon '{root}'"
                ))
                .into());
            }
            let entry = entries.get(name).ok_or_else(|| Error::ModuleUnavailable {
                module: name.clone(),
            })?;
            if entry.manifest().uninstall_hook().is_some()
                || self
                    .parts
                    .hooks
                    .iter()
                    .any(|hook| hook.addon == name.as_str() && hook.kind == HookKind::Uninstall)
            {
                return Err(unsupported(format!("uninstall hook of addon '{name}'")));
            }
        }
        let durable_identities =
            installed_identity::validate_names(&mut optx.tx, &installed, &content_identities, None)
                .await?;
        installed_identity::validate_sources(&mut optx.tx, &installed, &entries).await?;
        let old_orm = self.orm_for(&installed, &entries)?;
        optx.tx
            .operation_switch_orm(&optx.control, &old_orm)
            .await?;
        let old = Arc::clone(optx.tx.registry());
        let remaining_names: Vec<_> = installed
            .into_iter()
            .filter(|name| !resolved.contains(name))
            .collect();
        let remaining = self.orm_for(&remaining_names, &entries)?;
        let new = remaining
            .env(SUPERUSER_ID, erp_orm::Context::new())
            .registry()
            .clone();
        let removed_names: Vec<_> = resolved.iter().map(ModuleName::as_str).collect();
        for model in old.models() {
            if old_orm
                .dispatch()
                .implementations(model.id, model_hooks::REGISTER_HOOK)
                .iter()
                .any(|implementation| {
                    implementation.addon != "base" || implementation.model != "base"
                })
            {
                return Err(unsupported(format!(
                    "register hook of model '{}'",
                    model.name
                )));
            }
            if new.model(model.name).is_some()
                && model
                    .mro
                    .iter()
                    .any(|decl| removed_names.contains(&decl.addon))
            {
                return Err(unsupported(format!(
                    "survivor model extension '{}' (fields, selections or methods)",
                    model.name
                )));
            }
        }
        if relation_owners(&old) != relation_owners(&new) {
            return Err(unsupported("many2many relation table removal"));
        }
        let mut dropped_models = Vec::new();
        let mut drops = Vec::new();
        let mut target_count = 0;
        let mut schema_definition_bytes = 0usize;
        for model in old
            .models()
            .iter()
            .filter(|model| model.has_generic_table() && new.model(model.name).is_none())
        {
            if model.partition.is_some() {
                return Err(unsupported(format!("partitioned model '{}'", model.name)));
            }
            if new
                .models()
                .iter()
                .any(|kept| kept.has_table() && kept.table == model.table)
            {
                return Err(unsupported(format!(
                    "shared survivor table '{}'",
                    model.table
                )));
            }
            if drops
                .iter()
                .any(|drop: &DropEffect| drop.table == model.table)
            {
                return Err(unsupported(format!(
                    "shared removed table '{}'",
                    model.table
                )));
            }
            let oid = tables
                .get(&model.table)
                .ok_or_else(|| unsupported(format!("missing target table '{}'", model.table)))?
                .clone();
            admit_index_expressions(&mut optx.tx, &schema, &[model.table.as_str()]).await?;
            let (schema_object_count, schema_objects) =
                drop_schema_identities(&mut optx.tx, &oid).await?;
            let additional_bytes: usize = schema_objects.iter().map(String::len).sum();
            schema_definition_bytes = schema_definition_bytes
                .checked_add(additional_bytes)
                .ok_or_else(|| unsupported("schema definition byte count overflow"))?;
            if schema_definition_bytes > MAX_PLANNING_BYTES as usize {
                return Err(unsupported(
                    "aggregate schema definitions exceed planning admission bytes",
                ));
            }
            target_count = admit_targets(target_count, schema_objects.len())?;
            let (count, ids) = identities(&mut optx.tx, &schema, &model.table, "TRUE", &[]).await?;
            target_count = admit_targets(target_count, ids.len())?;
            dropped_models.push(DroppedModelImpact {
                model: model.name.into(),
                table: model.table.clone(),
                row_count: count.to_string(),
            });
            drops.push(DropEffect {
                table: model.table.clone(),
                oid,
                schema_objects,
                schema_object_count,
                ids,
                count,
            });
        }
        dropped_models.sort_by(|a, b| a.model.cmp(&b.model));
        drops.sort_by(|a, b| a.table.cmp(&b.table));
        let dropped_names: Vec<_> = dropped_models
            .iter()
            .map(|model| model.model.as_str())
            .collect();
        let dropped_tables: Vec<_> = drops.iter().map(|drop| drop.table.as_str()).collect();
        let mut deletes = Vec::new();
        // Refuse all owned survivor records, including unsupported schema metadata kinds.
        let owned_bounds = optx.tx.query("SELECT COUNT(*),COALESCE(sum(pg_catalog.octet_length(model)),0)::bigint FROM ir_model_data WHERE module OPERATOR(pg_catalog.=) ANY($1)",&[&removed_names]).await?;
        let owned_count: i64 = owned_bounds[0].try_get(0).map_err(shape)?;
        let owned_bytes: i64 = owned_bounds[0].try_get(1).map_err(shape)?;
        if owned_count > MAX_UNINSTALL_TARGETS as i64 || owned_bytes > MAX_PLANNING_BYTES {
            return Err(unsupported(
                "external ID source exceeds planning admission bounds",
            ));
        }
        let owned = optx.tx.query("SELECT DISTINCT model FROM ir_model_data WHERE module OPERATOR(pg_catalog.=) ANY($1) AND res_id IS NOT NULL ORDER BY model LIMIT 10001", &[&removed_names]).await?;
        admit_targets(0, owned.len())?;
        let mut survivor_models = Vec::new();
        for row in owned {
            let model: String = row.try_get(0).map_err(shape)?;
            if !dropped_names.contains(&model.as_str())
                && !["ir.model", "ir.model.fields", "ir.model.relation"].contains(&model.as_str())
            {
                let Some(meta) = new.model(&model) else {
                    return Err(unsupported(format!(
                        "survivor record deletion for unavailable model '{model}'"
                    )));
                };
                if !meta.has_table() {
                    return Err(unsupported(format!(
                        "survivor record deletion for table-less model '{model}'"
                    )));
                }
                if meta.partition.is_some() {
                    return Err(unsupported(format!(
                        "survivor record deletion for partitioned model '{model}'"
                    )));
                }
                let old_meta = old.model(&model).ok_or_else(|| {
                    unsupported(format!(
                        "survivor record deletion for unavailable model '{model}'"
                    ))
                })?;
                // Model IDs belong to one registry; the remaining registry may number this model
                // differently. Raw DELETE cannot skip even base.unlink: it performs policy,
                // company-dependent, default and dependent-record work outside this effect plan.
                if !old_orm
                    .dispatch()
                    .implementations(old_meta.id, "unlink")
                    .is_empty()
                {
                    return Err(unsupported(format!(
                        "unlink hook of survivor model '{model}'"
                    )));
                }
                survivor_models.push((model, meta.table.clone()));
            }
        }
        if tables.contains_key("ir_ui_view") {
            let copies = optx
                .tx
                .query(
                    "SELECT EXISTS(SELECT 1 FROM ir_ui_view WHERE pg_catalog.split_part(key,'.',1)=ANY($1))",
                    &[&removed_names],
                )
                .await?;
            if copies[0].try_get::<_, bool>(0).map_err(shape)? {
                return Err(unsupported("copied view cascade"));
            }
        }
        // Exact reflection cleanup, child rows first. Arbitrary FK effects are checked below.
        if tables.contains_key("ir_model_fields") {
            push_delete(
                &mut optx.tx,
                &mut deletes,
                &mut target_count,
                (&schema, "ir_model_fields"),
                "model OPERATOR(pg_catalog.=) ANY($1)",
                &[&dropped_names],
            )
            .await?;
        }
        if tables.contains_key("ir_model_relation") {
            push_delete(
                &mut optx.tx,
                &mut deletes,
                &mut target_count,
                (&schema, "ir_model_relation"),
                "model_id IN (SELECT id FROM ir_model WHERE model OPERATOR(pg_catalog.=) ANY($1))",
                &[&dropped_names],
            )
            .await?;
        }
        if tables.contains_key("ir_model") {
            let generated = optx.tx.query("SELECT EXISTS(SELECT 1 FROM ir_model WHERE model OPERATOR(pg_catalog.=) ANY($1) AND NULLIF(company_restriction,'') IS NOT NULL)", &[&dropped_names]).await?;
            if generated[0].try_get::<_, bool>(0).map_err(shape)? {
                return Err(unsupported("generated company restriction reconciliation"));
            }
            push_delete(
                &mut optx.tx,
                &mut deletes,
                &mut target_count,
                (&schema, "ir_model"),
                "model OPERATOR(pg_catalog.=) ANY($1)",
                &[&dropped_names],
            )
            .await?;
        }
        // Every external ID deleted, including other owners' references to removed records.
        // Reject schema XIDs pointing at survivor metadata before taking their ownership away.
        let reflection: BTreeMap<_, _> = deletes
            .iter()
            .map(|effect| (effect.table.as_str(), effect.ids.as_slice()))
            .collect();
        let schema_xids = optx.tx.query("SELECT model,res_id FROM ir_model_data WHERE module OPERATOR(pg_catalog.=) ANY($1) AND model OPERATOR(pg_catalog.=) ANY($2) AND res_id IS NOT NULL LIMIT 10001", &[&removed_names, &vec!["ir.model","ir.model.fields","ir.model.relation"]]).await?;
        admit_targets(0, schema_xids.len())?;
        for row in schema_xids {
            let model: String = row.try_get(0).map_err(shape)?;
            let id: i64 = row.try_get(1).map_err(shape)?;
            let table = model.replace('.', "_");
            if !reflection
                .get(table.as_str())
                .is_some_and(|ids| ids.contains(&id))
            {
                return Err(unsupported(format!(
                    "survivor reflection ownership '{model}'"
                )));
            }
        }
        let mut xid_predicate =
            "module OPERATOR(pg_catalog.=) ANY($1) OR model OPERATOR(pg_catalog.=) ANY($2)"
                .to_owned();
        // Include XIDs of metadata removed under another owning addon as well.
        for effect in &deletes {
            let model = effect.table.replace('_', ".");
            let ids = effect
                .ids
                .iter()
                .map(i64::to_string)
                .collect::<Vec<_>>()
                .join(",");
            if !ids.is_empty() {
                xid_predicate.push_str(&format!(
                    " OR (model OPERATOR(pg_catalog.=) '{}' AND res_id IN ({ids}))",
                    model
                ));
            }
        }
        push_delete(
            &mut optx.tx,
            &mut deletes,
            &mut target_count,
            (&schema, "ir_model_data"),
            &xid_predicate,
            &[&removed_names, &dropped_names],
        )
        .await?;
        // Purge XIDs before their metadata; no unplanned ownership remains.
        if let Some(position) = deletes
            .iter()
            .position(|effect| effect.table == "ir_model_data")
        {
            let xids = deletes.remove(position);
            deletes.insert(0, xids);
        }
        push_delete(
            &mut optx.tx,
            &mut deletes,
            &mut target_count,
            (&schema, "ir_module_post_commit"),
            "module OPERATOR(pg_catalog.=) ANY($1) OR table_name OPERATOR(pg_catalog.=) ANY($2)",
            &[&removed_names, &dropped_tables],
        )
        .await?;
        for (model, table) in survivor_models {
            let predicate = "id IN (SELECT DISTINCT res_id FROM ir_model_data WHERE module OPERATOR(pg_catalog.=) ANY($1) AND model OPERATOR(pg_catalog.=) $2 AND res_id IS NOT NULL AND NOT EXISTS (SELECT 1 FROM ir_model_data other WHERE other.model OPERATOR(pg_catalog.=) $2 AND other.res_id OPERATOR(pg_catalog.=) ir_model_data.res_id AND NOT (other.module OPERATOR(pg_catalog.=) ANY($1))))";
            admit_index_expressions(&mut optx.tx, &schema, &[table.as_str()]).await?;
            push_delete(
                &mut optx.tx,
                &mut deletes,
                &mut target_count,
                (&schema, &table),
                predicate,
                &[&removed_names, &model],
            )
            .await?;
        }
        let trusted_signals =
            check_database_effects(&mut optx.tx, &schema, &old, &drops, &deletes).await?;
        let mut requested = names.to_vec();
        requested.sort();
        let mut metadata: Vec<_> = deletes
            .iter()
            .map(|effect| MetadataImpact {
                table: effect.table.clone(),
                kind: "delete",
                row_count: effect.count.to_string(),
            })
            .collect();
        for drop in &drops {
            metadata.push(MetadataImpact {
                table: drop.table.clone(),
                kind: "drop_schema_objects",
                row_count: drop.schema_object_count.to_string(),
            });
        }
        if !trusted_signals.is_empty() {
            metadata.push(MetadataImpact {
                table: "erp_signal_generation".into(),
                kind: "bump_policy",
                row_count: "1".into(),
            });
        }
        metadata.push(MetadataImpact {
            table: "erp_signal_generation".into(),
            kind: "bump_registry",
            row_count: "1".into(),
        });
        let mut impact = UninstallImpact {
            version: 1,
            requested,
            resolved,
            dropped_models,
            metadata,
            target_count,
            fingerprint: ImpactFingerprint(String::new()),
        };
        let identity = optx.tx.query("SELECT pg_catalog.current_database()::text, (SELECT oid::text FROM pg_database WHERE datname=pg_catalog.current_database()), (SELECT system_identifier::text FROM pg_catalog.pg_control_system()), (SELECT oid::text FROM pg_namespace WHERE nspname=$1), COALESCE((SELECT sum(weight)::text FROM erp_signal_generation WHERE key='registry'),'0')", &[&schema]).await?;
        let identity: Vec<String> = (0..5)
            .map(|column| identity[0].try_get(column).map_err(shape))
            .collect::<Result<_>>()?;
        // Capture actual catalog/dependency values, not merely the resolved set or counts.
        let catalog_state = optx
            .tx
            .query(
                "SELECT pg_catalog.jsonb_agg(pg_catalog.to_jsonb(m) ORDER BY m.id)::text FROM ir_module_module m",
                &[],
            )
            .await?;
        let catalog_state: Option<String> = catalog_state[0].try_get(0).map_err(shape)?;
        let dependencies = optx.tx.query("SELECT pg_catalog.jsonb_agg(pg_catalog.to_jsonb(d) ORDER BY d.id)::text FROM ir_module_module_dependency d", &[]).await?;
        let dependencies: Option<String> = dependencies[0].try_get(0).map_err(shape)?;
        installed_identity::verify_entries_async(&entries).await?;
        let canonical = json(&(
            &impact,
            &identity,
            authorization.principal.subject().get(),
            &authorization.session_id,
            &drops,
            &deletes,
            &trusted_signals,
            &native_expressions,
            catalog_state,
            dependencies,
            &content_identities,
            &durable_identities,
        ))?;
        impact.fingerprint = fingerprint(&canonical);
        admit_bytes(json(&impact)?.len())?;
        Ok(Plan {
            impact,
            schema,
            drops,
            deletes,
            remaining,
            entries,
        })
    }
}

fn fingerprint(canonical: &[u8]) -> ImpactFingerprint {
    let digest = Sha256::digest(canonical);
    let encoded = erp_orm::node::Digest::from_bytes(digest.into()).to_string();
    ImpactFingerprint(format!("uninstall-v1:{encoded}"))
}

async fn identities(
    tx: &mut Transaction,
    schema: &str,
    table: &str,
    predicate: &str,
    params: &[&(dyn tokio_postgres::types::ToSql + Sync)],
) -> Result<(i64, Vec<i64>)> {
    let table = qualified(schema, table);
    let count = tx
        .query(
            &format!("SELECT COUNT(*) FROM {table} WHERE {predicate}"),
            params,
        )
        .await?;
    let count: i64 = count[0].try_get(0).map_err(shape)?;
    let cardinality =
        usize::try_from(count).map_err(|_| unsupported("invalid target cardinality"))?;
    admit_targets(0, cardinality)?;
    let rows = tx
        .query(
            &format!(
                "SELECT id FROM {table} WHERE {predicate} ORDER BY id LIMIT {}",
                MAX_UNINSTALL_TARGETS + 1
            ),
            params,
        )
        .await?;
    let ids = rows
        .iter()
        .map(|row| row.try_get(0).map_err(shape))
        .collect::<Result<Vec<i64>>>()?;
    if ids.len() != cardinality {
        return Err(erp_fields::Error::conflict(
            "Uninstall impact target membership changed while planning",
        )
        .into());
    }
    Ok((count, ids))
}
async fn push_delete(
    tx: &mut Transaction,
    effects: &mut Vec<DeleteEffect>,
    total: &mut usize,
    source: (&str, &str),
    predicate: &str,
    params: &[&(dyn tokio_postgres::types::ToSql + Sync)],
) -> Result<()> {
    let (schema, table) = source;
    let (count, ids) = identities(tx, schema, table, predicate, params).await?;
    *total = admit_targets(*total, ids.len())?;
    if count == 0 {
        return Ok(());
    }
    effects.push(DeleteEffect {
        table: table.into(),
        ids,
        count,
    });
    Ok(())
}

/// Schema object identities are bounded independently before fetching their definitions.
/// DDL text contains schema metadata, never stored record values.
async fn drop_schema_identities(tx: &mut Transaction, oid: &str) -> Result<(i64, Vec<String>)> {
    let inheritance = tx.query("SELECT EXISTS(SELECT 1 FROM pg_inherits WHERE inhrelid=$1::text::oid OR inhparent=$1::text::oid)",&[&oid]).await?;
    if inheritance[0].try_get::<_, bool>(0).map_err(shape)? {
        return Err(unsupported("inherited table schema removal"));
    }
    let objects = "WITH objects AS (
        SELECT 'column:'||a.attnum::text||':'||pg_catalog.to_jsonb(a)::text AS identity FROM pg_attribute a WHERE a.attrelid=$1::text::oid AND a.attnum>0
        UNION ALL SELECT 'default:'||d.oid::text||':'||pg_get_expr(d.adbin,d.adrelid) FROM pg_attrdef d WHERE d.adrelid=$1::text::oid
        UNION ALL SELECT 'constraint:'||c.oid::text||':'||pg_catalog.pg_get_constraintdef(c.oid)||':'||c.convalidated::text FROM pg_constraint c WHERE c.conrelid=$1::text::oid
        UNION ALL SELECT 'index:'||i.indexrelid::text||':'||pg_catalog.pg_get_indexdef(i.indexrelid) FROM pg_index i WHERE i.indrelid=$1::text::oid
        UNION ALL SELECT 'trigger:'||t.oid::text||':'||pg_catalog.pg_get_triggerdef(t.oid) FROM pg_trigger t WHERE t.tgrelid=$1::text::oid
        UNION ALL SELECT 'owned-sequence:'||s.oid::text||':'||sn.nspname||'.'||s.relname FROM pg_depend d JOIN pg_class s ON s.oid=d.objid AND s.relkind='S' JOIN pg_namespace sn ON sn.oid=s.relnamespace WHERE d.classid='pg_class'::regclass AND d.refclassid='pg_class'::regclass AND d.refobjid=$1::text::oid
    )";
    let bounds = tx.query(&format!("{objects} SELECT COUNT(*), COALESCE(sum(pg_catalog.octet_length(identity)),0)::bigint FROM objects"),&[&oid]).await?;
    let count: i64 = bounds[0].try_get(0).map_err(shape)?;
    let bytes: i64 = bounds[0].try_get(1).map_err(shape)?;
    admit_targets(
        0,
        usize::try_from(count).map_err(|_| unsupported("negative schema object count"))?,
    )?;
    if bytes > MAX_PLANNING_BYTES {
        return Err(unsupported(
            "schema object definitions exceed planning admission bytes",
        ));
    }
    let rows = tx
        .query(
            &format!("{objects} SELECT identity FROM objects ORDER BY identity LIMIT 10001"),
            &[&oid],
        )
        .await?;
    let identities = rows
        .iter()
        .map(|row| row.try_get(0).map_err(shape))
        .collect::<Result<Vec<String>>>()?;
    Ok((count, identities))
}

// Both relation lookup and function/operator lookup must reach pg_catalog first. An implicit
// temporary namespace in front of it can shadow raw system-catalog references or regclass casts.
async fn require_catalog_precedence(tx: &mut Transaction) -> Result<()> {
    let rows = tx
        .query("SELECT pg_catalog.current_schemas(true)", &[])
        .await?;
    let schemas: Vec<String> = rows[0].try_get(0).map_err(shape)?;
    if schemas.first().map(String::as_str) != Some("pg_catalog") {
        return Err(unsupported("search_path without pg_catalog precedence"));
    }
    Ok(())
}

// Fixed PostgreSQL scalar identities used by native field/core storage, and their builtin
// array types. Domain, composite, enum and extension I/O is never invoked to inspect records.
// Namespace precedence alone does not prevent an exact application overload from beating a
// polymorphic native function/operator. Refuse such lookup candidates before record queries.
async fn admit_native_lookup(tx: &mut Transaction) -> Result<()> {
    let rows = tx
        .query("SELECT pg_catalog.current_schemas(true)", &[])
        .await?;
    let schemas: Vec<String> = rows[0].try_get(0).map_err(shape)?;
    let schemas: Vec<_> = schemas
        .into_iter()
        .filter(|schema| schema != "pg_catalog" && !schema.starts_with("pg_temp_"))
        .collect();
    let functions=tx.query("SELECT n.nspname::text FROM pg_catalog.pg_proc p JOIN pg_catalog.pg_namespace n ON n.oid=p.pronamespace WHERE n.nspname::text OPERATOR(pg_catalog.=) ANY($1) AND EXISTS(SELECT 1 FROM pg_catalog.pg_proc builtin JOIN pg_catalog.pg_namespace bn ON bn.oid=builtin.pronamespace WHERE bn.nspname='pg_catalog' AND builtin.oid<16384 AND builtin.proname=p.proname) ORDER BY n.nspname,p.proname LIMIT 1",&[&schemas]).await?;
    if let Some(row) = functions.first() {
        return Err(unsupported(format!(
            "builtin function lookup overload in schema '{}'",
            row.try_get::<_, String>(0).map_err(shape)?
        )));
    }
    // Bounded symbol inventory from this planner and store::load: comparisons, array/string
    // concatenation and the trigger event-mask bit operation. Unused pg_trgm '%'/'<->' are
    // outside it. Reject every application overload for a used symbol, without reimplementing
    // PostgreSQL's implicit-coercion or polymorphic operator resolution.
    let symbols = vec!["=", "<>", "<", ">", "<=", ">=", "||", "&"];
    let operators=tx.query("SELECT n.nspname::text FROM pg_catalog.pg_operator op JOIN pg_catalog.pg_namespace n ON n.oid=op.oprnamespace WHERE n.nspname::text OPERATOR(pg_catalog.=) ANY($1) AND op.oprname::text OPERATOR(pg_catalog.=) ANY($2) ORDER BY n.nspname,op.oprname LIMIT 1",&[&schemas,&symbols]).await?;
    if let Some(row) = operators.first() {
        return Err(unsupported(format!(
            "custom operator lookup in schema '{}'",
            row.try_get::<_, String>(0).map_err(shape)?
        )));
    }
    Ok(())
}

async fn admit_relation_expression_dependencies(tx: &mut Transaction, schema: &str) -> Result<()> {
    let owners="WITH owners AS (
        SELECT 'CHECK'::text AS kind,'pg_constraint'::regclass AS classid,k.oid,k.conbin::text AS expression,c.relname FROM pg_catalog.pg_constraint k JOIN pg_catalog.pg_class c ON c.oid=k.conrelid JOIN pg_catalog.pg_namespace n ON n.oid=c.relnamespace WHERE n.nspname=$1 AND c.relkind='r' AND k.contype='c'
        UNION ALL SELECT 'default','pg_attrdef'::regclass,d.oid,d.adbin::text,c.relname FROM pg_catalog.pg_attrdef d JOIN pg_catalog.pg_class c ON c.oid=d.adrelid JOIN pg_catalog.pg_namespace n ON n.oid=c.relnamespace WHERE n.nspname=$1 AND c.relkind='r'
        UNION ALL SELECT 'trigger','pg_trigger'::regclass,t.oid,t.tgqual::text,c.relname FROM pg_catalog.pg_trigger t JOIN pg_catalog.pg_class c ON c.oid=t.tgrelid JOIN pg_catalog.pg_namespace n ON n.oid=c.relnamespace WHERE n.nspname=$1 AND c.relkind='r'
    )";
    let bounds=tx.query(&format!("{owners} SELECT COUNT(*),COALESCE(sum(pg_catalog.octet_length(expression)),0)::bigint FROM owners"),&[&schema]).await?;
    let count: i64 = bounds[0].try_get(0).map_err(shape)?;
    let bytes: i64 = bounds[0].try_get(1).map_err(shape)?;
    if count > MAX_UNINSTALL_TARGETS as i64 || bytes > MAX_PLANNING_BYTES {
        return Err(unsupported(
            "relation expression dependency admission bounds",
        ));
    }
    let dependencies=tx.query(&format!("{owners} SELECT o.relname::text,o.kind FROM owners o JOIN pg_catalog.pg_depend d ON d.classid=o.classid AND d.objid=o.oid WHERE
        (o.kind<>'trigger' AND d.refclassid='pg_proc'::regclass AND NOT EXISTS(SELECT 1 FROM pg_catalog.pg_proc p JOIN pg_catalog.pg_namespace n ON n.oid=p.pronamespace WHERE p.oid=d.refobjid AND p.oid<16384 AND n.nspname='pg_catalog')) OR
        (d.refclassid='pg_operator'::regclass AND NOT EXISTS(SELECT 1 FROM pg_catalog.pg_operator p JOIN pg_catalog.pg_namespace n ON n.oid=p.oprnamespace WHERE p.oid=d.refobjid AND p.oid<16384 AND n.nspname='pg_catalog')) OR
        (d.refclassid='pg_type'::regclass AND NOT EXISTS(SELECT 1 FROM pg_catalog.pg_type p JOIN pg_catalog.pg_namespace n ON n.oid=p.typnamespace WHERE p.oid=d.refobjid AND p.oid<16384 AND n.nspname='pg_catalog')) OR
        (d.refclassid='pg_collation'::regclass AND NOT EXISTS(SELECT 1 FROM pg_catalog.pg_collation p JOIN pg_catalog.pg_namespace n ON n.oid=p.collnamespace WHERE p.oid=d.refobjid AND p.oid<16384 AND n.nspname='pg_catalog')) OR
        (d.refclassid='pg_cast'::regclass AND d.refobjid>=16384)
        ORDER BY o.relname,o.kind LIMIT 1"),&[&schema]).await?;
    if let Some(row) = dependencies.first() {
        let table: String = row.try_get(0).map_err(shape)?;
        let kind: String = row.try_get(1).map_err(shape)?;
        return Err(unsupported(format!(
            "executable {kind} expression on table '{table}'"
        )));
    }
    Ok(())
}

async fn admit_native_column_types(tx: &mut Transaction, schema: &str) -> Result<()> {
    let scalar_oids: Vec<i64> = vec![
        16, 17, 18, 19, 20, 21, 23, 25, 26, 114, 700, 701, 869, 1042, 1043, 1082, 1083, 1114, 1184,
        1186, 1266, 1700, 2950, 3802,
    ];
    let rows = tx.query("SELECT c.relname::text FROM pg_catalog.pg_attribute a JOIN pg_catalog.pg_class c ON c.oid=a.attrelid JOIN pg_catalog.pg_namespace n ON n.oid=c.relnamespace JOIN pg_catalog.pg_type t ON t.oid=a.atttypid JOIN pg_catalog.pg_namespace tn ON tn.oid=t.typnamespace WHERE n.nspname=$1 AND c.relkind='r' AND a.attnum>0 AND NOT a.attisdropped AND NOT (tn.nspname='pg_catalog' AND t.oid<16384 AND t.typtype='b' AND (t.oid::bigint=ANY($2) OR (t.typcategory='A' AND t.typelem::bigint=ANY($2)))) ORDER BY c.relname,a.attnum LIMIT 1",&[&schema,&scalar_oids]).await?;
    if let Some(row) = rows.first() {
        return Err(unsupported(format!(
            "custom column type on table '{}'",
            row.try_get::<_, String>(0).map_err(shape)?
        )));
    }
    Ok(())
}

// SELECT planning and row writes may enter index support functions. Expression/predicate
// evaluation and user-defined access methods/operator classes remain outside this subset.
async fn admit_native_index_storage(tx: &mut Transaction, schema: &str) -> Result<()> {
    let storage=tx.query("SELECT c.relname::text FROM pg_catalog.pg_class c JOIN pg_catalog.pg_namespace n ON n.oid=c.relnamespace JOIN pg_catalog.pg_am am ON am.oid=c.relam JOIN pg_catalog.pg_proc p ON p.oid=am.amhandler JOIN pg_catalog.pg_namespace pn ON pn.oid=p.pronamespace WHERE n.nspname=$1 AND c.relkind='r' AND NOT (am.amtype='t' AND am.oid<16384 AND p.oid<16384 AND pn.nspname='pg_catalog') ORDER BY c.relname LIMIT 1",&[&schema]).await?;
    if let Some(row) = storage.first() {
        return Err(unsupported(format!(
            "custom table storage on table '{}'",
            row.try_get::<_, String>(0).map_err(shape)?
        )));
    }
    let generated=tx.query("SELECT c.relname::text FROM pg_catalog.pg_attribute a JOIN pg_catalog.pg_class c ON c.oid=a.attrelid JOIN pg_catalog.pg_namespace n ON n.oid=c.relnamespace WHERE n.nspname=$1 AND c.relkind='r' AND a.attnum>0 AND NOT a.attisdropped AND a.attgenerated<>'' ORDER BY c.relname,a.attnum LIMIT 1",&[&schema]).await?;
    if let Some(row) = generated.first() {
        return Err(unsupported(format!(
            "executable generated expression on table '{}'",
            row.try_get::<_, String>(0).map_err(shape)?
        )));
    }
    let custom=tx.query("SELECT c.relname::text FROM pg_catalog.pg_index i JOIN pg_catalog.pg_class c ON c.oid=i.indrelid JOIN pg_catalog.pg_namespace n ON n.oid=c.relnamespace JOIN pg_catalog.pg_class ix ON ix.oid=i.indexrelid JOIN pg_catalog.pg_am am ON am.oid=ix.relam JOIN pg_catalog.pg_proc handler ON handler.oid=am.amhandler JOIN pg_catalog.pg_namespace hn ON hn.oid=handler.pronamespace CROSS JOIN LATERAL pg_catalog.unnest(i.indclass) opclass(oid) JOIN pg_catalog.pg_opclass op ON op.oid=opclass.oid JOIN pg_catalog.pg_namespace onsp ON onsp.oid=op.opcnamespace JOIN pg_catalog.pg_opfamily family ON family.oid=op.opcfamily JOIN pg_catalog.pg_namespace fn ON fn.oid=family.opfnamespace WHERE n.nspname=$1 AND NOT (am.oid<16384 AND handler.oid<16384 AND hn.nspname='pg_catalog' AND op.oid<16384 AND onsp.nspname='pg_catalog' AND family.oid<16384 AND fn.nspname='pg_catalog') ORDER BY c.relname LIMIT 1",&[&schema]).await?;
    if let Some(row) = custom.first() {
        return Err(unsupported(format!(
            "custom index support on table '{}'",
            row.try_get::<_, String>(0).map_err(shape)?
        )));
    }
    Ok(())
}

async fn admit_index_expressions(
    tx: &mut Transaction,
    schema: &str,
    tables: &[&str],
) -> Result<()> {
    let expressions=tx.query("SELECT c.relname::text FROM pg_catalog.pg_index i JOIN pg_catalog.pg_class c ON c.oid=i.indrelid JOIN pg_catalog.pg_namespace n ON n.oid=c.relnamespace WHERE n.nspname=$1 AND c.relname::text OPERATOR(pg_catalog.=) ANY($2) AND (i.indexprs IS NOT NULL OR i.indpred IS NOT NULL) ORDER BY c.relname LIMIT 1",&[&schema,&tables]).await?;
    if let Some(row) = expressions.first() {
        return Err(unsupported(format!(
            "executable index expression on table '{}'",
            row.try_get::<_, String>(0).map_err(shape)?
        )));
    }
    Ok(())
}

// Names are not authority: admitted expression text and dependency provenance both match.
// The two lifecycle-audit operation expressions include `force_demo` since;
// the native DDL replaces both constraints on every run, so the older forms cannot remain.
fn native_check_expression(table: &str, expression: &str) -> bool {
    match table {
        "ir_module_module" => expression == "((state)::text = ANY ((ARRAY['uninstallable'::character varying, 'uninstalled'::character varying, 'installed'::character varying, 'to upgrade'::character varying, 'to remove'::character varying, 'to install'::character varying])::text[]))",
        "erp_signal_generation" => [
            "(key ~ '^(registry|policy|membership|cache:[a-z0-9_.]{1,64})$'::text)",
            "((weight >= 1) AND (weight <= '1099511627776'::bigint))",
        ].contains(&expression),
        "erp_module_lifecycle_audit" => [
            "(((operation = 'uninstall'::text) AND (fingerprint IS NOT NULL) AND (fingerprint ~ '^uninstall-v1:[0-9a-f]{64}$'::text)) OR ((operation = ANY (ARRAY['install'::text, 'upgrade'::text, 'force_demo'::text])) AND (fingerprint IS NULL)))",
            "(operation = ANY (ARRAY['install'::text, 'upgrade'::text, 'uninstall'::text, 'force_demo'::text]))",
            "(outcome = 'committed'::text)",
            "((cardinality(requested) >= 1) AND (cardinality(requested) <= 64))",
        ].contains(&expression),
        // No other table receives an INSERT or UPDATE in this executor.
        _ => false,
    }
}

async fn admit_native_write_expressions(tx: &mut Transaction, schema: &str) -> Result<Vec<String>> {
    let writes = vec![
        "ir_module_module",
        "erp_module_lifecycle_audit",
        "erp_signal_generation",
    ];
    let inserts = vec!["erp_module_lifecycle_audit", "erp_signal_generation"];
    let owners = "WITH owners AS (
        SELECT 'check'::text AS kind,'pg_constraint'::regclass AS classid,k.oid AS oid,c.relname,k.conbin AS expression,k.conrelid AS relid,k.conname::text AS name,0::smallint AS attnum
        FROM pg_catalog.pg_constraint k JOIN pg_catalog.pg_class c ON c.oid=k.conrelid JOIN pg_catalog.pg_namespace n ON n.oid=c.relnamespace WHERE n.nspname=$1 AND c.relname=ANY($2) AND k.contype='c'
        UNION ALL SELECT 'default','pg_attrdef'::regclass,d.oid,c.relname,d.adbin,d.adrelid,a.attname::text,a.attnum
        FROM pg_catalog.pg_attrdef d JOIN pg_catalog.pg_class c ON c.oid=d.adrelid JOIN pg_catalog.pg_namespace n ON n.oid=c.relnamespace JOIN pg_catalog.pg_attribute a ON a.attrelid=d.adrelid AND a.attnum=d.adnum WHERE n.nspname=$1 AND c.relname=ANY($3)
    )";
    let params: &[&(dyn tokio_postgres::types::ToSql + Sync)] = &[&schema, &writes, &inserts];
    // Never call pg_get_expr before checking dependencies: constant deparse can invoke a
    // user-defined type's output function even when no column of that type exists.
    let dependency = tx.query(&format!("{owners} SELECT o.relname::text,o.kind FROM owners o JOIN pg_catalog.pg_depend d ON d.classid=o.classid AND d.objid=o.oid WHERE
        (d.refclassid='pg_proc'::regclass AND NOT EXISTS(SELECT 1 FROM pg_catalog.pg_proc p JOIN pg_catalog.pg_namespace n ON n.oid=p.pronamespace WHERE p.oid=d.refobjid AND p.oid<16384 AND n.nspname='pg_catalog')) OR
        (d.refclassid='pg_operator'::regclass AND NOT EXISTS(SELECT 1 FROM pg_catalog.pg_operator p JOIN pg_catalog.pg_namespace n ON n.oid=p.oprnamespace WHERE p.oid=d.refobjid AND p.oid<16384 AND n.nspname='pg_catalog')) OR
        (d.refclassid='pg_type'::regclass AND NOT EXISTS(SELECT 1 FROM pg_catalog.pg_type p JOIN pg_catalog.pg_namespace n ON n.oid=p.typnamespace WHERE p.oid=d.refobjid AND p.oid<16384 AND n.nspname='pg_catalog')) OR
        (d.refclassid='pg_collation'::regclass AND NOT EXISTS(SELECT 1 FROM pg_catalog.pg_collation p JOIN pg_catalog.pg_namespace n ON n.oid=p.collnamespace WHERE p.oid=d.refobjid AND p.oid<16384 AND n.nspname='pg_catalog')) OR
        (d.refclassid='pg_cast'::regclass AND d.refobjid>=16384)
        ORDER BY o.relname,o.kind LIMIT 1"),params).await?;
    if let Some(row) = dependency.first() {
        let table: String = row.try_get(0).map_err(shape)?;
        let kind: String = row.try_get(1).map_err(shape)?;
        let label = if kind == "check" { "CHECK" } else { "default" };
        return Err(unsupported(format!(
            "executable {label} expression on table '{table}'"
        )));
    }
    let bounds=tx.query(&format!("{owners} SELECT COUNT(*),COALESCE(sum(pg_catalog.octet_length(expression::text)),0)::bigint FROM owners"),params).await?;
    let count: i64 = bounds[0].try_get(0).map_err(shape)?;
    let bytes: i64 = bounds[0].try_get(1).map_err(shape)?;
    if count > MAX_UNINSTALL_TARGETS as i64 || bytes > MAX_PLANNING_BYTES {
        return Err(unsupported("writable expression admission bounds"));
    }
    let identities=tx.query("SELECT c.relname::text FROM pg_catalog.pg_attribute a JOIN pg_catalog.pg_class c ON c.oid=a.attrelid JOIN pg_catalog.pg_namespace n ON n.oid=c.relnamespace WHERE n.nspname=$1 AND c.relname=ANY($2) AND a.attnum>0 AND a.attidentity<>'' ORDER BY c.relname,a.attnum LIMIT 1",&[&schema,&inserts]).await?;
    if let Some(row) = identities.first() {
        return Err(unsupported(format!(
            "executable default expression on table '{}'",
            row.try_get::<_, String>(0).map_err(shape)?
        )));
    }
    let expressions=tx.query(&format!("{owners} SELECT relname::text,kind,oid::text,name,pg_catalog.pg_get_expr(expression,relid),relid::text,attnum FROM owners ORDER BY relname,kind,name LIMIT 10001"),params).await?;
    let mut admitted = Vec::new();
    for row in expressions {
        let table: String = row.try_get(0).map_err(shape)?;
        let kind: String = row.try_get(1).map_err(shape)?;
        let oid: String = row.try_get(2).map_err(shape)?;
        let name: String = row.try_get(3).map_err(shape)?;
        let expression: String = row.try_get(4).map_err(shape)?;
        let relid: String = row.try_get(5).map_err(shape)?;
        let attnum: i16 = row.try_get(6).map_err(shape)?;
        let mut sequence_identity = None;
        let allowed = if kind == "check" {
            native_check_expression(&table, &expression)
        } else {
            match (table.as_str(), name.as_str(), expression.as_str()) {
                ("erp_module_lifecycle_audit", "created_at", "clock_timestamp()")
                | ("erp_module_lifecycle_audit", "uninstalled", "'{}'::text[]")
                | ("erp_signal_generation", "id", "uuidv7()")
                | ("erp_signal_generation", "created_at", "now()")
                | ("erp_signal_generation", "weight", "1") => true,
                ("erp_module_lifecycle_audit", "id", _) => {
                    let attribute_number = i32::from(attnum);
                    let sequences=tx.query("SELECT s.oid::text,pg_catalog.format('nextval(%L::regclass)',s.oid::regclass::text) FROM pg_catalog.pg_depend d JOIN pg_catalog.pg_class s ON s.oid=d.refobjid AND s.relkind='S' JOIN pg_catalog.pg_namespace n ON n.oid=s.relnamespace JOIN pg_catalog.pg_depend own ON own.classid='pg_class'::regclass AND own.objid=s.oid AND own.refclassid='pg_class'::regclass AND own.refobjid=$2::text::oid AND own.refobjsubid=$3 AND own.deptype IN ('a','i') WHERE d.classid='pg_attrdef'::regclass AND d.objid=$1::text::oid AND d.refclassid='pg_class'::regclass AND n.nspname=$4 ORDER BY s.oid LIMIT 2",&[&oid,&relid,&attribute_number,&schema]).await?;
                    if let [sequence] = sequences.as_slice() {
                        let expected: String = sequence.try_get(1).map_err(shape)?;
                        sequence_identity = Some(sequence.try_get::<_, String>(0).map_err(shape)?);
                        expression == expected
                    } else {
                        false
                    }
                }
                _ => false,
            }
        };
        if !allowed {
            let label = if kind == "check" { "CHECK" } else { "default" };
            return Err(unsupported(format!(
                "executable {label} expression on table '{table}'"
            )));
        }
        admitted.push(
            String::from_utf8(json(&(
                table,
                kind,
                oid,
                name,
                expression,
                sequence_identity,
            ))?)
            .map_err(|error| {
                erp_fields::Error::programming(format!(
                    "Invalid native expression attestation UTF-8: {error}"
                ))
            })?,
        );
    }
    Ok(admitted)
}

async fn admit_metadata_source(tx: &mut Transaction, schema: &str, table: &str) -> Result<()> {
    let sql = format!(
        "SELECT count(*), COALESCE(sum(pg_catalog.octet_length(pg_catalog.to_jsonb(source)::text)),0)::bigint FROM {} source",
        qualified(schema, table)
    );
    let bounds = tx.query(&sql, &[]).await?;
    let count: i64 = bounds[0].try_get(0).map_err(shape)?;
    let bytes: i64 = bounds[0].try_get(1).map_err(shape)?;
    if count > MAX_UNINSTALL_TARGETS as i64 || bytes > MAX_PLANNING_BYTES {
        return Err(unsupported(format!(
            "catalog source '{table}' exceeds planning admission bounds"
        )));
    }
    Ok(())
}

// RI triggers may plan SELECTs on either side of every FK incident to a write surface.
// No induced write surface is admitted: modifying FK actions below are refused unless the
// complete child deletion precedes its parent, so one-hop counterpart lookup closes this set.
async fn admit_foreign_key_lookup_indexes(
    tx: &mut Transaction,
    schema: &str,
    writes: &[&str],
) -> Result<()> {
    let relevant="WITH relevant AS (
        SELECT sn.nspname::text AS source_schema,s.relname::text AS source_table,tn.nspname::text AS target_schema,t.relname::text AS target_table,k.conname::text AS name
        FROM pg_catalog.pg_constraint k JOIN pg_catalog.pg_class s ON s.oid=k.conrelid JOIN pg_catalog.pg_namespace sn ON sn.oid=s.relnamespace JOIN pg_catalog.pg_class t ON t.oid=k.confrelid JOIN pg_catalog.pg_namespace tn ON tn.oid=t.relnamespace
        WHERE k.contype='f' AND ((sn.nspname=$1 AND s.relname::text OPERATOR(pg_catalog.=) ANY($2)) OR (tn.nspname=$1 AND t.relname::text OPERATOR(pg_catalog.=) ANY($2)))
    )";
    let bounds=tx.query(&format!("{relevant} SELECT COUNT(*),COALESCE(sum(pg_catalog.octet_length(source_schema)+pg_catalog.octet_length(source_table)+pg_catalog.octet_length(target_schema)+pg_catalog.octet_length(target_table)+pg_catalog.octet_length(name)),0)::bigint FROM relevant"),&[&schema,&writes]).await?;
    let count: i64 = bounds[0].try_get(0).map_err(shape)?;
    let bytes: i64 = bounds[0].try_get(1).map_err(shape)?;
    if count > MAX_UNINSTALL_TARGETS as i64 || bytes > MAX_PLANNING_BYTES {
        return Err(unsupported("foreign key lookup inventory admission bounds"));
    }
    let edges=tx.query(&format!("{relevant} SELECT source_schema,source_table,target_schema,target_table,name FROM relevant ORDER BY source_schema,source_table,target_schema,target_table,name LIMIT 10001"),&[&schema,&writes]).await?;
    let mut tables = BTreeSet::new();
    for edge in edges {
        let source_schema: String = edge.try_get(0).map_err(shape)?;
        let source: String = edge.try_get(1).map_err(shape)?;
        let target_schema: String = edge.try_get(2).map_err(shape)?;
        let target: String = edge.try_get(3).map_err(shape)?;
        let constraint: String = edge.try_get(4).map_err(shape)?;
        if source_schema != schema || target_schema != schema {
            return Err(unsupported(format!(
                "cross-schema foreign key lookup for constraint '{constraint}'"
            )));
        }
        tables.insert(source);
        tables.insert(target);
    }
    let tables: Vec<_> = tables.iter().map(String::as_str).collect();
    admit_index_expressions(tx, schema, &tables).await
}

async fn check_database_effects(
    tx: &mut Transaction,
    schema: &str,
    registry: &Registry,
    drops: &[DropEffect],
    deletes: &[DeleteEffect],
) -> Result<Vec<String>> {
    let events = tx.query("SELECT evtname::text FROM pg_event_trigger WHERE evtenabled<>'D' ORDER BY evtname LIMIT 1", &[]).await?;
    if let Some(event) = events.first() {
        return Err(unsupported(format!(
            "database event trigger '{}'",
            event.try_get::<_, String>(0).map_err(shape)?
        )));
    }
    let mut write_tables: Vec<&str> = deletes.iter().map(|effect| effect.table.as_str()).collect();
    write_tables.push("installed_addon_identity");
    write_tables.extend([
        "ir_module_module",
        "erp_signal_generation",
        "erp_module_lifecycle_audit",
    ]);
    admit_foreign_key_lookup_indexes(tx, schema, &write_tables).await?;
    let trigger_bounds = tx.query("SELECT count(*), COALESCE(sum(pg_catalog.octet_length(p.prosrc)),0)::bigint FROM pg_trigger t JOIN pg_class c ON c.oid=t.tgrelid JOIN pg_namespace n ON n.oid=c.relnamespace JOIN pg_proc p ON p.oid=t.tgfoid WHERE n.nspname=$1 AND c.relname=ANY($2) AND NOT t.tgisinternal AND t.tgenabled<>'D'", &[&schema,&write_tables]).await?;
    let trigger_count: i64 = trigger_bounds[0].try_get(0).map_err(shape)?;
    let trigger_bytes: i64 = trigger_bounds[0].try_get(1).map_err(shape)?;
    if trigger_count > MAX_UNINSTALL_TARGETS as i64 || trigger_bytes > MAX_PLANNING_BYTES {
        return Err(unsupported("trigger definition admission bounds"));
    }
    let triggers = tx.query("SELECT c.relname::text,t.tgname::text,t.tgtype::int,t.tgenabled::text,t.tgattr::text,p.prosrc,p.proconfig, (t.tgnargs=0 AND t.tgqual IS NULL AND t.tgconstraint=0 AND t.tgparentid=0 AND NOT t.tgdeferrable AND NOT t.tginitdeferred AND NOT p.prosecdef AND NOT p.proisstrict AND NOT p.proleakproof AND p.provolatile='v' AND p.proparallel='u' AND p.prokind='f' AND p.pronargs=0 AND p.prorettype='trigger'::regtype AND l.lanname='plpgsql' AND pn.nspname=n.nspname AND p.proname='erp_signal_bump_policy' AND p.proowner=c.relowner AND COALESCE(p.proconfig=ARRAY['search_path=pg_catalog, '||pg_catalog.quote_ident(n.nspname)||', pg_temp'],false)),p.oid::text FROM pg_trigger t JOIN pg_class c ON c.oid=t.tgrelid JOIN pg_namespace n ON n.oid=c.relnamespace JOIN pg_proc p ON p.oid=t.tgfoid JOIN pg_namespace pn ON pn.oid=p.pronamespace JOIN pg_language l ON l.oid=p.prolang WHERE n.nspname=$1 AND c.relname=ANY($2) AND NOT t.tgisinternal AND t.tgenabled<>'D' AND (t.tgtype::int & CASE WHEN c.relname IN ('erp_module_lifecycle_audit','erp_signal_generation') THEN 4 WHEN c.relname='ir_module_module' THEN 16 ELSE 8 END)<>0 ORDER BY c.relname,t.tgname", &[&schema,&write_tables]).await?;
    let mut trusted_signals = Vec::new();
    for trigger in triggers {
        let table: String = trigger.try_get(0).map_err(shape)?;
        let name: String = trigger.try_get(1).map_err(shape)?;
        let trigger_type: i32 = trigger.try_get(2).map_err(shape)?;
        let enabled: String = trigger.try_get(3).map_err(shape)?;
        let columns: String = trigger.try_get(4).map_err(shape)?;
        let body: String = trigger.try_get(5).map_err(shape)?;
        let config: Option<Vec<String>> = trigger.try_get(6).map_err(shape)?;
        let structural: bool = trigger.try_get(7).map_err(shape)?;
        let function_oid: String = trigger.try_get(8).map_err(shape)?;
        let declared = registry.models().iter().any(|model| {
            model.table_objects.iter().any(|object| {
                let erp_registry::TableObjectKind::SignalTrigger(declaration) = &object.kind else {
                    return false;
                };
                let expected_type = 4 | 8 | 32 | if declaration.updates { 16 } else { 0 };
                declaration.signal == erp_registry::TriggerSignal::Policy
                    && declaration.update_columns.is_empty()
                    && object.full_name == name
                    && trigger_type == expected_type
                    && model
                        .signal_trigger_table(declaration)
                        .is_ok_and(|physical| physical == table)
            })
        });
        if !structural
            || !declared
            || enabled != "O"
            || !columns.is_empty()
            || !erp_orm::signal::builtin_signal_function_matches(
                erp_registry::TriggerSignal::Policy,
                schema,
                &body,
            )
        {
            return Err(unsupported(format!("database trigger '{table}.{name}'")));
        }
        trusted_signals.push(
            String::from_utf8(json(&(
                table,
                name,
                trigger_type,
                function_oid,
                body,
                config,
            ))?)
            .map_err(|error| {
                erp_fields::Error::programming(format!(
                    "Invalid trigger attestation UTF-8: {error}"
                ))
            })?,
        );
    }
    // Rules can rewrite raw DELETE/UPDATE/INSERT just as triggers can.
    let rules = tx.query("SELECT c.relname::text,r.rulename::text FROM pg_rewrite r JOIN pg_class c ON c.oid=r.ev_class JOIN pg_namespace n ON n.oid=c.relnamespace WHERE n.nspname=$1 AND c.relname=ANY($2) AND r.rulename<>'_RETURN' ORDER BY c.relname,r.rulename LIMIT 1", &[&schema,&write_tables]).await?;
    if let Some(rule) = rules.first() {
        return Err(unsupported(format!(
            "database rewrite rule '{}.{}'",
            rule.try_get::<_, String>(0).map_err(shape)?,
            rule.try_get::<_, String>(1).map_err(shape)?
        )));
    }
    let drop_tables: Vec<_> = drops.iter().map(|drop| drop.table.as_str()).collect();
    // DROP RESTRICT would refuse these too; refuse at preview rather than promise success.
    let dependencies = tx.query("SELECT v.relname::text FROM pg_depend d JOIN pg_rewrite r ON r.oid=d.objid JOIN pg_class v ON v.oid=r.ev_class JOIN pg_class t ON t.oid=d.refobjid JOIN pg_namespace n ON n.oid=t.relnamespace WHERE d.classid='pg_rewrite'::regclass AND d.refclassid='pg_class'::regclass AND n.nspname=$1 AND t.relname=ANY($2) LIMIT 1", &[&schema,&drop_tables]).await?;
    if let Some(view) = dependencies.first() {
        return Err(unsupported(format!(
            "database view dependency '{}'",
            view.try_get::<_, String>(0).map_err(shape)?
        )));
    }
    let fks = tx.query("SELECT sn.nspname::text,s.relname::text,tn.nspname::text,t.relname::text,k.conname::text,k.conkey,k.confkey,sa.attname::text,ta.attname::text,k.confdeltype::text FROM pg_constraint k JOIN pg_class s ON s.oid=k.conrelid JOIN pg_namespace sn ON sn.oid=s.relnamespace JOIN pg_class t ON t.oid=k.confrelid JOIN pg_namespace tn ON tn.oid=t.relnamespace LEFT JOIN pg_attribute sa ON sa.attrelid=s.oid AND sa.attnum=k.conkey[1] LEFT JOIN pg_attribute ta ON ta.attrelid=t.oid AND ta.attnum=k.confkey[1] WHERE k.contype='f' AND tn.nspname=$1 ORDER BY sn.nspname,s.relname,k.conname LIMIT 10001", &[&schema]).await?;
    admit_targets(0, fks.len())?;
    for fk in fks {
        let source_schema: String = fk.try_get(0).map_err(shape)?;
        let source: String = fk.try_get(1).map_err(shape)?;
        let target: String = fk.try_get(3).map_err(shape)?;
        let constraint: String = fk.try_get(4).map_err(shape)?;
        if drop_tables.contains(&target.as_str()) {
            if source_schema != schema || !drop_tables.contains(&source.as_str()) {
                return Err(unsupported(format!(
                    "survivor foreign key '{constraint}' referencing dropped table '{target}'"
                )));
            }
            continue;
        }
        if target == "installed_addon_identity" {
            // Receipts are keyed by addon name rather than an admitted integer-id effect.
            // Refuse survivor relationships before promising a read-only preview or deleting
            // the receipt; their CASCADE/SET NULL/default effects are not in this plan.
            if source_schema != schema || !drop_tables.contains(&source.as_str()) {
                return Err(unsupported(format!(
                    "survivor foreign key '{constraint}' referencing installed addon identity"
                )));
            }
            continue;
        }
        let Some(deletion) = deletes
            .iter()
            .find(|effect| effect.table == target && !effect.ids.is_empty())
        else {
            continue;
        };
        let source_keys: Vec<i16> = fk.try_get(5).map_err(shape)?;
        let target_keys: Vec<i16> = fk.try_get(6).map_err(shape)?;
        let source_column: Option<String> = fk.try_get(7).map_err(shape)?;
        let target_column: Option<String> = fk.try_get(8).map_err(shape)?;
        if source_keys.len() != 1
            || target_keys.len() != 1
            || target_column.as_deref() != Some("id")
        {
            return Err(unsupported(format!(
                "composite metadata foreign key '{constraint}'"
            )));
        }
        let source_column = source_column
            .ok_or_else(|| unsupported(format!("missing foreign key column '{constraint}'")))?;
        if source_schema != schema {
            return Err(unsupported(format!(
                "cross-schema metadata foreign key '{constraint}'"
            )));
        }
        admit_index_expressions(tx, schema, &[source.as_str()]).await?;
        // Only built-in integer identities can be compared with the planned row IDs.
        // Relation tables and other shapes have no identity we can safely invent. Check only
        // existence there: no matching reference means no hidden effect; any match refuses.
        let identity = tx.query("SELECT EXISTS (SELECT 1 FROM pg_catalog.pg_attribute a JOIN pg_catalog.pg_class c ON c.oid=a.attrelid JOIN pg_catalog.pg_namespace n ON n.oid=c.relnamespace WHERE n.nspname=$1 AND c.relname=$2 AND a.attname='id' AND a.attnum>0 AND NOT a.attisdropped AND a.attnotnull AND a.atttypid IN (20,21,23))", &[&schema, &source]).await?;
        if !identity[0].try_get::<_, bool>(0).map_err(shape)? {
            let references = tx.query(
                &format!("SELECT EXISTS (SELECT 1 FROM {} WHERE {} OPERATOR(pg_catalog.=) ANY($1) LIMIT 1)", qualified(schema, &source), erp_sql::quote(&source_column)),
                &[&deletion.ids],
            ).await?;
            if references[0].try_get::<_, bool>(0).map_err(shape)? {
                return Err(unsupported(format!(
                    "survivor foreign key effect '{constraint}'"
                )));
            }
            continue;
        }
        let references = tx
            .query(
                &format!(
                    "SELECT id::pg_catalog.int8 FROM {} WHERE {} OPERATOR(pg_catalog.=) ANY($1) ORDER BY id LIMIT {}",
                    qualified(schema, &source),
                    erp_sql::quote(&source_column),
                    MAX_UNINSTALL_TARGETS + 1
                ),
                &[&deletion.ids],
            )
            .await?;
        let action: String = fk.try_get(9).map_err(shape)?;
        if !references.is_empty() {
            let modifying = match action.as_str() {
                "n" => Some("SET NULL"),
                "d" => Some("SET DEFAULT"),
                "c" => {
                    let source_position = deletes.iter().position(|effect| effect.table == source);
                    let target_position = deletes.iter().position(|effect| effect.table == target);
                    if source_position
                        .zip(target_position)
                        .is_some_and(|(source, target)| source < target)
                    {
                        None
                    } else {
                        Some("CASCADE")
                    }
                }
                "a" | "r" => None,
                _ => {
                    return Err(unsupported(format!(
                        "unknown metadata foreign key action for constraint '{constraint}'"
                    )));
                }
            };
            if let Some(action) = modifying {
                return Err(unsupported(format!(
                    "metadata foreign key action {action} for constraint '{constraint}'"
                )));
            }
        }
        for reference in references {
            let id: i64 = reference.try_get(0).map_err(shape)?;
            if !deletes
                .iter()
                .any(|effect| effect.table == source && effect.ids.contains(&id))
            {
                return Err(unsupported(format!(
                    "survivor foreign key effect '{constraint}'"
                )));
            }
        }
    }
    Ok(trusted_signals)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "unit test assertions")]
    use super::*;
    #[test]
    fn exact_target_admission_boundary() {
        assert_eq!(
            admit_targets(MAX_UNINSTALL_TARGETS - 1, 1).unwrap(),
            MAX_UNINSTALL_TARGETS
        );
        let error = admit_targets(MAX_UNINSTALL_TARGETS, 1).unwrap_err();
        match error {
            Error::Orm(error) => {
                assert_eq!(error.kind(), erp_fields::ErrorKind::Validation);
                assert_eq!(
                    error.message(),
                    "Uninstall impact exceeds 10000 target identities"
                );
            }
            other => panic!("unexpected {other:?}"),
        }
    }
    #[test]
    fn canonical_serialization_stops_at_the_byte_boundary() {
        assert_eq!(json_with_limit("abcd", 6).unwrap(), br#""abcd""#);
        let error = json_with_limit("abcd", 5).unwrap_err();
        match error {
            Error::Orm(error) => {
                assert_eq!(error.kind(), erp_fields::ErrorKind::Validation);
                assert_eq!(
                    error.message(),
                    "Uninstall impact exceeds 5 canonical bytes"
                );
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn exact_output_admission_boundary() {
        admit_bytes(MAX_UNINSTALL_IMPACT_BYTES).unwrap();
        let error = admit_bytes(MAX_UNINSTALL_IMPACT_BYTES + 1).unwrap_err();
        match error {
            Error::Orm(error) => {
                assert_eq!(error.kind(), erp_fields::ErrorKind::Validation);
                assert_eq!(
                    error.message(),
                    "Uninstall impact exceeds 65536 response bytes"
                );
            }
            other => panic!("unexpected {other:?}"),
        }
    }
    #[test]
    fn canonical_digest_binds_same_count_identity_replacement() {
        assert_ne!(
            fingerprint(&json(&vec![1_i64, 2]).unwrap()),
            fingerprint(&json(&vec![1_i64, 3]).unwrap())
        );
    }
    #[test]
    fn deadline_only_lowers_existing_limit() {
        let deadline = Duration::from_millis(UNINSTALL_STATEMENT_DEADLINE_MS);
        assert_eq!(lowered(0, deadline), deadline);
        assert_eq!(lowered(17, deadline), Duration::from_millis(17));
        assert_eq!(lowered(UNINSTALL_STATEMENT_DEADLINE_MS, deadline), deadline);
        assert_eq!(
            lowered(UNINSTALL_STATEMENT_DEADLINE_MS + 1, deadline),
            deadline
        );
    }
}
