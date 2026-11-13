//! `erp addon build --target wasm` and `erp addon package`.
//!
//! `build_wasm` compiles the addon's leaf crate to a `wasm32-wasip2` component with fat LTO,
//! places it at `wasm/<name>.wasm` of the addon directory and instantiates it with the WASM host
//! under its limits: `describe()` must list the addon and every WASM addon it depends on.
//! `package` writes the `.erpaddon` with the package writer and reads it back with the reader.
//! Design; author entry.

use crate::args::{AddonBuildArgs, AddonPackageArgs, BuildTarget};
use crate::error::{CliError, Result};
use crate::module::resolve_manifest_path;
use erp_modules::package::{ErpAddonPackage, ErpAddonPackageWriter};
use erp_modules::{Manifest, ModuleName, read_manifest_from_dir};
use erp_wasm_host::{HostError, Limits, WasmHost};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fmt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// The Rust target a WASM addon is compiled for.
pub const WASM_TARGET: &str = "wasm32-wasip2";

/// Command that installs the Rust target.
pub const INSTALL_WASM_TARGET: &str = "rustup target add wasm32-wasip2";

/// What the build needs to know about the machine's Rust toolchain. A trait so tests inject a
/// toolchain without the target instead of uninstalling it.
pub trait ToolchainEnv {
    /// The sysroot of the `rustc` that builds the addon in `cwd` (`rustc --print sysroot`).
    ///
    /// # Errors
    ///
    /// A description of why `rustc` could not be asked.
    fn rustc_sysroot(&self, cwd: &Path) -> std::result::Result<PathBuf, String>;
}

/// The toolchain of this machine: `$RUSTC` or `rustc` from `PATH`.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemToolchain;

impl ToolchainEnv for SystemToolchain {
    fn rustc_sysroot(&self, cwd: &Path) -> std::result::Result<PathBuf, String> {
        let rustc = std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into());
        let out = Command::new(&rustc)
            .args(["--print", "sysroot"])
            .current_dir(cwd)
            .output()
            .map_err(|e| format!("cannot run {}: {e}", Path::new(&rustc).display()))?;
        if !out.status.success() {
            return Err(format!(
                "`rustc --print sysroot` failed ({}): {}",
                out.status,
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        let text = String::from_utf8(out.stdout)
            .map_err(|_| "rustc printed a sysroot that is not UTF-8".to_owned())?;
        Ok(PathBuf::from(text.trim()))
    }
}

/// Checks that the toolchain that builds the addon in `cwd` has the `wasm32-wasip2` target.
///
/// `wasm-tools` is not needed: the component comes straight out of `rustc`, and the host that
/// checks it is linked into this binary.
///
/// # Errors
///
/// [`CliError::ToolchainMissing`] naming what is missing and the install command.
pub fn check_wasm_toolchain(env: &dyn ToolchainEnv, cwd: &Path) -> Result<()> {
    let sysroot = env
        .rustc_sysroot(cwd)
        .map_err(|detail| CliError::ToolchainMissing {
            tool: "rustc".to_owned(),
            detail,
            install: "install Rust from https://rustup.rs".to_owned(),
        })?;
    let target_lib = sysroot.join("lib").join("rustlib").join(WASM_TARGET);
    if !target_lib.is_dir() {
        return Err(CliError::ToolchainMissing {
            tool: format!("the {WASM_TARGET} target"),
            detail: format!("{} does not exist", target_lib.display()),
            install: INSTALL_WASM_TARGET.to_owned(),
        });
    }
    Ok(())
}

/// An addon located on disk with its parsed manifest.
#[derive(Debug, Clone)]
pub struct ResolvedAddon {
    /// Technical name (the directory name).
    pub name: ModuleName,
    /// The addon directory.
    pub dir: PathBuf,
    /// Its manifest.
    pub manifest: Manifest,
}

/// Finds an addon by directory or by technical name in `addons_path`.
///
/// # Errors
///
/// [`CliError::InvalidInput`] for a directory without a manifest, [`CliError::ModuleNotFound`]
/// for a name found nowhere, [`CliError::Manifest`] for a manifest that does not parse.
pub fn resolve_addon(spec: &str, addons_path: &[PathBuf]) -> Result<ResolvedAddon> {
    let as_path = Path::new(spec);
    let dir = if as_path.is_dir() {
        std::fs::canonicalize(as_path).map_err(|source| CliError::Io {
            path: as_path.to_path_buf(),
            source,
        })?
    } else {
        let name = ModuleName::new(spec).map_err(|e| CliError::InvalidInput {
            arg: "addon".to_owned(),
            reason: format!("'{spec}' is neither a directory nor a valid addon name: {e}"),
        })?;
        let manifest = resolve_manifest_path(&name, addons_path)?
            .ok_or_else(|| CliError::ModuleNotFound(name.clone()))?;
        let parent = manifest.parent().unwrap_or(Path::new("."));
        std::fs::canonicalize(parent).map_err(|source| CliError::Io {
            path: parent.to_path_buf(),
            source,
        })?
    };
    let dir_name =
        dir.file_name()
            .and_then(|n| n.to_str())
            .ok_or_else(|| CliError::InvalidInput {
                arg: "addon".to_owned(),
                reason: format!("cannot derive an addon name from '{}'", dir.display()),
            })?;
    let name = ModuleName::new(dir_name).map_err(|e| CliError::InvalidModuleName {
        name: dir_name.to_owned(),
        reason: e.to_string(),
    })?;
    let manifest = read_manifest_from_dir(&dir, &name)
        .map_err(|source| CliError::Manifest {
            path: dir.join("manifest.toml"),
            source: Box::new(source),
        })?
        .ok_or_else(|| CliError::InvalidInput {
            arg: "addon".to_owned(),
            reason: format!("'{}' has no manifest.toml", dir.display()),
        })?;
    Ok(ResolvedAddon {
        name,
        dir,
        manifest,
    })
}

fn require_wasm(addon: &ResolvedAddon) -> Result<()> {
    if addon.manifest.wasm().is_none() {
        return Err(CliError::NotWasmAddon {
            addon: addon.name.clone(),
            path: addon.dir.join("manifest.toml"),
        });
    }
    Ok(())
}

/// The WASM addons the component of `root` must describe: `root` and, transitively, every
/// dependency whose manifest has a `[wasm]` section. Dependencies found without one, or found
/// nowhere in the search paths, are returned too (second value) so the report shows what was
/// not expected rather than hiding it.
fn expected_addons(
    root: &ResolvedAddon,
    addons_path: &[PathBuf],
) -> Result<(BTreeSet<String>, BTreeMap<String, String>)> {
    let mut bases: Vec<PathBuf> = addons_path.to_vec();
    if let Some(parent) = root.dir.parent() {
        bases.push(parent.to_path_buf());
    }
    let mut expected = BTreeSet::from([root.name.to_string()]);
    let mut not_wasm = BTreeMap::new();
    let mut queue: VecDeque<ModuleName> = root.manifest.depends().iter().cloned().collect();
    let mut seen: BTreeSet<String> = BTreeSet::from([root.name.to_string()]);
    while let Some(dep) = queue.pop_front() {
        if !seen.insert(dep.to_string()) {
            continue;
        }
        let Some(manifest_path) = resolve_manifest_path(&dep, &bases)? else {
            not_wasm.insert(
                dep.to_string(),
                "not found in the addon search paths, not expected in the component".to_owned(),
            );
            continue;
        };
        let dir = manifest_path.parent().unwrap_or(Path::new("."));
        let manifest = read_manifest_from_dir(dir, &dep)
            .map_err(|source| CliError::Manifest {
                path: manifest_path.clone(),
                source: Box::new(source),
            })?
            .ok_or_else(|| CliError::ModuleNotFound(dep.clone()))?;
        if manifest.wasm().is_some() {
            expected.insert(dep.to_string());
            queue.extend(manifest.depends().iter().cloned());
        } else {
            not_wasm.insert(
                dep.to_string(),
                "native addon (no [wasm] section)".to_owned(),
            );
        }
    }
    Ok((expected, not_wasm))
}

/// Asks the component for `describe()` through the WASM host under its limits and checks that it
/// lists every addon of `expected`.
///
/// Returns the addons the component describes.
///
/// # Errors
///
/// [`CliError::WasmHost`] when the host cannot compile, instantiate or call the component,
/// [`CliError::DescribeMismatch`] when the text is not a description or misses an addon.
pub fn verify_described(
    addon: &ModuleName,
    component: &[u8],
    expected: &BTreeSet<String>,
) -> Result<BTreeSet<String>> {
    let host_error = |source: HostError| CliError::WasmHost {
        addon: addon.clone(),
        source,
    };
    let host = WasmHost::new().map_err(host_error)?;
    let compiled = host.component(component).map_err(host_error)?;
    // the hard CPU maximum and the default memory cap of the host: describe() only walks a table
    let limits = Limits::default()
        .with_cpu(Limits::MAX_CPU)
        .map_err(host_error)?;
    let mut store = host
        .inspection_store(&compiled, limits)
        .map_err(host_error)?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .map_err(|source| CliError::Io {
            path: PathBuf::from("<tokio runtime>"),
            source,
        })?;
    let text = runtime
        .block_on(store.call_describe())
        .map_err(host_error)?;
    let mismatch = |detail: String| CliError::DescribeMismatch {
        addon: addon.clone(),
        detail,
    };
    let doc: serde_json::Value = serde_json::from_str(&text)
        .map_err(|e| mismatch(format!("describe() is not JSON: {e}")))?;
    if doc["schema"] != erp_sdk::DESCRIBE_SCHEMA {
        return Err(mismatch(format!(
            "describe() has schema {}, this tool reads schema {}",
            doc["schema"],
            erp_sdk::DESCRIBE_SCHEMA
        )));
    }
    let entries = doc["contributions"]
        .as_array()
        .ok_or_else(|| mismatch("describe() has no `contributions` list".to_owned()))?;
    let described: BTreeSet<String> = entries
        .iter()
        .filter(|e| e["kind"] == "addon")
        .filter_map(|e| e["addon"].as_str().map(str::to_owned))
        .collect();
    let missing: Vec<&str> = expected
        .difference(&described)
        .map(String::as_str)
        .collect();
    if !missing.is_empty() {
        return Err(mismatch(format!(
            "describe() does not list {} (it lists: {}); a dependency addon that the leaf crate does \
             not reference is dropped by the linker unless it is a Cargo dependency of the leaf crate \
             built with fat LTO",
            missing.join(", "),
            described.iter().cloned().collect::<Vec<_>>().join(", ")
        )));
    }
    Ok(described)
}

/// What `erp addon build` did.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct BuildReport {
    /// The addon.
    pub addon: ModuleName,
    /// Where the component was placed.
    pub wasm_path: PathBuf,
    /// Size of the component.
    pub bytes: usize,
    /// Addons the component describes.
    pub described: BTreeSet<String>,
    /// Dependencies not expected in the component, with the reason.
    pub not_expected: BTreeMap<String, String>,
}

impl fmt::Display for BuildReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(
            f,
            "Built {} ({} bytes) at {}",
            self.addon,
            self.bytes,
            self.wasm_path.display()
        )?;
        writeln!(
            f,
            "describe() lists: {}",
            self.described
                .iter()
                .cloned()
                .collect::<Vec<_>>()
                .join(", ")
        )?;
        for (dep, why) in &self.not_expected {
            writeln!(f, "dependency {dep}: {why}")?;
        }
        Ok(())
    }
}

/// `erp addon build --target wasm`.
///
/// # Errors
///
/// Any [`CliError`] for an addon that cannot be found or is not a WASM addon, a missing
/// toolchain, a failed `cargo` build, or a component that does not describe the expected addons.
pub fn build_wasm(args: &AddonBuildArgs, env: &dyn ToolchainEnv) -> Result<BuildReport> {
    match args.target {
        BuildTarget::Wasm => {}
    }
    let addon = resolve_addon(&args.addon, &args.addons_path)?;
    require_wasm(&addon)?;
    let cargo_toml = addon.dir.join("Cargo.toml");
    if !cargo_toml.is_file() {
        return Err(CliError::InvalidInput {
            arg: "addon".to_owned(),
            reason: format!(
                "'{}' has no Cargo.toml: a WASM addon is a Rust crate that ends in `erp_sdk::export_component!();`",
                addon.dir.display()
            ),
        });
    }
    check_wasm_toolchain(env, &addon.dir)?;
    let artifact = run_cargo(&addon, &cargo_toml, args.target_dir.as_deref())?;
    let bytes = std::fs::read(&artifact).map_err(|source| CliError::Io {
        path: artifact.clone(),
        source,
    })?;
    let (expected, not_expected) = expected_addons(&addon, &args.addons_path)?;
    let described = verify_described(&addon.name, &bytes, &expected)?;
    let wasm_path = wasm_path_of(&addon);
    if let Some(dir) = wasm_path.parent() {
        std::fs::create_dir_all(dir).map_err(|source| CliError::Io {
            path: dir.to_path_buf(),
            source,
        })?;
    }
    // placed only after the host accepted it, so `wasm/` never holds a component that failed the check
    std::fs::write(&wasm_path, &bytes).map_err(|source| CliError::Io {
        path: wasm_path.clone(),
        source,
    })?;
    Ok(BuildReport {
        addon: addon.name,
        wasm_path,
        bytes: bytes.len(),
        described,
        not_expected,
    })
}

fn wasm_path_of(addon: &ResolvedAddon) -> PathBuf {
    addon.dir.join("wasm").join(format!("{}.wasm", addon.name))
}

/// Runs `cargo rustc` for the addon's crate as a `cdylib` for [`WASM_TARGET`] with fat LTO (this
/// build only: the environment of the child process) and returns the path of the `.wasm` it built.
fn run_cargo(
    addon: &ResolvedAddon,
    cargo_toml: &Path,
    target_dir: Option<&Path>,
) -> Result<PathBuf> {
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let mut cmd = Command::new(&cargo);
    cmd.args(["rustc", "--release", "--manifest-path"])
        .arg(cargo_toml)
        .args(["--crate-type", "cdylib", "--target", WASM_TARGET])
        .arg("--message-format=json-render-diagnostics");
    if let Some(dir) = target_dir {
        cmd.arg("--target-dir").arg(dir);
    }
    // Without fat LTO the linker drops the registrations of dependency addons from describe().
    cmd.env("CARGO_PROFILE_RELEASE_LTO", "fat")
        .current_dir(&addon.dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());
    let failed = |detail: String| CliError::BuildFailed {
        addon: addon.name.clone(),
        detail,
    };
    let out = cmd
        .output()
        .map_err(|e| failed(format!("cannot run {}: {e}", Path::new(&cargo).display())))?;
    if !out.status.success() {
        return Err(failed(format!(
            "cargo exited with {} (its diagnostics are above)",
            out.status
        )));
    }
    let own_manifest =
        std::fs::canonicalize(cargo_toml).unwrap_or_else(|_| cargo_toml.to_path_buf());
    let text = String::from_utf8_lossy(&out.stdout);
    let mut found = None;
    for line in text.lines() {
        let Ok(msg) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if msg["reason"] != "compiler-artifact" {
            continue;
        }
        let manifest = msg["manifest_path"].as_str().map(PathBuf::from);
        let is_ours = manifest
            .and_then(|m| std::fs::canonicalize(m).ok())
            .is_some_and(|m| m == own_manifest);
        if !is_ours {
            continue;
        }
        if let Some(files) = msg["filenames"].as_array() {
            for file in files.iter().filter_map(|f| f.as_str()) {
                if file.ends_with(".wasm") {
                    found = Some(PathBuf::from(file));
                }
            }
        }
    }
    found.ok_or_else(|| {
        failed(
            "cargo built no .wasm for this crate; check that the crate compiles as a cdylib for \
             wasm32-wasip2 and calls `erp_sdk::export_component!();`"
                .to_owned(),
        )
    })
}

/// What `erp addon package` wrote.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct PackageReport {
    /// The addon.
    pub addon: ModuleName,
    /// Its version.
    pub version: String,
    /// The archive.
    pub output: PathBuf,
    /// SHA-256 of the archive.
    pub archive_sha256: String,
    /// Files in the archive, `SHA256SUMS` included.
    pub files: Vec<String>,
    /// Capabilities the manifest requests, in their stored form.
    pub capabilities: Vec<String>,
    /// Digest of the shipped client code, when there is any.
    pub client_dist_digest: Option<String>,
}

impl fmt::Display for PackageReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(
            f,
            "Packaged {} {} ({} files) at {}",
            self.addon,
            self.version,
            self.files.len(),
            self.output.display()
        )?;
        writeln!(f, "archive sha256: {}", self.archive_sha256)?;
        if self.capabilities.is_empty() {
            writeln!(f, "capabilities: none")?;
        } else {
            writeln!(f, "capabilities: {}", self.capabilities.join(", "))?;
        }
        if let Some(digest) = &self.client_dist_digest {
            writeln!(
                f,
                "client dist digest: {digest} (an administrator must acknowledge it)"
            )?;
        }
        Ok(())
    }
}

/// Reads a regular file below `dir`; symlinks and other file types are refused.
fn read_source_file(dir: &Path, relative: &str) -> Result<Vec<u8>> {
    let path = dir.join(relative);
    let meta = std::fs::symlink_metadata(&path).map_err(|source| CliError::Io {
        path: path.clone(),
        source,
    })?;
    if !meta.file_type().is_file() {
        return Err(CliError::InvalidInput {
            arg: "addon".to_owned(),
            reason: format!(
                "'{}' is not a regular file (symlinks are not packaged)",
                path.display()
            ),
        });
    }
    std::fs::read(&path).map_err(|source| CliError::Io { path, source })
}

/// Adds every regular file below `dir/relative` (sorted), refusing symlinks and odd file types.
fn collect_tree(dir: &Path, relative: &str, out: &mut BTreeMap<String, Vec<u8>>) -> Result<()> {
    let root = dir.join(relative);
    match std::fs::symlink_metadata(&root) {
        Ok(meta) if meta.file_type().is_dir() => {}
        Ok(_) => {
            return Err(CliError::InvalidInput {
                arg: "addon".to_owned(),
                reason: format!(
                    "'{}' must be a directory (symlinks are not packaged)",
                    root.display()
                ),
            });
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(source) => return Err(CliError::Io { path: root, source }),
    }
    let mut entries = Vec::new();
    for entry in std::fs::read_dir(&root).map_err(|source| CliError::Io {
        path: root.clone(),
        source,
    })? {
        let entry = entry.map_err(|source| CliError::Io {
            path: root.clone(),
            source,
        })?;
        entries.push(entry.file_name());
    }
    entries.sort();
    for name in entries {
        let name = name.to_str().ok_or_else(|| CliError::InvalidInput {
            arg: "addon".to_owned(),
            reason: format!("a file name below '{}' is not UTF-8", root.display()),
        })?;
        let child = format!("{relative}/{name}");
        let path = dir.join(&child);
        let meta = std::fs::symlink_metadata(&path).map_err(|source| CliError::Io {
            path: path.clone(),
            source,
        })?;
        if meta.file_type().is_dir() {
            collect_tree(dir, &child, out)?;
        } else {
            out.insert(child.clone(), read_source_file(dir, &child)?);
        }
    }
    Ok(())
}

/// The manifest text of the archive: the source text, plus `technical_name` when it has none,
/// because the archive reader takes the module name from there when `name` is a display name.
fn archive_manifest(addon: &ResolvedAddon, source: &str) -> Result<String> {
    let table: toml::Table = toml::from_str(source).map_err(|e| CliError::Manifest {
        path: addon.dir.join("manifest.toml"),
        source: Box::new(erp_modules::Error::InvalidManifest {
            name: addon.name.clone(),
            message: e.to_string(),
        }),
    })?;
    if table.contains_key("technical_name") {
        return Ok(source.to_owned());
    }
    // top-level keys come before every table header, so a first line is always in the right place
    Ok(format!("technical_name = \"{}\"\n{source}", addon.name))
}

/// `erp addon package`.
///
/// Packs `manifest.toml`, the files its `data` and `demo` lists name, `i18n/`, `migrations/`,
/// `static/dist/` and the component, then reads the archive back from disk and compares it with
/// what was packed.
///
/// # Errors
///
/// [`CliError::NotWasmAddon`], [`CliError::WasmBinaryMissing`], [`CliError::WasmHost`] for a file
/// that is not a component, [`CliError::Package`] when the writer or reader refuses the archive,
/// [`CliError::PackageReadBack`] when the archive read back differs.
pub fn package(args: &AddonPackageArgs) -> Result<PackageReport> {
    let addon = resolve_addon(&args.addon, &args.addons_path)?;
    require_wasm(&addon)?;
    let wasm_path = args.wasm.clone().unwrap_or_else(|| wasm_path_of(&addon));
    let wasm = match std::fs::read(&wasm_path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(CliError::WasmBinaryMissing {
                addon: addon.name,
                path: wasm_path,
            });
        }
        Err(source) => {
            return Err(CliError::Io {
                path: wasm_path,
                source,
            });
        }
    };
    // a file that is not a valid component is refused here, not at install time
    WasmHost::new()
        .and_then(|host| host.component(&wasm).map(drop))
        .map_err(|source| CliError::WasmHost {
            addon: addon.name.clone(),
            source,
        })?;

    let manifest_path = addon.dir.join("manifest.toml");
    let manifest_source = String::from_utf8(read_source_file(&addon.dir, "manifest.toml")?)
        .map_err(|_| CliError::InvalidInput {
            arg: "addon".to_owned(),
            reason: format!("'{}' is not UTF-8", manifest_path.display()),
        })?;
    let mut files: BTreeMap<String, Vec<u8>> = BTreeMap::new();
    files.insert(
        "manifest.toml".to_owned(),
        archive_manifest(&addon, &manifest_source)?.into_bytes(),
    );
    for relative in addon.manifest.data().iter().chain(addon.manifest.demo()) {
        files.insert(relative.clone(), read_source_file(&addon.dir, relative)?);
    }
    for tree in ["i18n", "migrations", "static/dist"] {
        collect_tree(&addon.dir, tree, &mut files)?;
    }
    files.insert(format!("wasm/{}.wasm", addon.name), wasm);

    let output = args.output.clone().unwrap_or_else(|| {
        PathBuf::from(format!(
            "{}-{}.erpaddon",
            addon.name,
            addon.manifest.version()
        ))
    });
    let package_error = |source: erp_modules::Error| CliError::Package {
        path: output.clone(),
        source: Box::new(source),
    };
    let mut writer = ErpAddonPackageWriter::new();
    for (path, content) in &files {
        writer.add_file(path.clone(), content.clone());
    }
    let archive = writer.build_zip().map_err(package_error)?;
    std::fs::write(&output, &archive).map_err(|source| CliError::Io {
        path: output.clone(),
        source,
    })?;

    // read back from disk with the reader the installer uses
    let package = ErpAddonPackage::open(&output).map_err(package_error)?;
    let read_back = |detail: String| CliError::PackageReadBack {
        path: output.clone(),
        detail,
    };
    if package.name() != &addon.name {
        return Err(read_back(format!(
            "the archive names the module '{}', not '{}'",
            package.name(),
            addon.name
        )));
    }
    if package.version() != addon.manifest.version() {
        return Err(read_back(format!(
            "the archive has version {}, the manifest {}",
            package.version(),
            addon.manifest.version()
        )));
    }
    for (path, content) in &files {
        if package.get_file(path) != Some(content.as_slice()) {
            return Err(read_back(format!("'{path}' differs or is missing")));
        }
    }
    let listed: BTreeSet<&str> = package.file_paths().collect();
    let expected: BTreeSet<&str> = files
        .keys()
        .map(String::as_str)
        .chain(["SHA256SUMS"])
        .collect();
    if listed != expected {
        return Err(read_back(format!(
            "the archive holds {listed:?}, expected {expected:?}"
        )));
    }
    Ok(PackageReport {
        addon: addon.name,
        version: package.version().to_string(),
        output,
        archive_sha256: package.archive_sha256().to_owned(),
        files: package.file_paths().map(str::to_owned).collect(),
        capabilities: package
            .manifest()
            .wasm()
            .map(|w| w.capabilities.iter().map(ToString::to_string).collect())
            .unwrap_or_default(),
        client_dist_digest: package.client_dist_digest().map(str::to_owned),
    })
}
