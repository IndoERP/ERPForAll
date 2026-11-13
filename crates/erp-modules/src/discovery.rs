use crate::error::{Error, Result};
use crate::manifest::{Manifest, ModuleName};
use indexmap::IndexMap;
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Addon source origin with metadata.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
#[non_exhaustive]
pub enum AddonSourceKind {
    Embedded,
    Package { sha256: Option<String> },
    DevFolder(PathBuf),
}

impl std::fmt::Display for AddonSourceKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AddonSourceKind::Embedded => write!(f, "embedded"),
            AddonSourceKind::Package { .. } => write!(f, "package"),
            AddonSourceKind::DevFolder(path) => write!(f, "dev-folder:{}", path.display()),
        }
    }
}

/// File bytes retained from a verified package, indexed by archive path. Loader data and
/// client assets use separate maps; only explicitly declared loader files enter the data map.
pub type PackageFiles = BTreeMap<String, Vec<u8>>;

/// What the module system needs of a package to load its data files: the files, and the models its
/// `describe()` declares (new models) and extends (existing ones), which bound what the data files
/// may reshape. Client assets are retained separately for transactional asset
/// admission by the host; the data loader never reads that map.
#[derive(Debug, Clone, Default)]
pub struct PackageData {
    /// The `data` files by archive path.
    pub files: PackageFiles,
    /// All bounded `static/` client files from the verified archive, plus `i18n/` catalogues
    /// when a client manifest exists. Kept separately from loader data: absence of a client
    /// manifest is meaningful only after the complete verified client map has been retained.
    pub client_files: PackageFiles,
    /// Models the package declares.
    pub declared: BTreeSet<String>,
    /// Models the package extends in place (declared by another addon).
    pub extended: BTreeSet<String>,
}

/// An entry in the addon index, describing a discovered addon.
#[derive(Debug, Clone)]
pub struct AddonEntry {
    manifest: Arc<Manifest>,
    source_kind: AddonSourceKind,
    source_location: String,
    package_data: Option<Arc<PackageData>>,
    artifact_snapshot: Option<Arc<crate::artifact_identity::ArtifactSnapshot>>,
}

impl AddonEntry {
    pub fn new(
        manifest: Arc<Manifest>,
        source_kind: AddonSourceKind,
        source_location: impl Into<String>,
    ) -> Self {
        Self {
            manifest,
            source_kind,
            source_location: source_location.into(),
            package_data: None,
            artifact_snapshot: None,
        }
    }

    pub(crate) fn attach_artifact_snapshot(
        &mut self,
        snapshot: Arc<crate::artifact_identity::ArtifactSnapshot>,
    ) -> Result<()> {
        let bytes =
            snapshot
                .files()
                .get("manifest.toml")
                .ok_or_else(|| Error::ManifestNotFound {
                    name: self.name().clone(),
                })?;
        let text = std::str::from_utf8(bytes).map_err(|error| Error::Io {
            path: std::path::PathBuf::from("manifest.toml"),
            source: std::io::Error::new(std::io::ErrorKind::InvalidData, error),
        })?;
        // Discovery caches are not authority: execute the manifest parsed from admitted bytes.
        self.manifest = Arc::new(Manifest::from_toml(self.name().as_str(), text)?);
        self.artifact_snapshot = Some(snapshot);
        Ok(())
    }

    /// Immutable filesystem artifact bytes captured by module source admission.
    ///
    /// A module operation/load outcome retains these exact bytes for later host consumers;
    /// reading them never reopens the live source tree. This is source provenance, not a grant
    /// to expose arbitrary files. `None` means this entry has no admitted filesystem snapshot
    /// (for example a separately verified package or an entry constructed outside admission).
    /// Package client bytes remain in [`PackageData::client_files`].
    pub fn admitted_files(&self) -> Option<&BTreeMap<String, Vec<u8>>> {
        self.artifact_snapshot
            .as_ref()
            .map(|snapshot| snapshot.files())
    }

    pub(crate) fn artifact_snapshot(
        &self,
    ) -> Option<&Arc<crate::artifact_identity::ArtifactSnapshot>> {
        self.artifact_snapshot.as_ref()
    }

    /// The same entry with the data files of a package addon.
    #[must_use]
    pub fn with_package_data(mut self, data: Arc<PackageData>) -> Self {
        self.package_data = Some(data);
        self
    }

    /// The data files a package addon carries; `None` for any other source and for a package
    /// registered without files.
    pub fn package_data(&self) -> Option<&Arc<PackageData>> {
        self.package_data.as_ref()
    }

    pub fn name(&self) -> &ModuleName {
        self.manifest.name()
    }

    pub fn manifest(&self) -> &Arc<Manifest> {
        &self.manifest
    }

    pub fn source_kind(&self) -> &AddonSourceKind {
        &self.source_kind
    }

    pub fn source_location(&self) -> &str {
        &self.source_location
    }
}

/// Discovers and caches addon manifests across multiple sources according to.
///
/// Precedence order:
/// DevFolder (only when dev_mode is true) > Package > Embedded.
///
/// Rules:
/// 1. Without `--dev`, dev folders are not scanned at all.
/// 2. Duplicate addon names across sources (e.g. package vs embedded) produce an error.
/// 3. In `--dev` mode, a dev folder may override a package or embedded addon.
/// 4. Two entries with the same name within the same source produce an error at registration.
/// 5. Broken manifests, permission errors, nonexistent configured paths, or invalid folder names
///    are collected and reported.
#[derive(Debug, Default)]
pub struct AddonIndex {
    dev_mode: bool,
    dev_paths: Vec<PathBuf>,
    package_addons: HashMap<ModuleName, (Arc<Manifest>, String)>,
    /// The SHA-256 of each package file, recorded when it was read (the node digest needs it).
    package_sha256: HashMap<ModuleName, erp_orm::node::Digest>,
    /// The data files of each package registered with [`AddonIndex::add_package_with_data`].
    package_data: HashMap<ModuleName, Arc<PackageData>>,
    embedded_addons: HashMap<ModuleName, (Arc<Manifest>, String)>,
    cached_manifests: HashMap<ModuleName, Arc<Manifest>>,
}

impl AddonIndex {
    pub fn new(dev_mode: bool) -> Self {
        Self {
            dev_mode,
            dev_paths: Vec::new(),
            package_addons: HashMap::new(),
            package_sha256: HashMap::new(),
            package_data: HashMap::new(),
            embedded_addons: HashMap::new(),
            cached_manifests: HashMap::new(),
        }
    }

    pub fn dev_mode(&self) -> bool {
        self.dev_mode
    }

    /// Add a developer directory (either a single addon folder or an addons path containing addon subdirectories).
    pub fn add_dev_path(&mut self, path: impl Into<PathBuf>) {
        self.dev_paths.push(path.into());
        self.cached_manifests.clear();
    }

    /// Forget every cached manifest (`erp dev` reloads after an edit on disk). Registering a source
    /// (`add_*`) and [`AddonIndex::discover_all`] clear the cache themselves.
    pub fn invalidate_cache(&mut self) {
        self.cached_manifests.clear();
    }

    /// Register a package addon (from .erpaddon).
    ///
    /// Rejects duplicate registrations within the package source or collisions with embedded addons.
    pub fn add_package(&mut self, manifest: Manifest, location: impl Into<String>) -> Result<()> {
        let name = manifest.name().clone();
        let loc = location.into();

        if let Some((_, existing_loc)) = self.package_addons.get(&name) {
            return Err(Error::DuplicateAddon {
                name,
                source_a: existing_loc.clone(),
                source_b: loc,
            });
        }
        if let Some((_, existing_loc)) = self.embedded_addons.get(&name) {
            return Err(Error::DuplicateAddon {
                name,
                source_a: existing_loc.clone(),
                source_b: loc,
            });
        }

        self.package_addons.insert(name, (Arc::new(manifest), loc));
        self.cached_manifests.clear();
        Ok(())
    }

    /// Register a package addon with the SHA-256 of the package file as it was read: the node
    /// registry's addon digest ([`AddonIndex::digest`]) compares packages by it.
    ///
    /// # Errors
    /// As [`AddonIndex::add_package`].
    pub fn add_package_with_sha256(
        &mut self,
        manifest: Manifest,
        location: impl Into<String>,
        sha256: erp_orm::node::Digest,
    ) -> Result<()> {
        let name = manifest.name().clone();
        self.add_package(manifest, location)?;
        self.package_sha256.insert(name, sha256);
        Ok(())
    }

    /// As [`AddonIndex::add_package_with_sha256`], with the data files the manifest lists (read
    /// from the verified archive): the module system reads a package's `data` files from here.
    ///
    /// # Errors
    /// As [`AddonIndex::add_package`].
    pub fn add_package_with_data(
        &mut self,
        manifest: Manifest,
        location: impl Into<String>,
        sha256: erp_orm::node::Digest,
        data: Arc<PackageData>,
    ) -> Result<()> {
        let name = manifest.name().clone();
        self.add_package_with_sha256(manifest, location, sha256)?;
        self.package_data.insert(name, data);
        Ok(())
    }

    /// Forget a package addon (a runtime uninstall, or the undo of a refused install). Returns
    /// whether it was registered.
    pub fn remove_package(&mut self, name: &ModuleName) -> bool {
        let found = self.package_addons.remove(name).is_some();
        self.package_sha256.remove(name);
        self.package_data.remove(name);
        self.cached_manifests.clear();
        found
    }

    /// The recorded SHA-256 of a package addon.
    pub(crate) fn package_sha256(&self, name: &ModuleName) -> Option<erp_orm::node::Digest> {
        self.package_sha256.get(name).copied()
    }

    /// Register an embedded addon.
    ///
    /// Rejects duplicate registrations within embedded addons or collisions with package addons.
    pub fn add_embedded(&mut self, manifest: Manifest, location: impl Into<String>) -> Result<()> {
        let name = manifest.name().clone();
        let loc = location.into();

        if let Some((_, existing_loc)) = self.embedded_addons.get(&name) {
            return Err(Error::DuplicateAddon {
                name,
                source_a: existing_loc.clone(),
                source_b: loc,
            });
        }
        if let Some((_, existing_loc)) = self.package_addons.get(&name) {
            return Err(Error::DuplicateAddon {
                name,
                source_a: existing_loc.clone(),
                source_b: loc,
            });
        }

        self.embedded_addons.insert(name, (Arc::new(manifest), loc));
        self.cached_manifests.clear();
        Ok(())
    }

    /// Look up a module by name. Caches results so subsequent calls return the same `Arc<Manifest>`
    /// until a source changes.
    ///
    /// Reports problems the way [`AddonIndex::discover_all`] does (§4):
    /// - an invalid module name is an error, not "not found";
    /// - in `--dev` mode every dev path is checked: a configured path that does not exist, a broken
    ///   manifest of this module, or two dev folders providing it are errors, and a broken
    ///   higher-precedence copy never falls back to a lower-precedence one;
    /// - dev folders are only inspected in `--dev` mode.
    ///
    /// Returns `Ok(None)` if the module is in no source.
    pub fn lookup(&mut self, name: &str) -> Result<Option<Arc<Manifest>>> {
        let mod_name = ModuleName::new(name)?;

        if let Some(cached) = self.cached_manifests.get(&mod_name) {
            return Ok(Some(Arc::clone(cached)));
        }

        // 1. Dev folders (only when dev_mode is true)
        if self.dev_mode
            && let Some(manifest) = self.lookup_in_dev_paths(&mod_name)?
        {
            let arc = Arc::new(manifest);
            self.cached_manifests
                .insert(mod_name.clone(), Arc::clone(&arc));
            return Ok(Some(arc));
        }

        // 2. Package addons
        if let Some((manifest, _loc)) = self.package_addons.get(&mod_name) {
            let arc = Arc::clone(manifest);
            self.cached_manifests
                .insert(mod_name.clone(), Arc::clone(&arc));
            return Ok(Some(arc));
        }

        // 3. Embedded addons
        if let Some((manifest, _loc)) = self.embedded_addons.get(&mod_name) {
            let arc = Arc::clone(manifest);
            self.cached_manifests
                .insert(mod_name.clone(), Arc::clone(&arc));
            return Ok(Some(arc));
        }

        Ok(None)
    }

    /// Scan all configured sources and resolve all available addons into a unified map.
    ///
    /// Enforces source precedence and collects all discovery problems (§4).
    pub fn discover_all(&mut self) -> Result<IndexMap<ModuleName, Arc<Manifest>>> {
        Ok(self
            .discover_entries()?
            .into_iter()
            .map(|(name, entry)| (name, entry.manifest))
            .collect())
    }

    /// Like [`AddonIndex::discover_all`], with each addon's source (kind and location): the module
    /// system reads data files from it.
    ///
    /// # Errors
    /// As [`AddonIndex::discover_all`].
    pub fn discover_entries(&mut self) -> Result<IndexMap<ModuleName, AddonEntry>> {
        let mut candidates: HashMap<ModuleName, AddonEntry> = HashMap::new();
        let mut problems: Vec<Error> = Vec::new();
        self.cached_manifests.clear();

        // 1. Embedded addons
        for (name, (manifest, loc)) in &self.embedded_addons {
            candidates.insert(
                name.clone(),
                AddonEntry::new(Arc::clone(manifest), AddonSourceKind::Embedded, loc.clone()),
            );
        }

        // 2. Package addons
        for (name, (manifest, loc)) in &self.package_addons {
            if let Some(existing) = candidates.get(name) {
                problems.push(Error::DuplicateAddon {
                    name: name.clone(),
                    source_a: existing.source_location().to_string(),
                    source_b: loc.clone(),
                });
            } else {
                let entry = AddonEntry::new(
                    Arc::clone(manifest),
                    AddonSourceKind::Package {
                        sha256: self.package_sha256.get(name).map(ToString::to_string),
                    },
                    loc.clone(),
                );
                candidates.insert(
                    name.clone(),
                    match self.package_data.get(name) {
                        Some(data) => entry.with_package_data(Arc::clone(data)),
                        None => entry,
                    },
                );
            }
        }

        // 3. Dev folders (only scanned if dev_mode is enabled)
        if self.dev_mode {
            for dev_path in &self.dev_paths {
                match dev_path.try_exists() {
                    Ok(true) => {}
                    Ok(false) => {
                        problems.push(Error::DiscoveryError {
                            path: dev_path.clone(),
                            message: format!(
                                "configured dev path does not exist: {}",
                                dev_path.display()
                            ),
                        });
                        continue;
                    }
                    Err(e) => {
                        problems.push(Error::Io {
                            path: dev_path.clone(),
                            source: e,
                        });
                        continue;
                    }
                }

                // Check if dev_path itself is an addon
                let manifest_file = dev_path.join("manifest.toml");
                let manifest_exists = match path_present(&manifest_file) {
                    Ok(ex) => ex,
                    Err(e) => {
                        problems.push(Error::Io {
                            path: manifest_file.clone(),
                            source: e,
                        });
                        false
                    }
                };
                let dev_is_dir = match is_dir(dev_path) {
                    Ok(is_dir) => is_dir,
                    Err(e) => {
                        problems.push(Error::Io {
                            path: dev_path.clone(),
                            source: e,
                        });
                        continue;
                    }
                };
                if manifest_exists {
                    match folder_name(dev_path).and_then(ModuleName::new) {
                        Ok(mod_name) => match read_manifest_from_dir(dev_path, &mod_name) {
                            Ok(Some(manifest)) => {
                                self.register_dev_candidate(
                                    &mut candidates,
                                    &mut problems,
                                    mod_name,
                                    manifest,
                                    dev_path,
                                );
                            }
                            Ok(None) => {}
                            Err(e) => problems.push(e),
                        },
                        Err(e) => problems.push(e),
                    }
                } else if dev_is_dir {
                    // Scan subdirectories
                    match fs::read_dir(dev_path) {
                        Ok(entries) => {
                            for entry in entries {
                                match entry {
                                    Ok(entry) => {
                                        let sub_path = entry.path();
                                        match is_dir(&sub_path) {
                                            Ok(true) => {}
                                            Ok(false) => continue,
                                            Err(e) => {
                                                problems.push(Error::Io {
                                                    path: sub_path,
                                                    source: e,
                                                });
                                                continue;
                                            }
                                        }
                                        // A folder without manifest.toml is not an addon
                                        let sub_manifest = sub_path.join("manifest.toml");
                                        let sub_manifest_exists = match path_present(&sub_manifest)
                                        {
                                            Ok(ex) => ex,
                                            Err(e) => {
                                                problems.push(Error::Io {
                                                    path: sub_manifest,
                                                    source: e,
                                                });
                                                false
                                            }
                                        };
                                        if !sub_manifest_exists {
                                            continue;
                                        }
                                        match folder_name(&sub_path).and_then(ModuleName::new) {
                                            Ok(mod_name) => {
                                                match read_manifest_from_dir(&sub_path, &mod_name) {
                                                    Ok(Some(manifest)) => {
                                                        self.register_dev_candidate(
                                                            &mut candidates,
                                                            &mut problems,
                                                            mod_name,
                                                            manifest,
                                                            &sub_path,
                                                        );
                                                    }
                                                    Ok(None) => {}
                                                    Err(e) => problems.push(e),
                                                }
                                            }
                                            Err(e) => problems.push(e),
                                        }
                                    }
                                    Err(e) => {
                                        problems.push(Error::Io {
                                            path: dev_path.clone(),
                                            source: e,
                                        });
                                    }
                                }
                            }
                        }
                        Err(e) => {
                            problems.push(Error::Io {
                                path: dev_path.clone(),
                                source: e,
                            });
                        }
                    }
                } else {
                    problems.push(Error::DiscoveryError {
                        path: dev_path.clone(),
                        message: format!("path is not a directory: {}", dev_path.display()),
                    });
                }
            }
        }

        if !problems.is_empty() {
            if problems.len() == 1
                && let Some(err) = problems.pop()
            {
                return Err(err);
            }
            return Err(Error::DiscoveryErrors {
                count: problems.len(),
                errors: problems,
            });
        }

        // Sort by name for deterministic ordering
        let mut sorted_keys: Vec<ModuleName> = candidates.keys().cloned().collect();
        sorted_keys.sort();

        let mut result = IndexMap::new();
        for key in sorted_keys {
            if let Some(entry) = candidates.remove(&key) {
                self.cached_manifests
                    .insert(key.clone(), Arc::clone(entry.manifest()));
                result.insert(key, entry);
            }
        }

        Ok(result)
    }

    fn register_dev_candidate(
        &self,
        candidates: &mut HashMap<ModuleName, AddonEntry>,
        problems: &mut Vec<Error>,
        name: ModuleName,
        manifest: Manifest,
        path: &Path,
    ) {
        let loc = path.display().to_string();
        // A dev folder with a `[wasm]` section is the source of a package addon. It never replaces
        // the installed package of that name (the package is what runs); alone it stays listed so
        // the catalogue can refuse it with the reason.
        if manifest.wasm().is_some()
            && let Some(existing) = candidates.get(&name)
            && matches!(existing.source_kind(), AddonSourceKind::Package { .. })
        {
            return;
        }
        if let Some(existing) = candidates.get(&name)
            && matches!(existing.source_kind(), AddonSourceKind::DevFolder(_))
        {
            // Two dev folders with the same module name is an error
            problems.push(Error::DuplicateAddon {
                name,
                source_a: existing.source_location().to_string(),
                source_b: loc,
            });
            return;
        }

        candidates.insert(
            name,
            AddonEntry::new(
                Arc::new(manifest),
                AddonSourceKind::DevFolder(path.to_path_buf()),
                loc,
            ),
        );
    }

    fn lookup_in_dev_paths(&self, name: &ModuleName) -> Result<Option<Manifest>> {
        let mut problems: Vec<Error> = Vec::new();
        let mut found: Vec<(Manifest, PathBuf)> = Vec::new();
        // `try_exists`/`metadata`: a permission or I/O error is reported, never read as "absent".
        let exists = |path: &Path| {
            path_present(path).map_err(|e| Error::Io {
                path: path.to_path_buf(),
                source: e,
            })
        };
        for dev_path in &self.dev_paths {
            match exists(dev_path) {
                Ok(true) => {}
                Ok(false) => {
                    problems.push(Error::DiscoveryError {
                        path: dev_path.clone(),
                        message: format!(
                            "configured dev path does not exist: {}",
                            dev_path.display()
                        ),
                    });
                    continue;
                }
                Err(e) => {
                    problems.push(e);
                    continue;
                }
            }
            // The dev path is the addon itself, or an addons path holding it.
            let named_like = dev_path.file_name().and_then(|s| s.to_str()) == Some(name.as_str());
            let itself = named_like
                && match exists(&dev_path.join("manifest.toml")) {
                    Ok(present) => present,
                    Err(e) => {
                        problems.push(e);
                        continue;
                    }
                };
            let candidate = if itself {
                dev_path.clone()
            } else {
                dev_path.join(name.as_str())
            };
            match fs::metadata(&candidate) {
                Ok(meta) if meta.is_dir() => {}
                // Not there, or a plain file of that name: this dev path does not provide it.
                Ok(_) => continue,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => {
                    problems.push(Error::Io {
                        path: candidate,
                        source: e,
                    });
                    continue;
                }
            }
            match read_manifest_from_dir(&candidate, name) {
                Ok(Some(manifest)) => found.push((manifest, candidate)),
                Ok(None) => {}
                Err(e) => problems.push(e),
            }
        }
        if let [(_, first), (_, second), ..] = found.as_slice() {
            problems.push(Error::DuplicateAddon {
                name: name.clone(),
                source_a: first.display().to_string(),
                source_b: second.display().to_string(),
            });
        }
        match problems.len() {
            0 => Ok(found.into_iter().next().map(|(m, _)| m)),
            1 => Err(problems.remove(0)),
            count => Err(Error::DiscoveryErrors {
                count,
                errors: problems,
            }),
        }
    }
}

/// The last component of an addon directory's path as UTF-8: the module name comes from it.
fn folder_name(path: &Path) -> Result<&str> {
    path.file_name()
        .ok_or_else(|| Error::DiscoveryError {
            path: path.to_path_buf(),
            message: "the addon directory has no name".to_owned(),
        })?
        .to_str()
        .ok_or_else(|| Error::DiscoveryError {
            path: path.to_path_buf(),
            message: "the addon directory name is not valid UTF-8".to_owned(),
        })
}

/// Largest `manifest.toml` the discovery reads (bytes). Manifests are untrusted input.
pub const MAX_MANIFEST_BYTES: u64 = 256 * 1024;

/// Whether something is at `path` (a dangling symbolic link counts: `try_exists` follows links, so
/// a link to nothing would read as "no manifest" and the addon would vanish silently).
fn path_present(path: &Path) -> std::io::Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e),
    }
}

/// Whether `path` is a directory (links followed), telling "not there" (`false`) from "cannot
/// be read" (an error, e.g. permission denied): `Path::is_dir` reports both as `false`.
fn is_dir(path: &Path) -> std::io::Result<bool> {
    match fs::metadata(path) {
        Ok(metadata) => Ok(metadata.is_dir()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e),
    }
}

/// Whether `path` is a regular file (links followed); see [`is_dir`] for the error rule.
fn is_file(path: &Path) -> std::io::Result<bool> {
    match fs::metadata(path) {
        Ok(metadata) => Ok(metadata.is_file()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e),
    }
}

/// Read a `manifest.toml`: a regular file that is not a symbolic link and has one hard link, at
/// most [`MAX_MANIFEST_BYTES`], opened without following a link swapped in after the checks.
fn read_manifest_text(path: &Path) -> Result<String> {
    let refused = |message: String| Error::DiscoveryError {
        path: path.to_path_buf(),
        message,
    };
    let io = |source: std::io::Error| Error::Io {
        path: path.to_path_buf(),
        source,
    };
    let meta = fs::symlink_metadata(path).map_err(io)?;
    if meta.file_type().is_symlink() {
        return Err(refused(
            "manifest.toml is a symbolic link; a manifest must be a regular file of the addon"
                .to_owned(),
        ));
    }
    if !meta.is_file() {
        return Err(refused("manifest.toml is not a regular file".to_owned()));
    }
    let file = crate::safe_read::open_nofollow(path).map_err(io)?;
    let meta = file.metadata().map_err(io)?;
    if !meta.is_file() {
        return Err(refused("manifest.toml is not a regular file".to_owned()));
    }
    if let Some(links) = crate::safe_read::extra_hard_links(&meta) {
        return Err(refused(format!(
            "manifest.toml has {links} hard links; a manifest must belong to the addon only"
        )));
    }
    let Some(bytes) = crate::safe_read::read_capped(file, MAX_MANIFEST_BYTES).map_err(io)? else {
        return Err(refused(format!(
            "manifest.toml is larger than {MAX_MANIFEST_BYTES} bytes"
        )));
    };
    String::from_utf8(bytes)
        .map_err(|e| io(std::io::Error::new(std::io::ErrorKind::InvalidData, e)))
}

/// Read and parse manifest.toml from an addon directory.
///
/// Per and §2:
/// - Only a missing `manifest.toml` returns `Ok(None)`.
/// - Any unreadable file, TOML syntax error, or validation error returns `Err`.
/// - Enriches manifest with filesystem static assets (static path and icon).
pub fn read_manifest_from_dir(
    addon_dir: &Path,
    module_name: &ModuleName,
) -> Result<Option<Manifest>> {
    let addon_is_dir = is_dir(addon_dir).map_err(|source| Error::Io {
        path: addon_dir.to_path_buf(),
        source,
    })?;
    if !addon_is_dir {
        return Err(Error::DiscoveryError {
            path: addon_dir.to_path_buf(),
            message: "not a directory".to_string(),
        });
    }

    let manifest_path = addon_dir.join("manifest.toml");
    let manifest_exists = path_present(&manifest_path).map_err(|e| Error::Io {
        path: manifest_path.clone(),
        source: e,
    })?;
    if !manifest_exists {
        return Ok(None);
    }
    let content = read_manifest_text(&manifest_path)?;

    let mut manifest = Manifest::from_toml(module_name.as_str(), &content)?;

    // Probe filesystem assets (static directory and icon)
    let static_dir = addon_dir.join("static");
    let io = |path: &Path| {
        let path = path.to_path_buf();
        move |source: std::io::Error| Error::Io { path, source }
    };
    let static_path = if is_dir(&static_dir).map_err(io(&static_dir))? {
        Some(static_dir.clone())
    } else {
        None
    };
    let icon_file = static_dir.join("description").join("icon.png");
    let icon = if is_file(&icon_file).map_err(io(&icon_file))? {
        Some(format!("/{}/static/description/icon.png", module_name))
    } else {
        None
    };

    manifest.set_discovered_assets(static_path, icon);
    Ok(Some(manifest))
}
