//! Module scheduling rules on an in-memory catalog (MOD §4.2).
//!
//! Every rule fails loudly: a refused request leaves the catalog unchanged (each method works on a
//! copy of the states and commits it only when the whole request succeeded).

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::Arc;

use crate::country::CountryCode;
use crate::error::{CategoryMember, Error, ModuleOperation, Result};
use crate::graph::ModuleState;
use crate::manifest::{Manifest, ModuleName};
use crate::version::SemverVersion;

/// Nesting limit of `button_install`'s dependency marking (`_state_update(level=100)`).
pub const MAX_DEPENDENCY_DEPTH: usize = 100;

pub const SERVER_WIDE_MODULES: [&str; 3] = ["base", "rpc", "web"];

/// Never pulled into an upgrade of its dependencies.
pub const STUDIO_CUSTOMIZATION: &str = "studio_customization";

/// Longest category path accepted (segments).
pub const MAX_CATEGORY_DEPTH: usize = 16;

/// A category path, e.g. `["Sales & CRM", "Tools"]` for `category = "Sales & CRM/Tools"`.
///
/// Never empty; no segment is empty, contains `/` or has leading or trailing whitespace
/// (validated by [`CategoryPath::parse`] and [`CategoryPath::from_segments`]). `Display` joins
/// the segments with `/`, so `parse(path.to_string())` gives the path back.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CategoryPath(Vec<String>);

impl CategoryPath {
    /// Split a manifest category on `/` (`ir_module.py` `_update_category`).
    ///
    /// # Errors
    /// [`Error::InvalidCategory`] for an empty category, an empty segment (`"A//B"`, `"A/"`), a
    /// segment with leading or trailing whitespace (`"A / B"`) or more than
    /// [`MAX_CATEGORY_DEPTH`] segments.
    ///
    /// ```
    /// use erp_modules::lifecycle::CategoryPath;
    /// let path = CategoryPath::parse("Sales & CRM/Tools").unwrap();
    /// assert_eq!(path.name(), "Tools");
    /// assert!(CategoryPath::parse("Sales//Tools").is_err());
    /// assert!(CategoryPath::parse("Sales / Tools").is_err());
    /// ```
    pub fn parse(category: &str) -> Result<Self> {
        Self::from_segments(category.split('/').map(str::to_owned).collect())
    }

    /// From stored segments, root first.
    ///
    /// # Errors
    /// As [`CategoryPath::parse`]; a segment containing `/` is refused too (it could not be
    /// written back as a manifest category).
    ///
    /// ```
    /// use erp_modules::lifecycle::CategoryPath;
    /// assert!(CategoryPath::from_segments(vec!["A".into(), "B".into()]).is_ok());
    /// assert!(CategoryPath::from_segments(vec!["A/B".into()]).is_err());
    /// ```
    pub fn from_segments(segments: Vec<String>) -> Result<Self> {
        let invalid = |message: &str| Error::InvalidCategory {
            category: segments.join("/"),
            message: message.to_owned(),
        };
        if segments.is_empty() || segments.iter().all(String::is_empty) {
            return Err(invalid("the category is empty"));
        }
        if segments.iter().any(String::is_empty) {
            return Err(invalid("a category segment is empty"));
        }
        if segments.iter().any(|s| s.contains('/')) {
            return Err(invalid("a category segment contains '/'"));
        }
        if segments.iter().any(|s| s.trim() != s) {
            return Err(invalid(
                "a category segment has leading or trailing whitespace",
            ));
        }
        if segments.len() > MAX_CATEGORY_DEPTH {
            return Err(invalid(&format!(
                "more than {MAX_CATEGORY_DEPTH} nested categories"
            )));
        }
        Ok(Self(segments))
    }

    /// The segments, root first (never empty).
    pub fn segments(&self) -> &[String] {
        &self.0
    }

    /// The category's own name (last segment).
    pub fn name(&self) -> &str {
        // The constructors refuse an empty path, so `split_last` always matches.
        match self.0.split_last() {
            Some((last, _)) => last,
            None => "",
        }
    }

    /// The path itself and its ancestors, root first (`["A"]`, `["A", "B"]`, …).
    pub fn chain(&self) -> Vec<CategoryPath> {
        (1..=self.0.len())
            .map(|n| CategoryPath(self.0.iter().take(n).cloned().collect()))
            .collect()
    }

    /// True when `self` is `ancestor` or one of its sub-categories (`child_of`).
    pub fn is_within(&self, ancestor: &CategoryPath) -> bool {
        self.0.starts_with(&ancestor.0)
    }

    /// The external ID name of the category: `module_category_<segments>` (lower case, `&` →
    /// `and`, space → `_`, joined by `_`). The module of the
    /// external ID is `base`.
    pub fn xml_id_name(&self) -> String {
        let parts: Vec<String> = self
            .0
            .iter()
            .map(|s| s.to_lowercase().replace('&', "and").replace(' ', "_"))
            .collect();
        format!("module_category_{}", parts.join("_"))
    }

    /// Refuse a set of category paths (and their ancestors) in which two different paths share
    /// an external id name ([`CategoryPath::xml_id_name`]).
    ///
    /// # Errors
    /// [`Error::CategoryIdCollision`] naming the first colliding pair.
    ///
    /// ```
    /// use erp_modules::lifecycle::CategoryPath;
    /// let a_b = CategoryPath::parse("A/B").unwrap();
    /// let a_space_b = CategoryPath::parse("A B").unwrap();
    /// assert!(CategoryPath::check_xml_id_collisions([&a_b, &a_b]).is_ok());
    /// let err = CategoryPath::check_xml_id_collisions([&a_b, &a_space_b]).unwrap_err();
    /// assert_eq!(
    ///     err.to_string(),
    ///     "Module categories \"A/B\" and \"A B\" would share the external id \
    ///      'base.module_category_a_b'; rename one of them"
    /// );
    /// ```
    pub fn check_xml_id_collisions<'a>(
        paths: impl IntoIterator<Item = &'a CategoryPath>,
    ) -> Result<()> {
        let mut seen: BTreeMap<String, CategoryPath> = BTreeMap::new();
        for path in paths {
            for prefix in path.chain() {
                let xml_id = prefix.xml_id_name();
                match seen.get(&xml_id) {
                    Some(other) if *other != prefix => {
                        return Err(Error::CategoryIdCollision {
                            first: other.clone(),
                            second: prefix,
                            xml_id,
                        });
                    }
                    Some(_) => {}
                    None => {
                        seen.insert(xml_id, prefix);
                    }
                }
            }
        }
        Ok(())
    }
}

impl std::fmt::Display for CategoryPath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0.join("/"))
    }
}

/// The manifest fields copied to a module row (`get_values_from_terp`).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct ModuleInfo {
    /// The manifest `name`.
    pub shortdesc: String,
    /// `summary`.
    pub summary: String,
    /// `description`.
    pub description: String,
    /// `author`.
    pub author: String,
    /// `website`.
    pub website: String,
    /// `license`.
    pub license: String,
    /// `sequence`.
    pub sequence: i32,
    /// `application`.
    pub application: bool,
    /// Whether the manifest has any `auto_install` (a trigger list or `true`).
    pub auto_install: bool,
    /// The icon path.
    pub icon: String,
}

impl ModuleInfo {
    /// Empty values (the store fills them from a row).
    pub(crate) fn empty() -> Self {
        Self {
            shortdesc: String::new(),
            summary: String::new(),
            description: String::new(),
            author: String::new(),
            website: String::new(),
            license: String::new(),
            sequence: 100,
            application: false,
            auto_install: false,
            icon: String::new(),
        }
    }

    /// The row values a manifest gives.
    pub fn from_manifest(manifest: &Manifest) -> Self {
        Self {
            shortdesc: manifest.shortdesc().to_owned(),
            summary: manifest.summary().to_owned(),
            description: manifest.description().to_owned(),
            author: manifest.author().to_owned(),
            website: manifest.website().to_owned(),
            license: manifest.license().to_owned(),
            sequence: manifest.sequence(),
            application: manifest.application(),
            auto_install: manifest.auto_install().is_enabled(),
            icon: manifest.icon().to_owned(),
        }
    }
}

/// One `ir.module.module.dependency` row.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct DependencyRow {
    name: ModuleName,
    auto_install_required: bool,
}

impl DependencyRow {
    /// A dependency on `name`; `auto_install_required` when it is an `auto_install` trigger.
    pub fn new(name: ModuleName, auto_install_required: bool) -> Self {
        Self {
            name,
            auto_install_required,
        }
    }

    /// The name of the module depended on (the row stores the name, not a reference: the
    /// dependency may be unknown).
    pub fn name(&self) -> &ModuleName {
        &self.name
    }

    /// True when the dependency is an `auto_install` trigger.
    pub fn auto_install_required(&self) -> bool {
        self.auto_install_required
    }
}

/// A module row as stored in the database, the input of [`ModuleRow::from_stored`].
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct StoredModule {
    /// `name`.
    pub name: ModuleName,
    /// `state` (never `unavailable`).
    pub state: ModuleState,
    /// `latest_version`: the installed version.
    pub latest_version: Option<SemverVersion>,
    /// `demo`.
    pub demo: bool,
    /// The info columns.
    pub info: ModuleInfo,
    /// The category path of `category_id`.
    pub category: CategoryPath,
    /// The `ir.module.module.dependency` rows.
    pub depends: Vec<DependencyRow>,
    /// The `ir.module.module.exclusion` rows.
    pub excludes: BTreeSet<ModuleName>,
    /// ISO country codes.
    pub countries: BTreeSet<CountryCode>,
}

impl StoredModule {
    /// A stored row with no dependencies, exclusions or countries.
    pub fn new(
        name: ModuleName,
        state: ModuleState,
        info: ModuleInfo,
        category: CategoryPath,
    ) -> Self {
        Self {
            name,
            state,
            latest_version: None,
            demo: false,
            info,
            category,
            depends: Vec::new(),
            excludes: BTreeSet::new(),
            countries: BTreeSet::new(),
        }
    }

    /// The row `update_list` would store for `manifest`, in `state` (tests and tools that build a
    /// catalog in a given state; the module system reads rows from the database).
    ///
    /// # Errors
    /// [`Error::InvalidCategory`] when the manifest category is not a valid path.
    pub fn from_manifest(manifest: &Manifest, state: ModuleState) -> Result<Self> {
        let triggers = manifest.auto_install().to_set();
        let mut stored = Self::new(
            manifest.name().clone(),
            state,
            ModuleInfo::from_manifest(manifest),
            CategoryPath::parse(manifest.category())?,
        );
        stored.depends = manifest
            .depends()
            .iter()
            .map(|d| DependencyRow::new(d.clone(), triggers.contains(d)))
            .collect();
        stored.excludes = manifest.excludes().iter().cloned().collect();
        stored.countries = manifest.countries().iter().cloned().collect();
        Ok(stored)
    }
}

/// One `ir.module.module` row with its satellite rows.
#[derive(Debug, Clone)]
pub struct ModuleRow {
    name: ModuleName,
    state: ModuleState,
    latest_version: Option<SemverVersion>,
    demo: bool,
    info: ModuleInfo,
    category: CategoryPath,
    depends: Vec<DependencyRow>,
    excludes: BTreeSet<ModuleName>,
    countries: BTreeSet<CountryCode>,
    manifest: Option<Arc<Manifest>>,
}

impl ModuleRow {
    /// Whether the row is a WebAssembly package addon (its manifest has a `[wasm]` section). Its
    /// code and data change only through the package lifecycle, which records each change in the
    /// audit table, so a native upgrade leaves it out of the modules it upgrades with a dependency
    /// (it stays installed, with the package that is stored).
    fn is_package(&self) -> bool {
        self.manifest.as_ref().is_some_and(|m| m.wasm().is_some())
    }

    /// A new row for a discovered manifest (`uninstalled`, or `uninstallable`).
    ///
    /// # Errors
    /// [`Error::InvalidCategory`] when the manifest category is not a valid path.
    pub fn from_manifest(manifest: Arc<Manifest>) -> Result<Self> {
        let mut row = Self {
            name: manifest.name().clone(),
            state: if manifest.installable() {
                ModuleState::Uninstalled
            } else {
                ModuleState::Uninstallable
            },
            latest_version: None,
            demo: false,
            info: ModuleInfo::from_manifest(&manifest),
            category: CategoryPath::parse(manifest.category())?,
            depends: Vec::new(),
            excludes: BTreeSet::new(),
            countries: BTreeSet::new(),
            manifest: None,
        };
        row.sync_from_manifest(manifest)?;
        Ok(row)
    }

    /// A row loaded from the database, without a manifest until
    /// [`Catalog::attach_manifest`] or [`Catalog::update_list`] gives it one.
    pub fn from_stored(stored: StoredModule) -> Result<Self> {
        if stored.state == ModuleState::Unavailable {
            return Err(Error::UnavailableIsNotStored {
                module: stored.name,
            });
        }
        let mut depends = stored.depends;
        depends.sort();
        if depends.iter().any(|d| d.name == stored.name) {
            return Err(Error::SelfDependency {
                module: stored.name,
            });
        }
        if let Some(pair) = depends.windows(2).find(|w| w[0].name == w[1].name) {
            return Err(Error::DuplicateDependency {
                dependency: pair[0].name.clone(),
                module: stored.name,
            });
        }
        if stored.excludes.contains(&stored.name) {
            return Err(Error::SelfExclusion {
                module: stored.name,
            });
        }
        if stored.state == ModuleState::Uninstalled && stored.latest_version.is_some() {
            return Err(Error::InvalidModuleRow {
                module: stored.name,
                message: "state is 'uninstalled' but an installed version is stored".to_owned(),
            });
        }
        Ok(Self {
            name: stored.name,
            state: stored.state,
            latest_version: stored.latest_version,
            demo: stored.demo,
            info: stored.info,
            category: stored.category,
            depends,
            excludes: stored.excludes,
            countries: stored.countries,
            manifest: None,
        })
    }

    /// Dependency rows, countries, exclusions and category from the manifest (`_update_from_terp`).
    /// Returns true when something changed.
    fn sync_from_manifest(&mut self, manifest: Arc<Manifest>) -> Result<bool> {
        let triggers = manifest.auto_install().to_set();
        let mut depends: Vec<DependencyRow> = manifest
            .depends()
            .iter()
            .map(|d| DependencyRow::new(d.clone(), triggers.contains(d)))
            .collect();
        depends.sort();
        let countries: BTreeSet<CountryCode> = manifest.countries().iter().cloned().collect();
        let excludes: BTreeSet<ModuleName> = manifest.excludes().iter().cloned().collect();
        let category = CategoryPath::parse(manifest.category())?;
        let changed = self.depends != depends
            || self.countries != countries
            || self.excludes != excludes
            || self.category != category;
        self.depends = depends;
        self.countries = countries;
        self.excludes = excludes;
        self.category = category;
        self.manifest = Some(manifest);
        Ok(changed)
    }

    fn is_installable(&self) -> bool {
        self.manifest.as_ref().is_some_and(|m| m.installable())
    }

    /// The module name.
    pub fn name(&self) -> &ModuleName {
        &self.name
    }

    /// The state.
    pub fn state(&self) -> ModuleState {
        self.state
    }

    pub fn latest_version(&self) -> Option<SemverVersion> {
        self.latest_version
    }

    /// Whether the module's demo data is loaded.
    pub fn demo(&self) -> bool {
        self.demo
    }

    /// The manifest values copied to the row.
    pub fn info(&self) -> &ModuleInfo {
        &self.info
    }

    /// The category path.
    pub fn category(&self) -> &CategoryPath {
        &self.category
    }

    /// The dependency rows, sorted by name.
    pub fn depends(&self) -> &[DependencyRow] {
        &self.depends
    }

    /// Names of the modules this one excludes.
    pub fn excludes(&self) -> &BTreeSet<ModuleName> {
        &self.excludes
    }

    /// ISO country codes the module is restricted to; empty means all.
    pub fn countries(&self) -> &BTreeSet<CountryCode> {
        &self.countries
    }

    /// The on-disk manifest; `None` when no addon source provides the module.
    pub fn manifest(&self) -> Option<&Arc<Manifest>> {
        self.manifest.as_ref()
    }
}

/// Options of `button_install` (and of the installs `button_upgrade` schedules).
///
/// ```
/// use erp_modules::lifecycle::InstallOptions;
/// use erp_modules::CountryCode;
/// let be = CountryCode::parse("be").unwrap();
/// let options = InstallOptions::new()
///     .with_company_countries([be.clone()])
///     .with_skip_auto_install(true);
/// assert!(options.skip_auto_install());
/// assert!(options.company_countries().is_some_and(|c| c.contains(&be)));
/// ```
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct InstallOptions {
    /// `None`: not known (before M3's `res.company`); a country-restricted auto-install is then
    /// an error ([`Error::CompanyCountriesUnknown`]).
    company_countries: Option<BTreeSet<CountryCode>>,
    skip_auto_install: bool,
}

impl InstallOptions {
    /// Company countries not known, auto-install enabled.
    pub fn new() -> Self {
        Self::default()
    }

    /// Country codes of the database's companies (auto-install country filter).
    pub fn with_company_countries(
        mut self,
        countries: impl IntoIterator<Item = CountryCode>,
    ) -> Self {
        self.company_countries = Some(countries.into_iter().collect());
        self
    }

    pub fn with_skip_auto_install(mut self, skip: bool) -> Self {
        self.skip_auto_install = skip;
        self
    }

    /// The company country codes, when known.
    pub fn company_countries(&self) -> Option<&BTreeSet<CountryCode>> {
        self.company_countries.as_ref()
    }

    /// Whether the auto-install closure is skipped.
    pub fn skip_auto_install(&self) -> bool {
        self.skip_auto_install
    }
}

/// The window action the scheduling methods return (`ACTION_DICT`).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct ModuleAction {
    /// `Install`, `Uninstall` or `Apply Schedule Upgrade`.
    pub name: &'static str,
    /// The kind of action.
    pub action_type: ActionType,
    /// The model the action opens.
    pub res_model: &'static str,
    /// The view modes, comma separated.
    pub view_mode: &'static str,
    /// Where the client opens it.
    pub target: ActionTarget,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ActionType {
    /// `ir.actions.act_window`.
    ActWindow,
}

impl ActionType {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ActWindow => "ir.actions.act_window",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ActionTarget {
    /// A dialog (`new`).
    New,
}

impl ActionTarget {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::New => "new",
        }
    }
}

impl ModuleAction {
    fn window(name: &'static str) -> Self {
        Self {
            name,
            action_type: ActionType::ActWindow,
            res_model: "base.module.upgrade",
            view_mode: "form",
            target: ActionTarget::New,
        }
    }
}

/// Result of `update_list` (`[n_upgradable, n_added]`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub struct UpdateListResult {
    /// Modules whose manifest version is greater than the installed version.
    pub upgradable: usize,
    /// Modules seen for the first time.
    pub added: usize,
}

/// The stored values of a row that operations change.
type Saved = (ModuleState, Option<SemverVersion>, bool);

fn saved_of(row: &ModuleRow) -> Saved {
    (row.state, row.latest_version, row.demo)
}

/// The module rows of a database plus its exclusive categories.
#[derive(Debug, Clone, Default)]
pub struct Catalog {
    rows: BTreeMap<ModuleName, ModuleRow>,
    exclusive_categories: BTreeSet<CategoryPath>,
    /// State, installed version and demo flag as loaded (or as last [`Catalog::mark_saved`]), to
    /// compute what changed.
    saved: BTreeMap<ModuleName, Saved>,
    /// Rows whose info or satellite rows changed since load.
    dirty: BTreeSet<ModuleName>,
    /// Rows created since load.
    added: BTreeSet<ModuleName>,
}

impl Catalog {
    /// An empty catalog.
    pub fn new() -> Self {
        Self::default()
    }

    /// A catalog of stored rows (all considered saved): how the database layer builds it, and
    /// how tests build a catalog in a given state (rows from [`StoredModule::from_manifest`], then
    /// [`Catalog::attach_manifest`]).
    ///
    /// # Errors
    /// [`Error::DuplicateModuleRow`] when two rows have the same name (the table has a unique
    /// constraint on `name`, so this means a broken loader or database).
    pub fn from_rows(
        rows: impl IntoIterator<Item = ModuleRow>,
        exclusive_categories: impl IntoIterator<Item = CategoryPath>,
    ) -> Result<Self> {
        let mut by_name: BTreeMap<ModuleName, ModuleRow> = BTreeMap::new();
        for row in rows {
            let name = row.name.clone();
            if by_name.insert(name.clone(), row).is_some() {
                return Err(Error::DuplicateModuleRow { module: name });
            }
        }
        let saved = by_name
            .iter()
            .map(|(n, r)| (n.clone(), saved_of(r)))
            .collect();
        Ok(Self {
            rows: by_name,
            exclusive_categories: exclusive_categories.into_iter().collect(),
            saved,
            dirty: BTreeSet::new(),
            added: BTreeSet::new(),
        })
    }

    /// The categories flagged `exclusive`.
    pub fn exclusive_categories(&self) -> &BTreeSet<CategoryPath> {
        &self.exclusive_categories
    }

    pub fn set_exclusive(&mut self, category: CategoryPath, exclusive: bool) {
        if exclusive {
            self.exclusive_categories.insert(category);
        } else {
            self.exclusive_categories.remove(&category);
        }
    }

    /// A row.
    pub fn get(&self, name: &str) -> Option<&ModuleRow> {
        self.rows.get(name)
    }

    /// The state of a module, or `None` when the catalog does not know it.
    pub fn state(&self, name: &str) -> Option<ModuleState> {
        self.rows.get(name).map(|r| r.state)
    }

    /// All rows by name.
    pub fn rows(&self) -> impl Iterator<Item = &ModuleRow> {
        self.rows.values()
    }

    /// Attach the on-disk manifest of a known module without syncing anything (the loader does it
    /// for modules whose rows were loaded from the database).
    ///
    /// # Errors
    /// [`Error::UnknownModule`] when the catalog has no row of that name; nothing is attached.
    pub fn attach_manifest(&mut self, manifest: Arc<Manifest>) -> Result<()> {
        let name = manifest.name().clone();
        let row = self.row_mut(&name)?;
        row.manifest = Some(manifest);
        Ok(())
    }

    /// Modules whose state changed since load, with the new state.
    pub fn state_changes(&self) -> Vec<(ModuleName, ModuleState)> {
        self.rows
            .iter()
            .filter(|(n, r)| self.saved.get(*n).map(|s| s.0) != Some(r.state))
            .map(|(n, r)| (n.clone(), r.state))
            .collect()
    }

    /// Rows whose state, installed version or demo flag changed since load (rows created since
    /// load included).
    pub fn row_changes(&self) -> impl Iterator<Item = &ModuleRow> {
        self.rows
            .iter()
            .filter(|(n, r)| self.saved.get(*n) != Some(&saved_of(r)))
            .map(|(_, r)| r)
    }

    /// Rows created since load.
    pub fn added(&self) -> impl Iterator<Item = &ModuleRow> {
        // `added` only holds names inserted into `rows`, and rows are never removed.
        self.rows.values().filter(|r| self.added.contains(&r.name))
    }

    /// Existing rows whose info, dependencies, countries, exclusions or category changed.
    pub fn dirty(&self) -> impl Iterator<Item = &ModuleRow> {
        self.rows
            .values()
            .filter(|r| self.dirty.contains(&r.name) && !self.added.contains(&r.name))
    }

    /// Everything was written: forget the change tracking (the database layer only).
    pub(crate) fn mark_saved(&mut self) {
        self.saved = self
            .rows
            .iter()
            .map(|(n, r)| (n.clone(), saved_of(r)))
            .collect();
        self.dirty.clear();
        self.added.clear();
    }

    /// A module's step finished: `installed` with its version and demo flag (the runner only;
    /// the scheduling rules change states through their own methods).
    pub(crate) fn mark_installed(
        &mut self,
        name: &ModuleName,
        version: SemverVersion,
        demo: bool,
    ) -> Result<()> {
        let row = self.row_mut(name)?;
        row.state = ModuleState::Installed;
        row.latest_version = Some(version);
        row.demo = demo;
        Ok(())
    }

    /// A module was removed: `uninstalled`, no version, no demo (the runner only).
    pub(crate) fn mark_uninstalled(&mut self, name: &ModuleName) -> Result<()> {
        let row = self.row_mut(name)?;
        row.state = ModuleState::Uninstalled;
        row.latest_version = None;
        row.demo = false;
        Ok(())
    }

    fn row_mut(&mut self, name: &ModuleName) -> Result<&mut ModuleRow> {
        self.rows.get_mut(name).ok_or_else(|| Error::UnknownModule {
            names: vec![name.clone()],
        })
    }

    fn require_known(&self, names: &[ModuleName]) -> Result<()> {
        let unknown: Vec<ModuleName> = names
            .iter()
            .filter(|n| !self.rows.contains_key(n.as_str()))
            .cloned()
            .collect();
        if unknown.is_empty() {
            Ok(())
        } else {
            Err(Error::UnknownModule { names: unknown })
        }
    }

    /// Run `f` on a copy of the rows; keep the result only when `f` succeeds.
    fn transactional<T>(&mut self, f: impl FnOnce(&mut Self) -> Result<T>) -> Result<T> {
        let backup = self.rows.clone();
        let result = f(self);
        if result.is_err() {
            self.rows = backup;
        }
        result
    }

    // ---- button_install (MOD §4.2.1) --------------------------------------------------------

    /// Mark `names` and, recursively, their uninstalled dependencies `to install`; then the
    /// auto-install closure; then the exclusion checks.
    pub fn button_install(
        &mut self,
        names: &[ModuleName],
        options: &InstallOptions,
    ) -> Result<ModuleAction> {
        self.require_known(names)?;
        self.transactional(|cat| {
            for name in names {
                cat.check_requested_installable(name)?;
            }
            cat.install_closure(names.to_vec(), options)?;
            cat.check_exclusions()?;
            Ok(ModuleAction::window("Install"))
        })
    }

    /// A module asked for by name must be installable and present in a source.
    fn check_requested_installable(&self, name: &ModuleName) -> Result<()> {
        let row = self
            .rows
            .get(name.as_str())
            .ok_or_else(|| Error::UnknownModule {
                names: vec![name.clone()],
            })?;
        match row.state {
            ModuleState::Uninstallable => Err(Error::ModuleNotInstallable {
                module: name.clone(),
            }),
            ModuleState::Uninstalled if row.manifest.is_none() => Err(Error::ModuleUnavailable {
                module: name.clone(),
            }),
            ModuleState::Uninstalled
            | ModuleState::Installed
            | ModuleState::ToInstall
            | ModuleState::ToUpgrade
            | ModuleState::ToRemove => Ok(()),
            ModuleState::Unavailable => Err(Error::UnavailableIsNotStored {
                module: name.clone(),
            }),
        }
    }

    fn install_closure(
        &mut self,
        mut todo: Vec<ModuleName>,
        options: &InstallOptions,
    ) -> Result<()> {
        const INSTALL_STATES: [ModuleState; 3] = [
            ModuleState::Installed,
            ModuleState::ToInstall,
            ModuleState::ToUpgrade,
        ];
        while !todo.is_empty() {
            for name in &todo {
                self.state_update(name, MAX_DEPENDENCY_DEPTH)?;
            }
            if options.skip_auto_install {
                break;
            }
            let mut next = Vec::new();
            for m in self
                .rows
                .values()
                .filter(|m| m.state == ModuleState::Uninstalled && m.info.auto_install)
            {
                let states: Vec<Option<ModuleState>> = m
                    .depends
                    .iter()
                    .filter(|d| d.auto_install_required)
                    .map(|d| self.state(d.name.as_str()))
                    .collect();
                let triggered = states
                    .iter()
                    .all(|s| s.is_some_and(|s| INSTALL_STATES.contains(&s)))
                    && states.contains(&Some(ModuleState::ToInstall));
                if !triggered {
                    continue;
                }
                // Country filter: unknown company countries are an error, never
                // "no company matches" (which would skip the module silently).
                let country_ok = if m.countries.is_empty() {
                    true
                } else {
                    match &options.company_countries {
                        Some(companies) => !m.countries.is_disjoint(companies),
                        None => {
                            return Err(Error::CompanyCountriesUnknown {
                                module: m.name.clone(),
                                countries: m.countries.iter().cloned().collect(),
                            });
                        }
                    }
                };
                if country_ok {
                    next.push(m.name.clone());
                }
            }
            todo = next;
        }
        Ok(())
    }

    /// `_state_update('to install', ['uninstalled'], level)`.
    fn state_update(&mut self, name: &ModuleName, level: usize) -> Result<()> {
        if level < 1 {
            return Err(Error::DependencyRecursion);
        }
        let Some(row) = self.rows.get(name.as_str()) else {
            return Err(Error::UnknownModule {
                names: vec![name.clone()],
            });
        };
        if row.state != ModuleState::Uninstalled {
            return Ok(());
        }
        let depends = row.depends.clone();
        let mut update: Vec<&ModuleName> = Vec::new();
        for dep in &depends {
            match self.state(dep.name.as_str()) {
                None => {
                    return Err(Error::UnknownDependency {
                        module: name.clone(),
                        dependency: dep.name.clone(),
                    });
                }
                Some(ModuleState::ToInstall) => {}
                Some(_) => update.push(&dep.name),
            }
        }
        for dep in update {
            if self.state(dep.as_str()) == Some(ModuleState::Uninstallable) {
                return Err(Error::DependencyNotInstallable {
                    module: name.clone(),
                    dependency: dep.clone(),
                });
            }
            self.state_update(dep, level - 1)?;
        }
        let row = self.row_mut(name)?;
        if row.manifest.is_none() {
            return Err(Error::ModuleUnavailable {
                module: name.clone(),
            });
        }
        if !row.is_installable() {
            return Err(Error::ModuleNotInstallable {
                module: name.clone(),
            });
        }
        if let Some(manifest) = &row.manifest {
            manifest
                .external_dependencies()
                .check(name, ModuleOperation::Install)?;
        }
        row.state = ModuleState::ToInstall;
        Ok(())
    }

    /// Exclusions and exclusive categories over the installed/to install/to upgrade set.
    fn check_exclusions(&self) -> Result<()> {
        let install: Vec<&ModuleRow> = self
            .rows
            .values()
            .filter(|m| {
                matches!(
                    m.state,
                    ModuleState::Installed | ModuleState::ToInstall | ModuleState::ToUpgrade
                )
            })
            .collect();
        let by_name: BTreeMap<&str, &ModuleRow> =
            install.iter().map(|m| (m.name.as_str(), *m)).collect();
        for module in &install {
            for excluded in &module.excludes {
                if let Some(other) = by_name.get(excluded.as_str()) {
                    return Err(Error::IncompatibleModules {
                        module: module.name.clone(),
                        other: other.name.clone(),
                        module_display: module.info.shortdesc.clone(),
                        other_display: other.info.shortdesc.clone(),
                    });
                }
            }
        }
        for category in &self.exclusive_categories {
            let members: Vec<&ModuleRow> = install
                .iter()
                .copied()
                .filter(|m| m.category.is_within(category))
                .collect();
            if members.is_empty() {
                continue;
            }
            let member_names: BTreeSet<&str> = members.iter().map(|m| m.name.as_str()).collect();
            let valid = members.iter().any(|m| {
                let closure = self.upstream_closure(&m.name);
                member_names
                    .iter()
                    .all(|n| closure.contains(*n) || *n == m.name.as_str())
            });
            if !valid {
                return Err(Error::ExclusiveCategory {
                    category: category.clone(),
                    modules: members
                        .iter()
                        .map(|m| {
                            CategoryMember::new(m.name.clone(), m.info.shortdesc.clone(), m.state)
                        })
                        .collect(),
                });
            }
        }
        Ok(())
    }

    /// The module and all its transitive dependencies (names), whatever their state.
    pub fn upstream_closure(&self, name: &ModuleName) -> BTreeSet<ModuleName> {
        let mut seen: BTreeSet<ModuleName> = BTreeSet::new();
        let mut stack = vec![name.clone()];
        while let Some(n) = stack.pop() {
            if !seen.insert(n.clone()) {
                continue;
            }
            if let Some(row) = self.rows.get(n.as_str()) {
                stack.extend(row.depends.iter().map(|d| d.name.clone()));
            }
        }
        seen
    }

    /// Modules that directly or indirectly depend on `names`, in states other than `exclude`
    /// (`downstream_dependencies`).
    pub fn downstream(&self, names: &[ModuleName], exclude: &[ModuleState]) -> Vec<ModuleName> {
        let mut known: BTreeSet<ModuleName> = names.iter().cloned().collect();
        let mut result: Vec<ModuleName> = Vec::new();
        let mut frontier: Vec<ModuleName> = names.to_vec();
        while !frontier.is_empty() {
            let set: BTreeSet<&str> = frontier.iter().map(ModuleName::as_str).collect();
            let next: Vec<ModuleName> = self
                .rows
                .values()
                .filter(|m| !exclude.contains(&m.state) && !known.contains(&m.name))
                .filter(|m| m.depends.iter().any(|d| set.contains(d.name.as_str())))
                .map(|m| m.name.clone())
                .collect();
            for n in &next {
                known.insert(n.clone());
                result.push(n.clone());
            }
            frontier = next;
        }
        result
    }

    // ---- button_upgrade (MOD §4.2.2) --------------------------------------------------------

    /// Mark `names` and their installed dependents (transitively) `to upgrade`; schedule new
    /// uninstalled dependencies for installation.
    pub fn button_upgrade(
        &mut self,
        names: &[ModuleName],
        options: &InstallOptions,
    ) -> Result<ModuleAction> {
        self.require_known(names)?;
        self.transactional(|cat| {
            let mut todo: Vec<ModuleName> = names.to_vec();
            if names.iter().any(|n| n.as_str() == "base") {
                todo.extend(
                    cat.rows
                        .values()
                        .filter(|m| {
                            m.state == ModuleState::Installed
                                && m.name.as_str() != STUDIO_CUSTOMIZATION
                                && !m.is_package()
                                && !names.contains(&m.name)
                        })
                        .map(|m| m.name.clone()),
                );
            }
            let mut queue: VecDeque<ModuleName> = todo.iter().cloned().collect();
            while let Some(name) = queue.pop_front() {
                let row = cat
                    .rows
                    .get(name.as_str())
                    .ok_or_else(|| Error::UnknownModule {
                        names: vec![name.clone()],
                    })?;
                if !matches!(row.state, ModuleState::Installed | ModuleState::ToUpgrade) {
                    return Err(Error::UpgradeNotInstalled { module: name });
                }
                let Some(manifest) = &row.manifest else {
                    return Err(Error::ModuleUnavailable { module: name });
                };
                if manifest.installable() {
                    manifest
                        .external_dependencies()
                        .check(&name, ModuleOperation::Upgrade)?;
                }
                let dependents: Vec<ModuleName> = cat
                    .rows
                    .values()
                    .filter(|m| {
                        m.state == ModuleState::Installed
                            && m.name.as_str() != STUDIO_CUSTOMIZATION
                            && !m.is_package()
                            && m.depends.iter().any(|d| d.name == name)
                            && !todo.contains(&m.name)
                    })
                    .map(|m| m.name.clone())
                    .collect();
                queue.extend(dependents.iter().cloned());
                todo.extend(dependents);
            }
            for name in &todo {
                cat.row_mut(name)?.state = ModuleState::ToUpgrade;
            }
            let mut to_install: Vec<ModuleName> = Vec::new();
            for name in &todo {
                let row = cat.row_mut(name)?;
                if !row.is_installable() {
                    continue;
                }
                let depends = row.depends.clone();
                for dep in depends {
                    match cat.state(dep.name.as_str()) {
                        None => {
                            return Err(Error::UpgradeUnknownDependency {
                                module: name.clone(),
                                dependency: dep.name,
                            });
                        }
                        Some(ModuleState::Uninstalled) if !to_install.contains(&dep.name) => {
                            to_install.push(dep.name)
                        }
                        Some(_) => {}
                    }
                }
            }
            if !to_install.is_empty() {
                cat.install_closure(to_install, options)?;
                cat.check_exclusions()?;
            }
            Ok(ModuleAction::window("Apply Schedule Upgrade"))
        })
    }

    // ---- force_demo (MOD §4.7) -----------------------------------

    /// Set the demo flag on every native row, installed or not, and schedule every installed
    /// native module `to upgrade` so the runner reloads its demo files (`force_demo`,
    /// ). A module that is installed later follows the flag
    /// of its dependencies, so it gets demo data too.
    ///
    /// Unlike [`Catalog::button_upgrade`] this schedules no dependency closure and installs
    /// nothing: the run only loads demo files, so it is refused while any module is
    /// in a transient state, while an installed addon's manifest version differs from its
    /// installed version (the run must not stamp a version without its migrations), and while an
    /// installed addon depends on one that is not installed. WebAssembly package rows and
    /// `studio_customization` are left alone: the run does not reload them.
    ///
    /// # Errors
    /// [`Error::ForceDemoPending`], [`Error::ForceDemoVersionMismatch`],
    /// [`Error::ForceDemoMissingDependency`], [`Error::ModuleUnavailable`] for an installed addon
    /// with no source. A refused request changes nothing.
    pub fn force_demo(&mut self) -> Result<ModuleAction> {
        let pending: Vec<ModuleName> = self
            .rows
            .values()
            .filter(|m| m.state.is_transient())
            .map(|m| m.name.clone())
            .collect();
        if !pending.is_empty() {
            return Err(Error::ForceDemoPending { modules: pending });
        }
        let reloaded = |m: &&ModuleRow| {
            m.state == ModuleState::Installed
                && m.name.as_str() != STUDIO_CUSTOMIZATION
                && !m.is_package()
        };
        let mut targets: Vec<ModuleName> = Vec::new();
        let mut stale: Vec<ModuleName> = Vec::new();
        for row in self.rows.values().filter(reloaded) {
            let Some(manifest) = &row.manifest else {
                return Err(Error::ModuleUnavailable {
                    module: row.name.clone(),
                });
            };
            if manifest.version().semver().copied() != row.latest_version {
                stale.push(row.name.clone());
            }
            for dep in manifest.depends() {
                if self.state(dep.as_str()) != Some(ModuleState::Installed) {
                    return Err(Error::ForceDemoMissingDependency {
                        module: row.name.clone(),
                        dependency: dep.clone(),
                    });
                }
            }
            targets.push(row.name.clone());
        }
        if !stale.is_empty() {
            return Err(Error::ForceDemoVersionMismatch { modules: stale });
        }
        for row in self.rows.values_mut() {
            if row.name.as_str() != STUDIO_CUSTOMIZATION && !row.is_package() {
                row.demo = true;
            }
        }
        for name in &targets {
            self.row_mut(name)?.state = ModuleState::ToUpgrade;
        }
        Ok(ModuleAction::window("Load demo data"))
    }

    // ---- button_uninstall (MOD §4.2.3) ------------------------------------------------------

    /// Mark `names` and their downstream dependents `to remove`.
    pub fn button_uninstall(&mut self, names: &[ModuleName]) -> Result<ModuleAction> {
        self.require_known(names)?;
        let mut protected: Vec<ModuleName> = names
            .iter()
            .filter(|n| SERVER_WIDE_MODULES.contains(&n.as_str()))
            .cloned()
            .collect();
        if !protected.is_empty() {
            protected.sort();
            protected.dedup();
            return Err(Error::CannotUninstall { modules: protected });
        }
        if names.iter().any(|n| {
            !matches!(
                self.state(n.as_str()),
                Some(ModuleState::Installed | ModuleState::ToUpgrade)
            )
        }) {
            return Err(Error::AlreadyUninstalled);
        }
        let deps = self.downstream(
            names,
            &[
                ModuleState::Uninstalled,
                ModuleState::Uninstallable,
                ModuleState::ToRemove,
            ],
        );
        // Refusing here keeps it from staying `to remove`.
        for name in names.iter().chain(&deps) {
            match self
                .rows
                .get(name.as_str())
                .and_then(|r| r.manifest.as_ref())
            {
                None => {
                    return Err(Error::UninstallUnavailable {
                        module: name.clone(),
                    });
                }
                Some(manifest) if !manifest.installable() => {
                    return Err(Error::UninstallNotInstallable {
                        module: name.clone(),
                    });
                }
                Some(_) => {}
            }
        }
        self.transactional(|cat| {
            for name in names.iter().chain(&deps) {
                cat.row_mut(name)?.state = ModuleState::ToRemove;
            }
            Ok(ModuleAction::window("Uninstall"))
        })
    }

    // ---- database initialization --------------------------------------------

    /// The install set of a new database: `base`, then (unless
    /// `options.skip_auto_install()`) every `auto_install` module whose dependencies all exist and whose
    /// trigger dependencies are all `to install`, plus every dependency of the modules marked,
    /// until nothing changes.
    /// Returns the modules marked, in marking order.
    ///
    /// # Errors
    /// [`Error::UnknownModule`] when the catalog has no `base` row (no addon source provides
    /// `base`).
    pub fn seed_initial(&mut self, options: &InstallOptions) -> Result<Vec<ModuleName>> {
        let base = self.row_mut(&ModuleName::new("base")?)?;
        base.state = ModuleState::ToInstall;
        let mut marked = vec![base.name.clone()];
        if options.skip_auto_install {
            return Ok(marked);
        }
        loop {
            let to_install =
                |cat: &Self, name: &str| cat.state(name) == Some(ModuleState::ToInstall);
            let mut round: Vec<ModuleName> = self
                .rows
                .values()
                .filter(|m| {
                    m.info.auto_install
                        && !matches!(m.state, ModuleState::ToInstall | ModuleState::Uninstallable)
                        && m.depends.iter().all(|d| {
                            self.rows.contains_key(d.name.as_str())
                                && (!d.auto_install_required || to_install(self, d.name.as_str()))
                        })
                })
                .map(|m| m.name.clone())
                .collect();
            let selected: BTreeSet<ModuleName> = round.iter().cloned().collect();
            let mut deps: BTreeSet<ModuleName> = BTreeSet::new();
            for m in self.rows.values() {
                if m.state == ModuleState::ToInstall || selected.contains(&m.name) {
                    for d in &m.depends {
                        if self.rows.contains_key(d.name.as_str())
                            && !to_install(self, d.name.as_str())
                            && !selected.contains(&d.name)
                        {
                            deps.insert(d.name.clone());
                        }
                    }
                }
            }
            round.extend(deps);
            if round.is_empty() {
                return Ok(marked);
            }
            for name in round {
                self.row_mut(&name)?.state = ModuleState::ToInstall;
                marked.push(name);
            }
        }
    }

    // ---- reset ---------------------------------------------------------------

    /// `button_reset_state` / `reset_modules_state`: `to install` → `uninstalled`, `to upgrade`
    /// and `to remove` → `installed`. Returns the modules reset.
    pub fn reset_state(&mut self) -> Vec<ModuleName> {
        let mut reset = Vec::new();
        for row in self.rows.values_mut() {
            let new = match row.state {
                ModuleState::ToInstall => ModuleState::Uninstalled,
                ModuleState::ToUpgrade | ModuleState::ToRemove => ModuleState::Installed,
                ModuleState::Uninstalled
                | ModuleState::Installed
                | ModuleState::Uninstallable
                | ModuleState::Unavailable => continue,
            };
            row.state = new;
            reset.push(row.name.clone());
        }
        reset
    }

    // ---- update_list (MOD §4.2.5) -----------------------------------------------------------

    /// Sync the catalog with the manifests of every addon source: changed info is
    /// written, new modules are added `uninstalled`/`uninstallable`, an `uninstallable` module
    /// that became installable becomes `uninstalled`, dependency rows, countries, exclusions and
    /// category follow the manifest. Modules in no source keep their row with no manifest
    /// (`unavailable`).
    ///
    /// # Errors
    /// [`Error::InvalidCategory`] for a manifest category that is not a valid path;
    /// [`Error::CategoryIdCollision`] when two different categories would share an external id;
    /// the catalog is unchanged then.
    pub fn update_list<'a>(
        &mut self,
        manifests: impl IntoIterator<Item = &'a Arc<Manifest>>,
    ) -> Result<UpdateListResult> {
        let manifests: Vec<&Arc<Manifest>> = manifests.into_iter().collect();
        // Validate first so that a refused list changes nothing.
        let mut names: BTreeSet<&ModuleName> = BTreeSet::new();
        let mut categories: Vec<CategoryPath> = Vec::new();
        for manifest in &manifests {
            if !names.insert(manifest.name()) {
                return Err(Error::DuplicateModuleRow {
                    module: manifest.name().clone(),
                });
            }
            categories.push(CategoryPath::parse(manifest.category())?);
        }
        // Rows of modules the list does not mention keep their category.
        categories.extend(
            self.rows
                .values()
                .filter(|r| !names.contains(&r.name))
                .map(|r| r.category.clone()),
        );
        CategoryPath::check_xml_id_collisions(&categories)?;
        let default_version = SemverVersion::new(1, 0, 0);
        let mut result = UpdateListResult::default();
        let mut seen: BTreeSet<ModuleName> = BTreeSet::new();
        for manifest in manifests {
            let name = manifest.name().clone();
            seen.insert(name.clone());
            match self.rows.get_mut(name.as_str()) {
                Some(row) => {
                    let info = ModuleInfo::from_manifest(manifest);
                    let mut changed = row.info != info;
                    row.info = info;
                    if manifest.installable() && row.state == ModuleState::Uninstallable {
                        row.state = ModuleState::Uninstalled;
                    }
                    // A manifest of another series has no semver form: it is not installable and
                    // never counts as upgradable.
                    if let Some(v) = manifest.version().semver()
                        && *v > row.latest_version.unwrap_or(default_version)
                    {
                        result.upgradable += 1;
                    }
                    changed |= row.sync_from_manifest(Arc::clone(manifest))?;
                    if changed {
                        self.dirty.insert(name);
                    }
                }
                None => {
                    let row = ModuleRow::from_manifest(Arc::clone(manifest))?;
                    self.rows.insert(name.clone(), row);
                    self.added.insert(name);
                    result.added += 1;
                }
            }
        }
        for (name, row) in &mut self.rows {
            if !seen.contains(name) {
                row.manifest = None;
            }
        }
        Ok(result)
    }
}
