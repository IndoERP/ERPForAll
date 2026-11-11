use crate::country::CountryCode;
use crate::dependencies::ExternalDependencies;
use crate::error::{Error, Result};
use crate::version::ModuleVersion;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeSet, HashSet};
use std::path::{Path, PathBuf};
use toml::Table;
use tracing::warn;

/// Technical module name newtype matching `^[a-zA-Z0-9_]{1,256}$`.
///
/// Note: Enforcing ASCII alphanumeric characters or underscores is divergent from
/// Python's `\w` (which matches Unicode letters in Python 3).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String")]
pub struct ModuleName(String);

impl ModuleName {
    pub fn new(name: impl Into<String>) -> Result<Self> {
        let name = name.into();
        Self::validate(&name)?;
        Ok(Self(name))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn into_inner(self) -> String {
        self.0
    }

    pub fn validate(name: &str) -> Result<()> {
        if name.is_empty() || name.len() > 256 {
            return Err(Error::InvalidModuleName {
                name: name.to_string(),
                reason: format!(
                    "length must be between 1 and 256 characters (got {})",
                    name.len()
                ),
            });
        }
        if !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
            return Err(Error::InvalidModuleName {
                name: name.to_string(),
                reason: "must contain only ASCII alphanumeric characters or underscores"
                    .to_string(),
            });
        }
        Ok(())
    }
}

impl std::ops::Deref for ModuleName {
    type Target = str;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl AsRef<str> for ModuleName {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl std::borrow::Borrow<str> for ModuleName {
    fn borrow(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for ModuleName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl TryFrom<&str> for ModuleName {
    type Error = Error;
    fn try_from(value: &str) -> Result<Self> {
        Self::new(value)
    }
}

impl TryFrom<String> for ModuleName {
    type Error = Error;
    fn try_from(value: String) -> Result<Self> {
        Self::new(value)
    }
}

impl From<ModuleName> for String {
    fn from(m: ModuleName) -> Self {
        m.0
    }
}

/// Helper function to validate technical module name without instantiating `ModuleName`.
pub fn validate_module_name(name: &str) -> Result<()> {
    ModuleName::validate(name)
}

/// Validated hook function name.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
pub struct HookName(String);

impl HookName {
    pub(crate) fn new(module: &ModuleName, name: impl Into<String>) -> Result<Self> {
        let name = name.into();
        if name.is_empty() {
            return Err(Error::InvalidManifest {
                name: module.clone(),
                message: "hook name cannot be empty".to_string(),
            });
        }
        if !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
            return Err(Error::InvalidManifest {
                name: module.clone(),
                message: format!(
                    "hook name '{name}' must contain only alphanumeric characters or underscores"
                ),
            });
        }
        Ok(Self(name))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::ops::Deref for HookName {
    type Target = str;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl AsRef<str> for HookName {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for HookName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Auto-install trigger configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum AutoInstall {
    Disabled,
    Triggers(BTreeSet<ModuleName>),
}

impl AutoInstall {
    pub fn is_enabled(&self) -> bool {
        matches!(self, AutoInstall::Triggers(_))
    }

    pub fn triggers(&self) -> Option<&BTreeSet<ModuleName>> {
        match self {
            AutoInstall::Disabled => None,
            AutoInstall::Triggers(set) => Some(set),
        }
    }

    pub fn to_set(&self) -> BTreeSet<ModuleName> {
        match self {
            AutoInstall::Disabled => BTreeSet::new(),
            AutoInstall::Triggers(set) => set.clone(),
        }
    }
}

/// Raw DTO deserialized strictly from manifest.toml.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawManifest {
    name: Option<String>,
    /// Technical name written into a `.erpaddon` manifest, where `name` stays the display name
    /// . The caller of `from_toml` already knows the technical name; a value that
    /// differs from it is refused.
    technical_name: Option<String>,
    author: Option<String>,
    license: Option<String>,
    depends: Option<Vec<String>>,
    auto_install: Option<RawAutoInstall>,
    installable: Option<bool>,
    version: Option<String>,
    summary: Option<String>,
    description: Option<String>,
    website: Option<String>,
    category: Option<String>,
    sequence: Option<i64>,
    application: Option<bool>,
    exact_only: Option<bool>,
    data: Option<Vec<String>>,
    demo: Option<Vec<String>>,
    pre_init_hook: Option<String>,
    post_init_hook: Option<String>,
    uninstall_hook: Option<String>,
    post_load: Option<toml::Value>,
    external_dependencies: Option<RawExternalDependencies>,
    countries: Option<Vec<String>>,
    excludes: Option<Vec<String>>,
    #[serde(default)]
    extra: Option<Table>,
    wasm: Option<crate::package::RawWasmSection>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum RawAutoInstall {
    Bool(bool),
    List(Vec<String>),
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawExternalDependencies {
    #[serde(default)]
    bin: Vec<String>,
    #[serde(default)]
    python: Vec<String>,
}

/// A parsed, normalized, validated and immutable module manifest.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[non_exhaustive]
pub struct Manifest {
    name: ModuleName,
    shortdesc: String,
    summary: String,
    description: String,
    author: String,
    website: String,
    license: String,
    category: String,
    sequence: i32,
    installable: bool,
    auto_install: AutoInstall,
    application: bool,
    exact_only: bool,
    version: ModuleVersion,
    depends: Vec<ModuleName>,
    data: Vec<String>,
    demo: Vec<String>,
    pre_init_hook: Option<HookName>,
    post_init_hook: Option<HookName>,
    uninstall_hook: Option<HookName>,
    external_dependencies: ExternalDependencies,
    icon: String,
    static_path: Option<PathBuf>,
    countries: Vec<CountryCode>,
    excludes: Vec<ModuleName>,
    extra: Table,
    wasm: Option<crate::package::WasmSection>,
}

impl Manifest {
    /// Pure parser: parses and normalizes a manifest from TOML content.
    ///
    /// No filesystem probes are performed here. Discovered filesystem assets
    /// (such as static path and custom icon) are attached by discovery via `set_discovered_assets`.
    pub fn from_toml(module_name: &str, content: &str) -> Result<Self> {
        let mod_name = ModuleName::new(module_name)?;

        let raw: RawManifest = toml::from_str(content).map_err(|e| Error::InvalidManifest {
            name: mod_name.clone(),
            message: e.to_string(),
        })?;

        if let Some(technical) = &raw.technical_name
            && technical != mod_name.as_str()
        {
            return Err(Error::InvalidManifest {
                name: mod_name,
                message: format!(
                    "technical_name '{technical}' differs from the module's technical name"
                ),
            });
        }

        // post_load is rejected citing
        if raw.post_load.is_some() {
            return Err(Error::InvalidManifest {
                name: mod_name,
                message: "post_load is not supported".to_string(),
            });
        }

        // Sequence must fit in i32
        let sequence = match raw.sequence {
            Some(seq) => i32::try_from(seq).map_err(|_| Error::InvalidManifest {
                name: mod_name.clone(),
                message: format!("sequence value {seq} is out of range for i32"),
            })?,
            None => 100,
        };

        let shortdesc = raw.name.unwrap_or_else(|| mod_name.to_string());

        let author = match raw.author {
            Some(a) => a,
            None => {
                warn!(
                    "Missing `author` key in manifest for '{}', defaulting to ''",
                    mod_name
                );
                String::new()
            }
        };

        let license = match raw.license {
            Some(l) => l,
            None => {
                warn!(
                    "Missing `license` key in manifest for '{}', defaulting to LGPL-3",
                    mod_name
                );
                "LGPL-3".to_string()
            }
        };

        // Depends invariants:
        // 1. 'base' cannot depend on anything (forced to [])
        // 2. non-base modules default to ['base'] if empty/omitted
        // 3. no self dependency
        // 4. no duplicate dependencies
        let depends = if mod_name.as_str() == "base" {
            Vec::new()
        } else {
            let raw_deps = raw.depends.unwrap_or_default();
            if raw_deps.is_empty() {
                vec![ModuleName::new("base")?]
            } else {
                let mut deps = Vec::with_capacity(raw_deps.len());
                let mut seen = HashSet::new();
                for d in raw_deps {
                    let dep_name = ModuleName::new(&d)?;
                    if dep_name == mod_name {
                        return Err(Error::SelfDependency {
                            module: mod_name.clone(),
                        });
                    }
                    if !seen.insert(dep_name.clone()) {
                        return Err(Error::DuplicateDependency {
                            module: mod_name.clone(),
                            dependency: dep_name,
                        });
                    }
                    deps.push(dep_name);
                }
                deps
            }
        };

        // Auto install:
        let auto_install = match raw.auto_install {
            None => AutoInstall::Disabled,
            Some(RawAutoInstall::Bool(false)) => AutoInstall::Disabled,
            Some(RawAutoInstall::Bool(true)) => {
                AutoInstall::Triggers(depends.iter().cloned().collect())
            }
            Some(RawAutoInstall::List(triggers_list)) => {
                let mut triggers = BTreeSet::new();
                let depends_set: HashSet<&ModuleName> = depends.iter().collect();
                let mut non_deps = Vec::new();

                for t in triggers_list {
                    let trigger_mod = ModuleName::new(&t)?;
                    if !depends_set.contains(&trigger_mod) {
                        non_deps.push(trigger_mod.clone());
                    }
                    triggers.insert(trigger_mod);
                }

                if !non_deps.is_empty() {
                    return Err(Error::AutoInstallTriggerMismatch {
                        name: mod_name.clone(),
                        non_dependencies: non_deps,
                    });
                }
                AutoInstall::Triggers(triggers)
            }
        };

        let mut installable = raw.installable.unwrap_or(true);

        // Version adaptation
        let version = if let Some(ref v) = raw.version {
            match ModuleVersion::parse(&mod_name, v) {
                Ok(mv) => {
                    if !mv.is_compatible() && installable {
                        warn!(
                            "The module {} has an incompatible version, setting installable=False",
                            mod_name
                        );
                        installable = false;
                    }
                    mv
                }
                Err(e) => {
                    if installable {
                        return Err(e);
                    }
                    ModuleVersion::incompatible(v, "invalid version on non-installable module")
                }
            }
        } else {
            ModuleVersion::default_version()
        };

        let summary = raw.summary.unwrap_or_default();
        let description = raw.description.unwrap_or_default();
        let website = raw.website.unwrap_or_default();
        let category = raw.category.unwrap_or_else(|| "Uncategorized".to_string());
        crate::lifecycle::CategoryPath::parse(&category).map_err(|e| Error::InvalidManifest {
            name: mod_name.clone(),
            message: e.to_string(),
        })?;
        let application = raw.application.unwrap_or(false);
        let mandatory_exact = erp_registry::AddonDescriptor::requires_exact(mod_name.as_str());
        if mandatory_exact && raw.exact_only == Some(false) {
            return Err(Error::InvalidManifest {
                name: mod_name.clone(),
                message: "exact_only cannot be disabled for this addon".to_owned(),
            });
        }
        // An absent policy defaults only as specified by approved L-9.
        let exact_only = raw.exact_only.unwrap_or(mandatory_exact);
        let data = raw.data.unwrap_or_default();
        let demo = raw.demo.unwrap_or_default();

        let parse_hook = |opt: &Option<String>| -> Result<Option<HookName>> {
            match opt {
                Some(h) => HookName::new(&mod_name, h).map(Some),
                None => Ok(None),
            }
        };
        let pre_init_hook = parse_hook(&raw.pre_init_hook)?;
        let post_init_hook = parse_hook(&raw.post_init_hook)?;
        let uninstall_hook = parse_hook(&raw.uninstall_hook)?;

        let validate_paths = |field: &str, paths: &[String]| -> Result<()> {
            for p in paths {
                let path = Path::new(p);
                if path.is_absolute() || path.has_root() {
                    return Err(Error::InvalidManifest {
                        name: mod_name.clone(),
                        message: format!("{field} path '{p}' cannot be absolute"),
                    });
                }
                if path
                    .components()
                    .any(|c| c == std::path::Component::ParentDir)
                {
                    return Err(Error::InvalidManifest {
                        name: mod_name.clone(),
                        message: format!("{field} path '{p}' cannot contain '..'"),
                    });
                }
            }
            Ok(())
        };
        validate_paths("data", &data)?;
        validate_paths("demo", &demo)?;

        let external_dependencies = match raw.external_dependencies {
            Some(raw_ed) => ExternalDependencies {
                bin: raw_ed.bin,
                python: raw_ed.python,
            },
            None => ExternalDependencies::default(),
        };

        let countries = raw
            .countries
            .unwrap_or_default()
            .iter()
            .map(|code| CountryCode::parse(code))
            .collect::<Result<Vec<_>>>()
            .map_err(|e| Error::InvalidManifest {
                name: mod_name.clone(),
                message: e.to_string(),
            })?;

        let excludes = match raw.excludes {
            Some(ex_list) => {
                let mut res = Vec::with_capacity(ex_list.len());
                let mut seen = HashSet::new();
                for e in ex_list {
                    let ex_name = ModuleName::new(&e)?;
                    if ex_name == mod_name {
                        return Err(Error::SelfExclusion {
                            module: mod_name.clone(),
                        });
                    }
                    if !seen.insert(ex_name.clone()) {
                        return Err(Error::DuplicateExclusion {
                            module: mod_name.clone(),
                            exclusion: ex_name,
                        });
                    }
                    res.push(ex_name);
                }
                res
            }
            None => Vec::new(),
        };

        let icon = "/base/static/description/icon.png".to_string();
        let static_path = None;
        let extra = raw.extra.unwrap_or_default();

        let wasm = if let Some(raw_wasm) = raw.wasm {
            let sdk_version = match raw_wasm.sdk_version {
                Some(ver) if !ver.trim().is_empty() => ver,
                _ => {
                    return Err(Error::InvalidManifest {
                        name: mod_name,
                        message: "missing or empty sdk_version in [wasm] section".to_string(),
                    });
                }
            };
            let mut capabilities = Vec::with_capacity(raw_wasm.capabilities.len());
            for cap_str in raw_wasm.capabilities {
                let cap = crate::package::WasmCapability::parse(&cap_str)?;
                capabilities.push(cap);
            }
            Some(crate::package::WasmSection {
                sdk_version,
                capabilities,
            })
        } else {
            None
        };

        Ok(Self {
            name: mod_name,
            shortdesc,
            summary,
            description,
            author,
            website,
            license,
            category,
            sequence,
            installable,
            auto_install,
            application,
            exact_only,
            version,
            depends,
            data,
            demo,
            pre_init_hook,
            post_init_hook,
            uninstall_hook,
            external_dependencies,
            icon,
            static_path,
            countries,
            excludes,
            extra,
            wasm,
        })
    }

    /// Internal helper used by discovery to attach detected filesystem assets.
    pub(crate) fn set_discovered_assets(
        &mut self,
        static_path: Option<PathBuf>,
        icon: Option<String>,
    ) {
        if let Some(sp) = static_path {
            self.static_path = Some(sp);
        }
        if let Some(ic) = icon {
            self.icon = ic;
        }
    }

    pub fn name(&self) -> &ModuleName {
        &self.name
    }

    pub fn shortdesc(&self) -> &str {
        &self.shortdesc
    }

    pub fn summary(&self) -> &str {
        &self.summary
    }

    pub fn description(&self) -> &str {
        &self.description
    }

    pub fn author(&self) -> &str {
        &self.author
    }

    pub fn website(&self) -> &str {
        &self.website
    }

    pub fn license(&self) -> &str {
        &self.license
    }

    pub fn category(&self) -> &str {
        &self.category
    }

    pub fn sequence(&self) -> i32 {
        self.sequence
    }

    pub fn installable(&self) -> bool {
        self.installable
    }

    pub fn auto_install(&self) -> &AutoInstall {
        &self.auto_install
    }

    /// Whether this addon requires exact numeric columns (approved L-9).
    pub fn exact_only(&self) -> bool {
        self.exact_only
    }

    pub fn application(&self) -> bool {
        self.application
    }

    pub fn version(&self) -> &ModuleVersion {
        &self.version
    }

    pub fn depends(&self) -> &[ModuleName] {
        &self.depends
    }

    pub fn data(&self) -> &[String] {
        &self.data
    }

    pub fn demo(&self) -> &[String] {
        &self.demo
    }

    pub fn pre_init_hook(&self) -> Option<&HookName> {
        self.pre_init_hook.as_ref()
    }

    pub fn post_init_hook(&self) -> Option<&HookName> {
        self.post_init_hook.as_ref()
    }

    pub fn uninstall_hook(&self) -> Option<&HookName> {
        self.uninstall_hook.as_ref()
    }

    pub fn external_dependencies(&self) -> &ExternalDependencies {
        &self.external_dependencies
    }

    pub fn icon(&self) -> &str {
        &self.icon
    }

    pub fn static_path(&self) -> Option<&Path> {
        self.static_path.as_deref()
    }

    pub fn countries(&self) -> &[CountryCode] {
        &self.countries
    }

    pub fn excludes(&self) -> &[ModuleName] {
        &self.excludes
    }

    pub fn extra(&self) -> &Table {
        &self.extra
    }

    pub fn wasm(&self) -> Option<&crate::package::WasmSection> {
        self.wasm.as_ref()
    }
}
