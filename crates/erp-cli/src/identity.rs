//! Owner-operated addon identity enrollment and forward activation publication.
//! These commands never run through request credentials, an RPC handler or implicit load recovery.
use std::collections::BTreeMap;
use std::io::Read;
use std::path::Path;

use erp_modules::{ModuleName, artifact_identity::ArtifactSnapshot};
use erp_server::config::ServerConfig;
use serde_json::Value;

use crate::args::ModuleAction;
use crate::{CliError, Result, access::pool_with_size, node::Node};

mod linked {
    include!(concat!(env!("OUT_DIR"), "/linked_addon_identities.rs"));
}

fn refused(message: impl Into<String>) -> CliError {
    CliError::Execution(message.into())
}
fn module_error(error: erp_modules::Error) -> CliError {
    CliError::ModuleOperation(Box::new(error))
}

/// The per-addon source receipts embedded when this binary was built. These attest linked
/// addon sources independently of unrelated binary changes; they are not executable digests.
/// # Errors
/// Invalid generated names are a build-provenance error, never a silent missing identity.
pub fn native_artifact_identities() -> Result<BTreeMap<ModuleName, String>> {
    linked::LINKED_ADDON_IDENTITIES
        .iter()
        .map(|(name, hash)| {
            Ok((
                ModuleName::new(*name).map_err(module_error)?,
                (*hash).to_owned(),
            ))
        })
        .collect()
}

/// Resolve an operator-selected search directory once (not an addon discovered inside it).
/// This permits the deployment's current pointer and the host's temporary-directory prefix;
/// discovered addon roots and their contents still undergo no-follow snapshot admission.
pub(crate) fn configured_addon_path(path: &Path) -> Result<std::path::PathBuf> {
    let physical = match path.file_name() {
        Some(name) => {
            let parent = match path.parent() {
                Some(parent) if !parent.as_os_str().is_empty() => parent,
                Some(_) | None => Path::new("."), // A bare operator-selected name is relative to cwd.
            };
            std::fs::canonicalize(parent)
                .map_err(|_| refused("Cannot resolve configured addon search path"))?
                .join(name)
        }
        None => std::fs::canonicalize(path)
            .map_err(|_| refused("Cannot resolve configured addon search path"))?,
    };
    let metadata = std::fs::symlink_metadata(&physical)
        .map_err(|_| refused("Configured addon search path is unavailable"))?;
    if metadata.file_type().is_symlink() {
        return Err(refused("Configured addon artifact root is a symbolic link"));
    }
    Ok(physical)
}

// A package receipt is an authorization input. Duplicate keys are ambiguous even when a JSON
// library would silently retain the final value. Retain serde_json's default recursion limit.
struct UniqueJson(Value);
impl<'de> serde::Deserialize<'de> for UniqueJson {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        struct Visitor;
        impl<'de> serde::de::Visitor<'de> for Visitor {
            type Value = UniqueJson;
            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("JSON without duplicate object keys")
            }
            fn visit_map<M: serde::de::MapAccess<'de>>(
                self,
                mut access: M,
            ) -> std::result::Result<Self::Value, M::Error> {
                let mut object = serde_json::Map::new();
                while let Some((key, value)) = access.next_entry::<String, UniqueJson>()? {
                    if object.insert(key, value.0).is_some() {
                        return Err(serde::de::Error::custom("duplicate object key"));
                    }
                }
                Ok(UniqueJson(Value::Object(object)))
            }
            fn visit_seq<S: serde::de::SeqAccess<'de>>(
                self,
                mut access: S,
            ) -> std::result::Result<Self::Value, S::Error> {
                let mut values = Vec::new();
                while let Some(value) = access.next_element::<UniqueJson>()? {
                    values.push(value.0);
                }
                Ok(UniqueJson(Value::Array(values)))
            }
            fn visit_bool<E: serde::de::Error>(
                self,
                value: bool,
            ) -> std::result::Result<Self::Value, E> {
                Ok(UniqueJson(value.into()))
            }
            fn visit_i64<E: serde::de::Error>(
                self,
                value: i64,
            ) -> std::result::Result<Self::Value, E> {
                Ok(UniqueJson(value.into()))
            }
            fn visit_u64<E: serde::de::Error>(
                self,
                value: u64,
            ) -> std::result::Result<Self::Value, E> {
                Ok(UniqueJson(value.into()))
            }
            fn visit_f64<E: serde::de::Error>(
                self,
                value: f64,
            ) -> std::result::Result<Self::Value, E> {
                serde_json::Number::from_f64(value)
                    .map(|number| UniqueJson(Value::Number(number)))
                    .ok_or_else(|| serde::de::Error::custom("non-finite number"))
            }
            fn visit_str<E: serde::de::Error>(
                self,
                value: &str,
            ) -> std::result::Result<Self::Value, E> {
                Ok(UniqueJson(value.into()))
            }
            fn visit_unit<E: serde::de::Error>(self) -> std::result::Result<Self::Value, E> {
                Ok(UniqueJson(Value::Null))
            }
        }
        deserializer.deserialize_any(Visitor)
    }
}

fn bounded_bytes(path: &Path, private: bool, maximum: usize) -> Result<Vec<u8>> {
    let meta = std::fs::symlink_metadata(path)
        .map_err(|_| refused("Cannot read identity receipt metadata"))?;
    if !meta.is_file()
        || meta.file_type().is_symlink()
        || meta.len()
            > u64::try_from(maximum).map_err(|_| refused("Identity receipt size bound overflow"))?
    {
        return Err(refused(format!(
            "Identity receipt must be a regular file of at most {maximum} bytes"
        )));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
        if meta.nlink() != 1 || (private && meta.permissions().mode() & 0o077 != 0) {
            return Err(refused(
                "Identity receipt must have one link and protected operator receipts must be private",
            ));
        }
        let mut options = std::fs::OpenOptions::new();
        options
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
        let mut file = options
            .open(path)
            .map_err(|_| refused("Cannot open identity receipt without following links"))?;
        let opened = file
            .metadata()
            .map_err(|_| refused("Cannot verify opened identity receipt"))?;
        if !opened.is_file()
            || opened.dev() != meta.dev()
            || opened.ino() != meta.ino()
            || opened.nlink() != 1
            || (private && opened.permissions().mode() & 0o077 != 0)
        {
            return Err(refused("Identity receipt changed while opening"));
        }
        let mut bytes = Vec::new();
        let read_limit = maximum
            .checked_add(1)
            .ok_or_else(|| refused("Identity receipt size bound overflow"))?;
        let read_limit = u64::try_from(read_limit)
            .map_err(|_| refused("Identity receipt size bound overflow"))?;
        Read::by_ref(&mut file)
            .take(read_limit)
            .read_to_end(&mut bytes)
            .map_err(|_| refused("Cannot read identity receipt"))?;
        if bytes.len() > maximum {
            return Err(refused(format!("Identity receipt exceeds {maximum} bytes")));
        }
        let after = file
            .metadata()
            .map_err(|_| refused("Cannot recheck identity receipt"))?;
        if opened.len() != after.len()
            || opened.mtime() != after.mtime()
            || opened.mtime_nsec() != after.mtime_nsec()
            || opened.ctime() != after.ctime()
            || opened.ctime_nsec() != after.ctime_nsec()
        {
            return Err(refused("Identity receipt changed while reading"));
        }
        Ok(bytes)
    }
    #[cfg(not(unix))]
    {
        let _ = private;
        Err(refused(
            "Pinned identity receipt admission requires a supported Unix host",
        ))
    }
}

fn bounded_json(path: &Path, private: bool) -> Result<Value> {
    let bytes = bounded_bytes(path, private, 1_048_576)?;
    serde_json::from_slice::<UniqueJson>(&bytes)
        .map(|value| value.0)
        .map_err(|_| refused("Invalid identity receipt JSON"))
}

fn revision(root: &Path) -> Result<String> {
    let bytes = bounded_bytes(&root.join("REVISION"), false, 41)?;
    let text = std::str::from_utf8(&bytes)
        .map_err(|_| refused("Release REVISION must be 40 lowercase hexadecimal characters"))?;
    let value = text.trim();
    if value.len() != 40
        || !value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err(refused(
            "Release REVISION must be 40 lowercase hexadecimal characters",
        ));
    }
    Ok(value.to_owned())
}

/// Verify per-addon source receipts from a prepared release against bounded admitted bytes.
/// The root must already identify the explicit immutable release, rather than its `current` link.
/// # Errors
/// Refuses malformed or absent receipts, missing source files, links, changed bytes and bounds.
pub fn release_identities(root: &Path) -> Result<BTreeMap<ModuleName, String>> {
    let receipt = release_receipt(root)?;
    let hashes = receipt
        .get("sha256")
        .and_then(Value::as_object)
        .ok_or_else(|| refused("Release file hashes are missing"))?;
    let addons = match receipt.get("addon_identities") {
        Some(value) => value
            .as_object()
            .ok_or_else(|| refused("Release per-addon identities must be an object"))?,
        None => {
            return Err(refused(
                "Release per-addon identities are missing; prepare the release with the identity-aware packager",
            ));
        }
    };
    if addons.len() > 64 || addons.is_empty() {
        return Err(refused(
            "Release identity supports between 1 and 64 addon receipts",
        ));
    }
    let mut result = BTreeMap::new();
    let mut total_bytes = 0usize;
    for (name, expected) in addons {
        let name = ModuleName::new(name).map_err(module_error)?;
        let snapshot =
            ArtifactSnapshot::capture(name.as_str(), &root.join("addons").join(name.as_str()))
                .map_err(module_error)?;
        if expected.as_str() != Some(snapshot.digest()) {
            return Err(refused(format!(
                "Release addon '{}' identity differs from its admitted files",
                name
            )));
        }
        for (path, bytes) in snapshot.files() {
            total_bytes = total_bytes
                .checked_add(bytes.len())
                .ok_or_else(|| refused("Release addon byte count overflow"))?;
            if total_bytes > 67_108_864 {
                return Err(refused(
                    "Release addon snapshots exceed the 67108864-byte operation limit",
                ));
            }
            use sha2::{Digest, Sha256};
            let digest: String = Sha256::digest(bytes)
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect();
            let key = format!("addons/{name}/{path}");
            if hashes.get(&key).and_then(Value::as_str) != Some(digest.as_str()) {
                return Err(refused(format!(
                    "Release addon file receipt does not match '{key}'"
                )));
            }
        }
        snapshot.verify_unchanged().map_err(module_error)?;
        result.insert(name, snapshot.digest().to_owned());
    }
    Ok(result)
}

/// An absent identity field is the sole legacy discriminator. Corruption never selects legacy.
#[derive(Debug)]
pub(crate) enum PreviousRelease {
    Verified(BTreeMap<ModuleName, String>),
    Legacy,
}

fn release_receipt(root: &Path) -> Result<Value> {
    let metadata =
        std::fs::symlink_metadata(root).map_err(|_| refused("Release root is unavailable"))?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(refused(
            "Release root must be an immutable directory, not a symbolic link",
        ));
    }
    let receipt = bounded_json(&root.join("PACKAGE.json"), false)?;
    let source = revision(root)?;
    if receipt.get("source").and_then(Value::as_str) != Some(source.as_str()) {
        return Err(refused(
            "Release identity receipt source differs from REVISION",
        ));
    }
    let hashes = receipt
        .get("sha256")
        .and_then(Value::as_object)
        .ok_or_else(|| refused("Release file hashes are missing"))?;
    if hashes.is_empty() || hashes.len() > 40_000 {
        return Err(refused(
            "Release file receipt requires between 1 and 40000 hashes",
        ));
    }
    for (path, value) in hashes {
        if path.is_empty()
            || path
                .split('/')
                .any(|part| part.is_empty() || part == "." || part == "..")
            || path.contains('\\')
        {
            return Err(refused("Release file receipt path is invalid"));
        }
        let hash = value
            .as_str()
            .ok_or_else(|| refused("Release file hash must be a SHA256 string"))?;
        if hash.len() != 64
            || !hash
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(refused("Release file hash must be a SHA256 string"));
        }
    }
    Ok(receipt)
}

/// Validate old package file receipts, without borrowing candidate bytes or treating them as
/// installed preimages. Only explicit bootstrap enrollment consumes the resulting file hashes.
fn legacy_release_hashes(root: &Path) -> Result<BTreeMap<ModuleName, String>> {
    let receipt = release_receipt(root)?;
    if receipt.get("addon_identities").is_some() {
        return Err(refused(
            "Legacy bootstrap requires a previous release without addon_identities",
        ));
    }
    let hashes = receipt
        .get("sha256")
        .and_then(Value::as_object)
        .ok_or_else(|| refused("Release file hashes are missing"))?;
    let mut declared = BTreeMap::<ModuleName, usize>::new();
    for path in hashes.keys() {
        if let Some(path) = path.strip_prefix("addons/") {
            let (name, _) = path
                .split_once('/')
                .ok_or_else(|| refused("Release addon file receipt path is invalid"))?;
            let name = ModuleName::new(name).map_err(module_error)?;
            let count = declared.entry(name).or_insert(0);
            *count = count
                .checked_add(1)
                .ok_or_else(|| refused("Release file count overflow"))?;
        }
    }
    if declared.is_empty() || declared.len() > 64 {
        return Err(refused(
            "Release identity supports between 1 and 64 addon receipts",
        ));
    }
    let mut result = BTreeMap::new();
    let mut total_bytes = 0usize;
    for (name, count) in declared {
        let snapshot =
            ArtifactSnapshot::capture(name.as_str(), &root.join("addons").join(name.as_str()))
                .map_err(module_error)?;
        if count != snapshot.files().len() {
            return Err(refused(format!(
                "Release addon '{name}' file set differs from its receipt"
            )));
        }
        for (path, bytes) in snapshot.files() {
            total_bytes = total_bytes
                .checked_add(bytes.len())
                .ok_or_else(|| refused("Release addon byte count overflow"))?;
            if total_bytes > 67_108_864 {
                return Err(refused(
                    "Release addon snapshots exceed the 67108864-byte operation limit",
                ));
            }
            use sha2::{Digest, Sha256};
            let digest: String = Sha256::digest(bytes)
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect();
            let key = format!("addons/{name}/{path}");
            if hashes.get(&key).and_then(Value::as_str) != Some(digest.as_str()) {
                return Err(refused(format!(
                    "Release addon file receipt does not match '{key}'"
                )));
            }
        }
        snapshot.verify_unchanged().map_err(module_error)?;
        result.insert(name, snapshot.digest().to_owned());
    }
    Ok(result)
}

pub(crate) fn previous_release_identities(root: &Path) -> Result<PreviousRelease> {
    let receipt = release_receipt(root)?;
    if receipt.get("addon_identities").is_some() {
        Ok(PreviousRelease::Verified(release_identities(root)?))
    } else {
        legacy_release_hashes(root)?;
        Ok(PreviousRelease::Legacy)
    }
}

fn validate_activation(candidate: &Path, previous: &Path, proof: &Path) -> Result<()> {
    let source = revision(candidate)?;
    let old = revision(previous)?;
    if source == old {
        return Err(refused(
            "Identity activation requires a forward candidate distinct from PREVIOUS",
        ));
    }
    let current = candidate
        .parent()
        .and_then(Path::parent)
        .ok_or_else(|| refused("Invalid candidate release layout"))?
        .join("current");
    let active = std::fs::canonicalize(current)
        .map_err(|_| refused("Cannot verify the active release pointer"))?;
    if active != candidate {
        return Err(refused(
            "Identity activation candidate is not the active release",
        ));
    }
    let receipt = bounded_json(proof, true)?;
    if receipt.get("source").and_then(Value::as_str) != Some(source.as_str())
        || receipt.get("previous").and_then(Value::as_str) != Some(old.as_str())
        || receipt.get("exit").and_then(Value::as_i64) != Some(0)
        || receipt
            .get("upgrade_before_activation")
            .and_then(Value::as_bool)
            != Some(true)
    {
        return Err(refused(
            "Identity activation requires a successful matching forward migration receipt",
        ));
    }
    Ok(())
}

/// Execute the explicitly owner-operated identity commands, without loading or replaying
/// pending lifecycle work. Errors preserve owner-connection confidentiality.
/// # Errors
/// Invalid input/configuration, artifact admission, lifecycle serialization, audit or database failure.
pub fn execute(action: &ModuleAction) -> Result<String> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|_| refused("Cannot start identity administration runtime"))?;
    runtime.block_on(async {
        match action {
            ModuleAction::IdentityEnroll(args) => {
                if args.installed != args.legacy_release.is_some() || (args.installed && !args.modules.is_empty()) {
                    return Err(refused("Legacy enrollment requires --installed with --legacy-release and no addon names"));
                }
                let config =
                    ServerConfig::load(&args.config).map_err(|e| refused(e.to_string()))?;
                let legacy_hashes = args.legacy_release.as_deref().map(legacy_release_hashes).transpose()?;
                let node = match &args.legacy_release {
                    Some(old) => Node::open_legacy_enrollment(pool_with_size(config.owner_connection(), 4)?, &[old.join("addons")])?,
                    None => Node::open(pool_with_size(config.owner_connection(), 4)?, config.addons(), None)?,
                };
                node.lifecycle()
                    .admit_installed_packages()
                    .await
                    .map_err(module_error)?;
                match legacy_hashes {
                    Some(hashes) => node.system().identity_enroll_legacy_installed(&hashes, &args.operator, &args.note).await,
                    None => node.system().identity_enroll(&args.modules, &args.operator, &args.note).await,
                }.map_err(module_error)?;
                Ok("Installed addon identity enrollment committed and audited.".to_owned())
            }
            ModuleAction::IdentityActivate(args) => {
                validate_activation(&args.release, &args.previous, &args.upgrade_receipt)?;
                let previous = previous_release_identities(&args.previous)?;
                let candidate = release_identities(&args.release)?;
                for (name, linked) in native_artifact_identities()? {
                    if candidate.get(&name) != Some(&linked) {
                        return Err(refused(format!(
                            "Release addon '{name}' does not match this candidate binary"
                        )));
                    }
                }
                let config =
                    ServerConfig::load(&args.config).map_err(|e| refused(e.to_string()))?;
                let node = Node::open(
                    pool_with_size(config.owner_connection(), 4)?,
                    &[args.release.join("addons")],
                    None,
                )?;
                node.lifecycle()
                    .admit_installed_packages()
                    .await
                    .map_err(module_error)?;
                let operator = format!("release:{}", revision(&args.release)?);
                let note = "Successful forward release activation";
                match previous {
                    PreviousRelease::Verified(hashes) => node.system().identity_activate(&hashes, &operator, note).await,
                    PreviousRelease::Legacy => node.system().identity_activate_enrolled(&operator, note).await,
                }.map_err(module_error)?;
                Ok("Forward addon identity activation committed and audited.".to_owned())
            }
            ModuleAction::Install(_) | ModuleAction::Upgrade(_) | ModuleAction::Uninstall(_) => {
                Err(refused("Expected an identity administration command"))
            }
        }
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic, reason = "test assertions")]
    use super::*;

    #[test]
    fn configured_prefix_resolution_keeps_artifact_root_links_refused() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let release = root.join("release");
        std::fs::create_dir_all(release.join("addons/note")).unwrap();
        std::fs::write(release.join("addons/note/manifest.toml"), "name='Note'\n").unwrap();
        std::os::unix::fs::symlink(&release, root.join("current")).unwrap();
        let admitted = configured_addon_path(&root.join("current/addons")).unwrap();
        assert_eq!(admitted, release.join("addons"));
        std::os::unix::fs::symlink(&admitted, root.join("root-link")).unwrap();
        match configured_addon_path(&root.join("root-link")).unwrap_err() {
            CliError::Execution(message) => {
                assert_eq!(message, "Configured addon artifact root is a symbolic link")
            }
            error => panic!("unexpected configured-root error: {error:?}"),
        }
        std::os::unix::fs::symlink(admitted.join("note"), admitted.join("linked_note")).unwrap();
        match ArtifactSnapshot::capture("linked_note", &admitted.join("linked_note")).unwrap_err() {
            erp_modules::Error::NodeDigest(erp_orm::node::NodeError::AddonSource {
                addon,
                message,
            }) => {
                assert_eq!(addon, "linked_note");
                assert_eq!(message, "symbolic link in artifact path");
            }
            error => panic!("unexpected artifact-root error: {error:?}"),
        }
    }

    #[test]
    fn legacy_previous_requires_absent_field_and_verified_old_files() {
        use sha2::{Digest, Sha256};
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        std::fs::create_dir_all(root.join("addons/note")).unwrap();
        let manifest = b"name='Note'\n";
        std::fs::write(root.join("addons/note/manifest.toml"), manifest).unwrap();
        let revision = "a".repeat(40);
        std::fs::write(root.join("REVISION"), format!("{revision}\n")).unwrap();
        let hash: String = Sha256::digest(manifest)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        let mut receipt =
            serde_json::json!({"source": revision, "sha256": {"addons/note/manifest.toml": hash}});
        std::fs::write(root.join("PACKAGE.json"), receipt.to_string()).unwrap();
        assert!(matches!(
            previous_release_identities(&root).unwrap(),
            PreviousRelease::Legacy
        ));
        receipt["addon_identities"] = serde_json::Value::Null;
        std::fs::write(root.join("PACKAGE.json"), receipt.to_string()).unwrap();
        match previous_release_identities(&root).unwrap_err() {
            CliError::Execution(message) => {
                assert_eq!(message, "Release per-addon identities must be an object")
            }
            error => panic!("unexpected error: {error:?}"),
        }
        receipt["addon_identities"] = serde_json::json!({});
        std::fs::write(root.join("PACKAGE.json"), receipt.to_string()).unwrap();
        match previous_release_identities(&root).unwrap_err() {
            CliError::Execution(message) => assert_eq!(
                message,
                "Release identity supports between 1 and 64 addon receipts"
            ),
            error => panic!("unexpected error: {error:?}"),
        }
        receipt.as_object_mut().unwrap().remove("addon_identities");
        let duplicate = format!(
            "{{\"source\":\"{}\",{}",
            "b".repeat(40),
            &receipt.to_string()[1..]
        );
        std::fs::write(root.join("PACKAGE.json"), duplicate).unwrap();
        match previous_release_identities(&root).unwrap_err() {
            CliError::Execution(message) => assert_eq!(message, "Invalid identity receipt JSON"),
            error => panic!("unexpected error: {error:?}"),
        }
        std::fs::write(root.join("PACKAGE.json"), receipt.to_string()).unwrap();
        std::fs::write(root.join("addons/note/manifest.toml"), b"name='Tampered'\n").unwrap();
        match previous_release_identities(&root).unwrap_err() {
            CliError::Execution(message) => assert_eq!(
                message,
                "Release addon file receipt does not match 'addons/note/manifest.toml'"
            ),
            error => panic!("unexpected error: {error:?}"),
        }
    }

    #[test]
    fn revision_metadata_refuses_unsafe_legacy_source() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let source = root.join("source");
        let target = root.join("REVISION");
        std::fs::write(&source, format!("{}\n", "a".repeat(40))).unwrap();
        std::os::unix::fs::symlink(&source, &target).unwrap();
        match revision(&root).unwrap_err() {
            CliError::Execution(message) => assert_eq!(
                message,
                "Identity receipt must be a regular file of at most 41 bytes"
            ),
            error => panic!("unexpected error: {error:?}"),
        }
        std::fs::remove_file(&target).unwrap();
        std::fs::hard_link(&source, &target).unwrap();
        match revision(&root).unwrap_err() {
            CliError::Execution(message) => assert_eq!(
                message,
                "Identity receipt must have one link and protected operator receipts must be private"
            ),
            error => panic!("unexpected error: {error:?}"),
        }
        std::fs::remove_file(&target).unwrap();
        std::fs::write(&target, format!("{}\n\n", "a".repeat(40))).unwrap();
        match revision(&root).unwrap_err() {
            CliError::Execution(message) => assert_eq!(
                message,
                "Identity receipt must be a regular file of at most 41 bytes"
            ),
            error => panic!("unexpected error: {error:?}"),
        }
    }
}
