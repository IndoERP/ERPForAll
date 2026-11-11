//! Addon migration scripts.
//!
//! An addon provides migration scripts to update its schema and data when upgraded from an older
//! version to a newer version. Migrations are executed in three stages:
//! - `pre`: before the module's schema is updated (receives the operation transaction for raw SQL;
//!   the module's new models/tables do not exist yet);
//! - `post`: after the module's schema and data files are loaded (receives the operation transaction
//!   with `tx.env()` available on the updated registry containing the module's models);
//! - `end`: after all modules in the upgrade operation have loaded (receives the operation transaction
//!   with `tx.env()`).
//!
//! Migration sources:
//! - Native Rust migrations: `#[erp_sdk::migration(addon, version, stage)]` functions, registered
//!   as [`MigrationImpl`]s in [`MIGRATIONS`] (declared in `erp_data::migration`, so an addon that
//!   depends only on `erp-sdk` reaches it), or given to
//!   [`crate::system::ModuleSystemBuilder::migrations`];
//! - SQL files: files in `migrations/<version>/<stage>-*.sql` below the addon directory.
//!
//! Version folders are mapped:
//! - `0.0.0` -> `0.0.0`
//! - `x.y` -> `x.y.0`
//! - `x.y.z` -> `x.y.z`
//! - `20.0.x.y` -> `x.y.0`
//!
//! Folder filtering:
//! - non-version folders (e.g. `bogus`) log a warning and are skipped.
//!
//! Selection:
//! - `0.0.0` runs when `installed_version < target_version`;
//! - version `v` runs when `installed_version < v <= target_version`.
//!
//! Order:
//! - `pre`: `0.0.0` runs first, then remaining versions in ascending semver order;
//! - `post`: versions in ascending semver order, then `0.0.0` runs last;
//! - `end`: runs after all modules loaded, versions in ascending semver order, then `0.0.0` last;
//! - within each (version, stage), steps (native and SQL) run sorted by name lexicographically;
//!   equal names are ordered by their origin (`file:line` of a native step, the path of a SQL
//!   file), so the order never depends on the directory listing.

use crate::error::{Error, Result};
use crate::manifest::ModuleName;
use crate::migration_sql::MigrationSql;
use crate::version::{ModuleVersion, SemverVersion};
use erp_orm::Transaction;
use std::collections::HashMap;
use std::path::{Component, Path};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

pub use crate::migration_sql::MigrationSqlRefusal;

/// The native migration declarations (`erp_data::migration`, the element type of the slice that
/// `#[erp_sdk::migration]` fills), re-exported under this module's path.
pub use erp_data::migration::{MIGRATIONS, MigrationFn, MigrationImpl, MigrationStage};

/// A native migration whose addon name and version were checked when the module system was
/// built ([`validate_native`]).
#[derive(Debug)]
pub(crate) struct NativeMigration {
    pub(crate) module: ModuleName,
    pub(crate) version: SemverVersion,
    pub(crate) imp: &'static MigrationImpl,
}

/// Check every native migration once, at `ModuleSystemBuilder::build`: the addon is a valid
/// module name, the version is a migration version, and no (addon, version, stage,
/// name) is implemented twice. Versions are compared in their semver form, so `1.1` and `1.1.0`
/// are the same version.
///
/// # Errors
/// [`Error::InvalidModuleName`] for the addon, [`Error::InvalidVersion`] for the version,
/// [`Error::MigrationDuplicate`] naming both locations.
pub(crate) fn validate_native(
    migrations: impl IntoIterator<Item = &'static MigrationImpl>,
) -> Result<Vec<NativeMigration>> {
    let mut out: Vec<NativeMigration> = Vec::new();
    let mut seen: HashMap<(ModuleName, SemverVersion, MigrationStage, &'static str), usize> =
        HashMap::new();
    for imp in migrations {
        let module = ModuleName::new(imp.addon())?;
        let Some(version) = parse_migration_folder_version(imp.version()) else {
            return Err(Error::InvalidVersion {
                name: module,
                version: imp.version().to_owned(),
                message: format!(
                    "migration '{}' at {} is not for a migration version (0.0.0, x.y, x.y.z or \
                     20.0.x.y)",
                    imp.name(),
                    imp.location()
                ),
            });
        };
        let key = (module.clone(), version, imp.stage(), imp.name());
        if let Some(&first) = seen.get(&key) {
            return Err(Error::MigrationDuplicate {
                module,
                version,
                stage: imp.stage(),
                name: imp.name().to_owned(),
                first: out[first].imp.location(),
                second: imp.location(),
            });
        }
        seen.insert(key, out.len());
        out.push(NativeMigration {
            module,
            version,
            imp,
        });
    }
    Ok(out)
}

/// Parse a migration folder name or version string into a [`SemverVersion`].
///
/// Rules:
/// - `"0.0.0"` -> `SemverVersion::new(0, 0, 0)`
/// - `"x.y"` -> `SemverVersion::new(x, y, 0)`
/// - `"x.y.z"` -> `SemverVersion::new(x, y, z)`
/// - `"20.0.x.y"` -> `SemverVersion::new(x, y, 0)`
/// - non-version / invalid strings -> `None`
pub fn parse_migration_folder_version(name: &str) -> Option<SemverVersion> {
    if name == "0.0.0" {
        return Some(SemverVersion::new(0, 0, 0));
    }
    if name == "tests" {
        return None;
    }
    // `ModuleVersion::parse` names a module in its errors, which are not used here
    let Ok(dummy) = ModuleName::new("migration_version") else {
        return None;
    };
    match ModuleVersion::parse(&dummy, name) {
        Ok(ModuleVersion::Compatible { semver, .. }) => Some(semver),
        // not a migration version: the callers decide (a folder is skipped with a warning,
        Ok(ModuleVersion::Incompatible { .. }) | Err(_) => None,
    }
}

/// Whether a migration script for `step_version` should execute when upgrading an addon from
/// `installed_version` to `target_version`.
///
/// Rules:
/// - `0.0.0` runs when `installed_version < target_version`;
/// - version `v` runs when `installed_version < v <= target_version`.
pub fn should_run_migration(
    step_version: &SemverVersion,
    installed_version: &SemverVersion,
    target_version: &SemverVersion,
) -> bool {
    if step_version.is_zero() {
        installed_version < target_version
    } else {
        installed_version < step_version && step_version <= target_version
    }
}

/// Order migration versions for execution in `stage`.
///
/// Rules:
/// - In `Pre`: `0.0.0` runs FIRST, followed by normal versions in ascending semver order.
/// - In `Post` and `End`: normal versions in ascending semver order, followed by `0.0.0` LAST.
pub fn order_migration_versions(
    stage: MigrationStage,
    mut versions: Vec<SemverVersion>,
) -> Vec<SemverVersion> {
    versions.sort();
    versions.dedup();
    let zero = SemverVersion::new(0, 0, 0);
    let has_zero = versions.contains(&zero);
    if has_zero {
        versions.retain(|v| *v != zero);
        match stage {
            MigrationStage::Pre => versions.insert(0, zero),
            MigrationStage::Post | MigrationStage::End => versions.push(zero),
        }
    }
    versions
}

/// The source of a migration step (native Rust function or SQL file content).
#[non_exhaustive]
pub(crate) enum MigrationStepSource {
    /// Native Rust migration function.
    Native(MigrationFn),
    /// SQL text checked for transaction control, run with `Transaction::batch`.
    Sql(MigrationSql),
}

impl std::fmt::Debug for MigrationStepSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Native(_) => f.write_str("Native(<fn>)"),
            Self::Sql(sql) => f.debug_tuple("Sql").field(&sql.as_str()).finish(),
        }
    }
}

/// One selected migration step of an upgrade.
#[derive(Debug)]
pub(crate) struct MigrationStep {
    pub(crate) module: ModuleName,
    pub(crate) version: SemverVersion,
    pub(crate) stage: MigrationStage,
    /// Orders the step within its version and stage.
    pub(crate) name: String,
    /// Where the step comes from: `file:line` of a native migration, the addon-relative path of
    /// a SQL file. Breaks ties between equal names and names the step in errors.
    pub(crate) origin: String,
    pub(crate) source: MigrationStepSource,
}

impl MigrationStep {
    /// The step of a validated native migration.
    pub(crate) fn native(native: &NativeMigration) -> Self {
        Self {
            module: native.module.clone(),
            version: native.version,
            stage: native.imp.stage(),
            name: native.imp.name().to_owned(),
            origin: native.imp.location(),
            source: MigrationStepSource::Native(native.imp.run()),
        }
    }

    /// Run this step in `tx`. A SQL script is sent only while the session reads strings the way
    /// [`MigrationSql::check`] did (`standard_conforming_strings` on).
    ///
    /// # Errors
    /// [`Error::MigrationSqlRefused`] ([`MigrationSqlRefusal::NonStandardStrings`]) before a SQL
    /// script is sent; [`Error::MigrationFailed`] with the step's module, version, stage and
    /// name, and the function's or the SQL statement's error as its source.
    pub(crate) async fn run(
        &self,
        tx: &mut Transaction,
        installed_version: &SemverVersion,
    ) -> Result<()> {
        let result = match &self.source {
            MigrationStepSource::Native(func) => func(tx, installed_version).await,
            MigrationStepSource::Sql(sql) => match standard_strings(tx).await {
                Ok(true) => tx.batch(sql.as_str()).await,
                Ok(false) => {
                    return Err(Error::MigrationSqlRefused {
                        module: self.module.clone(),
                        path: self.origin.clone(),
                        refusal: MigrationSqlRefusal::NonStandardStrings,
                    });
                }
                Err(e) => Err(e),
            },
        };
        result.map_err(|source| Error::MigrationFailed {
            module: self.module.clone(),
            version: self.version,
            stage: self.stage,
            name: self.name.clone(),
            source: Box::new(source),
        })
    }
}

/// Whether the session's `standard_conforming_strings` is on.
async fn standard_strings(tx: &mut Transaction) -> erp_fields::Result<bool> {
    let rows = tx
        .query("SELECT current_setting('standard_conforming_strings')", &[])
        .await?;
    let value: String = rows
        .first()
        .ok_or_else(|| erp_fields::Error::db("SELECT current_setting returned no row"))?
        .try_get(0)
        .map_err(|e| {
            erp_fields::Error::database(
                erp_fields::ErrorKind::Database,
                None,
                "unexpected row of SELECT current_setting",
                e,
            )
        })?;
    Ok(value == "on")
}

/// What the SQL migrations of one operation may select and read, all its modules together: at
/// most `max_files` selected script files (and at most `max_files` version folders in one
/// module's `migrations/`), and their bytes charged to the operation's data-byte counter (the
/// one data files use, [`crate::system::ModuleSystemBuilder::max_operation_data_bytes`]).
/// Clones share the counters.
#[derive(Debug, Clone)]
pub(crate) struct MigrationBudget {
    files: Arc<AtomicUsize>,
    max_files: usize,
    bytes: Arc<AtomicU64>,
    max_bytes: u64,
}

impl MigrationBudget {
    pub(crate) fn new(max_files: usize, bytes: Arc<AtomicU64>, max_bytes: u64) -> Self {
        Self {
            files: Arc::new(AtomicUsize::new(0)),
            max_files,
            bytes,
            max_bytes,
        }
    }

    /// Count one selected script file before it is read.
    ///
    /// # Errors
    /// [`Error::MigrationFilesTooMany`] when it is one more than `max_files`.
    fn charge_file(&self, module: &ModuleName, path: &str) -> Result<()> {
        // one atomic step, so reads that run side by side cannot pass the limit together
        self.files
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| {
                n.checked_add(1).filter(|n| *n <= self.max_files)
            })
            .map(drop)
            .map_err(|_| Error::MigrationFilesTooMany {
                module: module.clone(),
                path: path.to_owned(),
                limit: self.max_files,
            })
    }

    /// Charge the `len` bytes of a script file that was read.
    ///
    /// # Errors
    /// [`Error::OperationDataTooLarge`] when the operation's total passes `max_bytes`.
    fn charge_bytes(&self, module: &ModuleName, path: &str, len: usize) -> Result<()> {
        crate::system::charge_data_bytes(&self.bytes, self.max_bytes, module, path, len)
    }
}

fn io_error(path: &Path, source: std::io::Error) -> Error {
    Error::Io {
        path: path.to_path_buf(),
        source,
    }
}

fn invalid_input(path: &Path, what: &str) -> Error {
    io_error(
        path,
        std::io::Error::new(std::io::ErrorKind::InvalidInput, what.to_owned()),
    )
}

/// Read a SQL migration file with all safe-read bounds and checks: no symbolic link on the path,
/// a regular file without other hard links, at most `limit` bytes, UTF-8 text.
fn read_migration_sql_file(
    dir: &Path,
    module: &ModuleName,
    rel_path: &Path,
    limit: u64,
) -> Result<String> {
    let symlink = || Error::DataFileSymlink {
        module: module.clone(),
        path: rel_path.to_string_lossy().to_string(),
    };
    let mut full = dir.to_path_buf();
    for component in rel_path.components() {
        match component {
            Component::Normal(part) => full.push(part),
            Component::CurDir => continue,
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(Error::InvariantViolation {
                    message: format!(
                        "migration file path '{}' of module {module} is not relative to the addon",
                        rel_path.display()
                    ),
                });
            }
        }
        let meta = std::fs::symlink_metadata(&full).map_err(|e| io_error(&full, e))?;
        if meta.file_type().is_symlink() {
            return Err(symlink());
        }
    }
    let root = std::fs::canonicalize(dir).map_err(|e| io_error(dir, e))?;
    let resolved = std::fs::canonicalize(&full).map_err(|e| io_error(&full, e))?;
    if !resolved.starts_with(&root) {
        return Err(symlink());
    }
    let meta = std::fs::symlink_metadata(&resolved).map_err(|e| io_error(&full, e))?;
    if !meta.is_file() {
        return Err(invalid_input(&full, "not a regular file"));
    }
    let file = crate::safe_read::open_nofollow(&resolved).map_err(|e| io_error(&full, e))?;
    let meta = file.metadata().map_err(|e| io_error(&full, e))?;
    if !meta.is_file() {
        return Err(invalid_input(&full, "not a regular file"));
    }
    if let Some(links) = crate::safe_read::extra_hard_links(&meta) {
        return Err(Error::DataFileHardLinked {
            module: module.clone(),
            path: rel_path.to_string_lossy().to_string(),
            links,
        });
    }
    let too_large = || Error::DataFileTooLarge {
        module: module.clone(),
        path: rel_path.to_string_lossy().to_string(),
        limit,
    };
    if meta.len() > limit {
        return Err(too_large());
    }
    let Some(bytes) = crate::safe_read::read_capped(file, limit).map_err(|e| io_error(&full, e))?
    else {
        return Err(too_large());
    };
    String::from_utf8(bytes).map_err(|e| Error::Io {
        path: full,
        source: std::io::Error::new(std::io::ErrorKind::InvalidData, e),
    })
}

/// One version folder below `migrations/`.
#[derive(Debug)]
pub(crate) struct VersionFolder {
    pub(crate) version: SemverVersion,
    /// The folder's name as written (`1.1`, `20.0.1.1`).
    pub(crate) name: String,
}

/// The version folders of an addon's `migrations/` directory, without reading any file in them.
///
/// `tests` and folders starting with `.` are skipped; a folder whose name is not a migration
/// version logs "Invalid version for upgrade script" and is skipped (
/// ).
///
/// # Errors
/// [`Error::DataFileSymlink`] for `migrations` or a version folder that is a symbolic link;
/// [`Error::MigrationFoldersTooMany`] for more than `max_folders` version folders;
/// [`Error::Io`] when `migrations` is not a directory, a name is not UTF-8, or reading fails.
pub(crate) fn migration_version_folders(
    dir: &Path,
    module: &ModuleName,
    max_folders: usize,
) -> Result<Vec<VersionFolder>> {
    let migrations_dir = dir.join("migrations");
    let meta = match std::fs::symlink_metadata(&migrations_dir) {
        Ok(meta) => meta,
        // an addon without migrations
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(io_error(&migrations_dir, e)),
    };
    if meta.file_type().is_symlink() {
        return Err(Error::DataFileSymlink {
            module: module.clone(),
            path: "migrations".to_owned(),
        });
    }
    if !meta.is_dir() {
        return Err(invalid_input(&migrations_dir, "not a directory"));
    }
    let entries = std::fs::read_dir(&migrations_dir).map_err(|e| io_error(&migrations_dir, e))?;
    let mut folders = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|e| io_error(&migrations_dir, e))?;
        let path = entry.path();
        let file_type = entry.file_type().map_err(|e| io_error(&path, e))?;
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            return Err(Error::Io {
                path,
                source: std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "migration folder name is not UTF-8",
                ),
            });
        };
        if file_type.is_symlink() {
            return Err(Error::DataFileSymlink {
                module: module.clone(),
                path: format!("migrations/{name}"),
            });
        }
        if !file_type.is_dir() || name.starts_with('.') || name == "tests" {
            continue;
        }
        let Some(version) = parse_migration_folder_version(&name) else {
            tracing::warn!("Invalid version for upgrade script: {}", path.display());
            continue;
        };
        if folders.len() == max_folders {
            return Err(Error::MigrationFoldersTooMany {
                module: module.clone(),
                limit: max_folders,
            });
        }
        folders.push(VersionFolder { version, name });
    }
    Ok(folders)
}

/// The SQL steps of one version folder (`migrations/<folder>/<stage>-*.sql`), each file counted
/// against `budget` before it is read, read with [`read_migration_sql_file`], its bytes charged
/// to `budget` and its text checked for transaction control ([`MigrationSql::check`]).
///
/// # Errors
/// [`Error::MigrationFilesTooMany`] or [`Error::OperationDataTooLarge`] past the budget;
/// [`Error::DataFileSymlink`], [`Error::DataFileHardLinked`], [`Error::DataFileTooLarge`] or
/// [`Error::Io`] (not UTF-8, unreadable) for a script file; [`Error::MigrationSqlRefused`] for
/// a script with transaction control.
pub(crate) fn read_version_folder(
    dir: &Path,
    module: &ModuleName,
    folder: &VersionFolder,
    limit: u64,
    budget: &MigrationBudget,
) -> Result<Vec<MigrationStep>> {
    let rel_dir = Path::new("migrations").join(&folder.name);
    let path = dir.join(&rel_dir);
    let entries = std::fs::read_dir(&path).map_err(|e| io_error(&path, e))?;
    let mut steps = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|e| io_error(&path, e))?;
        let file_path = entry.path();
        let file_type = entry.file_type().map_err(|e| io_error(&file_path, e))?;
        let Some(file_name) = entry.file_name().to_str().map(str::to_owned) else {
            return Err(Error::Io {
                path: file_path,
                source: std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "migration file name is not UTF-8",
                ),
            });
        };
        let rel_path = rel_dir.join(&file_name);
        if file_type.is_symlink() {
            return Err(Error::DataFileSymlink {
                module: module.clone(),
                path: rel_path.to_string_lossy().to_string(),
            });
        }
        if !file_type.is_file() {
            continue;
        }
        let Some(step_name) = file_name.strip_suffix(".sql") else {
            continue;
        };
        let stage = if step_name.starts_with("pre-") {
            MigrationStage::Pre
        } else if step_name.starts_with("post-") {
            MigrationStage::Post
        } else if step_name.starts_with("end-") {
            MigrationStage::End
        } else {
            continue;
        };
        let origin = rel_path.to_string_lossy().to_string();
        budget.charge_file(module, &origin)?;
        let sql = read_migration_sql_file(dir, module, &rel_path, limit)?;
        budget.charge_bytes(module, &origin, sql.len())?;
        let sql = MigrationSql::check(sql).map_err(|refusal| Error::MigrationSqlRefused {
            module: module.clone(),
            path: origin.clone(),
            refusal,
        })?;
        steps.push(MigrationStep {
            module: module.clone(),
            version: folder.version,
            stage,
            name: step_name.to_owned(),
            origin,
            source: MigrationStepSource::Sql(sql),
        });
    }
    Ok(steps)
}

/// Version inventory from the immutable lifecycle snapshot, including empty version folders.
pub(crate) fn snapshot_version_folders(
    snapshot: &crate::artifact_identity::ArtifactSnapshot,
    module: &ModuleName,
    max_folders: usize,
) -> Result<Vec<VersionFolder>> {
    if snapshot.files().contains_key("migrations") {
        return Err(invalid_input(
            &snapshot.root_path().join("migrations"),
            "not a directory",
        ));
    }
    let mut folders = Vec::new();
    for path in snapshot.directories() {
        let Some(name) = path.strip_prefix("migrations/") else {
            continue;
        };
        if name.contains('/') || name.starts_with('.') || name == "tests" {
            continue;
        }
        let Some(version) = parse_migration_folder_version(name) else {
            tracing::warn!("Invalid version for upgrade script: {path}");
            continue;
        };
        if folders.len() == max_folders {
            return Err(Error::MigrationFoldersTooMany {
                module: module.clone(),
                limit: max_folders,
            });
        }
        folders.push(VersionFolder {
            version,
            name: name.to_owned(),
        });
    }
    Ok(folders)
}

/// Stage SQL from exactly the bytes admitted and hashed for this lifecycle operation.
pub(crate) fn read_snapshot_version_folder(
    snapshot: &crate::artifact_identity::ArtifactSnapshot,
    module: &ModuleName,
    folder: &VersionFolder,
    limit: u64,
    budget: &MigrationBudget,
) -> Result<Vec<MigrationStep>> {
    let prefix = format!("migrations/{}/", folder.name);
    let mut steps = Vec::new();
    for (path, bytes) in snapshot.files() {
        let Some(file_name) = path.strip_prefix(&prefix) else {
            continue;
        };
        if file_name.contains('/') {
            continue;
        }
        let Some(step_name) = file_name.strip_suffix(".sql") else {
            continue;
        };
        let stage = if step_name.starts_with("pre-") {
            MigrationStage::Pre
        } else if step_name.starts_with("post-") {
            MigrationStage::Post
        } else if step_name.starts_with("end-") {
            MigrationStage::End
        } else {
            continue;
        };
        budget.charge_file(module, path)?;
        if bytes.len() as u64 > limit {
            return Err(Error::DataFileTooLarge {
                module: module.clone(),
                path: path.clone(),
                limit,
            });
        }
        let sql = std::str::from_utf8(bytes).map_err(|error| {
            io_error(
                &snapshot.root_path().join(path),
                std::io::Error::new(std::io::ErrorKind::InvalidData, error),
            )
        })?;
        budget.charge_bytes(module, path, sql.len())?;
        let sql =
            MigrationSql::check(sql.to_owned()).map_err(|refusal| Error::MigrationSqlRefused {
                module: module.clone(),
                path: path.clone(),
                refusal,
            })?;
        steps.push(MigrationStep {
            module: module.clone(),
            version: folder.version,
            stage,
            name: step_name.to_owned(),
            origin: path.clone(),
            source: MigrationStepSource::Sql(sql),
        });
    }
    Ok(steps)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic, reason = "tests")]
mod tests {
    use super::*;
    use std::fs;

    fn module() -> ModuleName {
        ModuleName::new("x").unwrap()
    }

    /// An addon folder `x` with `migrations/1.1/<name>` holding `content`.
    fn addon_with(name: &str, content: &[u8]) -> (tempfile::TempDir, std::path::PathBuf) {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("x");
        fs::create_dir_all(dir.join("migrations/1.1")).unwrap();
        fs::write(dir.join("migrations/1.1").join(name), content).unwrap();
        (root, dir)
    }

    /// A budget no test here reaches.
    fn budget() -> MigrationBudget {
        MigrationBudget::new(16, Arc::new(AtomicU64::new(0)), 1 << 20)
    }

    fn folder_1_1() -> VersionFolder {
        VersionFolder {
            version: SemverVersion::new(1, 1, 0),
            name: "1.1".to_owned(),
        }
    }

    #[test]
    fn reads_a_regular_script() {
        let (_root, dir) = addon_with("post-a.sql", b"SELECT 1;");
        let steps = read_version_folder(&dir, &module(), &folder_1_1(), 1024, &budget()).unwrap();
        assert_eq!(steps.len(), 1);
        let step = &steps[0];
        assert_eq!(step.name, "post-a");
        assert_eq!(step.stage, MigrationStage::Post);
        assert_eq!(step.version, SemverVersion::new(1, 1, 0));
        assert_eq!(step.origin, "migrations/1.1/post-a.sql");
        match &step.source {
            MigrationStepSource::Sql(sql) => assert_eq!(sql.as_str(), "SELECT 1;"),
            other => panic!("expected Sql, got {other:?}"),
        }
    }

    /// Only the last `.sql` is the extension (`strip_suffix`, not `trim_end_matches`).
    #[test]
    fn step_name_strips_one_sql_suffix() {
        let (_root, dir) = addon_with("pre-a.sql.sql", b"SELECT 1;");
        let steps = read_version_folder(&dir, &module(), &folder_1_1(), 1024, &budget()).unwrap();
        assert_eq!(steps.len(), 1);
        assert_eq!(steps[0].name, "pre-a.sql");
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_script_is_refused() {
        let (root, dir) = addon_with("other.txt", b"SELECT 1;");
        let outside = root.path().join("outside.sql");
        fs::write(&outside, b"SELECT 1;").unwrap();
        std::os::unix::fs::symlink(&outside, dir.join("migrations/1.1/post-a.sql")).unwrap();
        let err = read_version_folder(&dir, &module(), &folder_1_1(), 1024, &budget()).unwrap_err();
        let message = err.to_string();
        match err {
            Error::DataFileSymlink { module: m, path } => {
                assert_eq!(m, module());
                assert_eq!(path, "migrations/1.1/post-a.sql");
                assert_eq!(
                    message,
                    "Module \"x\": data file 'migrations/1.1/post-a.sql' is a symbolic link or \
                     lies under one; data files must be regular files inside the addon"
                );
            }
            other => panic!("expected DataFileSymlink, got {other:?}"),
        }
        // the reader itself refuses it too, whoever calls it
        let err = read_migration_sql_file(
            &dir,
            &module(),
            Path::new("migrations/1.1/post-a.sql"),
            1024,
        )
        .unwrap_err();
        assert!(matches!(err, Error::DataFileSymlink { .. }), "{err:?}");
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_version_folder_is_refused() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("x");
        fs::create_dir_all(dir.join("migrations")).unwrap();
        let elsewhere = root.path().join("elsewhere");
        fs::create_dir_all(&elsewhere).unwrap();
        std::os::unix::fs::symlink(&elsewhere, dir.join("migrations/1.1")).unwrap();
        let err = migration_version_folders(&dir, &module(), 16).unwrap_err();
        match err {
            Error::DataFileSymlink { module: m, path } => {
                assert_eq!(m, module());
                assert_eq!(path, "migrations/1.1");
            }
            other => panic!("expected DataFileSymlink, got {other:?}"),
        }
    }

    #[cfg(unix)]
    #[test]
    fn hard_linked_script_is_refused() {
        let (root, dir) = addon_with("post-a.sql", b"SELECT 1;");
        fs::hard_link(
            dir.join("migrations/1.1/post-a.sql"),
            root.path().join("second-name.sql"),
        )
        .unwrap();
        let err = read_version_folder(&dir, &module(), &folder_1_1(), 1024, &budget()).unwrap_err();
        let message = err.to_string();
        match err {
            Error::DataFileHardLinked {
                module: m,
                path,
                links,
            } => {
                assert_eq!(m, module());
                assert_eq!(path, "migrations/1.1/post-a.sql");
                assert_eq!(links, 2);
                assert_eq!(
                    message,
                    "Module \"x\": data file 'migrations/1.1/post-a.sql' has 2 hard links; data \
                     files must be regular files that belong to the addon only"
                );
            }
            other => panic!("expected DataFileHardLinked, got {other:?}"),
        }
    }

    #[test]
    fn script_over_the_limit_is_refused() {
        let (_root, dir) = addon_with("post-a.sql", b"SELECT 12;");
        // exactly at the limit is read
        assert_eq!(
            read_version_folder(&dir, &module(), &folder_1_1(), 10, &budget())
                .unwrap()
                .len(),
            1
        );
        let err = read_version_folder(&dir, &module(), &folder_1_1(), 9, &budget()).unwrap_err();
        let message = err.to_string();
        match err {
            Error::DataFileTooLarge {
                module: m,
                path,
                limit,
            } => {
                assert_eq!(m, module());
                assert_eq!(path, "migrations/1.1/post-a.sql");
                assert_eq!(limit, 9);
                assert_eq!(
                    message,
                    "Module \"x\": data file 'migrations/1.1/post-a.sql' is larger than 9 bytes"
                );
            }
            other => panic!("expected DataFileTooLarge, got {other:?}"),
        }
    }

    #[test]
    fn script_that_is_not_utf8_is_refused() {
        let (_root, dir) = addon_with("post-a.sql", b"SELECT '\xff';");
        let err = read_version_folder(&dir, &module(), &folder_1_1(), 1024, &budget()).unwrap_err();
        match err {
            Error::Io { path, source } => {
                assert_eq!(path, dir.join("migrations/1.1/post-a.sql"));
                assert_eq!(source.kind(), std::io::ErrorKind::InvalidData);
                assert_eq!(
                    source.to_string(),
                    "invalid utf-8 sequence of 1 bytes from index 8"
                );
            }
            other => panic!("expected Io, got {other:?}"),
        }
    }

    #[test]
    fn version_folders_skip_tests_hidden_and_non_versions() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("x");
        for folder in ["1.1", "20.0.1.2", "0.0.0", "tests", ".hidden", "bogus"] {
            fs::create_dir_all(dir.join("migrations").join(folder)).unwrap();
        }
        fs::write(dir.join("migrations/README"), b"not a folder").unwrap();
        let mut folders: Vec<(SemverVersion, String)> =
            migration_version_folders(&dir, &module(), 16)
                .unwrap()
                .into_iter()
                .map(|f| (f.version, f.name))
                .collect();
        folders.sort();
        assert_eq!(
            folders,
            vec![
                (SemverVersion::new(0, 0, 0), "0.0.0".to_owned()),
                (SemverVersion::new(1, 1, 0), "1.1".to_owned()),
                (SemverVersion::new(1, 2, 0), "20.0.1.2".to_owned()),
            ]
        );
    }

    #[test]
    fn no_migrations_folder_means_no_versions() {
        let root = tempfile::tempdir().unwrap();
        assert!(
            migration_version_folders(root.path(), &module(), 16)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn migrations_that_is_a_file_is_refused() {
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join("migrations"), b"").unwrap();
        let err = migration_version_folders(root.path(), &module(), 16).unwrap_err();
        match err {
            Error::Io { path, source } => {
                assert_eq!(path, root.path().join("migrations"));
                assert_eq!(source.kind(), std::io::ErrorKind::InvalidInput);
                assert_eq!(source.to_string(), "not a directory");
            }
            other => panic!("expected Io, got {other:?}"),
        }
    }

    /// A script with transaction control is refused when it is read, naming the module, the
    /// file and the statement.
    #[test]
    fn script_with_transaction_control_is_refused() {
        let (_root, dir) = addon_with("pre-a.sql", b"SELECT 1;\nROLLBACK;\n");
        let err = read_version_folder(&dir, &module(), &folder_1_1(), 1024, &budget()).unwrap_err();
        let message = err.to_string();
        match err {
            Error::MigrationSqlRefused {
                module: m,
                path,
                refusal,
            } => {
                assert_eq!(m, module());
                assert_eq!(path, "migrations/1.1/pre-a.sql");
                assert_eq!(
                    refusal,
                    MigrationSqlRefusal::TransactionControl {
                        statement: 2,
                        keyword: "ROLLBACK".to_owned(),
                    }
                );
                assert_eq!(
                    message,
                    "Module \"x\": SQL migration 'migrations/1.1/pre-a.sql' is refused: \
                     statement 2 is transaction control (ROLLBACK); a migration runs inside the \
                     upgrade's transaction and must not begin, commit or roll back one"
                );
            }
            other => panic!("expected MigrationSqlRefused, got {other:?}"),
        }
    }

    /// The file count is charged before the file is read: a file past the count is refused
    /// with the count error, not with what reading it would find (here: not UTF-8). Files that
    /// are not scripts are not counted.
    #[test]
    fn file_count_is_charged_before_the_read() {
        let (_root, dir) = addon_with("post-a.sql", b"SELECT '\xff';");
        fs::write(dir.join("migrations/1.1/notes.txt"), b"x").unwrap();
        fs::write(dir.join("migrations/1.1/other.sql"), b"x").unwrap();
        let spent = MigrationBudget::new(1, Arc::new(AtomicU64::new(0)), 1 << 20);
        spent.charge_file(&module(), "elsewhere").unwrap();
        let err = read_version_folder(&dir, &module(), &folder_1_1(), 1024, &spent).unwrap_err();
        match err {
            Error::MigrationFilesTooMany {
                module: m,
                path,
                limit,
            } => {
                assert_eq!(m, module());
                assert_eq!(path, "migrations/1.1/post-a.sql");
                assert_eq!(limit, 1);
            }
            other => panic!("expected MigrationFilesTooMany, got {other:?}"),
        }
    }

    /// Clones of a budget share its counters; the bytes are charged to the shared counter
    /// up to exactly the limit.
    #[test]
    fn budget_clones_share_the_counters() {
        let bytes = Arc::new(AtomicU64::new(0));
        let a = MigrationBudget::new(2, Arc::clone(&bytes), 10);
        let b = a.clone();
        a.charge_file(&module(), "a").unwrap();
        b.charge_file(&module(), "b").unwrap();
        assert!(matches!(
            a.charge_file(&module(), "c"),
            Err(Error::MigrationFilesTooMany { limit: 2, .. })
        ));
        a.charge_bytes(&module(), "a", 6).unwrap();
        b.charge_bytes(&module(), "b", 4).unwrap();
        assert_eq!(bytes.load(Ordering::Relaxed), 10);
        assert!(matches!(
            a.charge_bytes(&module(), "c", 1),
            Err(Error::OperationDataTooLarge { limit: 10, .. })
        ));
    }

    /// Exactly `max_folders` version folders are listed; one more is refused. Skipped folders
    /// (`tests`, hidden, not a version) and plain files do not count.
    #[test]
    fn version_folder_count_is_bounded() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("x");
        for folder in ["1.1", "1.2", "tests", ".hidden", "bogus"] {
            fs::create_dir_all(dir.join("migrations").join(folder)).unwrap();
        }
        fs::write(dir.join("migrations/README"), b"x").unwrap();
        assert_eq!(
            migration_version_folders(&dir, &module(), 2).unwrap().len(),
            2
        );
        let err = migration_version_folders(&dir, &module(), 1).unwrap_err();
        let message = err.to_string();
        match err {
            Error::MigrationFoldersTooMany { module: m, limit } => {
                assert_eq!(m, module());
                assert_eq!(limit, 1);
                assert_eq!(
                    message,
                    "Module \"x\": migrations/ holds more than 1 version folders \
                     (ModuleSystemBuilder::max_migration_files)"
                );
            }
            other => panic!("expected MigrationFoldersTooMany, got {other:?}"),
        }
    }
}
