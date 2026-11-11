//! Bind each native addon's runtime files to the source tree compiled with this CLI.
//!
//! The content encoding matches erp_modules::artifact_identity and the verified PACKAGE receipt.
//! This is per-addon provenance, independent of unrelated framework or binary changes.
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::error::Error;
use std::fmt::Write;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::{env, fs};

#[path = "../erp-modules/src/artifact_paths.rs"]
mod artifact_paths;

const MAX_BYTES: u64 = 64 * 1024 * 1024;
const MAX_FILES: usize = 20_000;
const MAX_ENTRIES: usize = 40_000;

fn fail(message: impl Into<String>) -> Box<dyn Error> {
    std::io::Error::other(message.into()).into()
}

fn part(hash: &mut Sha256, bytes: &[u8]) {
    hash.update((bytes.len() as u64).to_be_bytes());
    hash.update(bytes);
}

fn collect(
    root: &Path,
    at: &Path,
    depth: usize,
    files: &mut BTreeMap<String, (u64, [u8; 32])>,
    entries: &mut usize,
    total: &mut u64,
) -> Result<(), Box<dyn Error>> {
    if depth > 32 {
        return Err(fail("native addon exceeds identity depth limit"));
    }
    for entry in fs::read_dir(at)? {
        let path = entry?.path();
        *entries = entries
            .checked_add(1)
            .ok_or_else(|| fail("native addon entry overflow"))?;
        if *entries > MAX_ENTRIES {
            return Err(fail("native addon exceeds identity entry limit"));
        }
        let metadata = fs::symlink_metadata(&path)?;
        if metadata.file_type().is_symlink() {
            return Err(fail(format!(
                "native addon contains a symlink: {}",
                path.display()
            )));
        }
        if path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(artifact_paths::excluded)
        {
            if !metadata.is_dir() && !metadata.is_file() {
                return Err(fail(format!(
                    "native addon contains a special file: {}",
                    path.display()
                )));
            }
            // Match prepared releases and runtime snapshots. Symlink entries already refuse;
            // excluded tooling/metadata bytes and descendants are never traversed.
            continue;
        }
        if metadata.is_dir() {
            collect(root, &path, depth + 1, files, entries, total)?;
        } else if metadata.is_file() {
            if files.len() == MAX_FILES {
                return Err(fail("native addon exceeds identity file limit"));
            }
            let relative = path
                .strip_prefix(root)?
                .to_str()
                .ok_or_else(|| fail("native addon path is not UTF-8"))?
                .to_owned();
            if relative.len() > 4096 {
                return Err(fail("native addon exceeds identity path limit"));
            }
            let fd = rustix::fs::open(
                &path,
                rustix::fs::OFlags::RDONLY
                    | rustix::fs::OFlags::NOFOLLOW
                    | rustix::fs::OFlags::NONBLOCK
                    | rustix::fs::OFlags::CLOEXEC,
                rustix::fs::Mode::empty(),
            )?;
            let before = rustix::fs::fstat(&fd)?;
            if rustix::fs::FileType::from_raw_mode(before.st_mode)
                != rustix::fs::FileType::RegularFile
                || before.st_nlink != 1
            {
                return Err(fail(format!(
                    "native addon file is special or linked: {}",
                    path.display()
                )));
            }
            let size = u64::try_from(before.st_size)?;
            *total = total
                .checked_add(size)
                .ok_or_else(|| fail("native addon byte overflow"))?;
            if *total > MAX_BYTES {
                return Err(fail("native addon exceeds identity byte limit"));
            }
            let mut file = fs::File::from(fd);
            let mut content = Vec::new();
            (&mut file).take(size + 1).read_to_end(&mut content)?;
            let after = rustix::fs::fstat(&file)?;
            if content.len() as u64 != size
                || before.st_size != after.st_size
                || before.st_mtime != after.st_mtime
                || before.st_mtime_nsec != after.st_mtime_nsec
                || before.st_ctime != after.st_ctime
                || before.st_ctime_nsec != after.st_ctime_nsec
            {
                return Err(fail(format!(
                    "native addon changed during build identity: {}",
                    path.display()
                )));
            }
            println!("cargo:rerun-if-changed={}", path.display());
            files.insert(relative, (size, Sha256::digest(content).into()));
        } else {
            return Err(fail(format!(
                "native addon contains a special file: {}",
                path.display()
            )));
        }
    }
    Ok(())
}

fn identity(name: &str, root: &Path) -> Result<String, Box<dyn Error>> {
    if fs::symlink_metadata(root)?.file_type().is_symlink() {
        return Err(fail("native addon root is a symlink"));
    }
    let mut files = BTreeMap::new();
    collect(root, root, 0, &mut files, &mut 0, &mut 0)?;
    if !files.contains_key("manifest.toml") {
        return Err(fail(format!("native addon manifest missing: {name}")));
    }
    println!("cargo:rerun-if-changed={}", root.display());
    let mut hash = Sha256::new();
    hash.update(b"erp-addon-artifact-v1\0");
    part(&mut hash, name.as_bytes());
    hash.update((files.len() as u64).to_be_bytes());
    for (path, (size, digest)) in files {
        part(&mut hash, path.as_bytes());
        hash.update(size.to_be_bytes());
        hash.update(digest);
    }
    let mut encoded = String::from("addon-v1:");
    for byte in hash.finalize() {
        write!(&mut encoded, "{byte:02x}")?;
    }
    Ok(encoded)
}

fn native_identity() -> Result<(), Box<dyn Error>> {
    let manifest = PathBuf::from(
        std::env::var_os("CARGO_MANIFEST_DIR")
            .ok_or_else(|| fail("CARGO_MANIFEST_DIR is missing"))?,
    );
    println!(
        "cargo:rerun-if-changed={}",
        manifest
            .join("../erp-modules/src/artifact_paths.rs")
            .display()
    );
    let root = manifest
        .parent()
        .and_then(Path::parent)
        .ok_or_else(|| fail("CLI workspace root is missing"))?;
    let mut output = String::from(
        "/// Per-addon content receipts compiled with the CLI's native addons.\npub const LINKED_ADDON_IDENTITIES: &[(&str, &str)] = &[\n",
    );
    for name in [
        "acceptance_notes",
        "base",
        "mail",
        "product",
        "theme_office_blue",
        "uom",
        "web",
    ] {
        let digest = identity(name, &root.join("addons").join(name))?;
        output.push_str(&format!("    ({name:?}, {digest:?}),\n"));
    }
    output.push_str("];\n");
    let out = PathBuf::from(std::env::var_os("OUT_DIR").ok_or_else(|| fail("OUT_DIR is missing"))?);
    fs::write(out.join("linked_addon_identities.rs"), output)?;
    Ok(())
}

fn asset_files(path: &Path, out: &mut Vec<PathBuf>) -> std::io::Result<()> {
    if path.is_dir() {
        let mut entries = fs::read_dir(path)?
            .map(|e| e.map(|e| e.path()))
            .collect::<std::io::Result<Vec<_>>>()?;
        entries.sort();
        for entry in entries {
            asset_files(&entry, out)?;
        }
    } else {
        out.push(path.to_path_buf());
    }
    Ok(())
}
fn asset_identity() -> Result<(), Box<dyn std::error::Error>> {
    let root = PathBuf::from(env::var("CARGO_MANIFEST_DIR")?).join("../..");
    let mut inputs = Vec::new();
    for path in [
        "crates/erp-ui-compiler/src",
        "crates/erp-views/src",
        "web/src",
        "web/vendor",
        "web/package.json",
        "addons/web/src/assets/assembly.rs",
        "crates/erp-cli/src/ui_assets.rs",
        "Cargo.lock",
    ] {
        println!("cargo:rerun-if-changed=../../{path}");
        asset_files(&root.join(path), &mut inputs)?;
    }
    inputs.sort();
    let mut hash = Sha256::new();
    for input in inputs {
        let relative = input
            .strip_prefix(&root)?
            .to_str()
            .ok_or("non-UTF8 asset compiler input path")?;
        let content = fs::read(&input)?;
        hash.update(u32::try_from(relative.len())?.to_be_bytes());
        hash.update(relative.as_bytes());
        hash.update(u32::try_from(content.len())?.to_be_bytes());
        hash.update(content);
    }
    let digest = hash
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>();
    println!("cargo:rustc-env=ERP_UI_COMPILER_ID={digest}");
    Ok(())
}

fn main() -> Result<(), Box<dyn Error>> {
    native_identity()?;
    asset_identity()?;
    Ok(())
}
