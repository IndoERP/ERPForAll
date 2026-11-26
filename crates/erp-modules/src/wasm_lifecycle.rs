//! Zero-restart install, upgrade and uninstall of WebAssembly addons (
//!
//! A WebAssembly addon is a module of the catalog whose source is a `.erpaddon` package. Its
//! operations run through the same pipeline as every native module operation
//! ([`ModuleSystem`]): one transaction under the operation lock, dependency and state checks,
//! schema sync, the registry-change signal other nodes wake on, rollback on any error. This file
//! adds what is specific to a package:
//!
//! * before the operation, the package is checked (consent by type, `describe()` against the
//!   manifest, dependencies installed, capabilities this host can enforce) and *staged*: the
//!   models read from `describe()` and the method layers become visible to the registry
//!   composition of that one operation, and to nothing else;
//! * inside the operation's transaction ([`PackageRuntime::before_commit`]) the package blob and
//!   an audit row are written, and any change the request did not name (a cascade, an automatic
//!   dependency install) refuses the whole operation;
//! * after the commit the staged package becomes live; on any failure it is dropped and the
//!   previous registry is the only one that exists.
//!
//! Another node, or a restarted server, loads the packages from `ir_module_package`
//! ([`WasmLifecycle::load`]); the registry generation bump of the operation tells it when.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use erp_fields::{Digits, IndexKind, OnDelete};
use erp_orm::{
    Actor, BoxFuture, Env, Orm, Transaction, WasmComputeSpec, WasmConstraintSpec, WasmMethodImpl,
    wasm_compute_impl, wasm_constraint_impl,
};
use erp_registry::{
    CompanyAccessDef, CompanyBranch, DefaultValue, EmptyCompany, FieldDef, KeyDef, KindDef,
    ModelDef, ModelKind, Registry,
};
use erp_wasm_host::{
    CallableMethods, DescribedMethod, Limits, MAX_DESCRIBED_METHODS, OrmCapabilities, WasmAddon,
    WasmHost, callable_from, described_methods,
};
use serde_json::Value;

use crate::discovery::{AddonIndex, AddonSourceKind, PackageData, PackageFiles};
use crate::error::{Error, Result, WasmReason};
use crate::graph::ModuleState;
use crate::install::{InstallAuthorization, InstallAuthorizer};
use crate::manifest::{Manifest, ModuleName};
use crate::package::{ConsentedPackage, ErpAddonPackage, WasmCapability, store_package};
use crate::package_runtime::{PackageChange, PackageChecks, PackageRuntime};
use crate::system::{LoadOutcome, ModuleSystem, OperationOutcome, PackageAccess, PackageOp};

/// The table of lifecycle audit rows of WebAssembly addons: one insert per operation, written in
/// the operation's own transaction.
///
/// Insert-only, in two layers. The table is a core table (`core_ddl!` below), so the schema owner
/// creates and owns it; the release grants give the runtime role nothing on it
/// ([`revoke_wasm_lifecycle_runtime_writes`]), exactly like the install audit. A row
/// trigger refuses `UPDATE` and `DELETE` and a statement trigger refuses `TRUNCATE`, so code running
/// as the owner role cannot rewrite history by accident either.
pub const AUDIT_TABLE: &str = "ir_module_wasm_audit";

/// The read [`WasmLifecycle::load`] makes: the newest stored package of every installed package addon
/// with the approval recorded for it in the audit. Runs as the role the module system connects with:
/// the owner role (the runtime role has no privilege on the audit).
pub const INSTALLED_PACKAGES_SQL: &str = "SELECT DISTINCT ON (p.name) p.name, p.sha256, p.package_data, \
                            p.capabilities, p.client_dist_digest, \
                            a.id IS NOT NULL, a.capabilities, a.client_dist_digest \
                     FROM ir_module_package p \
                     JOIN ir_module_module m ON m.name = p.name \
                     LEFT JOIN LATERAL ( \
                         SELECT w.id, w.capabilities, w.client_dist_digest \
                         FROM ir_module_wasm_audit w \
                         WHERE w.module = p.name AND w.sha256 = p.sha256 \
                           AND w.operation IN ('install', 'upgrade') \
                         ORDER BY w.id DESC LIMIT 1) a ON true \
                     WHERE m.state IN ('installed', 'to upgrade', 'to remove') \
                     ORDER BY p.name, p.id DESC";

/// The table the verified archives are stored in (`ir_module_package`).
pub const PACKAGE_TABLE: &str = "ir_module_package";

const AUDIT_DDL: &str = "CREATE TABLE IF NOT EXISTS ir_module_wasm_audit (
    id BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    operation TEXT NOT NULL CHECK (operation IN ('install', 'upgrade', 'uninstall')),
    module VARCHAR(128) NOT NULL,
    version VARCHAR(32) NOT NULL,
    previous_version VARCHAR(32),
    sha256 CHAR(64),
    capabilities JSONB NOT NULL,
    client_dist_digest CHAR(64),
    actor_uid BIGINT NOT NULL,
    actor_kind TEXT NOT NULL CHECK (actor_kind IN ('operator', 'session')),
    session_id UUID,
    created_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
    CONSTRAINT ir_module_wasm_audit_session_ck CHECK ((actor_kind = 'session') = (session_id IS NOT NULL))
);
REVOKE ALL ON ir_module_wasm_audit FROM PUBLIC;
DO $f$ BEGIN
    -- created once, never replaced: replacing needs ownership of the function, and this DDL also
    -- runs from roles that may only have the right to create the missing tables
    IF to_regproc(current_schema() || '.ir_module_wasm_audit_insert_only') IS NULL THEN
        CREATE FUNCTION ir_module_wasm_audit_insert_only() RETURNS trigger LANGUAGE plpgsql AS $b$
        BEGIN
            RAISE EXCEPTION 'ir_module_wasm_audit is insert-only (% refused)', TG_OP USING ERRCODE = '42501';
        END
        $b$;
    END IF;
END $f$;
DO $t$ BEGIN
    IF NOT EXISTS (SELECT 1 FROM pg_trigger WHERE tgrelid = 'ir_module_wasm_audit'::regclass
                   AND tgname = 'ir_module_wasm_audit_rows_insert_only') THEN
        CREATE TRIGGER ir_module_wasm_audit_rows_insert_only BEFORE UPDATE OR DELETE
            ON ir_module_wasm_audit FOR EACH ROW EXECUTE FUNCTION ir_module_wasm_audit_insert_only();
    END IF;
    IF NOT EXISTS (SELECT 1 FROM pg_trigger WHERE tgrelid = 'ir_module_wasm_audit'::regclass
                   AND tgname = 'ir_module_wasm_audit_truncate_insert_only') THEN
        CREATE TRIGGER ir_module_wasm_audit_truncate_insert_only BEFORE TRUNCATE
            ON ir_module_wasm_audit FOR EACH STATEMENT EXECUTE FUNCTION ir_module_wasm_audit_insert_only();
    END IF;
END $t$";

erp_orm::core_ddl!(
    WASM_AUDIT,
    owner = "erp-modules",
    name = "ir_module_wasm_audit",
    purpose = "audit of WebAssembly addon install, upgrade and uninstall",
    tables = ["ir_module_wasm_audit"],
    sql = AUDIT_DDL
);
erp_orm::core_ddl!(
    PACKAGE_STORE,
    owner = "erp-modules",
    name = "ir_module_package",
    purpose = "verified .erpaddon archives of installed WebAssembly addons",
    tables = ["ir_module_package"],
    sql = crate::package::PACKAGE_TABLE_DDL
);

/// References the two registrations above so the linker keeps them in every binary that holds the
/// module system (called by `install::core_tables`).
pub(crate) fn keep_core_tables_linked() {
    std::hint::black_box(&WASM_AUDIT);
    std::hint::black_box(&PACKAGE_STORE);
}

/// Most models one package's `describe()` may contribute. A package over this or the next limits is
/// refused with [`WasmReason::TooLarge`] before anything is interned or built from it (G security
/// review r2 N1). They are admission limits, not performance budgets.
pub const MAX_DESCRIBED_MODELS: usize = 256;
/// Most fields, over all models, one package's `describe()` may contribute.
pub const MAX_DESCRIBED_FIELDS: usize = 4096;
/// Most methods one package's `describe()` may contribute (the limit of `erp-wasm-host`).
pub const MAX_PACKAGE_METHODS: usize = MAX_DESCRIBED_METHODS;

/// Revoke from the runtime role `role` everything that would let it write the lifecycle tables:
/// all privileges on the audit table and its sequence (the install audit's rule; the server
/// and the operator commands read it with the owner connection) and
/// every write on the package store (it keeps `SELECT`, which a node needs to follow another node's
/// install). The release grants call this after granting the runtime role the ordinary tables.
///
/// # Errors
/// `Access` unless `tx` acts as the `System` principal with full elevation; `Programming` when
/// `role` is empty, longer than 63 bytes, contains NUL or owns one of the tables (the role that
/// owns them must be able to write them); `Database` when a statement fails.
pub async fn revoke_wasm_lifecycle_runtime_writes(
    tx: &mut Transaction,
    role: &str,
) -> erp_fields::Result<()> {
    crate::system::require_system(&tx.env(), "revoke_wasm_lifecycle_runtime_writes")?;
    if role.is_empty() || role.len() > 63 || role.contains('\0') {
        return Err(erp_fields::Error::programming(format!(
            "invalid role name {role:?}: 1 to 63 bytes without NUL"
        )));
    }
    for table in [AUDIT_TABLE, PACKAGE_TABLE] {
        let rows = tx
            .query(
                "SELECT pg_get_userbyid(c.relowner)::text FROM pg_class c \
                 WHERE c.oid = to_regclass($1)",
                &[&table],
            )
            .await?;
        let Some(row) = rows.first() else {
            return Err(erp_fields::Error::programming(format!(
                "the table {table} is missing: initialize the database before revoking the \
                 runtime role"
            )));
        };
        let owner: String = row.try_get(0).map_err(|e| {
            erp_fields::Error::programming(format!("unexpected owner of {table}: {e}"))
        })?;
        if owner == role {
            return Err(erp_fields::Error::programming(format!(
                "role {role:?} owns {table}: the runtime role must be another role"
            )));
        }
    }
    let role = erp_sql::quote(role);
    tx.batch(&format!(
        "REVOKE ALL ON ir_module_wasm_audit FROM {role}; \
         REVOKE ALL ON SEQUENCE ir_module_wasm_audit_id_seq FROM {role}; \
         REVOKE INSERT, UPDATE, DELETE, TRUNCATE, REFERENCES, TRIGGER ON ir_module_package FROM {role}; \
         REVOKE ALL ON SEQUENCE ir_module_package_id_seq FROM {role}"
    ))
    .await
}

fn refuse(addon: &ModuleName, reason: WasmReason) -> Error {
    Error::WasmAddon {
        addon: addon.clone(),
        reason,
    }
}

// ---- the component seam --------------------------------------------------------------------

/// A loaded component: its `describe()` text and the method layers it contributes.
pub trait WasmComponent: Send + Sync + fmt::Debug {
    /// The `describe()` text, read once at load.
    fn describe(&self) -> &str;

    /// The method layers for the ORM of one module set. `plain` is that set's ORM without
    /// WebAssembly layers (its exposure table limits what a guest may call).
    ///
    /// # Errors
    /// The description is not a schema-1 document.
    fn method_impls(&self, plain: &Orm) -> Result<Vec<WasmMethodImpl>>;

    /// The methods an RPC client may call (`model.method`, sorted): the ones the author marked.
    /// What an administrator is told at install time. A component with none lists none.
    fn exposed_methods(&self) -> Vec<String> {
        Vec::new()
    }
}

/// Turns the bytes of a component into a [`WasmComponent`].
pub trait ComponentLoader: Send + Sync {
    /// Compile and describe `wasm` for `module`.
    ///
    /// # Errors
    /// The bytes are not a valid component, or it traps in `describe()`.
    fn load<'a>(
        &'a self,
        module: &'a ModuleName,
        wasm: &'a [u8],
    ) -> BoxFuture<'a, Result<Arc<dyn WasmComponent>>>;
}

/// The loader for production: `erp-wasm-host`.
///
/// A component's ORM rights are [`OrmCapabilities::all`]: reading and writing the ORM is the
/// baseline of every WebAssembly addon and is always bounded by the caller's own access rights;
/// the manifest `[wasm]` capabilities name what goes beyond it.
pub struct HostLoader {
    host: Arc<WasmHost>,
    limits: Limits,
}

impl HostLoader {
    /// A loader over `host` with the limits every call of a loaded component runs under.
    pub fn new(host: Arc<WasmHost>, limits: Limits) -> Self {
        Self { host, limits }
    }
}

impl ComponentLoader for HostLoader {
    fn load<'a>(
        &'a self,
        module: &'a ModuleName,
        wasm: &'a [u8],
    ) -> BoxFuture<'a, Result<Arc<dyn WasmComponent>>> {
        Box::pin(async move {
            let fail = |e: &dyn fmt::Display| refuse(module, WasmReason::Component(e.to_string()));
            let component = self.host.component(wasm).map_err(|e| fail(&e))?;
            let probe = WasmAddon::new(
                Arc::clone(&self.host),
                module.as_str(),
                component.clone(),
                self.limits,
                OrmCapabilities::all(),
                CallableMethods::default(),
            );
            let describe = probe.describe().await.map_err(|e| fail(&e))?;
            // Read once, here: the callable set and the ORM layers are both built from this list
            // (G security review r2 N2), and a package over the method limit stops at load.
            let described = described_methods(module.as_str(), &describe).map_err(|e| fail(&e))?;
            Ok(Arc::new(HostComponent {
                host: Arc::clone(&self.host),
                module: module.clone(),
                bytes: Arc::from(wasm),
                limits: self.limits,
                describe,
                described,
            }) as Arc<dyn WasmComponent>)
        })
    }
}

struct HostComponent {
    host: Arc<WasmHost>,
    module: ModuleName,
    /// The component bytes: compiling again hits the host's cache (same digest, version, cpu).
    bytes: Arc<[u8]>,
    limits: Limits,
    describe: String,
    /// The methods `describe` lists for this addon, read once at load.
    described: Vec<DescribedMethod>,
}

impl fmt::Debug for HostComponent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HostComponent")
            .field("module", &self.module)
            .finish_non_exhaustive()
    }
}

impl WasmComponent for HostComponent {
    fn describe(&self) -> &str {
        &self.describe
    }

    fn exposed_methods(&self) -> Vec<String> {
        erp_wasm_host::exposed_methods(&self.described)
    }

    fn method_impls(&self, plain: &Orm) -> Result<Vec<WasmMethodImpl>> {
        let fail =
            |e: &dyn fmt::Display| refuse(&self.module, WasmReason::Component(e.to_string()));
        // One list for both: the methods a guest may call (`callable_from`) and the layers of the
        // chain (`method_impls_from`) never come from two readings of `describe()`.
        let callable = callable_from(plain, &self.described);
        let component = self.host.component(&self.bytes).map_err(|e| fail(&e))?;
        let addon = WasmAddon::new(
            Arc::clone(&self.host),
            self.module.as_str(),
            component,
            self.limits,
            OrmCapabilities::all(),
            callable,
        );
        Ok(addon.method_impls_from(&self.described))
    }
}

// ---- describe() -> model definitions ---------------------------------------------------------

/// The models, fields and checks `describe()` yields for the addon `manifest` names.
///
/// Only the contributions of the addon itself count (a component also holds the registrations of
/// the addons it was linked with). Everything this build cannot run yet is refused by name; nothing
/// is skipped.
///
/// # Errors
/// [`WasmReason::DescribeMismatch`] for a name or dependency the manifest does not agree with,
/// [`WasmReason::Unsupported`] for a contribution, field kind or attribute not available yet.
pub fn models_from_describe(manifest: &Manifest, describe: &str) -> Result<Vec<ModelDef>> {
    let name = manifest.name();
    let mismatch = |text: String| refuse(name, WasmReason::DescribeMismatch(text));
    let unsupported = |text: String| refuse(name, WasmReason::Unsupported(text));
    let doc: Value = serde_json::from_str(describe)
        .map_err(|e| mismatch(format!("describe() is not JSON: {e}")))?;
    if doc["schema"] != 1 {
        return Err(mismatch(format!(
            "unknown describe schema {}",
            doc["schema"]
        )));
    }
    let list = doc["contributions"]
        .as_array()
        .ok_or_else(|| mismatch("describe() has no contributions list".to_owned()))?;
    let too_large = |text: String| refuse(name, WasmReason::TooLarge(text));
    let mut declared: BTreeSet<&str> = BTreeSet::new();
    let mut addon_entry: Option<&Value> = None;
    let mut defs = Vec::new();
    let mut with_methods: BTreeSet<&str> = BTreeSet::new();
    let (mut methods, mut fields) = (0usize, 0usize);
    for entry in list {
        let (Some(owner), Some(kind)) = (entry["addon"].as_str(), entry["kind"].as_str()) else {
            return Err(mismatch(format!("malformed contribution {entry}")));
        };
        declared.insert(owner);
        if owner != name.as_str() {
            continue;
        }
        let model = entry["model"].as_str().unwrap_or_default();
        let what = entry["name"].as_str().unwrap_or_default();
        match kind {
            "addon" => addon_entry = Some(entry),
            "method" | "override" => {
                with_methods.insert(model);
                methods += 1;
                if methods > MAX_PACKAGE_METHODS {
                    return Err(too_large(format!(
                        "more than {MAX_PACKAGE_METHODS} methods"
                    )));
                }
            }
            "model" | "extend" => {
                // counted before the model is built, so nothing of an over-large package is
                let here = entry["detail"]["fields"].as_array().map_or(0, Vec::len);
                if defs.len() >= MAX_DESCRIBED_MODELS {
                    return Err(too_large(format!(
                        "more than {MAX_DESCRIBED_MODELS} models"
                    )));
                }
                fields = fields.saturating_add(here);
                if fields > MAX_DESCRIBED_FIELDS {
                    return Err(too_large(format!(
                        "more than {MAX_DESCRIBED_FIELDS} fields"
                    )));
                }
                defs.push(model_def(name, kind == "extend", model, entry)?);
            }
            // read by `checks_from_describe`
            "compute" | "constraint" => {}
            // read by `refs_from_describe`
            "model_ref" => {}
            other => {
                return Err(unsupported(format!(
                    "{other} '{what}' on '{model}': overrides of a compute and onchange methods \
                     of a WebAssembly addon are not available in this build"
                )));
            }
        }
    }
    // An extension needs a field, method or company policy. A policy-only extension can
    // narrow an existing policy; the registry rejects one that widens it.
    for def in &defs {
        if def.inherit.contains(&def.name)
            && def.fields.is_empty()
            && !with_methods.contains(def.name.as_str())
            && def.company_access.is_none()
        {
            return Err(mismatch(format!(
                "the addon extends '{}' but adds no field and no method to it",
                def.name
            )));
        }
    }
    let Some(entry) = addon_entry else {
        return Err(mismatch(format!(
            "describe() lists no addon named '{name}' (it lists: [{}])",
            declared.iter().copied().collect::<Vec<_>>().join(", ")
        )));
    };
    // `addon!` adds `base` to the dependencies it is given; a manifest without dependencies
    // defaults to `base` as well, so the two sides compare as sets with `base` in both.
    let described: BTreeSet<String> = entry["detail"]["depends"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|d| d.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default();
    let mut manifest_depends: BTreeSet<String> =
        manifest.depends().iter().map(ToString::to_string).collect();
    let mut described = described;
    if name.as_str() != "base" {
        manifest_depends.insert("base".to_owned());
        described.insert("base".to_owned());
    }
    if described != manifest_depends {
        return Err(mismatch(format!(
            "describe() depends on [{}] but the manifest on [{}]",
            described.into_iter().collect::<Vec<_>>().join(", "),
            manifest_depends.into_iter().collect::<Vec<_>>().join(", ")
        )));
    }
    let exact = match entry["detail"].get("exact_only") {
        Some(serde_json::Value::Bool(value)) => *value,
        None => erp_registry::AddonDescriptor::requires_exact(name.as_str()),
        Some(_) => {
            return Err(mismatch(
                "describe() exact_only must be a boolean".to_owned(),
            ));
        }
    };
    if exact != manifest.exact_only() {
        return Err(mismatch(
            "describe() and the manifest disagree on exact_only".to_owned(),
        ));
    }
    Ok(defs)
}

/// A typed handle a package names for a field another addon declares (`model_ref!`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DescribedRefField {
    /// The field's name.
    pub name: String,
    /// The `ttype` names that can hold the handle's Rust type.
    pub kinds: Vec<String>,
    /// The model a relational field must point to.
    pub comodel: Option<String>,
}

/// The typed handles a package names on one model of another addon.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DescribedRef {
    /// The model's technical name.
    pub model: String,
    /// The fields, in source order.
    pub fields: Vec<DescribedRefField>,
}

/// The typed model references `describe()` yields for the addon `manifest` names.
///
/// # Errors
/// [`WasmReason::DescribeMismatch`] for a malformed entry, [`WasmReason::TooLarge`] past
/// [`MAX_DESCRIBED_FIELDS`] named fields.
pub fn refs_from_describe(manifest: &Manifest, describe: &str) -> Result<Vec<DescribedRef>> {
    let name = manifest.name();
    let mismatch = |text: String| refuse(name, WasmReason::DescribeMismatch(text));
    let doc: Value = serde_json::from_str(describe)
        .map_err(|e| mismatch(format!("describe() is not JSON: {e}")))?;
    let list = doc["contributions"]
        .as_array()
        .ok_or_else(|| mismatch("describe() has no contributions list".to_owned()))?;
    let mut out = Vec::new();
    let mut count = 0usize;
    for entry in list {
        if entry["addon"].as_str() != Some(name.as_str()) || entry["kind"] != "model_ref" {
            continue;
        }
        let Some(model) = entry["model"].as_str().filter(|m| !m.is_empty()) else {
            return Err(mismatch(format!(
                "model_ref entry without a model: {entry}"
            )));
        };
        let what = format!(
            "model_ref '{}' of '{model}'",
            entry["name"].as_str().unwrap_or("?")
        );
        let Some(items) = entry["detail"]["fields"].as_array() else {
            return Err(mismatch(format!("{what}: fields is missing or not a list")));
        };
        let mut fields = Vec::new();
        for item in items {
            count = count.saturating_add(1);
            if count > MAX_DESCRIBED_FIELDS {
                return Err(refuse(
                    name,
                    WasmReason::TooLarge(format!(
                        "more than {MAX_DESCRIBED_FIELDS} fields named by model references"
                    )),
                ));
            }
            let (Some(field), Some(kinds)) = (item["name"].as_str(), item["kinds"].as_array())
            else {
                return Err(mismatch(format!("{what}: malformed field entry {item}")));
            };
            let kinds = kinds
                .iter()
                .map(|k| {
                    k.as_str()
                        .map(str::to_owned)
                        .ok_or_else(|| mismatch(format!("{what}.{field}: kind {k} is not a name")))
                })
                .collect::<Result<Vec<_>>>()?;
            let comodel = match &item["comodel"] {
                Value::Null => None,
                Value::String(text) => Some(text.clone()),
                other => {
                    return Err(mismatch(format!(
                        "{what}.{field}: comodel {other} is not a name"
                    )));
                }
            };
            fields.push(DescribedRefField {
                name: field.to_owned(),
                kinds,
                comodel,
            });
        }
        out.push(DescribedRef {
            model: model.to_owned(),
            fields,
        });
    }
    Ok(out)
}

/// What the registry says about one field a reference names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RefLookup {
    /// No model of that name is in the registry.
    NoModel,
    /// The model has no such field.
    NoField,
    /// The field's `ttype` name and comodel.
    Field {
        /// `char`, `many2one`, ...
        kind: &'static str,
        /// The comodel of a relational field.
        comodel: Option<String>,
    },
}

/// [`check_refs`] against the models of `registry`.
///
/// # Errors
/// As [`check_refs`].
pub fn check_refs_in_registry(
    addon: &ModuleName,
    refs: &[DescribedRef],
    registry: &Registry,
) -> Result<()> {
    check_refs(addon, refs, |model, field| match registry.model(model) {
        None => RefLookup::NoModel,
        Some(spec) => match spec.field(field) {
            None => RefLookup::NoField,
            Some(f) => RefLookup::Field {
                kind: f.ty.type_name(),
                comodel: f.ty.comodel().map(str::to_owned),
            },
        },
    })
}

/// Checks every handle in `refs` against what `lookup(model, field)` says of the installed models.
///
/// # Errors
/// [`WasmReason::ModelRef`] naming the addon, the model, the field and what differs.
pub fn check_refs(
    addon: &ModuleName,
    refs: &[DescribedRef],
    lookup: impl Fn(&str, &str) -> RefLookup,
) -> Result<()> {
    let bad = |text: String| refuse(addon, WasmReason::ModelRef(text));
    for reference in refs {
        for field in &reference.fields {
            let (model, name) = (reference.model.as_str(), field.name.as_str());
            match lookup(model, name) {
                RefLookup::NoModel => {
                    return Err(bad(format!(
                        "model '{model}' is not in the registry (does the addon depend on the addon that declares it?)"
                    )));
                }
                RefLookup::NoField => {
                    return Err(bad(format!("model '{model}' has no field '{name}'")));
                }
                RefLookup::Field { kind, comodel } => {
                    if !field.kinds.iter().any(|k| k == kind) {
                        return Err(bad(format!(
                            "{model}.{name} is a {kind} field but the handle's Rust type holds [{}]",
                            field.kinds.join(", ")
                        )));
                    }
                    if let Some(want) = &field.comodel
                        && comodel.as_deref() != Some(want.as_str())
                    {
                        return Err(bad(format!(
                            "{model}.{name} points to {} but the handle names {want}",
                            comodel.as_deref().unwrap_or("no model")
                        )));
                    }
                }
            }
        }
    }
    Ok(())
}

/// The computes and constraints a package's `describe()` declares for its own addon.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DescribedChecks {
    /// Scalar computes, in `describe()` order.
    pub computes: Vec<WasmComputeSpec>,
    /// Constraints, in `describe()` order.
    pub constraints: Vec<WasmConstraintSpec>,
}

/// Most computes and constraints (each) one package's `describe()` may declare.
pub const MAX_PACKAGE_CHECKS: usize = 1024;

/// The computes and constraints `describe()` yields for the addon `manifest` names.
///
/// A compute runs in the component from plain field values of its `depends` and `reads`; an async
/// compute, a path through a relation in `depends` or `reads`, and `compute_override` are refused
/// by name, as is anything else this build cannot run. Nothing is skipped.
///
/// # Errors
/// [`WasmReason::Unsupported`] for such a contribution, [`WasmReason::TooLarge`] past
/// [`MAX_PACKAGE_CHECKS`], [`WasmReason::DescribeMismatch`] for a malformed entry.
pub fn checks_from_describe(manifest: &Manifest, describe: &str) -> Result<DescribedChecks> {
    let name = manifest.name();
    let mismatch = |text: String| refuse(name, WasmReason::DescribeMismatch(text));
    let unsupported = |text: String| refuse(name, WasmReason::Unsupported(text));
    let doc: Value = serde_json::from_str(describe)
        .map_err(|e| mismatch(format!("describe() is not JSON: {e}")))?;
    let list = doc["contributions"]
        .as_array()
        .ok_or_else(|| mismatch("describe() has no contributions list".to_owned()))?;
    let mut out = DescribedChecks::default();
    let strings = |entry: &Value, key: &str, what: &str| -> Result<Vec<String>> {
        let mut names = Vec::new();
        let Some(items) = entry["detail"][key].as_array() else {
            return Err(mismatch(format!("{what}: {key} is missing or not a list")));
        };
        for item in items {
            let Some(text) = item.as_str() else {
                return Err(mismatch(format!("{what}: {key} lists {item}, not a name")));
            };
            if text.contains('.') {
                return Err(unsupported(format!(
                    "{what}: {key} names the path '{text}'; a WebAssembly addon computes from \
                     plain fields of its own model in this build"
                )));
            }
            names.push(text.to_owned());
        }
        Ok(names)
    };
    for entry in list {
        if entry["addon"].as_str() != Some(name.as_str()) {
            continue;
        }
        let kind = entry["kind"].as_str().unwrap_or_default();
        let model = entry["model"].as_str().unwrap_or_default().to_owned();
        let method = entry["name"].as_str().unwrap_or_default().to_owned();
        let what = format!("{kind} '{method}' on '{model}'");
        match kind {
            "compute" => {
                if flag(name, &what, &entry["detail"]["async"])? {
                    return Err(unsupported(format!(
                        "{what}: an async compute is not available in a WebAssembly addon; write \
                         a synchronous compute over the declared fields"
                    )));
                }
                if out.computes.len() >= MAX_PACKAGE_CHECKS {
                    return Err(refuse(
                        name,
                        WasmReason::TooLarge(format!("more than {MAX_PACKAGE_CHECKS} computes")),
                    ));
                }
                out.computes.push(WasmComputeSpec {
                    addon: name.as_str().to_owned(),
                    model,
                    method,
                    depends: strings(entry, "depends", &what)?,
                    reads: strings(entry, "reads", &what)?,
                    depends_context: strings(entry, "depends_context", &what)?,
                });
            }
            "constraint" => {
                if out.constraints.len() >= MAX_PACKAGE_CHECKS {
                    return Err(refuse(
                        name,
                        WasmReason::TooLarge(format!("more than {MAX_PACKAGE_CHECKS} constraints")),
                    ));
                }
                out.constraints.push(WasmConstraintSpec {
                    addon: name.as_str().to_owned(),
                    model,
                    method,
                    fields: strings(entry, "fields", &what)?,
                });
            }
            _ => {}
        }
    }
    Ok(out)
}

/// Every computed field names a compute the addon declares on the same model, and every compute
/// fills some field: a package that breaks this is refused when it is loaded, naming the field or
/// the compute, instead of failing later when the registry is built.
///
/// # Errors
/// [`WasmReason::DescribeMismatch`] naming the field or the compute.
fn check_compute_wiring(
    name: &ModuleName,
    defs: &[ModelDef],
    checks: &DescribedChecks,
) -> Result<()> {
    let mismatch = |text: String| refuse(name, WasmReason::DescribeMismatch(text));
    for def in defs {
        for field in &def.fields {
            let Some(method) = field.compute.as_deref() else {
                continue;
            };
            if !checks
                .computes
                .iter()
                .any(|c| c.method == method && c.model == def.name)
            {
                return Err(mismatch(format!(
                    "field {}.{} names the compute '{method}', but the addon declares no \
                     #[compute] of that name on '{}'",
                    def.name, field.name, def.name
                )));
            }
        }
    }
    for compute in &checks.computes {
        let used = defs.iter().any(|d| {
            d.name == compute.model
                && d.fields
                    .iter()
                    .any(|f| f.compute.as_deref() == Some(compute.method.as_str()))
        });
        if !used {
            return Err(mismatch(format!(
                "compute '{}' on '{}' is not the compute of any field the addon declares on it",
                compute.method, compute.model
            )));
        }
    }
    Ok(())
}

/// The text of a Rust string literal as `describe()` carries it.
fn unquote(addon: &ModuleName, what: &str, text: &str) -> Result<String> {
    serde_json::from_str::<String>(text).map_err(|_| {
        refuse(
            addon,
            WasmReason::Unsupported(format!(
                "{what} = {}: only string literals are read",
                text.chars().take(64).collect::<String>()
            )),
        )
    })
}

/// A transient policy threshold in the source text of a description: a decimal integer literal
/// with no suffix and no sign, the form the native macro accepts (`_` separators allowed). The
/// registry build checks the bounds, as for a native declaration.
fn policy_integer(addon: &ModuleName, model: &str, attr: &str, text: &str) -> Result<u64> {
    let unsupported = |reason: &str| {
        refuse(
            addon,
            WasmReason::Unsupported(format!(
                "model '{model}': {attr} = {}: {reason}",
                text.chars().take(64).collect::<String>()
            )),
        )
    };
    // `to_string` of the tokens renders a negative literal as `- 1`
    if text.trim_start().starts_with('-') {
        return Err(unsupported(
            "must not be negative (0 disables the threshold)",
        ));
    }
    let digits_only = !text.is_empty()
        && text.starts_with(|c: char| c.is_ascii_digit())
        && text.chars().all(|c| c.is_ascii_digit() || c == '_');
    if !digits_only {
        return Err(unsupported(
            "only a decimal integer literal without suffix is read",
        ));
    }
    let digits: String = text.chars().filter(|c| *c != '_').collect();
    digits
        .parse::<u64>()
        .map_err(|_| unsupported("does not fit in 64 bits"))
}

fn flag(addon: &ModuleName, what: &str, value: &Value) -> Result<bool> {
    match value {
        Value::Bool(b) => Ok(*b),
        Value::String(s) if s == "true" => Ok(true),
        Value::String(s) if s == "false" => Ok(false),
        other => Err(refuse(
            addon,
            WasmReason::Unsupported(format!("{what} = {other}: only true and false are read")),
        )),
    }
}

fn model_def(addon: &ModuleName, extend: bool, model: &str, entry: &Value) -> Result<ModelDef> {
    let unsupported = |text: String| refuse(addon, WasmReason::Unsupported(text));
    let mut def = if extend {
        ModelDef::extension(addon.as_str(), model)
    } else {
        ModelDef::new(addon.as_str(), model)
    };
    def.file = format!("wasm/{addon}.wasm");
    if let Some(raw_args) = entry["detail"].get("args") {
        let args = raw_args
            .as_object()
            .ok_or_else(|| unsupported(format!("model '{model}': args must be an object")))?;
        for (key, value) in args {
            let text = value.as_str().unwrap_or_default();
            match key.as_str() {
                // `name` repeats the model name the entry already carries
                "name" => {}
                "audit_reads" => {
                    def.audit_reads = flag(addon, &format!("model '{model}': audit_reads"), value)?
                }
                "eventful" => {
                    def.eventful = flag(addon, &format!("model '{model}': eventful"), value)?
                }
                "description" => def.description = Some(unquote(addon, "description", text)?),
                "order" => def.order = Some(unquote(addon, "order", text)?),
                "rec_name" => def.rec_name = Some(unquote(addon, "rec_name", text)?),
                "transient_max_age_seconds" => {
                    def.transient_max_age_seconds = Some(policy_integer(addon, model, key, text)?);
                }
                "transient_max_count" => {
                    def.transient_max_count = Some(policy_integer(addon, model, key, text)?);
                }
                "kind" => {
                    def.kind = Some(
                        match unquote(addon, &format!("model '{model}': kind"), text)?.as_str() {
                            "regular" => ModelKind::Regular,
                            "abstract" => ModelKind::Abstract,
                            "transient" => ModelKind::Transient,
                            other => {
                                return Err(unsupported(format!(
                                    "model '{model}': kind \"{}\" (use \"regular\", \"abstract\" \
                                 or \"transient\")",
                                    other.chars().take(64).collect::<String>()
                                )));
                            }
                        },
                    );
                }
                "company_access" => {
                    let source = value.as_str().ok_or_else(|| {
                        unsupported(format!("model '{model}': company_access: must be a string"))
                    })?;
                    def.company_access = Some(parse_company_access(source).map_err(|why| {
                        unsupported(format!("model '{model}': company_access: {why}"))
                    })?);
                }
                "parent_store" | "parent_name" | "keys" if extend => {
                    return Err(unsupported(format!(
                        "model '{model}': {key} is refused on an extension of another model; a \
                         package declares keys and a parent store only on the models it creates"
                    )));
                }
                "parent_store" => {
                    def = def.parent_store(flag(
                        addon,
                        &format!("model '{model}': parent_store"),
                        value,
                    )?);
                }
                "parent_name" => {
                    def = def.parent_name(unquote(
                        addon,
                        &format!("model '{model}': parent_name"),
                        text,
                    )?);
                }
                "keys" => {
                    for key in parse_keys(text)
                        .map_err(|why| unsupported(format!("model '{model}': keys: {why}")))?
                    {
                        def = def.key(key);
                    }
                }
                "constraints" | "indexes" | "unique_indexes" => {
                    return Err(unsupported(format!(
                        "model '{model}': {key} are raw SQL table objects, which only a native \
                         addon declares; use keys = [key(name = \"_x\", fields = [...], \
                         message = \"...\")] for a unique constraint"
                    )));
                }
                other => {
                    return Err(unsupported(format!(
                        "model '{model}': attribute '{other}' is not read from a WebAssembly addon"
                    )));
                }
            }
        }
    }
    for field in entry["detail"]["fields"].as_array().into_iter().flatten() {
        def = def.field(field_def(addon, model, field)?);
    }
    Ok(def)
}

/// Parse the macro's already-serialized source spelling at the package boundary.
/// The registry checks semantic field/edge existence after this structural check.
fn parse_company_access(source: &str) -> std::result::Result<CompanyAccessDef, String> {
    const FORMS: &str = "expected direct(…), parent(…) or unrestricted(…)";
    let mut t = Tokens::new(source, MAX_COMPANY_ACCESS_SOURCE_BYTES)?;
    let form = t.ident(FORMS)?;
    let allowed: &[&str] = match form.as_str() {
        "direct" => &["field", "branch", "empty", "parent", "escape"],
        "parent" => &["edge"],
        "unrestricted" => &["reason"],
        _ => return Err(FORMS.into()),
    };
    t.punct('(', FORMS)?;
    let mut args = BTreeMap::new();
    while !t.at(')') {
        let key = t.ident(&format!("expected a parameter name inside {form}(…)"))?;
        t.punct('=', &format!("expected key = value inside {form}(…)"))?;
        if !allowed.contains(&key.as_str()) {
            return Err(format!("unknown parameter '{key}' in {form}(…)"));
        }
        if args.contains_key(&key) {
            return Err(format!("parameter '{key}' is given twice"));
        }
        let value = t.string(&format!("{key} must be a string literal"))?;
        args.insert(key, value);
        if t.at(',') {
            t.pos += 1;
        } else if !t.at(')') {
            return Err(format!("expected key = value inside {form}(…)"));
        }
    }
    t.punct(')', FORMS)?;
    t.end(FORMS)?;
    let get = |key: &str| args.get(key).map(String::as_str);
    match form.as_str() {
        "direct" => {
            let missing: Vec<_> = ["field", "branch", "empty"]
                .into_iter()
                .filter(|key| get(key).is_none())
                .collect();
            if !missing.is_empty() {
                return Err(format!(
                    "direct(…) needs field, branch and empty (missing: {})",
                    missing.join(", ")
                ));
            }
            let field = get("field").ok_or("field missing")?;
            if field.is_empty() {
                return Err("the field name is empty".into());
            }
            let branch = CompanyBranch::parse(get("branch").ok_or("branch missing")?)
                .map_err(|_| "branch must be \"exact\" or \"ancestor\"")?;
            let empty = EmptyCompany::parse(get("empty").ok_or("empty missing")?)
                .map_err(|_| "empty must be \"allow\" or \"deny\"")?;
            match (get("parent"), get("escape")) {
                (Some(_), Some(_)) => {
                    Err("escape cannot be combined with parent in direct(…)".into())
                }
                (Some(""), None) => Err("the edge name is empty".into()),
                (None, Some("")) => Err("the escape name is empty".into()),
                (Some(edge), None) => Ok(CompanyAccessDef::direct_and_parent(
                    field, branch, empty, edge,
                )),
                (None, Some(escape)) => Ok(CompanyAccessDef::direct_escaping(
                    field, branch, empty, escape,
                )),
                (None, None) => Ok(CompanyAccessDef::direct(field, branch, empty)),
            }
        }
        "parent" => match get("edge") {
            Some("") => Err("the edge name is empty".into()),
            Some(edge) => Ok(CompanyAccessDef::parent(edge)),
            None => Err("parent(…) needs edge (missing: edge)".into()),
        },
        "unrestricted" => match get("reason") {
            Some(reason)
                if !reason.trim().is_empty()
                    && reason.len() <= erp_registry::MAX_UNRESTRICTED_REASON_BYTES =>
            {
                Ok(CompanyAccessDef::unrestricted(reason))
            }
            _ => Err(format!(
                "unrestricted(…) needs a reason that is not blank and at most {} bytes long",
                erp_registry::MAX_UNRESTRICTED_REASON_BYTES
            )),
        },
        _ => Err("expected direct(…), parent(…) or unrestricted(…)".into()),
    }
}

fn field_def(addon: &ModuleName, model: &str, field: &Value) -> Result<FieldDef> {
    let unsupported = |text: String| refuse(addon, WasmReason::Unsupported(text));
    let name = field["name"].as_str().unwrap_or_default();
    let kind_name = field["kind"].as_str().unwrap_or_default();
    let attrs = field["attrs"].as_object();
    let size = match attrs.and_then(|a| a.get("size")) {
        None => None,
        Some(v) => Some(
            v.as_str()
                .and_then(|s| s.parse::<u32>().ok())
                .ok_or_else(|| {
                    unsupported(format!("{model}.{name}: size = {v} is not a number"))
                })?,
        ),
    };
    // refused by name before the kind is read, so a field that only has one of these is told so
    for refused in ["selection_add", "selection_method", "default_fn"] {
        if attrs.is_some_and(|a| a.contains_key(refused)) {
            return Err(unsupported(format!(
                "field {model}.{name}: {refused} is not read from a WebAssembly addon in this \
                 build ({})",
                match refused {
                    "selection_add" =>
                        "extending another addon's selection follows in \
                        a later WebAssembly slice",
                    "selection_method" => "it names a Rust function",
                    _ => "it names a Rust function; use a literal default",
                }
            )));
        }
    }
    let kind = match kind_name {
        "boolean" => KindDef::Boolean,
        "bigint" => KindDef::BigInt,
        "integer" => KindDef::Integer,
        "float" => KindDef::Float {
            digits: match attrs.and_then(|a| a.get("digits")) {
                None => None,
                Some(v) => {
                    Some(parse_digits(v.as_str().unwrap_or_default()).map_err(|why| {
                        unsupported(format!("field {model}.{name}: digits: {why}"))
                    })?)
                }
            },
        },
        "selection" => {
            let Some(v) = attrs.and_then(|a| a.get("selection")) else {
                return Err(unsupported(format!(
                    "field {model}.{name}: a selection needs selection = [(\"value\", \"Label\"), ...]"
                )));
            };
            KindDef::Selection {
                values: Some(
                    parse_selection(v.as_str().unwrap_or_default()).map_err(|why| {
                        unsupported(format!("field {model}.{name}: selection: {why}"))
                    })?,
                ),
                add: Vec::new(),
                ondelete: Vec::new(),
            }
        }
        "char" => KindDef::Char { size },
        "text" => KindDef::Text,
        "html" => KindDef::Html,
        "date" => KindDef::Date,
        "datetime" => KindDef::Datetime,
        "many2one" => {
            let comodel = match attrs.and_then(|a| a.get("comodel")) {
                Some(v) => unquote(
                    addon,
                    &format!("{model}.{name}: comodel"),
                    v.as_str().unwrap_or_default(),
                )?,
                None => {
                    return Err(unsupported(format!(
                        "field {model}.{name}: a many2one needs comodel = \"model.name\""
                    )));
                }
            };
            let ondelete = match attrs.and_then(|a| a.get("ondelete")) {
                None => None,
                Some(v) => {
                    let what = format!("{model}.{name}: ondelete");
                    Some(
                        match unquote(addon, &what, v.as_str().unwrap_or_default())?.as_str() {
                            "set null" => OnDelete::SetNull,
                            "restrict" => OnDelete::Restrict,
                            "cascade" => OnDelete::Cascade,
                            _ => {
                                return Err(unsupported(format!(
                                    "field {model}.{name}: ondelete {} (use \"set null\", \
                                 \"restrict\" or \"cascade\")",
                                    v.to_string().chars().take(64).collect::<String>()
                                )));
                            }
                        },
                    )
                }
            };
            if comodel.is_empty() {
                return Err(unsupported(format!(
                    "field {model}.{name}: a many2one needs a non-empty comodel"
                )));
            }
            KindDef::Many2one { comodel, ondelete }
        }
        other => {
            return Err(unsupported(format!(
                "field {model}.{name}: kind '{other}' (a WebAssembly addon declares boolean, \
                 integer, float, selection, char, text, html, date, datetime and many2one fields \
                 in this build)"
            )));
        }
    };
    let mut out = FieldDef::new(name, kind);
    for (key, value) in attrs.into_iter().flatten() {
        let what = format!("{model}.{name}: {key}");
        match key.as_str() {
            "size" if kind_name == "char" => {}
            "comodel" | "ondelete" if kind_name == "many2one" => {}
            // read with the kind above
            "digits" if kind_name == "float" => {}
            "selection" if kind_name == "selection" => {}
            "digits" | "selection" => {
                return Err(unsupported(format!(
                    "field {model}.{name}: {key} does not apply to a {kind_name} field"
                )));
            }
            "index" => {
                out.index =
                    Some(parse_index(value).map_err(|why| {
                        unsupported(format!("field {model}.{name}: index: {why}"))
                    })?);
            }
            "default" => {
                out.default = Some(
                    parse_default(value.as_str().unwrap_or_default(), kind_name).map_err(
                        |why| unsupported(format!("field {model}.{name}: default: {why}")),
                    )?,
                );
            }
            "groups" => {
                out.groups = Some(unquote(addon, &what, value.as_str().unwrap_or_default())?);
            }
            "string" => {
                out = out.string(unquote(addon, &what, value.as_str().unwrap_or_default())?);
            }
            "help" => out.help = Some(unquote(addon, &what, value.as_str().unwrap_or_default())?),
            "required" => out.required = Some(flag(addon, &what, value)?),
            "readonly" => out.readonly = Some(flag(addon, &what, value)?),
            "copy" => out.copy = Some(flag(addon, &what, value)?),
            "store" => out.store = Some(flag(addon, &what, value)?),
            "compute" => {
                out = out.compute(unquote(addon, &what, value.as_str().unwrap_or_default())?);
            }
            "compute_sudo" => {
                if flag(addon, &what, value)? {
                    return Err(unsupported(format!(
                        "field {model}.{name}: compute_sudo = true; a compute of a package never \
                         runs elevated"
                    )));
                }
            }
            "translate" => out.translate = Some(flag(addon, &what, value)?),
            "collation" => {
                let raw = value
                    .as_str()
                    .ok_or_else(|| unsupported(format!("field {what}: expected a string")))?;
                out.collation = Some(unquote(addon, &what, raw)?);
            }
            "sanitize" if kind_name == "html" => {
                if let Value::String(preset) = value
                    && preset != "true"
                    && preset != "false"
                {
                    return Err(unsupported(format!(
                        "field {model}.{name}: HTML presets are unavailable until a later release"
                    )));
                }
                out = out.sanitize(flag(addon, &what, value)?);
            }
            "sanitize_overridable" => {
                return Err(unsupported(format!(
                    "field {model}.{name}: sanitize_overridable is forbidden by"
                )));
            }
            other => {
                return Err(unsupported(format!(
                    "field {model}.{name}: attribute '{other}' is not read from a WebAssembly \
                     addon in this build"
                )));
            }
        }
    }
    // like the registry (and a model file): `brin` and `gin` need a stored column
    if let Some(Some(index)) = out.index
        && index.field_types().is_some()
    {
        let stored = out.store.unwrap_or(out.compute.is_none());
        if !stored {
            return Err(unsupported(format!(
                "field {model}.{name}: index = \"{}\" needs a stored field (this field has no \
                 column)",
                index.as_str()
            )));
        }
        if let Some(reason) = erp_registry::index_kind_mismatch(index, kind_name) {
            return Err(unsupported(format!("field {model}.{name}: {reason}")));
        }
    }
    // the ORM's default is `compute_sudo = store`: a package compute is never elevated
    if out.compute.is_some() {
        out = out.compute_sudo(false);
    }
    Ok(out)
}

/// Longest `default` or `digits` source text.
const MAX_SCALAR_SOURCE_BYTES: usize = 1024;
/// Longest `selection` source text: the registry's entry limit times a value and a label of
/// [`erp_registry::MAX_NAME_BYTES`] each, with the quotes, parentheses and separators.
const MAX_SELECTION_SOURCE_BYTES: usize =
    erp_registry::MAX_SELECTION_ENTRIES * (2 * erp_registry::MAX_NAME_BYTES + 16);
/// Longest `keys` source text: the registry's table object limit times a name, the widest field
/// list PostgreSQL allows and a message of 2 KiB (the message itself is bounded again by
/// [`erp_registry::MAX_TEXT_BYTES`] when the definition is checked).
const MAX_KEYS_SOURCE_BYTES: usize = erp_registry::MAX_TABLE_OBJECTS_PER_MODEL
    * (erp_registry::MAX_NAME_BYTES
        + erp_registry::MAX_CONSTRAINT_COLUMNS * (erp_registry::MAX_NAME_BYTES + 8)
        + 2048
        + 64);
/// Longest `company_access` source text.
const MAX_COMPANY_ACCESS_SOURCE_BYTES: usize = 8192;

/// One token of a declaration value. The values a package declares are flat: literals, names and
/// `( ) [ ] , = -`. They are read with this lexer and a fixed-shape matcher that only loops, never
/// by a general expression parser: nothing here nests with the length of the text, so no input
/// can exhaust the stack.
#[derive(Debug, Clone, PartialEq)]
enum Tok {
    /// A string literal, decoded.
    Str(String),
    /// An integer literal, as written (digits and `_`).
    Int(String),
    /// A float literal, as written.
    Float(String),
    /// An identifier or keyword.
    Ident(String),
    /// One of `( ) [ ] , = -`.
    Punct(char),
}

/// The tokens of `source`, refused when it is longer than `max_bytes` or has a character outside
/// the flat grammar. String literals (escapes, raw strings) are decoded one at a time with
/// `syn::LitStr`, which does not recurse.
fn lex(source: &str, max_bytes: usize) -> std::result::Result<Vec<Tok>, String> {
    if source.len() > max_bytes {
        return Err(format!("the declaration exceeds {max_bytes} bytes"));
    }
    let bytes = source.as_bytes();
    let is_ident = |b: u8| b.is_ascii_alphanumeric() || b == b'_';
    let literal = |text: &str| {
        syn::parse_str::<syn::LitStr>(text)
            .map(|lit| Tok::Str(lit.value()))
            .map_err(|_| "a string literal is not valid".to_string())
    };
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b.is_ascii_whitespace() {
            i += 1;
        } else if b == b'"' {
            let start = i;
            i += 1;
            while i < bytes.len() && bytes[i] != b'"' {
                i += if bytes[i] == b'\\' { 2 } else { 1 };
            }
            if i >= bytes.len() {
                return Err("a string literal is not closed".into());
            }
            i += 1;
            out.push(literal(&source[start..i])?);
        } else if b.is_ascii_alphabetic() || b == b'_' {
            let start = i;
            while i < bytes.len() && is_ident(bytes[i]) {
                i += 1;
            }
            let word = &source[start..i];
            // `r"…"` and `r#"…"#`: the closing quote is followed by the same number of `#`
            let raw = word == "r" && matches!(bytes.get(i), Some(b'#' | b'"'));
            if raw {
                let mut hashes = 0;
                while bytes.get(i) == Some(&b'#') {
                    hashes += 1;
                    i += 1;
                }
                if bytes.get(i) != Some(&b'"') {
                    return Err("a raw string literal is not valid".into());
                }
                i += 1;
                loop {
                    let Some(rel) = bytes[i..].iter().position(|c| *c == b'"') else {
                        return Err("a raw string literal is not closed".into());
                    };
                    i += rel + 1;
                    if bytes.len() >= i + hashes && bytes[i..i + hashes].iter().all(|c| *c == b'#')
                    {
                        i += hashes;
                        break;
                    }
                }
                out.push(literal(&source[start..i])?);
            } else {
                out.push(Tok::Ident(word.to_owned()));
            }
        } else if b.is_ascii_digit() {
            let start = i;
            let digits = |i: &mut usize| {
                while *i < bytes.len() && (bytes[*i].is_ascii_digit() || bytes[*i] == b'_') {
                    *i += 1;
                }
            };
            digits(&mut i);
            let mut float = false;
            if bytes.get(i) == Some(&b'.') && bytes.get(i + 1).is_some_and(u8::is_ascii_digit) {
                float = true;
                i += 1;
                digits(&mut i);
            }
            if matches!(bytes.get(i), Some(b'e' | b'E')) {
                let mut j = i + 1;
                if matches!(bytes.get(j), Some(b'+' | b'-')) {
                    j += 1;
                }
                if bytes.get(j).is_some_and(u8::is_ascii_digit) {
                    float = true;
                    i = j;
                    digits(&mut i);
                }
            }
            if bytes.get(i).is_some_and(|c| is_ident(*c) || *c == b'.') {
                return Err("a number with a suffix or a trailing dot is not read".into());
            }
            let text = source[start..i].to_owned();
            out.push(if float {
                Tok::Float(text)
            } else {
                Tok::Int(text)
            });
        } else if matches!(b, b'(' | b')' | b'[' | b']' | b',' | b'=' | b'-') {
            out.push(Tok::Punct(char::from(b)));
            i += 1;
        } else {
            return Err(format!(
                "unexpected character {:?} outside a string literal",
                source[i..].chars().next().unwrap_or('?')
            ));
        }
    }
    Ok(out)
}

/// A cursor over the tokens of one declaration value.
struct Tokens {
    toks: Vec<Tok>,
    pos: usize,
}

impl Tokens {
    fn new(source: &str, max_bytes: usize) -> std::result::Result<Self, String> {
        Ok(Self {
            toks: lex(source, max_bytes)?,
            pos: 0,
        })
    }

    fn peek(&self) -> Option<&Tok> {
        self.toks.get(self.pos)
    }

    fn next(&mut self) -> Option<Tok> {
        let tok = self.toks.get(self.pos).cloned();
        self.pos += usize::from(tok.is_some());
        tok
    }

    fn at(&self, c: char) -> bool {
        self.peek() == Some(&Tok::Punct(c))
    }

    /// Consume `c` or fail with `shape`.
    fn punct(&mut self, c: char, shape: &str) -> std::result::Result<(), String> {
        if self.at(c) {
            self.pos += 1;
            Ok(())
        } else {
            Err(shape.to_owned())
        }
    }

    fn string(&mut self, shape: &str) -> std::result::Result<String, String> {
        match self.next() {
            Some(Tok::Str(text)) => Ok(text),
            _ => Err(shape.to_owned()),
        }
    }

    fn ident(&mut self, shape: &str) -> std::result::Result<String, String> {
        match self.next() {
            Some(Tok::Ident(name)) => Ok(name),
            _ => Err(shape.to_owned()),
        }
    }

    fn end(&self, shape: &str) -> std::result::Result<(), String> {
        if self.pos == self.toks.len() {
            Ok(())
        } else {
            Err(shape.to_owned())
        }
    }
}

/// `[(\"a\", \"A\"), ...]` as the macro's token text carries it (`[(\"a\" , \"A\")]`), with an
/// optional trailing comma.
fn parse_selection(source: &str) -> std::result::Result<Vec<(String, String)>, String> {
    const SHAPE: &str = "expected a list of (\"value\", \"Label\") string pairs";
    let mut t = Tokens::new(source, MAX_SELECTION_SOURCE_BYTES)?;
    t.punct('[', SHAPE)?;
    let mut out = Vec::new();
    while !t.at(']') {
        t.punct('(', SHAPE)?;
        let value = t.string(SHAPE)?;
        t.punct(',', SHAPE)?;
        let label = t.string(SHAPE)?;
        t.punct(')', SHAPE)?;
        out.push((value, label));
        if out.len() > erp_registry::MAX_SELECTION_ENTRIES {
            return Err(format!(
                "more than the limit of {} entries",
                erp_registry::MAX_SELECTION_ENTRIES
            ));
        }
        if t.at(',') {
            t.pos += 1;
        } else if !t.at(']') {
            return Err(SHAPE.into());
        }
    }
    t.punct(']', SHAPE)?;
    t.end(SHAPE)?;
    Ok(out)
}

/// `(16 , 2)`: `digits = (precision, scale)`. A name (`digits = \"price\"`) is refused: the owned
/// definition has no interning for it.
fn parse_digits(source: &str) -> std::result::Result<Digits, String> {
    const SHAPE: &str = "expected (precision, scale)";
    let mut t = Tokens::new(source, MAX_SCALAR_SOURCE_BYTES)?;
    if matches!(t.peek(), Some(Tok::Str(_))) {
        return Err(
            "a named precision is not read from a WebAssembly addon; use (precision, \
                    scale)"
                .into(),
        );
    }
    let int = |t: &mut Tokens| match t.next() {
        Some(Tok::Int(text)) => text
            .replace('_', "")
            .parse::<u32>()
            .map_err(|_| "precision and scale are unsigned integer literals".to_string()),
        _ => Err("precision and scale are unsigned integer literals".to_string()),
    };
    t.punct('(', SHAPE)?;
    let precision = int(&mut t)?;
    t.punct(',', SHAPE)?;
    let scale = int(&mut t)?;
    t.punct(')', SHAPE)?;
    t.end(SHAPE)?;
    if precision == 0 {
        return Err("the precision must be at least 1".into());
    }
    if scale > precision {
        return Err("the scale must not be larger than the precision".into());
    }
    Ok(Digits::Fixed { precision, scale })
}

/// `index` (a bare flag is `true`), `index = true`, `index = false` or `index = \"trigram\"`.
fn parse_index(value: &Value) -> std::result::Result<Option<IndexKind>, String> {
    let text = match value {
        Value::Bool(true) => return Ok(Some(IndexKind::Btree)),
        Value::Bool(false) => return Ok(None),
        Value::String(text) => text.as_str(),
        _ => return Err("expected true, false or an index kind".into()),
    };
    match text {
        "true" => return Ok(Some(IndexKind::Btree)),
        "false" => return Ok(None),
        _ => {}
    }
    let name = serde_json::from_str::<String>(text)
        .map_err(|_| "expected true, false or an index kind in quotes".to_string())?;
    IndexKind::from_name(&name).map(Some).ok_or_else(|| {
        format!(
            "unknown index kind \"{}\"",
            name.chars().take(64).collect::<String>()
        )
    })
}

/// A static default: one string, integer (negative too), float or boolean literal, or `False`.
fn parse_default(source: &str, kind_name: &str) -> std::result::Result<DefaultValue, String> {
    const SHAPE: &str = "expected a string, number or boolean literal (a function is not read)";
    let mut t = Tokens::new(source, MAX_SCALAR_SOURCE_BYTES)?;
    let negative = t.at('-');
    if negative {
        t.pos += 1;
    }
    let value = match (negative, t.next()) {
        (false, Some(Tok::Str(text))) => DefaultValue::Str(text),
        (false, Some(Tok::Ident(word))) => match word.as_str() {
            "False" => DefaultValue::False,
            "true" => DefaultValue::Bool(true),
            "false" if kind_name != "boolean" => DefaultValue::False,
            "false" => DefaultValue::Bool(false),
            _ => return Err(SHAPE.into()),
        },
        (_, Some(Tok::Int(text))) => {
            let magnitude = text
                .replace('_', "")
                .parse::<i128>()
                .map_err(|_| "the integer does not fit in 64 bits".to_string())?;
            i64::try_from(if negative { -magnitude } else { magnitude })
                .map(DefaultValue::Int)
                .map_err(|_| "the integer does not fit in 64 bits".to_string())?
        }
        (_, Some(Tok::Float(text))) => {
            let value = text
                .replace('_', "")
                .parse::<f64>()
                .map_err(|_| "invalid float literal".to_string())?;
            if !value.is_finite() {
                return Err("the float is not finite".to_string());
            }
            DefaultValue::Float(if negative { -value } else { value })
        }
        _ => return Err(SHAPE.into()),
    };
    t.end(SHAPE)?;
    Ok(value)
}

/// `[key(name = \"_x\", fields = [\"a\"], message = \"...\", nulls_distinct = true), ...]`.
/// A key with `without_overlaps` is a temporal key, which a package cannot declare.
fn parse_keys(source: &str) -> std::result::Result<Vec<KeyDef>, String> {
    const SHAPE: &str =
        "expected an array of key(name = \"_x\", fields = [...], message = \"...\")";
    const ARG: &str = "expected name = value inside key(…)";
    let mut t = Tokens::new(source, MAX_KEYS_SOURCE_BYTES)?;
    t.punct('[', SHAPE)?;
    let mut out = Vec::new();
    while !t.at(']') {
        if t.ident(SHAPE)? != "key" {
            return Err(SHAPE.into());
        }
        t.punct('(', SHAPE)?;
        let (mut name, mut message, mut fields, mut nulls_distinct) = (None, None, None, None);
        while !t.at(')') {
            let param = t.ident(ARG)?;
            t.punct('=', ARG)?;
            let given_twice = || format!("parameter '{param}' is given twice");
            match param.as_str() {
                "without_overlaps" => {
                    return Err("without_overlaps (a temporal key) is not read from a \
                                WebAssembly addon"
                        .into());
                }
                "name" => {
                    let value = t.string("key(…) needs name = \"...\"")?;
                    if name.replace(value).is_some() {
                        return Err(given_twice());
                    }
                }
                "message" => {
                    let value = t.string("key(…) needs message = \"...\"")?;
                    if message.replace(value).is_some() {
                        return Err(given_twice());
                    }
                }
                "fields" => {
                    const FIELDS: &str = "key fields are string literals";
                    t.punct('[', FIELDS)?;
                    let mut list = Vec::new();
                    while !t.at(']') {
                        list.push(t.string(FIELDS)?);
                        if t.at(',') {
                            t.pos += 1;
                        } else if !t.at(']') {
                            return Err(FIELDS.into());
                        }
                    }
                    t.punct(']', FIELDS)?;
                    if fields.replace(list).is_some() {
                        return Err(given_twice());
                    }
                }
                "nulls_distinct" => {
                    let value = match t.ident("nulls_distinct is true or false")?.as_str() {
                        "true" => true,
                        "false" => false,
                        _ => return Err("nulls_distinct is true or false".into()),
                    };
                    if nulls_distinct.replace(value).is_some() {
                        return Err(given_twice());
                    }
                }
                other => return Err(format!("unknown parameter '{other}' in key(…)")),
            }
            if t.at(',') {
                t.pos += 1;
            } else if !t.at(')') {
                return Err(ARG.into());
            }
        }
        t.punct(')', SHAPE)?;
        let name = name.ok_or("key(…) needs name = \"...\"")?;
        let message = message.ok_or("key(…) needs message = \"...\"")?;
        let fields = fields.ok_or("key(…) needs fields = [\"...\", ...]")?;
        // bounded before the duplicate check, which compares every pair (r3 review SHOULD 1)
        if fields.len() > erp_registry::MAX_CONSTRAINT_COLUMNS {
            return Err(format!(
                "key '{name}' names {} fields, more than the limit of {}",
                fields.len(),
                erp_registry::MAX_CONSTRAINT_COLUMNS
            ));
        }
        if let Some(dup) = fields
            .iter()
            .enumerate()
            .find_map(|(i, f)| fields[..i].contains(f).then_some(f))
        {
            return Err(format!("key '{name}' names field '{dup}' twice"));
        }
        out.push(
            KeyDef::new(name, message)
                .fields(fields)
                .nulls_distinct(nulls_distinct.unwrap_or(false)),
        );
        if t.at(',') {
            t.pos += 1;
        } else if !t.at(']') {
            return Err(SHAPE.into());
        }
    }
    t.punct(']', SHAPE)?;
    t.end(SHAPE)?;
    Ok(out)
}

/// A package's `extend` entry may only add fields: redeclaring a field another addon introduced
/// could weaken it (`sanitize = false` on an HTML field, another `comodel`, `readonly`, `required`,
/// `store`, `translate`, `collation`, an index), and the registry merges such a redeclaration
/// attribute by attribute. The field's owner is its first native declarer, else its first
/// declarer: the registry orders addons by depth, so a shallow package can load before a deeper
/// native addon and be listed first in `FieldSpec::modules` for that addon's field (cycle 18
/// security review). `packages` names the package addons of the registry. This runs for every
/// registry the module system composes with a package in it (install, upgrade, load at boot, a
/// node following another), through `check_model_refs`.
fn check_extension_fields(
    addon: &ModuleName,
    defs: &[ModelDef],
    registry: &Registry,
    packages: &[&str],
) -> Result<()> {
    for def in defs.iter().filter(|d| d.is_extension()) {
        let Some(model) = registry.model(&def.name) else {
            continue;
        };
        for field in &def.fields {
            let owner = model.field(&field.name).and_then(|spec| {
                let mut declarers = spec.modules.iter().copied();
                declarers
                    .clone()
                    .find(|m| !packages.contains(m))
                    .or_else(|| declarers.next())
            });
            if let Some(other) = owner.filter(|owner| *owner != addon.as_str()) {
                return Err(refuse(
                    addon,
                    WasmReason::Unsupported(format!(
                        "field {}.{}: a package may not redeclare a field declared by addon \
                         '{other}'; an extend entry only adds new fields",
                        def.name, field.name
                    )),
                ));
            }
        }
    }
    Ok(())
}

/// An upgrade may not turn `parent_store` on for a model the installed version already had
/// without it: the rows that exist would keep an empty `parent_path` (nothing fills the paths of
/// existing rows), so `child_of` would silently miss them. A model new in the upgrade has no rows.
fn refuse_parent_store_turned_on(
    addon: &ModuleName,
    previous: &[ModelDef],
    next: &[ModelDef],
) -> Result<()> {
    // the parent field of a model: `parent_name` (default `parent_id`) and where it points
    let parent_of = |def: &ModelDef| {
        let name = def.parent_name.as_deref().unwrap_or("parent_id");
        let comodel = def
            .fields
            .iter()
            .find(|f| f.name == name)
            .and_then(|f| match &f.kind {
                KindDef::Many2one { comodel, .. } => Some(comodel.clone()),
                _ => None,
            });
        (name.to_owned(), comodel)
    };
    for def in next.iter().filter(|d| d.parent_store == Some(true)) {
        // the stored paths follow the parent field: another one would leave them describing a tree
        // that no longer exists
        if let Some(old) = previous
            .iter()
            .find(|p| p.name == def.name && p.parent_store == Some(true))
            && parent_of(old) != parent_of(def)
        {
            return Err(refuse(
                addon,
                WasmReason::State(format!(
                    "the upgrade changes the parent field of model '{}' (parent_name or the model \
                     it points to) while parent_store stays on: the stored paths would no longer \
                     match the tree",
                    def.name
                )),
            ));
        }
        let existed_without = previous
            .iter()
            .any(|p| p.name == def.name && p.parent_store != Some(true));
        if existed_without {
            return Err(refuse(
                addon,
                WasmReason::State(format!(
                    "the upgrade turns parent_store on for model '{}', which the installed \
                     version has without it: the paths of its existing rows would stay empty. \
                     Declare parent_store from the first version of the model",
                    def.name
                )),
            ));
        }
    }
    Ok(())
}

// ---- loaded packages and the runtime state ---------------------------------------------------

/// A package loaded for use: its component, the models read from it, and what identifies it.
struct Loaded {
    manifest: Manifest,
    sha256: String,
    location: String,
    component: Arc<dyn WasmComponent>,
    defs: Vec<ModelDef>,
    checks: DescribedChecks,
    refs: Vec<DescribedRef>,
    capabilities: Vec<String>,
    client_dist_digest: Option<String>,
    /// The `data` files of the manifest and the models the package declares and extends.
    data: Arc<PackageData>,
}

impl fmt::Debug for Loaded {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Loaded")
            .field("module", self.manifest.name())
            .field("version", &self.manifest.version().raw())
            .field("sha256", &self.sha256)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Op {
    Install,
    Upgrade,
    Uninstall,
}

impl Op {
    fn as_str(self) -> &'static str {
        match self {
            Self::Install => "install",
            Self::Upgrade => "upgrade",
            Self::Uninstall => "uninstall",
        }
    }
}

/// The one lifecycle operation in flight. It is visible only to the task that runs the operation
/// ([`ACTIVE`]): the registry composition and `before_commit` of that task see it, and no other
/// task of the process does, so an unrelated plain operation running at the same time is never
/// mistaken for it (security review r1 M4).
struct Staged {
    op: Op,
    name: ModuleName,
    /// The new package (install, upgrade).
    loaded: Option<Arc<Loaded>>,
    consented: Option<ConsentedPackage>,
    /// Immutable pre-upgrade package admitted by this process, rechecked under lifecycle ownership.
    previous: Option<Arc<Loaded>>,
    /// The session id the trusted authorizer returned, for a session operation (audit row).
    session: Mutex<Option<String>>,
}

tokio::task_local! {
    /// The staged operation of the current task, set around the module operation only.
    static ACTIVE: Arc<Staged>;
}

fn active() -> Option<Arc<Staged>> {
    ACTIVE.try_with(Arc::clone).ok()
}

/// A transition available only inside the task-local, consented package upgrade.
#[derive(Debug, Clone)]
pub(crate) struct PackageIdentityUpgrade {
    pub(crate) name: ModuleName,
    pub(crate) previous: String,
    pub(crate) current: String,
}

pub(crate) fn identity_upgrade() -> Result<Option<PackageIdentityUpgrade>> {
    let Some(staged) = active() else {
        return Ok(None);
    };
    if staged.op != Op::Upgrade {
        return Ok(None);
    }
    let (Some(previous), Some(current)) = (&staged.previous, &staged.loaded) else {
        return Err(refuse(
            &staged.name,
            WasmReason::State("a staged upgrade preimage is missing".to_owned()),
        ));
    };
    Ok(Some(PackageIdentityUpgrade {
        name: staged.name.clone(),
        previous: format!("package-v1:{}", previous.sha256),
        current: format!("package-v1:{}", current.sha256),
    }))
}

#[derive(Default)]
struct State {
    live: BTreeMap<ModuleName, Arc<Loaded>>,
}

/// The [`PackageRuntime`] for WebAssembly addons: the packages loaded in this process for one
/// database. The operation being run is staged per task, not in this shared state.
pub struct WasmPackages {
    loader: Arc<dyn ComponentLoader>,
    state: Mutex<State>,
}

impl fmt::Debug for WasmPackages {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WasmPackages").finish_non_exhaustive()
    }
}

impl WasmPackages {
    /// A runtime that loads components with `loader`.
    pub fn new(loader: Arc<dyn ComponentLoader>) -> Arc<Self> {
        Arc::new(Self {
            loader,
            state: Mutex::new(State::default()),
        })
    }

    fn state(&self) -> MutexGuard<'_, State> {
        // A panic while the state was held leaves it whole (entries are replaced, not edited).
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The methods of the live (or, inside its operation, staged) package `module` that an RPC
    /// client may call, as `model.method`.
    ///
    /// # Errors
    /// The package is not loaded here.
    pub fn exposed_methods(&self, module: &ModuleName) -> Result<Vec<String>> {
        Ok(self.loaded(module)?.component.exposed_methods())
    }

    /// The models of other addons that package `module` extends, sorted.
    ///
    /// # Errors
    /// The package is not loaded here.
    pub fn extended_models(&self, module: &ModuleName) -> Result<Vec<String>> {
        Ok(self.loaded(module)?.data.extended.iter().cloned().collect())
    }

    fn loaded(&self, module: &ModuleName) -> Result<Arc<Loaded>> {
        self.current(module).ok_or_else(|| {
            refuse(
                module,
                WasmReason::State("the package is not loaded in this process".to_owned()),
            )
        })
    }

    /// The names of the packages loaded and live, sorted.
    pub fn live_modules(&self) -> Vec<ModuleName> {
        self.state().live.keys().cloned().collect()
    }

    /// The loaded package that the registry of the current task should see for `module`: the
    /// staged one when this task runs the lifecycle operation on it, else the live one.
    fn current(&self, module: &ModuleName) -> Option<Arc<Loaded>> {
        if let Some(staged) = active()
            && &staged.name == module
            && let Some(loaded) = &staged.loaded
        {
            return Some(Arc::clone(loaded));
        }
        self.state().live.get(module).cloned()
    }

    async fn build(&self, package: &ErpAddonPackage, location: String) -> Result<Loaded> {
        let name = package.name();
        let manifest = package.manifest();
        let Some(wasm) = manifest.wasm() else {
            return Err(refuse(
                name,
                WasmReason::NotWasm("the manifest has no [wasm] section".to_owned()),
            ));
        };
        // The SQL capability permits the module system to run archived migrations. It gives
        // the guest no SQL host function. Network and temporary files remain unavailable.
        if let Some(cap) = wasm
            .capabilities
            .iter()
            .find(|cap| **cap != WasmCapability::MigrationsSql)
        {
            return Err(refuse(
                name,
                WasmReason::CapabilityUnavailable(cap.canonical_str()),
            ));
        }
        let Some(bytes) = package.wasm_binary() else {
            return Err(refuse(
                name,
                WasmReason::NotWasm(format!("the package has no wasm/{name}.wasm")),
            ));
        };
        let files = package_data_files(package)?;
        let component = self.loader.load(name, bytes).await?;
        let defs = models_from_describe(manifest, component.describe())?;
        let checks = checks_from_describe(manifest, component.describe())?;
        let refs = refs_from_describe(manifest, component.describe())?;
        check_compute_wiring(name, &defs, &checks)?;
        let mut data = PackageData {
            files,
            migration_files: package_migration_files(
                package,
                wasm.capabilities.contains(&WasmCapability::MigrationsSql),
            )?,
            client_files: package_client_files(package)?,
            ..PackageData::default()
        };
        for def in &defs {
            // an in-place extension names itself in `inherit`
            if def.inherit.contains(&def.name) {
                data.extended.insert(def.name.clone());
            } else {
                data.declared.insert(def.name.clone());
            }
        }
        let capabilities: Vec<String> = wasm
            .capabilities
            .iter()
            .map(WasmCapability::canonical_str)
            .collect();
        Ok(Loaded {
            manifest: manifest.clone(),
            sha256: package.archive_sha256().to_owned(),
            location,
            component,
            defs,
            checks,
            refs,
            capabilities,
            client_dist_digest: package.client_dist_digest().map(str::to_owned),
            data: Arc::new(data),
        })
    }
}

fn package_migration_files(package: &ErpAddonPackage, approved: bool) -> Result<PackageFiles> {
    let mut files = PackageFiles::new();
    for path in package
        .file_paths()
        .filter(|path| crate::migrations::parse_migration_path(path).is_some())
    {
        if !approved {
            return Err(refuse(
                package.name(),
                WasmReason::Unsupported(format!(
                    "SQL migration '{path}' requires the administrator-approved migrations-sql capability"
                )),
            ));
        }
        let bytes = package.get_file(path).ok_or_else(|| {
            refuse(
                package.name(),
                WasmReason::DataFile(format!(
                    "verified migration file '{path}' disappeared from the archive"
                )),
            )
        })?;
        files.insert(path.to_owned(), bytes.to_vec());
    }
    Ok(files)
}

/// Retain client sources only from the already integrity-checked archive. The host later
/// validates its strict client manifest and declaration set; these bytes never become data
/// loader files. Bounds are safety admission, not product performance thresholds.
fn package_client_files(package: &ErpAddonPackage) -> Result<PackageFiles> {
    const FILE_BYTES: usize = 16 * 1024 * 1024;
    const TOTAL_BYTES: usize = 128 * 1024 * 1024;
    const FILES: usize = 20_000;
    let has_manifest = package.get_file("static/dist/manifest.json").is_some();
    let mut files = PackageFiles::new();
    let mut total = 0usize;
    for path in package.file_paths() {
        if !path.starts_with("static/") && !(has_manifest && path.starts_with("i18n/")) {
            continue;
        }
        let bytes = package.get_file(path).ok_or_else(|| {
            refuse(
                package.name(),
                WasmReason::DataFile(format!(
                    "verified client file '{path}' disappeared from the archive"
                )),
            )
        })?;
        total = total.checked_add(bytes.len()).ok_or_else(|| {
            refuse(
                package.name(),
                WasmReason::Unsupported("client asset byte count overflow".to_owned()),
            )
        })?;
        if bytes.len() > FILE_BYTES || total > TOTAL_BYTES || files.len() >= FILES {
            return Err(refuse(
                package.name(),
                WasmReason::Unsupported(format!(
                    "client asset '{path}' exceeds hard file/count/byte admission limits"
                )),
            ));
        }
        files.insert(path.to_owned(), bytes.to_vec());
    }
    Ok(files)
}

/// The `data` files of a package: views, window actions, menus and access rows, as the manifest
/// lists them, taken from the verified archive. What a package may declare in them is checked when
/// they load (`ModuleSystem`: `Error::PackageDataRefused`); here the kinds this build does not
/// load at all are refused by name, before anything is staged.
fn package_data_files(package: &ErpAddonPackage) -> Result<PackageFiles> {
    let name = package.name();
    let manifest = package.manifest();
    if let Some(first) = manifest.demo().first() {
        return Err(refuse(
            name,
            WasmReason::Unsupported(format!(
                "demo data of a package ('{first}'): list the file under `data` or leave it out"
            )),
        ));
    }
    let mut files = PackageFiles::new();
    for path in manifest.data() {
        let extension = std::path::Path::new(path)
            .extension()
            .and_then(|e| e.to_str())
            .map(str::to_ascii_lowercase);
        let relative = std::path::Path::new(path);
        let inside = relative
            .components()
            .all(|c| matches!(c, std::path::Component::Normal(_)))
            && relative.components().count() >= 2
            && relative
                .components()
                .next()
                .and_then(|c| c.as_os_str().to_str())
                .is_some_and(|first| matches!(first, "views" | "data" | "security"));
        if !inside {
            return Err(refuse(
                name,
                WasmReason::Unsupported(format!(
                    "data file '{path}': a package's data files live in views/, data/ or security/"
                )),
            ));
        }
        if !matches!(extension.as_deref(), Some("xml" | "csv")) {
            return Err(refuse(
                name,
                WasmReason::Unsupported(format!(
                    "data file '{path}': a package carries .xml and .csv data files only"
                )),
            ));
        }
        let Some(content) = package.get_file(path) else {
            return Err(refuse(
                name,
                WasmReason::DataFile(format!(
                    "the manifest lists '{path}' but the package archive has no such file"
                )),
            ));
        };
        files.insert(path.clone(), content.to_vec());
    }
    Ok(files)
}

impl PackageRuntime for WasmPackages {
    fn supports_native_uninstall(&self) -> bool {
        true
    }

    fn model_defs(&self, module: &ModuleName) -> Result<Vec<ModelDef>> {
        match self.current(module) {
            Some(loaded) => Ok(loaded.defs.clone()),
            None => Err(refuse(
                module,
                WasmReason::State(
                    "a package addon of this database whose package is not loaded in this \
                     process; call WasmLifecycle::load"
                        .to_owned(),
                ),
            )),
        }
    }

    fn wasm_methods(&self, plain: &Orm, modules: &[ModuleName]) -> Result<Vec<WasmMethodImpl>> {
        let mut out = Vec::new();
        for module in modules {
            if let Some(loaded) = self.current(module) {
                out.extend(loaded.component.method_impls(plain)?);
            }
        }
        Ok(out)
    }

    fn check_model_refs(&self, registry: &Registry, modules: &[ModuleName]) -> Result<()> {
        let packages: Vec<&str> = modules
            .iter()
            .filter(|m| self.current(m).is_some())
            .map(ModuleName::as_str)
            .collect();
        for module in modules {
            if let Some(loaded) = self.current(module) {
                check_refs_in_registry(module, &loaded.refs, registry)?;
                check_extension_fields(module, &loaded.defs, registry, &packages)?;
            }
        }
        Ok(())
    }

    fn checks(&self, modules: &[ModuleName]) -> Result<PackageChecks> {
        let mut out = PackageChecks::default();
        for module in modules {
            if let Some(loaded) = self.current(module) {
                for spec in &loaded.checks.computes {
                    out.computes.push(wasm_compute_impl(spec)?);
                }
                for spec in &loaded.checks.constraints {
                    out.constraints.push(wasm_constraint_impl(spec)?);
                }
            }
        }
        Ok(out)
    }

    fn before_commit<'a>(
        &'a self,
        tx: &'a mut Transaction,
        change: PackageChange<'a>,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let Some(staged) = active() else {
                // An operation that is not this task's package lifecycle must leave package
                // addons alone, whatever else is staged elsewhere in the process.
                let live: BTreeSet<ModuleName> = self.state().live.keys().cloned().collect();
                let touched: Vec<String> = change
                    .installed
                    .iter()
                    .chain(change.upgraded)
                    .chain(change.uninstalled)
                    .filter(|m| live.contains(*m))
                    .map(ToString::to_string)
                    .collect();
                return match touched.first() {
                    None if !live.is_empty() && !change.upgraded.is_empty() => {
                        validate_installed_package_views(tx, &live).await
                    }
                    None => Ok(()),
                    Some(first) => Err(refuse(
                        &ModuleName::new(first)?,
                        WasmReason::State(format!(
                            "package addons [{}] change only through WasmLifecycle",
                            touched.join(", ")
                        )),
                    )),
                };
            };
            let unexpected = unexpected_changes(staged.op, &staged.name, change);
            if !unexpected.is_empty() {
                return Err(refuse(
                    &staged.name,
                    WasmReason::UnexpectedChange(unexpected),
                ));
            }
            self.write(tx, &staged).await
        })
    }
}

/// Check package-owned views after all native data and hooks have run, against the final
/// transaction registry. Packages are deliberately excluded from native upgrade cascades: their
/// data is not reloaded, but it must still compose. An error rolls back the native upgrade and
/// its registry signal; package versions, archives and audit rows are never rewritten here.
async fn validate_installed_package_views(
    tx: &mut Transaction,
    live: &BTreeSet<ModuleName>,
) -> Result<()> {
    if tx.registry().model("ir.ui.view").is_none() {
        // Headless registries have no views to preserve.
        return Ok(());
    }
    // ModuleSystem always registers ir.model.data; a missing model is a broken host contract.
    if tx.registry().model("ir.model.data").is_none() {
        return Err(Error::InvariantViolation {
            message: "the registry's ir.ui.view has no ir.model.data: installed package views \
                      cannot be identified after a native upgrade"
                .to_owned(),
        });
    }
    tx.flush().await?;
    let modules: Vec<&str> = live.iter().map(ModuleName::as_str).collect();
    // EXISTS deduplicates external-id aliases. Keyset windows bound the ids held here; the
    // existing validator enforces its own tree/depth/byte limits. No install_module context is
    // supplied: all native data is final and every installed package extension participates.
    let views = tx.env().records("ir.ui.view")?;
    // The existing validator accepts an empty set. This also refuses a missing validator even
    // when the installed packages currently own no views, without exposing ORM dispatch internals.
    views.call::<(), ()>(tx, "_check_xml", ()).await?;
    let mut after = 0_i64;
    loop {
        let rows = tx
            .query(
                "SELECT v.id FROM ir_ui_view v WHERE v.id > $2 AND EXISTS ( \
                SELECT 1 FROM ir_model_data md WHERE md.model = 'ir.ui.view' \
                  AND md.res_id = v.id AND md.module = ANY($1)) \
             ORDER BY v.id LIMIT 256",
                &[&modules, &after],
            )
            .await?;
        let ids: Vec<i64> = rows
            .iter()
            .map(|row| row.try_get(0))
            .collect::<std::result::Result<_, _>>()
            .map_err(|error| Error::InvariantViolation {
                message: format!("unexpected installed package view id: {error}"),
            })?;
        let Some(last) = ids.last() else {
            return Ok(());
        };
        after = *last;
        views
            .browse(ids)?
            .call::<(), ()>(tx, "_check_xml", ())
            .await?;
    }
}

/// What an operation changed beyond the one thing the lifecycle asked for (empty: nothing). A
/// cascade (dependents uninstalled with it), an automatic dependency install and a missing change
/// all count, and each refuses the operation.
fn unexpected_changes(op: Op, name: &ModuleName, change: PackageChange<'_>) -> Vec<String> {
    let others = |list: &[ModuleName], allow: Option<&ModuleName>| -> Vec<String> {
        list.iter()
            .filter(|m| Some(*m) != allow)
            .map(ToString::to_string)
            .collect()
    };
    let (mut unexpected, listed) = match op {
        Op::Install => {
            let mut v = others(change.installed, Some(name));
            v.extend(others(change.upgraded, None));
            v.extend(others(change.uninstalled, None));
            (v, change.installed)
        }
        // an upgrade also upgrades the installed dependents, as `button_upgrade` does
        Op::Upgrade => {
            let mut v = others(change.installed, None);
            v.extend(others(change.uninstalled, None));
            (v, change.upgraded)
        }
        Op::Uninstall => {
            let mut v = others(change.installed, None);
            v.extend(others(change.uninstalled, Some(name)));
            (v, change.uninstalled)
        }
    };
    if !listed.contains(name) {
        unexpected.push(format!("(the operation did not {} {name})", op.as_str()));
    }
    unexpected
}

/// Who the audit row names: the operator (the System environment of the CLI, uid 1) or the
/// authenticated administrator the trusted authorizer bound to the transaction.
struct AuditActor {
    uid: i64,
    kind: &'static str,
    session: Option<String>,
}

fn audit_actor(tx: &Transaction, staged: &Staged) -> Result<AuditActor> {
    let env = tx.env();
    let session = staged
        .session
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clone();
    let inconsistent = |text: &str| Error::InvariantViolation {
        message: format!("the lifecycle actor is inconsistent: {text}"),
    };
    match (env.principal().actor(), session) {
        (Some(Actor::Impersonator(original)), Some(session)) => Ok(AuditActor {
            uid: original.subject().get(),
            kind: "session",
            session: Some(session),
        }),
        (None, None) => Ok(AuditActor {
            uid: env.uid(),
            kind: "operator",
            session: None,
        }),
        (Some(_), None) => Err(inconsistent("a bound actor but no session")),
        (None, Some(_)) => Err(inconsistent("a session but no bound actor")),
        (Some(_), Some(_)) => Err(inconsistent(
            "an actor that is not the authorized session user",
        )),
    }
}

impl WasmPackages {
    /// The rows of one operation: the package (install, upgrade) or its removal (uninstall), and
    /// the audit record. The tables are core tables the schema owner created (`core_ddl!` above);
    /// this never creates a table, so the runtime role never owns one.
    async fn write(&self, tx: &mut Transaction, staged: &Staged) -> Result<()> {
        let (op, name) = (staged.op, &staged.name);
        let actor = audit_actor(tx, staged)?;
        let previous = self.state().live.get(name).cloned();
        let (version, previous_version, sha256, capabilities, digest) = match op {
            Op::Install | Op::Upgrade => {
                let (Some(loaded), Some(consented)) =
                    (staged.loaded.as_deref(), staged.consented.as_ref())
                else {
                    return Err(refuse(
                        name,
                        WasmReason::State("a staged package is missing".to_owned()),
                    ));
                };
                store_package(tx, consented).await?;
                (
                    loaded.manifest.version().to_string(),
                    previous.map(|p| p.manifest.version().to_string()),
                    Some(loaded.sha256.clone()),
                    loaded.capabilities.clone(),
                    loaded.client_dist_digest.clone(),
                )
            }
            Op::Uninstall => {
                let Some(previous) = previous else {
                    return Err(refuse(
                        name,
                        WasmReason::State("not a package addon loaded in this process".to_owned()),
                    ));
                };
                let removed = tx
                    .execute(
                        "DELETE FROM ir_module_package WHERE name = $1",
                        &[&name.as_str()],
                    )
                    .await?;
                if removed == 0 {
                    return Err(refuse(
                        name,
                        WasmReason::StoredPackage(
                            "no stored package to remove for an installed addon".to_owned(),
                        ),
                    ));
                }
                (
                    previous.manifest.version().to_string(),
                    None,
                    Some(previous.sha256.clone()),
                    previous.capabilities.clone(),
                    previous.client_dist_digest.clone(),
                )
            }
        };
        let capabilities =
            serde_json::to_value(capabilities).map_err(|e| Error::InvariantViolation {
                message: format!("capabilities do not serialize: {e}"),
            })?;
        tx.execute(
            "INSERT INTO ir_module_wasm_audit \
             (operation, module, version, previous_version, sha256, capabilities, \
              client_dist_digest, actor_uid, actor_kind, session_id) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10::text::uuid)",
            &[
                &op.as_str(),
                &name.as_str(),
                &version,
                &previous_version,
                &sha256,
                &capabilities,
                &digest,
                &actor.uid,
                &actor.kind,
                &actor.session,
            ],
        )
        .await?;
        Ok(())
    }
}

// ---- the lifecycle ---------------------------------------------------------------------------

/// Hex SHA-256 text as the 32 bytes the addon index records.
fn digest_of(hex: &str) -> Result<erp_orm::node::Digest> {
    let bad = || Error::InvariantViolation {
        message: format!("'{hex}' is not a SHA-256 digest"),
    };
    if hex.len() != 64 || !hex.is_ascii() {
        return Err(bad());
    }
    let mut bytes = [0u8; 32];
    for (i, byte) in bytes.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&hex[2 * i..2 * i + 2], 16).map_err(|_| bad())?;
    }
    Ok(erp_orm::node::Digest::from_bytes(bytes))
}

fn register(index: &mut AddonIndex, loaded: &Loaded) -> Result<()> {
    index.add_package_with_data(
        loaded.manifest.clone(),
        loaded.location.clone(),
        digest_of(&loaded.sha256)?,
        Arc::clone(&loaded.data),
    )
}

/// Names a package may never take: the native core addons. `base` and `web` are skipped when the
/// registry is composed (they are linked, never defined by a package), so a package of that name
/// would install and contribute nothing; it is refused instead. Other native addons are refused by
/// the addon index (`DuplicateAddon`) and, when installed, by the catalog state check.
fn reserved_name(name: &ModuleName) -> Result<()> {
    if matches!(name.as_str(), "base" | "web") {
        return Err(refuse(
            name,
            WasmReason::State(format!(
                "'{name}' is a native core addon; a package cannot take its name"
            )),
        ));
    }
    Ok(())
}

/// Passes the call to the trusted authorizer and remembers the session id it returned for the
/// audit row of this operation.
struct RecordingAuthorizer<'a> {
    inner: &'a dyn InstallAuthorizer,
    staged: Arc<Staged>,
}

impl InstallAuthorizer for RecordingAuthorizer<'_> {
    fn authorize<'a>(
        &'a self,
        tx: &'a mut Transaction,
    ) -> BoxFuture<'a, erp_fields::Result<InstallAuthorization>> {
        Box::pin(async move {
            let checked = self.inner.authorize(tx).await?;
            *self
                .staged
                .session
                .lock()
                .unwrap_or_else(PoisonError::into_inner) = Some(checked.session_id.clone());
            Ok(checked)
        })
    }
}

/// Runtime install, upgrade and uninstall of WebAssembly addons into one database, and the
/// reload of what is stored.
///
/// Build the [`WasmPackages`] first, give it to the module system
/// ([`ModuleSystemBuilder::package_runtime`](crate::system::ModuleSystemBuilder::package_runtime)),
/// then wrap both. One lifecycle operation runs at a time per process; across processes the
/// operation lock of the module system serialises them.
pub struct WasmLifecycle {
    system: Arc<ModuleSystem>,
    packages: Arc<WasmPackages>,
    gate: tokio::sync::Mutex<()>,
    after_stage: Option<StageHook>,
}

/// A test hook run once an operation is staged and before it takes the operation lock.
type StageHook = Arc<dyn Fn() -> BoxFuture<'static, ()> + Send + Sync>;

impl fmt::Debug for WasmLifecycle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WasmLifecycle").finish_non_exhaustive()
    }
}

/// Undoes the shared-index change of a staged operation unless [`Stage::done`] is called: also
/// when the future is dropped.
struct Stage<'a> {
    life: &'a WasmLifecycle,
    name: ModuleName,
    /// The package the index held before (an upgrade) to put back.
    previous: Option<Arc<Loaded>>,
    /// Whether the index entry of `name` was changed.
    index_touched: bool,
    armed: bool,
}

impl Stage<'_> {
    fn undo(&mut self) {
        self.armed = false;
        if !self.index_touched {
            return;
        }
        let restored = self.life.system.with_index(|index| {
            index.remove_package(&self.name);
            match &self.previous {
                Some(previous) => register(index, previous),
                None => Ok(()),
            }
        });
        if let Err(e) = restored {
            // The index could not take the previous package back: the next operation reports the
            // inconsistency (the catalog and the index disagree), and this says why.
            tracing::error!(module = %self.name, error = %e, "could not restore the addon index after a refused package operation");
        }
    }

    fn done(mut self) {
        self.armed = false;
    }
}

impl Drop for Stage<'_> {
    fn drop(&mut self) {
        if self.armed {
            self.undo();
        }
    }
}

impl WasmLifecycle {
    /// The lifecycle of `system`, whose package runtime must be `packages`.
    pub fn new(system: Arc<ModuleSystem>, packages: Arc<WasmPackages>) -> Self {
        Self {
            system,
            packages,
            gate: tokio::sync::Mutex::new(()),
            after_stage: None,
        }
    }

    /// Run `hook` after an operation is staged and before it takes the operation lock, to hold it
    /// in the window where an unrelated operation of the same process can run (a test seam; it
    /// changes nothing else).
    #[doc(hidden)]
    #[must_use]
    pub fn with_after_stage_hook(
        mut self,
        hook: impl Fn() -> BoxFuture<'static, ()> + Send + Sync + 'static,
    ) -> Self {
        self.after_stage = Some(Arc::new(hook));
        self
    }

    /// The module system.
    pub fn system(&self) -> &Arc<ModuleSystem> {
        &self.system
    }

    /// The loaded packages.
    pub fn packages(&self) -> &Arc<WasmPackages> {
        &self.packages
    }

    fn check_depends(manifest: &Manifest, catalog: &crate::lifecycle::Catalog) -> Result<()> {
        let missing: Vec<String> = manifest
            .depends()
            .iter()
            .filter(|d| catalog.get(d.as_str()).map(|r| r.state()) != Some(ModuleState::Installed))
            .map(ToString::to_string)
            .collect();
        if missing.is_empty() {
            Ok(())
        } else {
            Err(refuse(
                manifest.name(),
                WasmReason::DependsNotInstalled(missing),
            ))
        }
    }

    /// Install `package` into the database, without a restart, as the operator.
    ///
    /// The package is already consented ([`ErpAddonPackage::consent`]): its capabilities are
    /// approved and its client code acknowledged, by construction. `caller` is the System
    /// environment (uid 1) of this module system, the same rule as the native immediate
    /// operations: this is the operator (CLI) entry point and the audit row records
    /// `actor_kind = 'operator'` with that uid. An authenticated administrator uses
    /// [`WasmLifecycle::install_authorized`]. Returns the outcome of the module operation; switch to
    /// `outcome.orm`.
    ///
    /// # Errors
    /// [`Error::AccessDenied`] / [`Error::ForeignEnvironment`] for any other caller, before the
    /// package is looked at; [`Error::WasmAddon`] for a package this build cannot run, a name
    /// reserved for a native addon, a dependency that is not installed, a `describe()` that
    /// disagrees with the manifest, or an addon that is already installed; the module system's own
    /// errors (schema, signal, lock) as they are. Nothing persists on error and the registry in use
    /// stays the one it was.
    pub async fn install(
        &self,
        caller: &Env,
        package: ConsentedPackage,
    ) -> Result<OperationOutcome> {
        self.system
            .check_operator(caller, "WasmLifecycle::install")?;
        self.install_with(PackageAccess::Operator(caller), package)
            .await
    }

    /// [`WasmLifecycle::install`] for an authenticated administrator: `authorizer` (the trusted
    /// primary-session checker) rechecks the session and the module-administration
    /// right inside the operation transaction, after the lifecycle lock, before any effect. The
    /// audit row records the administrator's own uid and the session id.
    ///
    /// # Errors
    /// As [`WasmLifecycle::install`], and whatever the authorizer refuses with (nothing is written
    /// then).
    pub async fn install_authorized(
        &self,
        authorizer: &dyn InstallAuthorizer,
        package: ConsentedPackage,
    ) -> Result<OperationOutcome> {
        self.install_with(PackageAccess::Session(authorizer), package)
            .await
    }

    async fn install_with(
        &self,
        access: PackageAccess<'_>,
        package: ConsentedPackage,
    ) -> Result<OperationOutcome> {
        let _gate = self.gate.lock().await;
        let name = package.name().clone();
        reserved_name(&name)?;
        let catalog = self.system.read_catalog().await?;
        if let Some(row) = catalog.get(name.as_str())
            && matches!(
                row.state(),
                ModuleState::Installed
                    | ModuleState::ToInstall
                    | ModuleState::ToUpgrade
                    | ModuleState::ToRemove
            )
        {
            return Err(refuse(
                &name,
                WasmReason::State(format!(
                    "already {}; use upgrade to install a new version",
                    row.state()
                )),
            ));
        }
        Self::check_depends(package.manifest(), &catalog)?;
        let location = format!("package:{}", package.archive_sha256());
        let loaded = Arc::new(self.packages.build(&package, location).await?);
        self.run(access, Op::Install, name, Some(loaded), Some(package), None)
            .await
    }

    /// Upgrade an installed WebAssembly addon to a newer version of its package, as the operator.
    ///
    /// # Errors
    /// As [`WasmLifecycle::install`]; [`WasmReason::State`] when the addon is not installed by a
    /// package or the version is not newer.
    pub async fn upgrade(
        &self,
        caller: &Env,
        package: ConsentedPackage,
    ) -> Result<OperationOutcome> {
        self.system
            .check_operator(caller, "WasmLifecycle::upgrade")?;
        self.upgrade_with(PackageAccess::Operator(caller), package)
            .await
    }

    /// [`WasmLifecycle::upgrade`] for an authenticated administrator (see
    /// [`WasmLifecycle::install_authorized`]).
    ///
    /// # Errors
    /// As [`WasmLifecycle::upgrade`] and [`WasmLifecycle::install_authorized`].
    pub async fn upgrade_authorized(
        &self,
        authorizer: &dyn InstallAuthorizer,
        package: ConsentedPackage,
    ) -> Result<OperationOutcome> {
        self.upgrade_with(PackageAccess::Session(authorizer), package)
            .await
    }

    async fn upgrade_with(
        &self,
        access: PackageAccess<'_>,
        package: ConsentedPackage,
    ) -> Result<OperationOutcome> {
        let _gate = self.gate.lock().await;
        let name = package.name().clone();
        let Some(previous) = self.packages.state().live.get(&name).cloned() else {
            return Err(refuse(
                &name,
                WasmReason::State("not installed as a package addon; use install".to_owned()),
            ));
        };
        let catalog = self.system.read_catalog().await?;
        if catalog.get(name.as_str()).map(|r| r.state()) != Some(ModuleState::Installed) {
            return Err(refuse(
                &name,
                WasmReason::State("not installed; use install".to_owned()),
            ));
        }
        match (
            previous.manifest.version().semver(),
            package.version().semver(),
        ) {
            (Some(old), Some(new)) if new > old => {}
            _ => {
                return Err(refuse(
                    &name,
                    WasmReason::State(format!(
                        "version {} is not newer than the installed {}",
                        package.version(),
                        previous.manifest.version()
                    )),
                ));
            }
        }
        Self::check_depends(package.manifest(), &catalog)?;
        let location = format!("package:{}", package.archive_sha256());
        let loaded = Arc::new(self.packages.build(&package, location).await?);
        refuse_parent_store_turned_on(&name, &previous.defs, &loaded.defs)?;
        self.run(
            access,
            Op::Upgrade,
            name,
            Some(loaded),
            Some(package),
            Some(previous),
        )
        .await
    }

    /// Uninstall a WebAssembly addon and its data under the module system's uninstall rules, as the
    /// operator.
    ///
    /// # Errors
    /// [`WasmReason::HasDependents`] when other installed addons depend on it;
    /// [`WasmReason::State`] when it is not a package addon of this process; the module system's
    /// own uninstall errors.
    pub async fn uninstall(&self, caller: &Env, name: &ModuleName) -> Result<OperationOutcome> {
        self.system
            .check_operator(caller, "WasmLifecycle::uninstall")?;
        self.uninstall_with(PackageAccess::Operator(caller), name)
            .await
    }

    /// [`WasmLifecycle::uninstall`] for an authenticated administrator (see
    /// [`WasmLifecycle::install_authorized`]).
    ///
    /// # Errors
    /// As [`WasmLifecycle::uninstall`] and [`WasmLifecycle::install_authorized`].
    pub async fn uninstall_authorized(
        &self,
        authorizer: &dyn InstallAuthorizer,
        name: &ModuleName,
    ) -> Result<OperationOutcome> {
        self.uninstall_with(PackageAccess::Session(authorizer), name)
            .await
    }

    async fn uninstall_with(
        &self,
        access: PackageAccess<'_>,
        name: &ModuleName,
    ) -> Result<OperationOutcome> {
        let _gate = self.gate.lock().await;
        let Some(previous) = self.packages.state().live.get(name).cloned() else {
            return Err(refuse(
                name,
                WasmReason::State("not installed as a package addon".to_owned()),
            ));
        };
        let catalog = self.system.read_catalog().await?;
        let dependents: Vec<String> = catalog
            .downstream(
                std::slice::from_ref(name),
                &[ModuleState::Uninstalled, ModuleState::Uninstallable],
            )
            .iter()
            .map(ToString::to_string)
            .collect();
        if !dependents.is_empty() {
            return Err(refuse(name, WasmReason::HasDependents(dependents)));
        }
        self.run(
            access,
            Op::Uninstall,
            name.clone(),
            None,
            None,
            Some(previous),
        )
        .await
    }

    async fn run(
        &self,
        access: PackageAccess<'_>,
        op: Op,
        name: ModuleName,
        loaded: Option<Arc<Loaded>>,
        consented: Option<ConsentedPackage>,
        previous: Option<Arc<Loaded>>,
    ) -> Result<OperationOutcome> {
        let mut stage = Stage {
            life: self,
            name: name.clone(),
            previous: previous.clone(),
            index_touched: false,
            armed: true,
        };
        if let Some(loaded) = &loaded {
            self.system.with_index(|index| {
                if op == Op::Upgrade {
                    index.remove_package(&name);
                }
                stage.index_touched = true;
                register(index, loaded)
            })?;
        }
        // In development mode a development folder of the same name takes precedence over a package
        // in discovery; the package would then be stored and audited but never be the addon that
        // runs. Refuse that here, with the index undone by `stage`.
        if loaded.is_some() {
            let source = self.system.with_index(|index| {
                index
                    .discover_entries()
                    .map(|entries| entries.get(&name).map(|e| e.source_kind().clone()))
            })?;
            if !matches!(source, Some(AddonSourceKind::Package { .. })) {
                return Err(refuse(
                    &name,
                    WasmReason::State(format!(
                        "another addon source takes precedence over the package ({}); a package \
                         cannot share a name with a development folder",
                        source.map_or_else(|| "none".to_owned(), |s| s.to_string())
                    )),
                ));
            }
        }
        // The staged operation belongs to this task only (`ACTIVE`); the shared index above holds
        // the new manifest for the discovery of this operation (see the design note, section 5).
        let staged = Arc::new(Staged {
            op,
            name: name.clone(),
            loaded: loaded.clone(),
            consented,
            previous: previous.clone(),
            session: Mutex::new(None),
        });
        let names = [name.clone()];
        let package_op = match op {
            Op::Install => PackageOp::Install,
            Op::Upgrade => PackageOp::Upgrade,
            Op::Uninstall => PackageOp::Uninstall,
        };
        // A session authorizer is wrapped so the session id it returns reaches the audit row.
        let recording;
        let access = match access {
            PackageAccess::Session(inner) => {
                recording = RecordingAuthorizer {
                    inner,
                    staged: Arc::clone(&staged),
                };
                PackageAccess::Session(&recording)
            }
            operator @ PackageAccess::Operator(_) => operator,
        };
        if let Some(hook) = &self.after_stage {
            hook().await;
        }
        let result = ACTIVE
            .scope(
                Arc::clone(&staged),
                self.system.package_operation(package_op, &names, access),
            )
            .await;
        match result {
            Ok(outcome) => {
                {
                    let mut state = self.packages.state();
                    match (op, loaded) {
                        (Op::Install | Op::Upgrade, Some(loaded)) => {
                            state.live.insert(name.clone(), loaded);
                        }
                        _ => {
                            state.live.remove(&name);
                        }
                    }
                }
                if op == Op::Uninstall {
                    self.system.with_index(|index| index.remove_package(&name));
                }
                stage.done();
                tracing::info!(module = %name, operation = op.as_str(), "WebAssembly addon operation committed");
                Ok(outcome)
            }
            Err(e) => {
                stage.undo();
                Err(e)
            }
        }
    }

    /// The methods of the live package `module` that an RPC client may call, as `model.method`,
    /// sorted: what an administrator is told after an install or upgrade.
    ///
    /// # Errors
    /// The package is not live in this process.
    pub fn exposed_methods(&self, module: &ModuleName) -> Result<Vec<String>> {
        self.packages.exposed_methods(module)
    }

    /// The models of other addons the live package `module` extends, sorted.
    ///
    /// # Errors
    /// The package is not live in this process.
    pub fn extended_models(&self, module: &ModuleName) -> Result<Vec<String>> {
        self.packages.extended_models(module)
    }

    /// Load the packages of the installed WebAssembly addons from `ir_module_package`, then load
    /// the module set. This is what a restarted server, and a node whose registry generation moved
    /// because another node installed, upgraded or uninstalled a package, runs.
    ///
    /// Every stored blob is verified again (content address, `SHA256SUMS`, capabilities and
    /// client-code digest against the row) before it is used.
    ///
    /// # Errors
    /// [`WasmReason::StoredPackage`] for a stored package that no longer matches its row;
    /// the errors of [`ModuleSystem::load`].
    pub async fn load(&self) -> Result<LoadOutcome> {
        let _gate = self.gate.lock().await;
        self.admit_installed_packages_locked().await?;
        self.system.load().await
    }

    /// [`Self::load`] for the caller of a module operation that runs next: the packages, then
    /// [`ModuleSystem::load_before_operation`] (no `_register_hook` pass; the operation runs it on
    /// its final registry).
    ///
    /// # Errors
    /// The errors of [`Self::load`] other than those of `_register_hook`.
    pub async fn load_before_operation(&self) -> Result<LoadOutcome> {
        let _gate = self.gate.lock().await;
        self.admit_installed_packages_locked().await?;
        self.system.load_before_operation().await
    }

    /// Verify and register stored packages for explicit owner administration without loading
    /// the registry or recovering pending module work. This changes only the rebuildable
    /// process-local package index; it grants no lifecycle authority and writes no database rows.
    /// The eventual owner operation must recheck stored archive identity under its lifecycle lock.
    ///
    /// # Errors
    /// The same stored-archive, approval, capability and component admission errors as
    /// [`Self::load`], plus database errors; pending module operations are never replayed.
    pub async fn admit_installed_packages(&self) -> Result<()> {
        let _gate = self.gate.lock().await;
        self.admit_installed_packages_locked().await
    }

    async fn admit_installed_packages_locked(&self) -> Result<()> {
        let rows = self.read_installed_packages().await?;
        let mut seen: BTreeSet<ModuleName> = BTreeSet::new();
        for row in rows {
            let package = ErpAddonPackage::from_bytes(&row.data)?;
            let name = package.name().clone();
            let corrupt = |text: String| refuse(&name, WasmReason::StoredPackage(text));
            if package.archive_sha256() != row.sha256 {
                return Err(corrupt(format!(
                    "the stored archive hashes to {} but its row says {}",
                    package.archive_sha256(),
                    row.sha256
                )));
            }
            if package.name().as_str() != row.name {
                return Err(corrupt(format!("the row is named '{}'", row.name)));
            }
            // The approval record: the lifecycle writes the audit row in the same transaction as
            // the package, and the audit table is insert-only and closed to the runtime role, so a
            // package row without a matching install or upgrade row was written outside the
            // lifecycle and is not loaded (security review r1 L1).
            let Some(audit) = &row.audit else {
                return Err(corrupt(
                    "no install or upgrade row of the lifecycle audit records this archive"
                        .to_owned(),
                ));
            };
            if audit.capabilities != row.capabilities
                || audit.client_dist_digest != row.client_dist_digest
            {
                return Err(corrupt(
                    "the capabilities or client-code digest in the row differ from the ones the \
                     lifecycle audit recorded when it was installed"
                        .to_owned(),
                ));
            }
            let caps: Vec<String> = package
                .manifest()
                .wasm()
                .map(|w| {
                    w.capabilities
                        .iter()
                        .map(WasmCapability::canonical_str)
                        .collect()
                })
                .unwrap_or_default();
            if serde_json::to_value(&caps).ok().as_ref() != Some(&row.capabilities) {
                return Err(corrupt(
                    "the capabilities in the row differ from the manifest in the archive"
                        .to_owned(),
                ));
            }
            if package.client_dist_digest().map(str::to_owned) != row.client_dist_digest {
                return Err(corrupt(
                    "the client-code digest in the row differs from the archive".to_owned(),
                ));
            }
            seen.insert(name.clone());
            if self
                .packages
                .state()
                .live
                .get(&name)
                .is_some_and(|l| l.sha256 == row.sha256)
            {
                continue;
            }
            let location = format!("package:{}", package.archive_sha256());
            let loaded = Arc::new(self.packages.build(&package, location).await?);
            self.system.with_index(|index| {
                index.remove_package(&name);
                register(index, &loaded)
            })?;
            self.packages.state().live.insert(name, loaded);
        }
        // packages another node uninstalled
        let gone: Vec<ModuleName> = self
            .packages
            .state()
            .live
            .keys()
            .filter(|n| !seen.contains(*n))
            .cloned()
            .collect();
        for name in gone {
            self.packages.state().live.remove(&name);
            self.system.with_index(|index| index.remove_package(&name));
        }
        Ok(())
    }

    async fn read_installed_packages(&self) -> Result<Vec<StoredRow>> {
        let mut tx = self.system.begin_read().await?;
        let read = async {
            let exists = tx
                .query(
                    "SELECT to_regclass('ir_module_package') IS NOT NULL, \
                            to_regclass('ir_module_wasm_audit') IS NOT NULL",
                    &[],
                )
                .await?;
            let (package, audit): (bool, bool) = exists
                .first()
                .map_or((false, false), |r| (r.get(0), r.get(1)));
            if !package {
                return Ok(Vec::new());
            }
            if !audit {
                return Err(Error::InvariantViolation {
                    message: "ir_module_package exists but the lifecycle audit table \
                              ir_module_wasm_audit does not: no package can be checked against its \
                              approval record"
                        .to_owned(),
                });
            }
            let rows = tx.query(INSTALLED_PACKAGES_SQL, &[]).await?;
            Ok::<_, Error>(
                rows.iter()
                    .map(|r| {
                        let trim = |d: Option<String>| d.map(|d| d.trim_end().to_owned());
                        StoredRow {
                            name: r.get(0),
                            sha256: r.get::<_, String>(1).trim_end().to_owned(),
                            data: r.get(2),
                            capabilities: r.get(3),
                            client_dist_digest: trim(r.get(4)),
                            audit: r.get::<_, bool>(5).then(|| AuditRecord {
                                capabilities: r.get(6),
                                client_dist_digest: trim(r.get(7)),
                            }),
                        }
                    })
                    .collect(),
            )
        }
        .await;
        tx.rollback().await?;
        read
    }
}

struct StoredRow {
    name: String,
    sha256: String,
    data: Vec<u8>,
    capabilities: Value,
    client_dist_digest: Option<String>,
    /// The lifecycle audit row that recorded this archive, if there is one.
    audit: Option<AuditRecord>,
}

struct AuditRecord {
    capabilities: Value,
    client_dist_digest: Option<String>,
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]
mod tests {
    use super::*;

    /// prints, and refuses an empty escape and escape with parent)
    #[test]
    fn company_access_escape_round_trips_through_the_package_parser() {
        let decl = erp_registry::CompanyAccessDecl::direct_escaping(
            "company_id",
            erp_registry::CompanyBranch::Ancestor,
            erp_registry::EmptyCompany::Allow,
            "share",
        );
        assert_eq!(
            parse_company_access(&decl.spelling()).unwrap(),
            CompanyAccessDef::direct_escaping(
                "company_id",
                erp_registry::CompanyBranch::Ancestor,
                erp_registry::EmptyCompany::Allow,
                "share"
            )
        );
        let direct = r#"direct(field = "company_id", branch = "exact", empty = "deny""#;
        assert_eq!(
            parse_company_access(&format!(r#"{direct}, escape = "")"#)).unwrap_err(),
            "the escape name is empty"
        );
        assert_eq!(
            parse_company_access(&format!(r#"{direct}, parent = "a", escape = "share")"#))
                .unwrap_err(),
            "escape cannot be combined with parent in direct(…)"
        );
    }

    fn names(list: &[&str]) -> Vec<ModuleName> {
        list.iter().map(|n| ModuleName::new(*n).unwrap()).collect()
    }

    fn change<'a>(
        installed: &'a [ModuleName],
        upgraded: &'a [ModuleName],
        uninstalled: &'a [ModuleName],
    ) -> PackageChange<'a> {
        PackageChange {
            installed,
            upgraded,
            uninstalled,
        }
    }

    /// refused, whichever way: a cascade, an automatic dependency install, or nothing at all).
    #[test]
    fn only_the_requested_change_is_accepted() {
        let w = ModuleName::new("w").unwrap();
        let none: Vec<ModuleName> = Vec::new();
        let w_only = names(&["w"]);
        assert!(unexpected_changes(Op::Install, &w, change(&w_only, &none, &none)).is_empty());
        assert!(unexpected_changes(Op::Uninstall, &w, change(&none, &none, &w_only)).is_empty());
        // an upgrade may upgrade dependents
        let with_dependent = names(&["w", "d"]);
        assert!(
            unexpected_changes(Op::Upgrade, &w, change(&none, &with_dependent, &none)).is_empty()
        );

        // an install that also installed a dependency
        let with_dep = names(&["dep", "w"]);
        assert_eq!(
            unexpected_changes(Op::Install, &w, change(&with_dep, &none, &none)),
            ["dep"]
        );
        // an uninstall that cascaded to a dependent
        assert_eq!(
            unexpected_changes(Op::Uninstall, &w, change(&none, &none, &with_dependent)),
            ["d"]
        );
        // an uninstall that also installed something
        let other = names(&["x"]);
        assert_eq!(
            unexpected_changes(Op::Uninstall, &w, change(&other, &none, &w_only)),
            ["x"]
        );
        // an operation that did not do what was asked
        let missing = unexpected_changes(Op::Install, &w, change(&none, &none, &none));
        assert_eq!(missing.len(), 1);
        assert!(missing[0].contains("did not install w"));
    }

    fn manifest(depends: &str) -> Manifest {
        Manifest::from_toml(
            "w",
            &format!("name = 'W'\ndepends = [{depends}]\n[wasm]\nsdk_version = '1.0'\n"),
        )
        .unwrap()
    }

    fn doc(contributions: &Value) -> String {
        serde_json::json!({"schema": 1, "contributions": contributions}).to_string()
    }

    /// become definitions of the addon with their fields and attributes; contributions of the
    /// addons the component was linked with are not this addon's.
    #[test]
    fn describe_becomes_model_definitions() {
        let text = doc(&serde_json::json!([
            {"addon": "w", "kind": "addon", "model": "", "name": "w",
             "detail": {"depends": ["base", "nat"]}},
            {"addon": "w", "kind": "model", "model": "w.thing", "name": "Thing",
             "detail": {"args": {"name": "\"w.thing\"", "description": "\"A thing\""},
                        "fields": [{"name": "title", "kind": "char",
                                    "attrs": {"string": "\"Title\"", "required": "true", "size": "64"}},
                                   {"name": "qty", "kind": "integer", "attrs": {}}]}},
            {"addon": "w", "kind": "extend", "model": "nat.x", "name": "Ext",
             "detail": {"args": {}, "fields": [{"name": "note", "kind": "text", "attrs": {}}]}},
            {"addon": "linked", "kind": "model", "model": "linked.m", "name": "M",
             "detail": {"args": {}, "fields": [{"name": "z", "kind": "many2one", "attrs": {}}]}},
        ]));
        let defs = models_from_describe(&manifest("'nat'"), &text).unwrap();
        assert_eq!(defs.len(), 2);
        assert_eq!(defs[0].name, "w.thing");
        assert!(!defs[0].is_extension());
        assert_eq!(defs[0].description.as_deref(), Some("A thing"));
        assert_eq!(defs[0].fields[0].required, Some(true));
        assert_eq!(defs[0].fields[0].string.as_deref(), Some("Title"));
        assert_eq!(defs[0].fields[0].kind, KindDef::Char { size: Some(64) });
        assert!(defs[1].is_extension());
        assert_eq!(defs[1].fields[0].kind, KindDef::Text);
    }

    fn one_model(args: Value, fields: Value) -> String {
        doc(&serde_json::json!([
            {"addon": "w", "kind": "addon", "model": "", "name": "w",
             "detail": {"depends": ["base"]}},
            {"addon": "w", "kind": "model", "model": "w.wiz", "name": "Wiz",
             "detail": {"args": args, "fields": fields}},
        ]))
    }

    #[test]
    fn field_collation_description_is_preserved() {
        let describe = |value: Value| {
            one_model(
                serde_json::json!({}),
                serde_json::json!([
                    {"name":"code", "kind":"char", "attrs":{"collation": value}}
                ]),
            )
        };
        let defs =
            models_from_describe(&manifest(""), &describe(serde_json::json!("\"C\""))).unwrap();
        assert_eq!(defs[0].fields[0].collation.as_deref(), Some("C"));
        let error =
            models_from_describe(&manifest(""), &describe(serde_json::json!(42))).unwrap_err();
        assert!(matches!(error, Error::WasmAddon { .. }), "{error:?}");
        assert_eq!(
            error.to_string(),
            "WebAssembly addon 'w': not supported in this build: field w.wiz.code: collation: expected a string"
        );
    }

    #[test]
    fn html_policy_describe_preserves_raw_and_refuses_bypasses() {
        let describe = |attrs: Value| {
            one_model(
                serde_json::json!({}),
                serde_json::json!([{"name":"body", "kind":"html", "attrs":attrs}]),
            )
        };
        let defs = models_from_describe(
            &manifest(""),
            &describe(serde_json::json!({"sanitize":"false"})),
        )
        .unwrap();
        assert_eq!(
            defs[0].fields[0].html_policy,
            Some(erp_fields::html::FieldPolicy::UnsanitisedText)
        );
        for attrs in [
            serde_json::json!({"sanitize":"\"email_outgoing\""}),
            serde_json::json!({"sanitize_overridable":"true"}),
        ] {
            let error = models_from_describe(&manifest(""), &describe(attrs)).unwrap_err();
            assert!(error.to_string().contains("w.wiz.body"));
            assert!(
                error.to_string().contains("unavailable")
                    || error.to_string().contains("forbidden")
            );
        }
    }

    #[test]
    fn describe_reads_company_access_forms() {
        use erp_registry::{CompanyAccessDef, CompanyBranch, EmptyCompany};
        let cases = [
            (
                "direct(field = \"company_id\", branch = \"exact\", empty = \"deny\")",
                CompanyAccessDef::direct("company_id", CompanyBranch::Exact, EmptyCompany::Deny),
            ),
            (
                "direct(field = \"company_id\", branch = \"ancestor\", empty = \"allow\", parent = \"document_id\")",
                CompanyAccessDef::direct_and_parent(
                    "company_id",
                    CompanyBranch::Ancestor,
                    EmptyCompany::Allow,
                    "document_id",
                ),
            ),
            (
                "parent(edge = \"document_id\")",
                CompanyAccessDef::parent("document_id"),
            ),
            (
                r#"unrestricted(reason = "reviewed \"shared\" record")"#,
                CompanyAccessDef::unrestricted("reviewed \"shared\" record"),
            ),
        ];
        for (source, expected) in cases {
            let described = one_model(
                serde_json::json!({"company_access": source}),
                serde_json::json!([]),
            );
            let defs = models_from_describe(&manifest("'base'"), &described).unwrap();
            assert_eq!(defs[0].company_access.as_ref(), Some(&expected), "{source}");
        }
        let described = one_model(serde_json::json!({}), serde_json::json!([]));
        let defs = models_from_describe(&manifest("'base'"), &described).unwrap();
        assert_eq!(defs[0].company_access, None);
    }

    #[test]
    fn describe_refuses_malformed_company_access() {
        for (source, error) in [
            (
                serde_json::json!("parent(edge = \"a\", edge = \"b\")"),
                "parameter 'edge' is given twice",
            ),
            (
                serde_json::json!("direct(field = \"company_id\")"),
                "direct(…) needs field, branch and empty (missing: branch, empty)",
            ),
            (
                serde_json::json!("unrestricted(reason = \" \" )"),
                "unrestricted(…) needs a reason that is not blank and at most 512 bytes long",
            ),
            (
                serde_json::json!("parent(edge = 3)"),
                "edge must be a string literal",
            ),
            (serde_json::json!(23), "must be a string"),
        ] {
            let described = one_model(
                serde_json::json!({"company_access": source}),
                serde_json::json!([]),
            );
            let msg = models_from_describe(&manifest("'base'"), &described)
                .unwrap_err()
                .to_string();
            assert_eq!(
                msg,
                format!(
                    "WebAssembly addon 'w': not supported in this build: model 'w.wiz': company_access: {error}"
                )
            );
        }
    }

    #[test]
    fn describe_accepts_escaped_reason_at_native_limit_and_bounds_source() {
        let escaped = r"\u{61}".repeat(512);
        let source = format!("unrestricted(reason = \"{escaped}\")");
        let described = one_model(
            serde_json::json!({"company_access": source}),
            serde_json::json!([]),
        );
        let defs = models_from_describe(&manifest("'base'"), &described).unwrap();
        assert_eq!(
            defs[0].company_access,
            Some(CompanyAccessDef::unrestricted("a".repeat(512)))
        );
        let oversized = one_model(
            serde_json::json!({"company_access": format!("unrestricted(reason = \"{}\")", "a".repeat(8192))}),
            serde_json::json!([]),
        );
        assert_eq!(
            models_from_describe(&manifest("'base'"), &oversized)
                .unwrap_err()
                .to_string(),
            "WebAssembly addon 'w': not supported in this build: model 'w.wiz': company_access: the declaration exceeds 8192 bytes"
        );
    }

    #[test]
    fn describe_refuses_non_object_model_args() {
        let described = one_model(serde_json::json!(23), serde_json::json!([]));
        let msg = models_from_describe(&manifest("'base'"), &described)
            .unwrap_err()
            .to_string();
        assert_eq!(
            msg,
            "WebAssembly addon 'w': not supported in this build: model 'w.wiz': args must be an object"
        );
    }

    #[test]
    fn describe_accepts_policy_only_extension_but_refuses_empty_extension() {
        let contribution = |args: Value| {
            doc(&serde_json::json!([
                {"addon": "w", "kind": "addon", "model": "", "name": "w",
                 "detail": {"depends": ["base"]}},
                {"addon": "w", "kind": "extend", "model": "base.doc", "name": "DocPolicy",
                 "detail": {"args": args, "fields": []}}
            ]))
        };
        let source = "direct(field = \"company_id\", branch = \"exact\", empty = \"deny\")";
        let described = contribution(serde_json::json!({"company_access": source}));
        let defs = models_from_describe(&manifest("'base'"), &described).unwrap();
        assert_eq!(
            defs[0].company_access,
            Some(CompanyAccessDef::direct(
                "company_id",
                CompanyBranch::Exact,
                EmptyCompany::Deny
            ))
        );
        let empty = contribution(serde_json::json!({}));
        assert_eq!(
            models_from_describe(&manifest("'base'"), &empty)
                .unwrap_err()
                .to_string(),
            "WebAssembly addon 'w': describe() disagrees with the manifest: the addon extends 'base.doc' but adds no field and no method to it"
        );
    }

    #[test]
    fn packaged_policy_extension_narrows_and_widening_is_refused() {
        use erp_registry::{CompanyAccessDecl, CompanyAccessState, Overlay, RegistryBuilder};

        let described = |base_empty: &str, extension_empty: &str| {
            doc(&serde_json::json!([
                {"addon": "w", "kind": "addon", "model": "", "name": "w",
                 "detail": {"depends": ["base"]}},
                {"addon": "w", "kind": "model", "model": "res.company", "name": "Company",
                 "detail": {"args": {}, "fields": [{"name": "name", "kind": "char", "attrs": {}}]}},
                {"addon": "w", "kind": "model", "model": "w.doc", "name": "Doc",
                 "detail": {"args": {"company_access": format!("direct(field = \"company_id\", branch = \"exact\", empty = \"{base_empty}\")")},
                            "fields": [{"name": "company_id", "kind": "many2one", "attrs": {"comodel": "\"res.company\""}}]}},
                {"addon": "w", "kind": "extend", "model": "w.doc", "name": "Policy",
                 "detail": {"args": {"company_access": format!("direct(field = \"company_id\", branch = \"exact\", empty = \"{extension_empty}\")")}, "fields": []}}
            ]))
        };
        let manifest = manifest("'base'");
        let descriptor = crate::model_files::addon_descriptor(&manifest).unwrap();
        let build = |base_empty, extension_empty| {
            let defs =
                models_from_describe(&manifest, &described(base_empty, extension_empty)).unwrap();
            let mut overlay = Overlay::new();
            overlay.add_addon_defs(defs).unwrap();
            RegistryBuilder::new([descriptor])
                .linked(false)
                .overlay(&overlay)
                .build()
        };
        let registry = build("allow", "deny").unwrap();
        let CompanyAccessState::Declared(access) = registry.model("w.doc").unwrap().company_access
        else {
            panic!("w.doc has no declared access");
        };
        assert_eq!(
            access.declaration(),
            CompanyAccessDecl::direct("company_id", CompanyBranch::Exact, EmptyCompany::Deny)
        );
        let widened = build("deny", "allow").unwrap_err();
        assert_eq!(widened.kind(), erp_fields::ErrorKind::Programming);
        assert_eq!(
            widened.message(),
            "w.doc: company_access direct(field = \"company_id\", branch = \"exact\", empty = \"allow\") (declared by model definition 'w.doc' of addon 'w' (wasm/w.wasm:0)) is wider than direct(field = \"company_id\", branch = \"exact\", empty = \"deny\") (declared by model definition 'w.doc' of addon 'w' (wasm/w.wasm:0)): a later declaration may only narrow a model's company restriction"
        );
    }

    /// does, so the installed model is transient; the vacuum is a later slice).
    #[test]
    fn describe_reads_the_model_kind() {
        for (text, want) in [
            ("\"transient\"", ModelKind::Transient),
            ("\"abstract\"", ModelKind::Abstract),
            ("\"regular\"", ModelKind::Regular),
        ] {
            let described = one_model(serde_json::json!({"kind": text}), serde_json::json!([]));
            let defs = models_from_describe(&manifest("'base'"), &described).unwrap();
            assert_eq!(defs[0].kind, Some(want), "kind {text}");
        }
        let described = one_model(serde_json::json!({}), serde_json::json!([]));
        let defs = models_from_describe(&manifest("'base'"), &described).unwrap();
        assert_eq!(defs[0].kind, None, "not stated stays not stated");
    }

    #[test]
    fn describe_reads_eventful_opt_in() {
        for (value, want) in [
            (serde_json::json!("true"), true),
            (serde_json::json!("false"), false),
        ] {
            let described = one_model(
                serde_json::json!({"eventful": value}),
                serde_json::json!([]),
            );
            let defs = models_from_describe(&manifest("'base'"), &described).unwrap();
            assert_eq!(defs[0].eventful, want);
        }
        let described = one_model(serde_json::json!({}), serde_json::json!([]));
        let defs = models_from_describe(&manifest("'base'"), &described).unwrap();
        assert!(!defs[0].eventful);
    }

    #[test]
    fn describe_refuses_invalid_eventful_opt_in() {
        for value in [
            serde_json::json!(1),
            serde_json::json!("\"true\""),
            serde_json::Value::Null,
        ] {
            let described = one_model(
                serde_json::json!({"eventful": value.clone()}),
                serde_json::json!([]),
            );
            let error = models_from_describe(&manifest("'base'"), &described).unwrap_err();
            match error {
                Error::WasmAddon {
                    reason: WasmReason::Unsupported(message),
                    ..
                } => {
                    assert_eq!(
                        message,
                        format!("model 'w.wiz': eventful = {value}: only true and false are read")
                    );
                }
                other => panic!("expected typed unsupported eventful input, got {other:?}"),
            }
        }
    }

    #[test]
    fn describe_reads_audit_reads_opt_in() {
        for (value, want) in [
            (serde_json::json!("true"), true),
            (serde_json::json!("false"), false),
        ] {
            let described = one_model(
                serde_json::json!({"audit_reads": value}),
                serde_json::json!([]),
            );
            let defs = models_from_describe(&manifest("'base'"), &described).unwrap();
            assert_eq!(defs[0].audit_reads, want);
        }
        let described = one_model(serde_json::json!({}), serde_json::json!([]));
        let defs = models_from_describe(&manifest("'base'"), &described).unwrap();
        assert!(!defs[0].audit_reads);
    }

    #[test]
    fn describe_refuses_invalid_audit_reads_opt_in() {
        for value in [
            serde_json::json!(1),
            serde_json::json!("\"true\""),
            serde_json::Value::Null,
        ] {
            let described = one_model(
                serde_json::json!({"audit_reads": value.clone()}),
                serde_json::json!([]),
            );
            let error = models_from_describe(&manifest("'base'"), &described).unwrap_err();
            match error {
                Error::WasmAddon {
                    reason: WasmReason::Unsupported(message),
                    ..
                } => {
                    assert_eq!(
                        message,
                        format!(
                            "model 'w.wiz': audit_reads = {value}: only true and false are read"
                        )
                    );
                }
                other => panic!("expected typed unsupported audit_reads input, got {other:?}"),
            }
        }
    }

    /// model are read as the integers the native macro reads.
    #[test]
    fn describe_reads_the_transient_policy() {
        let args = serde_json::json!({
            "kind": "\"transient\"",
            "transient_max_age_seconds": "7_200",
            "transient_max_count": "0",
        });
        let described = one_model(args, serde_json::json!([]));
        let defs = models_from_describe(&manifest("'base'"), &described).unwrap();
        assert_eq!(defs[0].transient_max_age_seconds, Some(7200));
        assert_eq!(defs[0].transient_max_count, Some(0));
        let described = one_model(serde_json::json!({}), serde_json::json!([]));
        let defs = models_from_describe(&manifest("'base'"), &described).unwrap();
        assert_eq!(defs[0].transient_max_age_seconds, None);
        assert_eq!(defs[0].transient_max_count, None);
    }

    /// A malformed policy attribute is refused naming the model and the attribute.
    #[test]
    fn describe_refuses_a_malformed_transient_policy() {
        for (attr, text, hint) in [
            ("transient_max_age_seconds", "- 1", "must not be negative"),
            ("transient_max_count", "- 5", "must not be negative"),
            (
                "transient_max_age_seconds",
                "1.5",
                "decimal integer literal",
            ),
            ("transient_max_count", "\"10\"", "decimal integer literal"),
            ("transient_max_count", "10u64", "decimal integer literal"),
            ("transient_max_count", "0x10", "decimal integer literal"),
            ("transient_max_count", "true", "decimal integer literal"),
            ("transient_max_count", "18446744073709551616", "64 bits"),
        ] {
            let described = one_model(serde_json::json!({attr: text}), serde_json::json!([]));
            let msg = models_from_describe(&manifest("'base'"), &described)
                .unwrap_err()
                .to_string();
            assert!(
                msg.contains("model 'w.wiz'") && msg.contains(attr) && msg.contains(hint),
                "{attr} = {text}: {msg}"
            );
        }
    }

    /// An unknown or non-literal model kind is refused by name, never read as regular.
    #[test]
    fn describe_refuses_an_unknown_model_kind() {
        for (text, hint) in [
            (
                "\"wizard\"",
                "use \"regular\", \"abstract\" or \"transient\"",
            ),
            ("transient", "only string literals are read"),
            ("true", "only string literals are read"),
        ] {
            let described = one_model(serde_json::json!({"kind": text}), serde_json::json!([]));
            let msg = models_from_describe(&manifest("'base'"), &described)
                .unwrap_err()
                .to_string();
            assert!(
                msg.contains("model 'w.wiz': kind") && msg.contains(hint),
                "{text}: {msg}"
            );
        }
    }

    /// relation of the wizard fixture).
    #[test]
    fn describe_reads_a_many2one_field() {
        let fields = serde_json::json!([
            {"name": "partner_id", "kind": "many2one",
             "attrs": {"comodel": "\"res.partner\"", "ondelete": "\"cascade\"", "required": "true"}},
            {"name": "other_id", "kind": "many2one", "attrs": {"comodel": "\"res.partner\""}},
        ]);
        let described = one_model(serde_json::json!({}), fields);
        let defs = models_from_describe(&manifest("'base'"), &described).unwrap();
        assert_eq!(
            defs[0].fields[0].kind,
            KindDef::Many2one {
                comodel: "res.partner".to_owned(),
                ondelete: Some(OnDelete::Cascade)
            }
        );
        assert_eq!(defs[0].fields[0].required, Some(true));
        assert_eq!(
            defs[0].fields[1].kind,
            KindDef::Many2one {
                comodel: "res.partner".to_owned(),
                ondelete: None
            }
        );
    }

    /// A many2one without a comodel, or with an unknown `ondelete`, is refused naming the field.
    #[test]
    fn describe_refuses_a_malformed_many2one() {
        let cases = [
            (serde_json::json!({}), "comodel"),
            (
                serde_json::json!({"comodel": "\"res.partner\"", "ondelete": "\"nuke\""}),
                "ondelete",
            ),
            (serde_json::json!({"comodel": "res.partner"}), "comodel"),
            (serde_json::json!({"comodel": "\"\""}), "non-empty comodel"),
        ];
        for (attrs, needle) in cases {
            let fields = serde_json::json!([{"name": "p_id", "kind": "many2one", "attrs": attrs}]);
            let described = one_model(serde_json::json!({}), fields);
            let err = models_from_describe(&manifest("'base'"), &described).unwrap_err();
            let msg = err.to_string();
            assert!(
                msg.contains("w.wiz.p_id") && msg.contains(needle),
                "{attrs}: {msg}"
            );
        }
    }

    /// `comodel` is only a relation attribute: on a char field it stays refused.
    #[test]
    fn describe_refuses_comodel_on_a_scalar_field() {
        let fields = serde_json::json!([
            {"name": "c", "kind": "char", "attrs": {"comodel": "\"res.partner\""}}]);
        let described = one_model(serde_json::json!({}), fields);
        let msg = models_from_describe(&manifest("'base'"), &described)
            .unwrap_err()
            .to_string();
        assert!(msg.contains("w.wiz.c") && msg.contains("comodel"), "{msg}");
    }

    /// add on their own does not count as a difference, a real difference does).
    #[test]
    fn depends_compare_with_base_added_on_both_sides() {
        let entry = |deps: &[&str]| {
            doc(
                &serde_json::json!([{"addon": "w", "kind": "addon", "model": "", "name": "w",
                "detail": {"depends": deps}}]),
            )
        };
        assert!(models_from_describe(&manifest(""), &entry(&["base"])).is_ok());
        assert!(models_from_describe(&manifest("'nat'"), &entry(&["nat"])).is_ok());
        let err = models_from_describe(&manifest("'nat'"), &entry(&["base", "other"])).unwrap_err();
        assert!(err.to_string().contains("depends on"), "{err}");
        let err = models_from_describe(&manifest("'nat'"), "not json").unwrap_err();
        assert!(err.to_string().contains("not JSON"), "{err}");
        let err = models_from_describe(
            &manifest("'nat'"),
            &serde_json::json!({"schema": 2, "contributions": []}).to_string(),
        )
        .unwrap_err();
        assert!(err.to_string().contains("schema"), "{err}");
    }

    /// from `describe()`; those of linked addons are not its own; a computed field's `compute`
    /// attribute reaches the model definition).
    #[test]
    fn describe_yields_computes_constraints_and_computed_fields() {
        let text = entry_doc(vec![
            serde_json::json!({"addon": "w", "kind": "extend", "model": "nat.x", "name": "Ext",
                "detail": {"args": {}, "fields": [{"name": "total", "kind": "integer",
                    "attrs": {"compute": "\"compute_total\"", "store": "true"}}]}}),
            serde_json::json!({"addon": "w", "kind": "compute", "model": "nat.x", "name": "compute_total",
                "detail": {"async": false, "depends": ["qty"], "reads": ["name"], "depends_context": []}}),
            serde_json::json!({"addon": "w", "kind": "constraint", "model": "nat.x", "name": "check_qty",
                "detail": {"fields": ["qty"]}}),
            serde_json::json!({"addon": "linked", "kind": "compute", "model": "l.m", "name": "c",
                "detail": {"async": false, "depends": ["z"], "reads": [], "depends_context": []}}),
        ]);
        let checks = checks_from_describe(&manifest(""), &text).unwrap();
        assert_eq!(checks.computes.len(), 1, "{checks:?}");
        assert_eq!(checks.computes[0].method, "compute_total");
        assert_eq!(checks.computes[0].depends, ["qty"]);
        assert_eq!(checks.computes[0].reads, ["name"]);
        assert_eq!(checks.constraints.len(), 1);
        assert_eq!(checks.constraints[0].fields, ["qty"]);
        let defs = models_from_describe(&manifest(""), &text).unwrap();
        assert_eq!(defs[0].fields[0].compute.as_deref(), Some("compute_total"));
        assert_eq!(defs[0].fields[0].store, Some(true));
    }

    /// never skipped: async, a path through a relation, an override of a compute, onchange).
    #[test]
    fn describe_refuses_what_a_compute_cannot_do_yet() {
        let compute = |detail: Value| {
            entry_doc(vec![
                serde_json::json!({"addon": "w", "kind": "compute", "model": "nat.x", "name": "c", "detail": detail}),
            ])
        };
        let ok = serde_json::json!({"async": false, "depends": ["qty"], "reads": [], "depends_context": []});
        checks_from_describe(&manifest(""), &compute(ok)).unwrap();
        let asynchronous =
            serde_json::json!({"async": true, "depends": [], "reads": [], "depends_context": []});
        let err = checks_from_describe(&manifest(""), &compute(asynchronous)).unwrap_err();
        assert!(err.to_string().contains("async compute"), "{err}");
        let path = serde_json::json!({"async": false, "depends": ["line_ids.qty"], "reads": [], "depends_context": []});
        let err = checks_from_describe(&manifest(""), &compute(path)).unwrap_err();
        assert!(err.to_string().contains("line_ids.qty"), "{err}");
        // a missing list is an error, not an empty list (security review r1 L1)
        let missing = serde_json::json!({"async": false, "reads": [], "depends_context": []});
        let err = checks_from_describe(&manifest(""), &compute(missing)).unwrap_err();
        assert!(err.to_string().contains("depends is missing"), "{err}");
        // a package compute is never elevated (security review r1 H1)
        let text = entry_doc(vec![
            serde_json::json!({"addon": "w", "kind": "extend", "model": "nat.x", "name": "Ext",
                "detail": {"args": {}, "fields": [{"name": "total", "kind": "integer",
                    "attrs": {"compute": "\"c\"", "compute_sudo": "true"}}]}}),
        ]);
        let err = models_from_describe(&manifest(""), &text).unwrap_err();
        assert!(err.to_string().contains("never runs elevated"), "{err}");
        let text = entry_doc(vec![
            serde_json::json!({"addon": "w", "kind": "extend", "model": "nat.x", "name": "Ext",
                "detail": {"args": {}, "fields": [{"name": "total", "kind": "integer",
                    "attrs": {"compute": "\"c\""}}]}}),
        ]);
        let defs = models_from_describe(&manifest(""), &text).unwrap();
        assert_eq!(
            defs[0].fields[0].compute_sudo,
            Some(false),
            "forced, not the store default"
        );
        for kind in ["compute_override", "onchange"] {
            let text = entry_doc(vec![
                serde_json::json!({"addon": "w", "kind": kind, "model": "nat.x", "name": "c", "detail": {}}),
            ]);
            let err = models_from_describe(&manifest(""), &text).unwrap_err();
            assert!(err.to_string().contains(kind), "{err}");
        }
    }

    /// is the same table `create_package_table_sql` builds from the size constants).
    #[test]
    fn the_registered_package_ddl_matches_the_built_one() {
        let squash = |sql: &str| sql.split_whitespace().collect::<Vec<_>>().join(" ");
        let registered = squash(crate::package::PACKAGE_TABLE_DDL);
        let built = squash(&crate::package::create_package_table_sql());
        assert_eq!(
            registered,
            format!("{built}; REVOKE ALL ON ir_module_package FROM PUBLIC")
        );
    }

    fn entry_doc(extra: Vec<Value>) -> String {
        let mut list = vec![serde_json::json!(
            {"addon": "w", "kind": "addon", "model": "", "name": "w", "detail": {"depends": ["base"]}})];
        list.extend(extra);
        doc(&Value::Array(list))
    }

    /// MAX_DESCRIBED_MODELS models and MAX_DESCRIBED_FIELDS fields: at the bound it reads, one over
    /// is `TooLarge` naming the limit; G security review r2 N1).
    #[test]
    fn a_package_over_the_contribution_limits_is_refused() {
        let method = |i: usize| serde_json::json!({"addon": "w", "kind": "method", "model": "m.x", "name": format!("f_{i}")});
        let methods = |n: usize| entry_doc((0..n).map(method).collect());
        models_from_describe(&manifest(""), &methods(MAX_PACKAGE_METHODS)).unwrap();
        let err =
            models_from_describe(&manifest(""), &methods(MAX_PACKAGE_METHODS + 1)).unwrap_err();
        assert_eq!(
            err.to_string(),
            format!(
                "WebAssembly addon 'w': describe() is over the limits of one package: more than \
                 {MAX_PACKAGE_METHODS} methods"
            )
        );

        let model = |i: usize, fields: usize| {
            let fields: Vec<Value> = (0..fields)
                .map(|f| serde_json::json!({"name": format!("f{f}"), "kind": "char", "attrs": {}}))
                .collect();
            serde_json::json!({"addon": "w", "kind": "model", "model": format!("w.m{i}"),
                "name": "M", "detail": {"args": {}, "fields": fields}})
        };
        let models = |n: usize| entry_doc((0..n).map(|i| model(i, 0)).collect());
        assert_eq!(
            models_from_describe(&manifest(""), &models(MAX_DESCRIBED_MODELS))
                .unwrap()
                .len(),
            MAX_DESCRIBED_MODELS
        );
        let err =
            models_from_describe(&manifest(""), &models(MAX_DESCRIBED_MODELS + 1)).unwrap_err();
        assert_eq!(
            err.to_string(),
            format!(
                "WebAssembly addon 'w': describe() is over the limits of one package: more than \
                 {MAX_DESCRIBED_MODELS} models"
            )
        );

        let fields = |n: usize| entry_doc(vec![model(0, n)]);
        models_from_describe(&manifest(""), &fields(MAX_DESCRIBED_FIELDS)).unwrap();
        let err =
            models_from_describe(&manifest(""), &fields(MAX_DESCRIBED_FIELDS + 1)).unwrap_err();
        assert_eq!(
            err.to_string(),
            format!(
                "WebAssembly addon 'w': describe() is over the limits of one package: more than \
                 {MAX_DESCRIBED_FIELDS} fields"
            )
        );
        // the contributions of an addon the component was linked with do not count
        let linked: Vec<Value> = (0..=MAX_PACKAGE_METHODS)
            .map(|i| serde_json::json!({"addon": "other", "kind": "method", "model": "m.x", "name": format!("g_{i}")}))
            .collect();
        models_from_describe(&manifest(""), &entry_doc(linked)).unwrap();
    }

    fn ref_doc() -> String {
        doc(&serde_json::json!([
            {"addon": "w", "kind": "addon", "model": "", "name": "w",
             "detail": {"depends": ["base", "lib"]}},
            {"addon": "w", "kind": "model_ref", "model": "lib.loan", "name": "Loan",
             "detail": {"fields": [
                {"name": "state", "kinds": ["char", "text", "html", "selection"], "comodel": null},
                {"name": "book_id", "kinds": ["many2one"], "comodel": "lib.book"}]}},
            {"addon": "linked", "kind": "model_ref", "model": "lib.loan", "name": "Other",
             "detail": {"fields": [{"name": "zzz", "kinds": ["boolean"], "comodel": null}]}},
        ]))
    }

    /// The registry as these tests see it: `lib.loan` with a selection and a many2one.
    fn lookup(model: &str, field: &str) -> RefLookup {
        match (model, field) {
            ("lib.loan", "state") => RefLookup::Field {
                kind: "selection",
                comodel: None,
            },
            ("lib.loan", "book_id") => RefLookup::Field {
                kind: "many2one",
                comodel: Some("lib.book".to_owned()),
            },
            ("lib.loan", _) => RefLookup::NoField,
            _ => RefLookup::NoModel,
        }
    }

    /// own entries only, and `models_from_describe` leaves them alone).
    #[test]
    fn describe_yields_the_typed_model_references() {
        let text = ref_doc();
        let refs = refs_from_describe(&manifest("'lib'"), &text).unwrap();
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].model, "lib.loan");
        assert_eq!(refs[0].fields[1].name, "book_id");
        assert_eq!(refs[0].fields[1].comodel.as_deref(), Some("lib.book"));
        assert_eq!(models_from_describe(&manifest("'lib'"), &text).unwrap(), []);
    }

    #[test]
    fn matching_references_pass() {
        let refs = refs_from_describe(&manifest("'lib'"), &ref_doc()).unwrap();
        check_refs(&ModuleName::new("w").unwrap(), &refs, lookup).unwrap();
    }

    /// another comodel is refused by name and says what differs).
    #[test]
    fn mismatching_references_are_refused_by_name() {
        let w = ModuleName::new("w").unwrap();
        let one = |model: &str, field: &str, kinds: &[&str], comodel: Option<&str>| {
            vec![DescribedRef {
                model: model.to_owned(),
                fields: vec![DescribedRefField {
                    name: field.to_owned(),
                    kinds: kinds.iter().map(|k| (*k).to_owned()).collect(),
                    comodel: comodel.map(str::to_owned),
                }],
            }]
        };
        let refused =
            |refs: Vec<DescribedRef>| check_refs(&w, &refs, lookup).unwrap_err().to_string();
        assert!(
            refused(one("lib.nope", "state", &["char"], None))
                .contains("model 'lib.nope' is not in the registry")
        );
        assert!(
            refused(one("lib.loan", "nope", &["char"], None))
                .contains("model 'lib.loan' has no field 'nope'")
        );
        let kind = refused(one("lib.loan", "state", &["integer"], None));
        assert!(
            kind.contains("lib.loan.state is a selection field") && kind.contains("[integer]"),
            "{kind}"
        );
        let comodel = refused(one(
            "lib.loan",
            "book_id",
            &["many2one"],
            Some("res.partner"),
        ));
        assert!(
            comodel
                .contains("lib.loan.book_id points to lib.book but the handle names res.partner"),
            "{comodel}"
        );
    }
    // ---- WASM.MODEL-DECL: field attributes, keys and parent store ------------------------------

    const REFUSED: &str = "WebAssembly addon 'w': not supported in this build: ";

    fn fields_of(kind: &str, attrs: Value) -> std::result::Result<FieldDef, Error> {
        models_from_describe(
            &manifest(""),
            &one_model(
                serde_json::json!({}),
                serde_json::json!([{"name": "f", "kind": kind, "attrs": attrs}]),
            ),
        )
        .map(|mut defs| defs.remove(0).fields.remove(0))
    }

    fn model_args(extend: bool, args: Value) -> std::result::Result<ModelDef, Error> {
        let kind = if extend { "extend" } else { "model" };
        let text = doc(&serde_json::json!([
            {"addon": "w", "kind": "addon", "model": "", "name": "w",
             "detail": {"depends": ["base"]}},
            {"addon": "w", "kind": kind, "model": "w.wiz", "name": "Wiz",
             "detail": {"args": args, "fields": []}},
        ]));
        models_from_describe(&manifest(""), &text).map(|mut defs| defs.remove(0))
    }

    /// hand-written JSON; the real macro text is in `wasm_model_decl_describe`)
    #[test]
    fn field_attributes_are_read_from_token_text() {
        let state = fields_of(
            "selection",
            serde_json::json!({
                "selection": "[(\"draft\" , \"Draft\") , (\"done\" , \"Done\")]",
                "default": "\"draft\""
            }),
        )
        .unwrap();
        assert_eq!(
            state.kind,
            KindDef::Selection {
                values: Some(vec![
                    ("draft".into(), "Draft".into()),
                    ("done".into(), "Done".into())
                ]),
                add: vec![],
                ondelete: vec![],
            }
        );
        assert_eq!(state.default, Some(DefaultValue::Str("draft".into())));
        let amount = fields_of(
            "float",
            serde_json::json!({"digits": "(16 , 2)", "default": "1.5"}),
        )
        .unwrap();
        assert_eq!(
            amount.kind,
            KindDef::Float {
                digits: Some(Digits::Fixed {
                    precision: 16,
                    scale: 2
                })
            }
        );
        assert_eq!(amount.default, Some(DefaultValue::Float(1.5)));
        let defaults = |kind: &str, text: &str| {
            fields_of(kind, serde_json::json!({"default": text}))
                .unwrap()
                .default
        };
        assert_eq!(defaults("integer", "- 1"), Some(DefaultValue::Int(-1)));
        assert_eq!(defaults("integer", "7"), Some(DefaultValue::Int(7)));
        assert_eq!(defaults("float", "- 2.5"), Some(DefaultValue::Float(-2.5)));
        assert_eq!(defaults("boolean", "true"), Some(DefaultValue::Bool(true)));
        assert_eq!(
            defaults("boolean", "false"),
            Some(DefaultValue::Bool(false))
        );
        assert_eq!(defaults("char", "false"), Some(DefaultValue::False));
        assert_eq!(defaults("char", "False"), Some(DefaultValue::False));
        let index = |value: Value| fields_of("char", serde_json::json!({"index": value}));
        assert_eq!(
            index(serde_json::json!(true)).unwrap().index,
            Some(Some(IndexKind::Btree))
        );
        assert_eq!(
            index(serde_json::json!("true")).unwrap().index,
            Some(Some(IndexKind::Btree))
        );
        assert_eq!(index(serde_json::json!("false")).unwrap().index, Some(None));
        assert_eq!(
            index(serde_json::json!("\"trigram\"")).unwrap().index,
            Some(Some(IndexKind::Trigram))
        );
        let groups = fields_of(
            "char",
            serde_json::json!({"groups": "\"base.group_system\""}),
        );
        assert_eq!(groups.unwrap().groups.as_deref(), Some("base.group_system"));
    }

    #[test]
    fn field_attribute_refusals_name_the_attribute() {
        let cases: Vec<(&str, Value, &str)> = vec![
            (
                "selection",
                serde_json::json!({"selection_add": "[(\"x\" , \"X\")]"}),
                "field w.wiz.f: selection_add is not read from a WebAssembly addon in this build \
                 (extending another addon's selection follows in a later WebAssembly slice)",
            ),
            (
                "selection",
                serde_json::json!({"selection_method": "\"_m\""}),
                "field w.wiz.f: selection_method is not read from a WebAssembly addon in this \
                 build (it names a Rust function)",
            ),
            (
                "integer",
                serde_json::json!({"default_fn": "f"}),
                "field w.wiz.f: default_fn is not read from a WebAssembly addon in this build (it \
                 names a Rust function; use a literal default)",
            ),
            (
                "float",
                serde_json::json!({"digits": "\"price\""}),
                "field w.wiz.f: digits: a named precision is not read from a WebAssembly addon; \
                 use (precision, scale)",
            ),
            (
                "float",
                serde_json::json!({"digits": "(16 , x)"}),
                "field w.wiz.f: digits: precision and scale are unsigned integer literals",
            ),
            (
                "float",
                serde_json::json!({"digits": "0"}),
                "field w.wiz.f: digits: expected (precision, scale)",
            ),
            (
                "integer",
                serde_json::json!({"digits": "(16 , 2)"}),
                "field w.wiz.f: digits does not apply to a integer field",
            ),
            (
                "char",
                serde_json::json!({"selection": "[(\"a\" , \"A\")]"}),
                "field w.wiz.f: selection does not apply to a char field",
            ),
            (
                "selection",
                serde_json::json!({}),
                "field w.wiz.f: a selection needs selection = [(\"value\", \"Label\"), ...]",
            ),
            (
                "selection",
                serde_json::json!({"selection": "[(\"a\" , 1)]"}),
                "field w.wiz.f: selection: expected a list of (\"value\", \"Label\") string pairs",
            ),
            (
                "selection",
                serde_json::json!({"selection": "[(\"a\" , \"A\" , \"B\")]"}),
                "field w.wiz.f: selection: expected a list of (\"value\", \"Label\") string pairs",
            ),
            (
                "char",
                serde_json::json!({"index": "\"nope\""}),
                "field w.wiz.f: index: unknown index kind \"nope\"",
            ),
            (
                "char",
                serde_json::json!({"index": "\"gin\""}),
                "field w.wiz.f: index = \"gin\" is for json fields (this field is char)",
            ),
            (
                "char",
                serde_json::json!({"index": 3}),
                "field w.wiz.f: index: expected true, false or an index kind",
            ),
            (
                "integer",
                serde_json::json!({"default": "some_fn()"}),
                "field w.wiz.f: default: expected a string, number or boolean literal (a function \
                 is not read)",
            ),
            (
                "integer",
                serde_json::json!({"default": "none"}),
                "field w.wiz.f: default: expected a string, number or boolean literal (a function \
                 is not read)",
            ),
            (
                "integer",
                serde_json::json!({"default": "99999999999999999999"}),
                "field w.wiz.f: default: the integer does not fit in 64 bits",
            ),
        ];
        for (kind, attrs, message) in cases {
            let error = fields_of(kind, attrs.clone()).unwrap_err();
            assert!(matches!(error, Error::WasmAddon { .. }), "{error:?}");
            assert_eq!(error.to_string(), format!("{REFUSED}{message}"), "{attrs}");
        }
    }

    /// cut, and so is a description text over the parse bound)
    #[test]
    fn declaration_values_are_bounded() {
        let entries = |n: usize| {
            let pairs: Vec<String> = (0..n).map(|i| format!("(\"v{i}\" , \"L\")")).collect();
            format!("[{}]", pairs.join(" , "))
        };
        let at_limit = fields_of(
            "selection",
            serde_json::json!({"selection": entries(erp_registry::MAX_SELECTION_ENTRIES)}),
        )
        .unwrap();
        let KindDef::Selection { values, .. } = at_limit.kind else {
            panic!("a selection kind is expected");
        };
        assert_eq!(values.unwrap().len(), erp_registry::MAX_SELECTION_ENTRIES);
        let over = erp_registry::MAX_SELECTION_ENTRIES + 1;
        let error =
            fields_of("selection", serde_json::json!({"selection": entries(over)})).unwrap_err();
        assert_eq!(
            error.to_string(),
            format!(
                "{REFUSED}field w.wiz.f: selection: more than the limit of {} entries",
                erp_registry::MAX_SELECTION_ENTRIES
            )
        );
        let huge = format!("[(\"{}\" , \"L\")]", "x".repeat(MAX_SELECTION_SOURCE_BYTES));
        let error = fields_of("selection", serde_json::json!({"selection": huge})).unwrap_err();
        assert_eq!(
            error.to_string(),
            format!(
                "{REFUSED}field w.wiz.f: selection: the declaration exceeds \
                 {MAX_SELECTION_SOURCE_BYTES} bytes"
            )
        );
    }

    /// arguments; a bare flag and `= true` both count)
    #[test]
    fn model_keys_and_parent_store_are_read() {
        let keys = "[key(name = \"_serial_uniq\" , fields = [\"serial_no\"] , message = \"Unique.\" \
                    , nulls_distinct = true) , key(name = \"_pair\" , fields = [\"a\" , \"b\"] , \
                    message = \"Pair.\")]";
        for store in [serde_json::json!(true), serde_json::json!("true")] {
            let def = model_args(
                false,
                serde_json::json!({"keys": keys, "parent_store": store,
                                   "parent_name": "\"up_id\""}),
            )
            .unwrap();
            assert_eq!(def.parent_store, Some(true));
            assert_eq!(def.parent_name.as_deref(), Some("up_id"));
            assert_eq!(
                def.keys,
                vec![
                    KeyDef::new("_serial_uniq", "Unique.")
                        .fields(["serial_no"])
                        .nulls_distinct(true),
                    KeyDef::new("_pair", "Pair.").fields(["a", "b"]),
                ]
            );
        }
        let off = model_args(false, serde_json::json!({"parent_store": "false"})).unwrap();
        assert_eq!(off.parent_store, Some(false));
    }

    #[test]
    fn model_declaration_refusals_name_the_argument() {
        let key = |inner: &str| format!("[key({inner})]");
        let cases: Vec<(bool, Value, String)> = vec![
            (
                true,
                serde_json::json!({"keys": key("name = \"_k\" , fields = [\"a\"] , message = \"m\"")}),
                "model 'w.wiz': keys is refused on an extension of another model; a package \
                 declares keys and a parent store only on the models it creates"
                    .into(),
            ),
            (
                true,
                serde_json::json!({"parent_store": true}),
                "model 'w.wiz': parent_store is refused on an extension of another model; a \
                 package declares keys and a parent store only on the models it creates"
                    .into(),
            ),
            (
                true,
                serde_json::json!({"parent_name": "\"up_id\""}),
                "model 'w.wiz': parent_name is refused on an extension of another model; a \
                 package declares keys and a parent store only on the models it creates"
                    .into(),
            ),
            (
                false,
                serde_json::json!({"keys": key("name = \"_k\" , fields = [\"a\"] , message = \"m\" , without_overlaps = \"r\"")}),
                "model 'w.wiz': keys: without_overlaps (a temporal key) is not read from a \
                 WebAssembly addon"
                    .into(),
            ),
            (
                false,
                serde_json::json!({"keys": key("name = \"_k\" , fields = [\"a\"]")}),
                "model 'w.wiz': keys: key(…) needs message = \"...\"".into(),
            ),
            (
                false,
                serde_json::json!({"keys": key("name = \"_k\" , fields = [\"a\"] , message = \"m\" , extra = 1")}),
                "model 'w.wiz': keys: unknown parameter 'extra' in key(…)".into(),
            ),
            (
                false,
                serde_json::json!({"keys": key("name = \"_k\" , fields = [a] , message = \"m\"")}),
                "model 'w.wiz': keys: key fields are string literals".into(),
            ),
            (
                false,
                serde_json::json!({"keys": "key(name = \"_k\")"}),
                "model 'w.wiz': keys: expected an array of key(name = \"_x\", fields = [...], \
                 message = \"...\")"
                    .into(),
            ),
        ];
        for (extend, args, message) in cases {
            let error = model_args(extend, args.clone()).unwrap_err();
            assert!(matches!(error, Error::WasmAddon { .. }), "{error:?}");
            assert_eq!(error.to_string(), format!("{REFUSED}{message}"), "{args}");
        }
        for name in ["constraints", "indexes", "unique_indexes"] {
            let error = model_args(false, serde_json::json!({ name: "[]" })).unwrap_err();
            assert_eq!(
                error.to_string(),
                format!(
                    "{REFUSED}model 'w.wiz': {name} are raw SQL table objects, which only a \
                     native addon declares; use keys = [key(name = \"_x\", fields = [...], \
                     message = \"...\")] for a unique constraint"
                )
            );
        }
    }
    /// refused by the flat reader at 100000 repetitions, with the attribute named; nothing
    /// recursive runs, so the test cannot overflow a stack)
    #[test]
    fn repeated_expression_shapes_are_refused_by_every_entry_point() {
        const N: usize = 100_000;
        let shapes: Vec<(&str, String)> = vec![
            ("calls", format!("x{}", "()".repeat(N))),
            ("indexing", format!("x{}", "[0]".repeat(N))),
            ("binary minus", format!("1{}", "-(1)".repeat(N))),
            ("method chain", format!("x{}", ".f()".repeat(N))),
            ("casts", format!("1{}", " as u8".repeat(N))),
            ("return", format!("{}1", "return ".repeat(N))),
            ("assignments", format!("x{}", "=()".repeat(N))),
            ("unary minus", format!("{}1", "- ".repeat(N))),
            (
                "parentheses",
                format!("{}1{}", "(".repeat(N), ")".repeat(N)),
            ),
            ("brackets", format!("{}{}", "[".repeat(N), "]".repeat(N))),
            ("not", format!("{}1", "!".repeat(N))),
        ];
        for (what, source) in shapes {
            // field attributes: the shape is refused, by size where it exceeds the value's cap
            for (kind, attr, cap) in [
                ("selection", "selection", MAX_SELECTION_SOURCE_BYTES),
                ("float", "digits", MAX_SCALAR_SOURCE_BYTES),
                ("integer", "default", MAX_SCALAR_SOURCE_BYTES),
            ] {
                let error = fields_of(kind, serde_json::json!({ attr: source })).unwrap_err();
                let text = error.to_string();
                let prefix = format!("{REFUSED}field w.wiz.f: {attr}: ");
                assert!(
                    matches!(error, Error::WasmAddon { .. }) && text.starts_with(&prefix),
                    "{what} in {attr}: {text}"
                );
                if source.len() > cap {
                    assert_eq!(text, format!("{prefix}the declaration exceeds {cap} bytes"));
                }
            }
            for (arg, cap) in [
                ("keys", MAX_KEYS_SOURCE_BYTES),
                ("company_access", MAX_COMPANY_ACCESS_SOURCE_BYTES),
            ] {
                let error = model_args(false, serde_json::json!({ arg: source })).unwrap_err();
                let text = error.to_string();
                let prefix = format!("{REFUSED}model 'w.wiz': {arg}: ");
                assert!(
                    matches!(error, Error::WasmAddon { .. }) && text.starts_with(&prefix),
                    "{what} in {arg}: {text}"
                );
                if source.len() > cap {
                    assert_eq!(text, format!("{prefix}the declaration exceeds {cap} bytes"));
                }
            }
        }
        // the shapes that fit under a cap are refused for their shape, not accepted: the same
        // repetitions at a size every cap admits
        for source in [
            "x()()()()",
            "x[0][0]",
            "1-(1)-(1)",
            "x.f().f()",
            "1 as u8 as u8",
            "return 1",
        ] {
            assert!(parse_default(source, "integer").is_err(), "{source}");
            assert!(parse_digits(source).is_err(), "{source}");
            assert!(parse_selection(source).is_err(), "{source}");
            assert!(parse_keys(source).is_err(), "{source}");
            assert!(parse_company_access(source).is_err(), "{source}");
        }
    }

    /// strings included, and refuses what it does not read)
    #[test]
    fn the_flat_reader_decodes_literals_and_refuses_the_rest() {
        assert_eq!(
            lex("[(\"a\\\"b\" , r#\"c\"d\"#)]", 4096).unwrap(),
            vec![
                Tok::Punct('['),
                Tok::Punct('('),
                Tok::Str("a\"b".into()),
                Tok::Punct(','),
                Tok::Str("c\"d".into()),
                Tok::Punct(')'),
                Tok::Punct(']'),
            ]
        );
        assert_eq!(
            lex("- 1 1.5 1e-3 x_1", 4096).unwrap(),
            vec![
                Tok::Punct('-'),
                Tok::Int("1".into()),
                Tok::Float("1.5".into()),
                Tok::Float("1e-3".into()),
                Tok::Ident("x_1".into()),
            ]
        );
        assert_eq!(
            parse_selection("[(\"a\" , \"A\") ,]").unwrap(),
            vec![("a".to_owned(), "A".to_owned())]
        );
        for (source, why) in [
            (
                "a // c",
                "unexpected character '/' outside a string literal",
            ),
            ("\"open", "a string literal is not closed"),
            (
                "1u8",
                "a number with a suffix or a trailing dot is not read",
            ),
            ("1.", "a number with a suffix or a trailing dot is not read"),
            ("a + b", "unexpected character '+' outside a string literal"),
            ("{}", "unexpected character '{' outside a string literal"),
        ] {
            assert_eq!(lex(source, 4096), Err(why.to_owned()), "{source}");
        }
        assert_eq!(
            lex("abc", 2),
            Err("the declaration exceeds 2 bytes".to_owned())
        );
    }

    #[test]
    fn impossible_digits_and_duplicate_key_fields_are_refused() {
        for (source, why) in [
            ("(0 , 0)", "the precision must be at least 1"),
            ("(2 , 3)", "the scale must not be larger than the precision"),
        ] {
            let error = fields_of("float", serde_json::json!({"digits": source})).unwrap_err();
            assert_eq!(
                error.to_string(),
                format!("{REFUSED}field w.wiz.f: digits: {why}")
            );
        }
        fields_of("float", serde_json::json!({"digits": "(3 , 3)"})).unwrap();
        let keys = "[key(name = \"_k\" , fields = [\"a\" , \"b\" , \"a\"] , message = \"m\")]";
        let error = model_args(false, serde_json::json!({ "keys": keys })).unwrap_err();
        assert_eq!(
            error.to_string(),
            format!("{REFUSED}model 'w.wiz': keys: key '_k' names field 'a' twice")
        );
        // the field list is bounded before the pairwise duplicate check (r3 review SHOULD 1)
        let many: Vec<String> = (0..=erp_registry::MAX_CONSTRAINT_COLUMNS)
            .map(|i| format!("\"f{i}\""))
            .collect();
        let keys = format!(
            "[key(name = \"_k\" , fields = [{}] , message = \"m\")]",
            many.join(" , ")
        );
        let error = model_args(false, serde_json::json!({ "keys": keys })).unwrap_err();
        assert_eq!(
            error.to_string(),
            format!(
                "{REFUSED}model 'w.wiz': keys: key '_k' names {} fields, more than the limit of {}",
                erp_registry::MAX_CONSTRAINT_COLUMNS + 1,
                erp_registry::MAX_CONSTRAINT_COLUMNS
            )
        );
        // a float default that overflows to infinity is refused, not stored
        let error = fields_of("float", serde_json::json!({"default": "1e400"})).unwrap_err();
        assert_eq!(
            error.to_string(),
            format!("{REFUSED}field w.wiz.f: default: the float is not finite")
        );
    }
}
