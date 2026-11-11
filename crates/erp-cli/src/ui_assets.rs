//! Host adapters for declared UI assets, transactional lifecycle publication and startup admission.
//!
//! Every input is explicitly declared by `static/dist/manifest.json`; the host never exposes a
//! directory or compiles on request. Dev reads use directory descriptors without following links,
//! and packages use the separately retained, archive-verified client-file map.
//! Entries retained by module outcomes use their already admitted immutable source snapshot;
//! lifecycle/startup never substitute a later live directory read for those admitted bytes.

use erp_addon_web::assets::{
    assembly::{self, AddonAssets, AssembledBundle, AssemblyOptions, AssetInput, MAX_ASSET_BYTES},
    store,
};
use erp_fields::{Error, Result};
use erp_modules::{
    AddonEntry, AddonSourceKind, LifecycleChange, LifecycleHook, OPERATION_LOCK, PackageFiles,
};
use erp_orm::{BeginOptions, BoxFuture, Isolation, Orm, SUPERUSER_ID, Transaction};
use erp_sdk::ui_compiler::{BuildMode, CompileOptions};
use serde::Deserialize;
use std::{collections::BTreeSet, io::Read};

/// Monotonic runtime ABI epoch. Advance it only for an incompatible runtime contract change.
pub const RUNTIME_ABI: &str = "elang-1";
const MANIFEST_PATH: &str = "static/dist/manifest.json";
const MAX_MANIFEST_BYTES: usize = 256 * 1024;
const MAX_INPUT_BYTES: usize = 128 * 1024 * 1024;
const MAX_FILES: usize = 20_000;
const MAX_ADDONS: usize = 4096;

/// Whether the running runtime is current or is an allowed monotonic server upgrade.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum RuntimeAdmission {
    /// Exactly the same runtime epoch.
    Current,
    /// The running runtime advances the stored epoch.
    Upgrade,
}

fn epoch(identity: &str) -> Result<u32> {
    let bad = || {
        Error::validation(format!(
            "invalid UI runtime identity '{}': expected elang-<positive u32> without leading zeros",
            identity.chars().take(128).collect::<String>()
        ))
    };
    if identity.len() > 16 {
        return Err(bad());
    }
    let suffix = identity.strip_prefix("elang-").ok_or_else(bad)?;
    if suffix.is_empty() || suffix.starts_with('0') || !suffix.bytes().all(|b| b.is_ascii_digit()) {
        return Err(bad());
    }
    let value = suffix.parse::<u32>().map_err(|_| bad())?;
    if value == 0 {
        return Err(bad());
    }
    Ok(value)
}

/// Admit a runtime generation using strict monotonic epochs, never lexical or semver fallback.
/// # Errors
/// Malformed identities or a node older than the database's published runtime generation.
pub fn runtime_admission(stored: &str, running: &str) -> Result<RuntimeAdmission> {
    let stored_epoch = epoch(stored)?;
    let running_epoch = epoch(running)?;
    if stored_epoch > running_epoch {
        return Err(Error::validation(format!(
            "published UI runtime '{stored}' is newer than running runtime '{running}'; incompatible node cannot reassemble it"
        )));
    }
    Ok(if stored_epoch == running_epoch {
        RuntimeAdmission::Current
    } else {
        RuntimeAdmission::Upgrade
    })
}

/// Production assembly identities stamped from compiler/runtime source inputs by the CLI build.
/// # Errors
/// Invalid compiler/runtime identity, language, group or compile configuration.
pub fn host_options() -> Result<AssemblyOptions> {
    AssemblyOptions::new(
        env!("ERP_UI_COMPILER_ID"),
        RUNTIME_ABI,
        "en_US",
        "main",
        CompileOptions::new(BuildMode::Prod),
    )
    .map_err(assembly_error)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ClientManifest {
    schema: u8,
    runtime_abi: String,
    entry: Option<String>,
    modules: Vec<DeclaredModule>,
    templates: Vec<String>,
    static_files: Vec<String>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DeclaredModule {
    path: String,
    specifier: String,
}

fn input_error(addon: &str, path: &str, message: impl std::fmt::Display) -> Error {
    let message: String = message.to_string().chars().take(2048).collect();
    Error::validation(format!(
        "addon '{addon}' asset '{}': {message}",
        path.chars().take(1024).collect::<String>()
    ))
}
fn assembly_error(error: assembly::AssemblyError) -> Error {
    Error::validation(format!("UI asset assembly failed: {error}")).with_source(error)
}
fn normalized(addon: &str, path: &str) -> Result<()> {
    if path.is_empty()
        || path.len() > 1024
        || !path.starts_with("static/")
        || path
            .split('/')
            .any(|p| p.is_empty() || p == "." || p == "..")
        || !path
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.' | b'/'))
    {
        return Err(input_error(
            addon,
            path,
            "expected a bounded normalized relative static path",
        ));
    }
    Ok(())
}

struct Budget {
    files: usize,
    bytes: usize,
}
impl Budget {
    fn charge(&mut self, addon: &str, path: &str, bytes: usize) -> Result<()> {
        self.files = self
            .files
            .checked_add(1)
            .ok_or_else(|| input_error(addon, path, "file count overflow"))?;
        self.bytes = self
            .bytes
            .checked_add(bytes)
            .ok_or_else(|| input_error(addon, path, "input bytes overflow"))?;
        if self.files > MAX_FILES || self.bytes > MAX_INPUT_BYTES {
            return Err(input_error(
                addon,
                path,
                "declared input count/bytes exceeds hard admission limit",
            ));
        }
        Ok(())
    }
}

/// The source descriptor stays open for all declarations of one addon. Package maps were copied
/// from an entirely verified archive; an empty map proves absence of client declarations.
enum Source<'a> {
    Snapshot(&'a PackageFiles),
    Dev(rustix::fd::OwnedFd),
    Package(&'a PackageFiles),
    Embedded,
}
impl<'a> Source<'a> {
    fn open(entry: &'a AddonEntry) -> Result<Self> {
        if let Some(files) = entry.admitted_files() {
            return Ok(Self::Snapshot(files));
        }
        let addon = entry.name().as_str();
        match entry.source_kind() {
            AddonSourceKind::DevFolder(path) => {
                let fd = rustix::fs::open(
                    path,
                    rustix::fs::OFlags::RDONLY
                        | rustix::fs::OFlags::DIRECTORY
                        | rustix::fs::OFlags::NOFOLLOW
                        | rustix::fs::OFlags::CLOEXEC,
                    rustix::fs::Mode::empty(),
                )
                .map_err(|e| {
                    input_error(
                        addon,
                        MANIFEST_PATH,
                        format!("cannot read addon root {}: {e}", path.display()),
                    )
                    .with_source(e)
                })?;
                Ok(Self::Dev(fd))
            }
            AddonSourceKind::Package { .. } => {
                let data=entry.package_data().ok_or_else(|| input_error(addon,MANIFEST_PATH,"verified client files are unavailable; load the verified package before asset admission"))?;
                Ok(Self::Package(&data.client_files))
            }
            // Embedded addons have no host filesystem namespace. The host's compiled client is
            // admitted separately; no declared client manifest means no per-addon inputs here.
            AddonSourceKind::Embedded => Ok(Self::Embedded),
            _ => Err(input_error(
                addon,
                MANIFEST_PATH,
                "unsupported addon source kind",
            )),
        }
    }
    fn read(
        &self,
        addon: &str,
        path: &str,
        limit: usize,
        optional: bool,
    ) -> Result<Option<Vec<u8>>> {
        normalized(addon, path)?;
        match self {
            Self::Snapshot(files) => match files.get(path) {
                Some(bytes) if bytes.len() <= limit => Ok(Some(bytes.clone())),
                Some(_) => Err(input_error(
                    addon,
                    path,
                    "file bytes exceed hard admission limit",
                )),
                None if optional => Ok(None), // The complete admitted snapshot proves absence.
                None => Err(input_error(
                    addon,
                    path,
                    "cannot read declared file from admitted snapshot",
                )),
            },
            Self::Dev(root) => read_dev(root, addon, path, limit, optional),
            Self::Package(files) => match files.get(path) {
                Some(bytes) if bytes.len() <= limit => Ok(Some(bytes.clone())),
                Some(_) => Err(input_error(
                    addon,
                    path,
                    "file bytes exceed hard admission limit",
                )),
                None if optional => Ok(None), // The complete verified client map proves absence.
                None => Err(input_error(
                    addon,
                    path,
                    "cannot read declared file from verified package",
                )),
            },
            Self::Embedded if optional => Ok(None), // No embedded client-file declaration API exists.
            Self::Embedded => Err(input_error(
                addon,
                path,
                "cannot read undeclared embedded client file",
            )),
        }
    }
}
fn read_dev(
    root: &rustix::fd::OwnedFd,
    addon: &str,
    path: &str,
    limit: usize,
    optional: bool,
) -> Result<Option<Vec<u8>>> {
    use rustix::fs::{Mode, OFlags};
    let parts = path.split('/').collect::<Vec<_>>();
    let mut parent = None;
    for (index, part) in parts.iter().enumerate() {
        let last = index + 1 == parts.len();
        let flags = if last {
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK
        } else {
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC
        };
        let directory = parent.as_ref().unwrap_or(root); // First component is relative to the admitted root descriptor.
        let fd = match rustix::fs::openat(directory, *part, flags, Mode::empty()) {
            Ok(fd) => fd,
            Err(rustix::io::Errno::NOENT) if optional => return Ok(None), // A missing manifest or ancestor declares no client assets.
            Err(error) => {
                return Err(input_error(
                    addon,
                    path,
                    format!("cannot read declared file: {error}"),
                )
                .with_source(error));
            }
        };
        if last {
            let file = std::fs::File::from(fd);
            let metadata = file.metadata().map_err(|e| {
                input_error(addon, path, format!("cannot read file metadata: {e}")).with_source(e)
            })?;
            if !metadata.is_file() {
                return Err(input_error(
                    addon,
                    path,
                    "cannot read a non-regular declared file",
                ));
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt;
                if metadata.nlink() != 1 {
                    return Err(input_error(
                        addon,
                        path,
                        "cannot read a declared file with multiple hard links",
                    ));
                }
            }
            if metadata.len() > limit as u64 {
                return Err(input_error(
                    addon,
                    path,
                    "file bytes exceed hard admission limit",
                ));
            }
            let mut bytes = Vec::new();
            file.take((limit as u64) + 1)
                .read_to_end(&mut bytes)
                .map_err(|e| {
                    input_error(addon, path, format!("cannot read declared file: {e}"))
                        .with_source(e)
                })?;
            if bytes.len() > limit {
                return Err(input_error(
                    addon,
                    path,
                    "file bytes exceed hard admission limit",
                ));
            }
            return Ok(Some(bytes));
        }
        parent = Some(fd);
    }
    Err(input_error(
        addon,
        path,
        "cannot read empty declared file path",
    ))
}
fn required(source: &Source<'_>, addon: &str, path: &str, budget: &mut Budget) -> Result<Vec<u8>> {
    let bytes = source
        .read(addon, path, MAX_ASSET_BYTES, false)?
        .ok_or_else(|| input_error(addon, path, "cannot read declared file"))?;
    budget.charge(addon, path, bytes.len())?;
    Ok(bytes)
}

/// Collect only explicitly declared client files, preserving addon and declaration order.
///
/// A missing `static/dist/manifest.json` means no declared inputs for that addon. Other source,
/// shape and file errors are refused, including a package whose verified client files have not
/// been retained. The complete addon set still participates in the deterministic assembly key.
/// # Errors
/// Invalid/unknown manifest fields or ABI, duplicate or unsafe declarations, bounded input,
/// symlink/hard-link/non-regular files, missing declared files, or unavailable verified source.
pub fn collect_entries(entries: &[&AddonEntry], runtime: &str) -> Result<Vec<AddonAssets>> {
    epoch(runtime)?;
    if entries.len() > MAX_ADDONS {
        return Err(Error::validation(
            "UI addon set exceeds hard admission limit",
        ));
    }
    let mut budget = Budget { files: 0, bytes: 0 };
    let mut out = Vec::with_capacity(entries.len());
    for entry in entries {
        let addon = entry.name().as_str();
        let source = Source::open(entry)?;
        let manifest = source.read(addon, MANIFEST_PATH, MAX_MANIFEST_BYTES, true)?;
        let mut assets = AddonAssets::new(addon, &entry.manifest().version().to_string(), runtime)
            .map_err(|e| input_error(addon, MANIFEST_PATH, e))?;
        if let Some(bytes) = manifest {
            budget.charge(addon, MANIFEST_PATH, bytes.len())?;
            let manifest: ClientManifest = serde_json::from_slice(&bytes)
                .map_err(|e| input_error(addon, MANIFEST_PATH, &e).with_source(e))?;
            if manifest.schema != 1 {
                return Err(input_error(
                    addon,
                    MANIFEST_PATH,
                    format!("unsupported schema {}", manifest.schema),
                ));
            }
            if manifest.runtime_abi != runtime {
                return Err(input_error(
                    addon,
                    MANIFEST_PATH,
                    format!(
                        "runtime ABI '{}' does not match running '{runtime}'",
                        manifest.runtime_abi
                    ),
                ));
            }
            let mut declared = BTreeSet::new();
            for path in manifest
                .modules
                .iter()
                .map(|m| m.path.as_str())
                .chain(manifest.templates.iter().map(String::as_str))
                .chain(manifest.static_files.iter().map(String::as_str))
            {
                normalized(addon, path)?;
                if !declared.insert(path) {
                    return Err(input_error(
                        addon,
                        MANIFEST_PATH,
                        format!("duplicate path '{path}'"),
                    ));
                }
            }
            for module in manifest.modules {
                let bytes = required(&source, addon, &module.path, &mut budget)?;
                assets
                    .push(
                        AssetInput::module(&module.path, &module.specifier, bytes)
                            .map_err(|e| input_error(addon, MANIFEST_PATH, e))?,
                    )
                    .map_err(|e| input_error(addon, MANIFEST_PATH, e))?;
            }
            for path in manifest.templates {
                let bytes = required(&source, addon, &path, &mut budget)?;
                let xml = std::str::from_utf8(&bytes)
                    .map_err(|e| input_error(addon, &path, "expected UTF-8 XML").with_source(e))?;
                assets
                    .push(
                        AssetInput::template(&path, xml)
                            .map_err(|e| input_error(addon, MANIFEST_PATH, e))?,
                    )
                    .map_err(|e| input_error(addon, MANIFEST_PATH, e))?;
            }
            for path in manifest.static_files {
                let bytes = required(&source, addon, &path, &mut budget)?;
                assets
                    .push(
                        AssetInput::static_file(&path, bytes)
                            .map_err(|e| input_error(addon, MANIFEST_PATH, e))?,
                    )
                    .map_err(|e| input_error(addon, MANIFEST_PATH, e))?;
            }
            if let Some(entry) = manifest.entry {
                assets
                    .set_entry(&entry)
                    .map_err(|e| input_error(addon, MANIFEST_PATH, e))?;
            }
        }
        out.push(assets);
    }
    Ok(out)
}

// File reads, template compilation and compression must not block the async request/runtime
// executor. Clone only immutable source metadata; the operation's transaction stays on its owner.
async fn assemble_sources(
    entries: &[&AddonEntry],
    options: &AssemblyOptions,
) -> Result<AssembledBundle> {
    if entries.len() > MAX_ADDONS {
        return Err(Error::validation(
            "UI addon set exceeds hard admission limit",
        ));
    }
    let entries: Vec<AddonEntry> = entries.iter().map(|entry| (**entry).clone()).collect();
    let options = options.clone();
    tokio::task::spawn_blocking(move || {
        let references: Vec<&AddonEntry> = entries.iter().collect();
        let inputs = collect_entries(&references, options.runtime())?;
        assembly::assemble(&inputs, &options).map_err(assembly_error)
    })
    .await
    .map_err(|error| {
        Error::programming(format!("UI asset assembly worker failed: {error}")).with_source(error)
    })?
}

async fn admission_locks(tx: &mut Transaction) -> Result<()> {
    tx.query(
        "SELECT pg_advisory_xact_lock($1,$2)",
        &[&OPERATION_LOCK.0, &OPERATION_LOCK.1],
    )
    .await?;
    tx.query(
        "SELECT pg_advisory_xact_lock($1,$2)",
        &[&store::ASSET_LOCK.0, &store::ASSET_LOCK.1],
    )
    .await?;
    Ok(())
}

/// Assemble and publish the complete active set using the caller's lifecycle transaction.
/// `initialize` is true only while installing/upgrading web's service schema, never for startup.
/// # Errors
/// Propagates lock, source, compiler, runtime-compatibility and storage failures. The caller
/// retains commit/rollback ownership; this function never commits or uses an external authority.
pub async fn assemble_entries(
    tx: &mut Transaction,
    entries: &[&AddonEntry],
    options: &AssemblyOptions,
    initialize: bool,
) -> Result<AssembledBundle> {
    admission_locks(tx).await?;
    if initialize {
        store::initialize(tx).await?;
    }
    if let Some((_, runtime, _)) =
        store::current_identity(tx, options.language(), options.group()).await?
    {
        runtime_admission(&runtime, options.runtime())?;
    }
    let bundle = assemble_sources(entries, options).await?;
    store::publish(tx, &bundle).await?;
    Ok(bundle)
}

/// Transactional web asset service installed alongside, after package/WASM admission.
#[non_exhaustive]
pub struct AssetLifecycle {
    options: AssemblyOptions,
}
impl AssetLifecycle {
    /// Use explicit, already validated compiler/runtime/language identities.
    pub fn new(options: AssemblyOptions) -> Self {
        Self { options }
    }
}
impl LifecycleHook for AssetLifecycle {
    fn before_commit<'a>(
        &'a self,
        tx: &'a mut Transaction,
        change: LifecycleChange<'a>,
    ) -> BoxFuture<'a, erp_modules::Result<()>> {
        Box::pin(async move {
            if !change
                .active_addons()
                .iter()
                .any(|e| e.name().as_str() == "web")
            {
                return Ok(());
            }
            let delta = change.change();
            let initialize = delta
                .installed
                .iter()
                .chain(delta.upgraded)
                .any(|n| n.as_str() == "web");
            assemble_entries(tx, change.active_addons(), &self.options, initialize).await?;
            Ok(())
        })
    }
}

/// Admit a database's UI generation at startup in its own primary ReadCommitted transaction.
/// `observed` must be read from this database before the entire module load/source capture.
/// After taking lifecycle exclusion, a moved durable registry generation returns Concurrency
/// before assembly or publication. An older runtime refuses newer published state.
/// Same-epoch compiler/input changes and a
/// monotonic runtime upgrade are reassembled before serving; unchanged output is fully verified
/// from storage. This startup-only function must never run on a request or registry refresh.
/// # Errors
/// Concurrency when the observed registry generation changed; missing schema, incompatible
/// runtime, source/compiler/storage or transaction failure. Failure publishes nothing and is returned so the host can mark this database's web route unavailable.
pub async fn admit_startup(
    orm: &Orm,
    entries: &[&AddonEntry],
    options: &AssemblyOptions,
    observed: &erp_orm::Generations,
) -> Result<Option<AssembledBundle>> {
    if !entries.iter().any(|e| e.name().as_str() == "web") {
        return Ok(None);
    }
    let mut tx = orm
        .begin_opts(
            BeginOptions::new(SUPERUSER_ID)
                .isolation(Isolation::ReadCommitted)
                .on_primary(),
        )
        .await?;
    let result = async {
        admission_locks(&mut tx).await?;
        // The caller observed this generation BEFORE the entire module load/source capture.
        // Read after lifecycle exclusion under ReadCommitted so a lock wait cannot leave this
        // decision on an earlier transaction snapshot. Never publish a stale retained addon set.
        let rows = tx.query("SELECT COALESCE(sum(weight),0)::int8 FROM erp_signal_generation WHERE key='registry'", &[]).await?;
        let [row] = rows.as_slice() else {
            return Err(Error::programming("UI asset registry generation query returned an invalid row count"));
        };
        let generation: i64 = row.try_get(0).map_err(|error| Error::programming("invalid UI asset registry generation").with_source(error))?;
        let generation = u64::try_from(generation).map_err(|error| Error::programming("UI asset registry generation is negative").with_source(error))?;
        if generation != observed.get(&erp_orm::SignalKey::Registry) {
            return Err(Error::new(erp_fields::ErrorKind::Concurrency, "UI asset startup registry generation changed; reload before admission"));
        }
        let current = store::current_identity(&mut tx, options.language(), options.group()).await?;
        if let Some((_, runtime, _)) = &current {
            runtime_admission(runtime, options.runtime())?;
        }
        let bundle = assemble_sources(entries, options).await?;
        match current {
            Some((compiler, runtime, key))
                if compiler == options.compiler()
                    && runtime == options.runtime()
                    && key == bundle.key() =>
            {
                let current = store::current(
                    &mut tx,
                    options.language(),
                    options.group(),
                    options.runtime(),
                )
                .await?
                .ok_or_else(|| {
                    Error::validation(
                        "current UI asset generation disappeared during startup admission",
                    )
                })?;
                Ok(current)
            }
            Some(_) | None => {
                store::publish(&mut tx, &bundle).await?;
                Ok(bundle)
            } // Missing or changed current generation is rebuilt only by this startup transaction.
        }
    }
    .await;
    match result {
        Ok(bundle) => {
            tx.commit().await?;
            Ok(Some(bundle))
        }
        Err(error) => match tx.rollback().await {
            Ok(()) => Err(error),
            Err(rollback) => Err(Error::programming(format!(
                "UI asset startup admission failed: {error}; rollback failed: {rollback}"
            ))
            .with_source(rollback)),
        },
    }
}
