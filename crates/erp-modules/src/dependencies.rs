use crate::error::{Error, ModuleOperation, Result};
use crate::manifest::ModuleName;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExternalDependencies {
    pub bin: Vec<String>,
    pub python: Vec<String>,
}

impl ExternalDependencies {
    /// Check external binary dependencies against system PATH.
    ///
    /// Per and:
    /// - python external dependencies are n/a-by-construction in Rust.
    /// - bin dependencies must be present on PATH; if missing, returns ExternalDependencyMissing
    ///   naming `operation`.
    pub fn check(&self, module: &ModuleName, operation: ModuleOperation) -> Result<()> {
        for bin in &self.bin {
            if !is_binary_on_path(bin) {
                return Err(Error::ExternalDependencyMissing {
                    module: module.clone(),
                    dependency: bin.clone(),
                    operation,
                });
            }
        }
        Ok(())
    }
}

/// Check if an executable binary with the given name exists on PATH.
pub fn is_binary_on_path(bin: &str) -> bool {
    if let Some(path_var) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path_var) {
            let candidate = dir.join(bin);
            if candidate.is_file() {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    if let Ok(metadata) = candidate.metadata()
                        && metadata.permissions().mode() & 0o111 != 0
                    {
                        return true;
                    }
                }
                #[cfg(not(unix))]
                {
                    return true;
                }
            }
        }
    }
    false
}
