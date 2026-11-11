use crate::error::{Error, Result};
use crate::manifest::{Manifest, ModuleName};
use indexmap::IndexMap;
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use tracing::{info, warn};

/// State of a module (`ir.module.module.state`, MOD §4.2).
///
/// `Unavailable` is never stored: a module installed in the database but present in
/// no addon source gets it when the graph is built.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Serialize)]
#[non_exhaustive]
pub enum ModuleState {
    #[default]
    Uninstalled,
    Installed,
    ToInstall,
    ToUpgrade,
    ToRemove,
    Uninstallable,
    Unavailable,
}

impl std::fmt::Display for ModuleState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl ModuleState {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Uninstalled => "uninstalled",
            Self::Installed => "installed",
            Self::ToInstall => "to install",
            Self::ToUpgrade => "to upgrade",
            Self::ToRemove => "to remove",
            Self::Uninstallable => "uninstallable",
            Self::Unavailable => "unavailable",
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            Self::Uninstalled => "Not Installed",
            Self::Installed => "Installed",
            Self::ToInstall => "To be installed",
            Self::ToUpgrade => "To be upgraded",
            Self::ToRemove => "To be removed",
            Self::Uninstallable => "Uninstallable",
            Self::Unavailable => "Unavailable",
        }
    }

    /// Parse a stored value. `unavailable` is not a stored state and is refused.
    pub fn from_db(value: &str) -> Option<Self> {
        Some(match value {
            "uninstalled" => Self::Uninstalled,
            "installed" => Self::Installed,
            "to install" => Self::ToInstall,
            "to upgrade" => Self::ToUpgrade,
            "to remove" => Self::ToRemove,
            "uninstallable" => Self::Uninstallable,
            _ => return None,
        })
    }

    /// `to install`, `to upgrade` or `to remove`.
    pub fn is_transient(&self) -> bool {
        matches!(self, Self::ToInstall | Self::ToUpgrade | Self::ToRemove)
    }

    pub fn is_to_install(&self) -> bool {
        matches!(self, ModuleState::ToInstall)
    }

    pub fn is_installed_or_upgrading(&self) -> bool {
        matches!(self, ModuleState::Installed | ModuleState::ToUpgrade)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LoadMode {
    #[default]
    Load,
    Update,
}

/// Reason why a module was dropped from the module graph during build or extend.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[non_exhaustive]
pub enum DropReason {
    NotInstallable,
    NotInstalled,
    Unavailable,
    MissingDependency {
        dependency: ModuleName,
        cascade_chain: Vec<ModuleName>,
    },
    DependencyCycle {
        members: Vec<ModuleName>,
        cascade_chain: Vec<ModuleName>,
    },
}

impl std::fmt::Display for DropReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let chain = |c: &[ModuleName]| {
            c.iter()
                .map(ModuleName::as_str)
                .collect::<Vec<_>>()
                .join(" -> ")
        };
        match self {
            Self::NotInstallable => f.write_str("not installable"),
            Self::NotInstalled => f.write_str("not installed"),
            Self::Unavailable => f.write_str("unavailable (in no addon source)"),
            Self::MissingDependency {
                dependency,
                cascade_chain,
            } => write!(
                f,
                "missing dependency '{dependency}' ({})",
                chain(cascade_chain)
            ),
            Self::DependencyCycle {
                members,
                cascade_chain,
            } => write!(
                f,
                "dependency cycle [{}] ({})",
                members
                    .iter()
                    .map(ModuleName::as_str)
                    .collect::<Vec<_>>()
                    .join(", "),
                chain(cascade_chain)
            ),
        }
    }
}

/// Report containing all modules dropped during graph building or extending, with explicit reasons.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DropReport {
    dropped: IndexMap<ModuleName, DropReason>,
}

impl DropReport {
    pub fn dropped(&self) -> &IndexMap<ModuleName, DropReason> {
        &self.dropped
    }

    pub fn len(&self) -> usize {
        self.dropped.len()
    }

    pub fn is_empty(&self) -> bool {
        self.dropped.is_empty()
    }

    pub fn get(&self, name: &ModuleName) -> Option<&DropReason> {
        self.dropped.get(name)
    }
}

#[derive(Debug, Clone)]
pub struct ModuleNode {
    manifest: Arc<Manifest>,
    state: ModuleState,
    depth: usize,
    phase: usize,
    order_name: String,
}

impl ModuleNode {
    pub fn name(&self) -> &ModuleName {
        self.manifest.name()
    }

    pub fn manifest(&self) -> &Arc<Manifest> {
        &self.manifest
    }

    pub fn state(&self) -> ModuleState {
        self.state
    }

    pub fn depth(&self) -> usize {
        self.depth
    }

    pub fn phase(&self) -> usize {
        self.phase
    }

    pub fn order_name(&self) -> &str {
        &self.order_name
    }

    pub fn depends(&self) -> &[ModuleName] {
        self.manifest.depends()
    }
}

#[derive(Debug, Clone)]
pub struct ModuleGraph {
    mode: LoadMode,
    nodes: IndexMap<ModuleName, ModuleNode>,
    all_manifests: IndexMap<ModuleName, Arc<Manifest>>,
    all_states: IndexMap<ModuleName, ModuleState>,
}

impl ModuleGraph {
    pub fn new(mode: LoadMode) -> Self {
        Self {
            mode,
            nodes: IndexMap::new(),
            all_manifests: IndexMap::new(),
            all_states: IndexMap::new(),
        }
    }

    pub fn mode(&self) -> LoadMode {
        self.mode
    }

    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    pub fn contains(&self, name: &ModuleName) -> bool {
        self.nodes.contains_key(name)
    }

    pub fn get(&self, name: &ModuleName) -> Option<&ModuleNode> {
        self.nodes.get(name)
    }

    pub fn iter(&self) -> impl Iterator<Item = &ModuleNode> {
        self.nodes.values()
    }

    /// Return module names in resolved graph order (LoadMode::Load -> plain-load order;
    /// LoadMode::Update -> update phase order).
    pub fn module_names(&self) -> Vec<ModuleName> {
        self.nodes.keys().cloned().collect()
    }

    /// Return nodes in plain-load order: `(depth, order_name)`.
    ///
    /// Identical after install, upgrade, or restart.
    pub fn plain_load_order(&self) -> Vec<&ModuleNode> {
        let mut list: Vec<&ModuleNode> = self.nodes.values().collect();
        list.sort_by(|a, b| {
            a.depth
                .cmp(&b.depth)
                .then_with(|| a.order_name.cmp(&b.order_name))
        });
        list
    }

    /// Return module names in plain-load order: `(depth, order_name)`.
    pub fn plain_load_names(&self) -> Vec<ModuleName> {
        self.plain_load_order()
            .into_iter()
            .map(|n| n.name().clone())
            .collect()
    }

    /// Return nodes in update-mode phase order: `(phase, depth, order_name)`.
    pub fn phase_order(&self) -> Vec<&ModuleNode> {
        let mut list: Vec<&ModuleNode> = self.nodes.values().collect();
        list.sort_by(|a, b| {
            a.phase
                .cmp(&b.phase)
                .then_with(|| a.depth.cmp(&b.depth))
                .then_with(|| a.order_name.cmp(&b.order_name))
        });
        list
    }

    /// Return module names in update-mode phase order: `(phase, depth, order_name)`.
    pub fn phase_names(&self) -> Vec<ModuleName> {
        self.phase_order()
            .into_iter()
            .map(|n| n.name().clone())
            .collect()
    }

    /// Build and order the module graph from a collection of manifests and explicit module states.
    ///
    /// States must be provided for every module in manifests; missing states return an error.
    /// Returns the resolved ModuleGraph and DropReport listing every module dropped and why.
    pub fn build<'a, M, S>(manifests: M, states: S, mode: LoadMode) -> Result<(Self, DropReport)>
    where
        M: IntoIterator<Item = (&'a ModuleName, &'a Arc<Manifest>)>,
        S: IntoIterator<Item = (&'a ModuleName, &'a ModuleState)>,
    {
        let mut graph = Self::new(mode);
        let drop_report = graph.extend(manifests, states)?;
        Ok((graph, drop_report))
    }

    /// Extend graph with new module manifests and states.
    ///
    /// Accumulates manifests and states across rounds.
    /// In update mode, leaving out any module marked to install or upgrade returns an error.
    pub fn extend<'a, M, S>(&mut self, manifests: M, states: S) -> Result<DropReport>
    where
        M: IntoIterator<Item = (&'a ModuleName, &'a Arc<Manifest>)>,
        S: IntoIterator<Item = (&'a ModuleName, &'a ModuleState)>,
    {
        let input_manifests: IndexMap<ModuleName, Arc<Manifest>> = manifests
            .into_iter()
            .map(|(k, v)| (k.clone(), Arc::clone(v)))
            .collect();
        let input_states: IndexMap<ModuleName, ModuleState> =
            states.into_iter().map(|(k, v)| (k.clone(), *v)).collect();

        // Invariant: every manifest must have an explicit state
        for name in input_manifests.keys() {
            if !input_states.contains_key(name) && !self.all_states.contains_key(name) {
                return Err(Error::MissingModuleState {
                    module: name.clone(),
                });
            }
        }

        // Accumulate on clones (atomic extend: commit only on success)
        let mut new_manifests = self.all_manifests.clone();
        for (name, manifest) in input_manifests {
            new_manifests.insert(name, manifest);
        }
        let mut new_states = self.all_states.clone();
        for (name, state) in input_states {
            new_states.insert(name, state);
        }

        let mut active_manifests: IndexMap<ModuleName, Arc<Manifest>> = new_manifests.clone();
        let mut active_states: IndexMap<ModuleName, ModuleState> = new_states.clone();
        let mut dropped_reasons: IndexMap<ModuleName, DropReason> = IndexMap::new();

        // 1. Remove Unavailable modules and cascade
        let mut unavailable_to_remove = Vec::new();
        for name in active_manifests.keys() {
            let state = state_of(&active_states, name)?;
            if state == ModuleState::Unavailable {
                unavailable_to_remove.push(name.clone());
            }
        }
        for name in unavailable_to_remove {
            if active_manifests.contains_key(&name) {
                dropped_reasons.insert(name.clone(), DropReason::Unavailable);
                cascade_remove(
                    &name,
                    &mut active_manifests,
                    &mut active_states,
                    &mut dropped_reasons,
                    std::slice::from_ref(&name),
                    None,
                );
            }
        }

        // 2. Remove non-installable modules and cascade
        let mut not_installable_to_remove = Vec::new();
        for (name, manifest) in &active_manifests {
            let state = state_of(&active_states, name)?;
            if !manifest.installable() || state == ModuleState::Uninstallable {
                not_installable_to_remove.push(name.clone());
            }
        }
        for name in not_installable_to_remove {
            if active_manifests.contains_key(&name) {
                warn!("module {}: not installable, skipped", name);
                dropped_reasons.insert(name.clone(), DropReason::NotInstallable);
                cascade_remove(
                    &name,
                    &mut active_manifests,
                    &mut active_states,
                    &mut dropped_reasons,
                    std::slice::from_ref(&name),
                    None,
                );
            }
        }

        // 3. Remove modules with missing dependencies and cascade
        loop {
            let mut missing_deps_found = Vec::new();
            for (name, manifest) in &active_manifests {
                let mut missing = Vec::new();
                for dep in manifest.depends() {
                    if !active_manifests.contains_key(dep) {
                        missing.push(dep.clone());
                    }
                }
                if !missing.is_empty() {
                    missing_deps_found.push((name.clone(), missing));
                }
            }

            if missing_deps_found.is_empty() {
                break;
            }

            for (name, missing) in missing_deps_found {
                if active_manifests.contains_key(&name) {
                    let Some(first_missing) = missing.first().cloned() else {
                        return Err(Error::InvariantViolation {
                            message: format!("module {name} was listed with no missing dependency"),
                        });
                    };
                    let chain = vec![name.clone(), first_missing.clone()];
                    let reason = if let Some(prior_reason) = dropped_reasons.get(&first_missing) {
                        match prior_reason {
                            DropReason::DependencyCycle { members, .. } => {
                                DropReason::DependencyCycle {
                                    members: members.clone(),
                                    cascade_chain: chain.clone(),
                                }
                            }
                            // Whatever left the dependency out, for this module it is missing.
                            DropReason::NotInstallable
                            | DropReason::NotInstalled
                            | DropReason::Unavailable
                            | DropReason::MissingDependency { .. } => {
                                DropReason::MissingDependency {
                                    dependency: first_missing.clone(),
                                    cascade_chain: chain.clone(),
                                }
                            }
                        }
                    } else {
                        DropReason::MissingDependency {
                            dependency: first_missing.clone(),
                            cascade_chain: chain.clone(),
                        }
                    };

                    dropped_reasons.insert(name.clone(), reason.clone());
                    warn!(
                        "module {}: some depends are not loaded ({:?}), skipped",
                        name, missing
                    );
                    cascade_remove(
                        &name,
                        &mut active_manifests,
                        &mut active_states,
                        &mut dropped_reasons,
                        &chain,
                        Some(&reason),
                    );
                }
            }
        }

        // 4. Detect dependency cycles using Tarjan's SCC and drop all cycle participants and dependents
        let cycles = detect_cycles(&active_manifests)?;
        for cycle in cycles {
            for name in &cycle {
                if active_manifests.contains_key(name) {
                    let chain = vec![name.clone()];
                    let reason = DropReason::DependencyCycle {
                        members: cycle.clone(),
                        cascade_chain: chain.clone(),
                    };
                    dropped_reasons.insert(name.clone(), reason.clone());
                    warn!("module {}: in a dependency loop, skipped", name);
                    cascade_remove(
                        name,
                        &mut active_manifests,
                        &mut active_states,
                        &mut dropped_reasons,
                        &chain,
                        Some(&reason),
                    );
                }
            }
        }

        // 5. Mode-specific filtering
        if self.mode == LoadMode::Load {
            // An installed module (or one with a pending upgrade/removal) whose dependency is not
            // installed is an inconsistent database, not something to drop silently.
            let mut inconsistent: Vec<crate::error::InconsistentDependency> = Vec::new();
            for (name, manifest) in &active_manifests {
                let Some(state) = active_states.get(name).copied() else {
                    continue;
                };
                if !matches!(
                    state,
                    ModuleState::Installed | ModuleState::ToUpgrade | ModuleState::ToRemove
                ) {
                    continue;
                }
                for dep in manifest.depends() {
                    if let Some(dep_state @ (ModuleState::Uninstalled | ModuleState::ToInstall)) =
                        active_states.get(dep).copied()
                    {
                        inconsistent.push(crate::error::InconsistentDependency::new(
                            name.clone(),
                            dep.clone(),
                            dep_state,
                        ));
                    }
                }
            }
            if !inconsistent.is_empty() {
                inconsistent.sort();
                return Err(Error::InconsistentStates {
                    modules: inconsistent,
                });
            }
            let mut not_installed = Vec::new();
            for name in active_manifests.keys() {
                let state = state_of(&active_states, name)?;
                if state == ModuleState::ToInstall || state == ModuleState::Uninstalled {
                    not_installed.push(name.clone());
                }
            }
            for name in not_installed {
                if active_manifests.contains_key(&name) {
                    info!("module {}: not installed, skipped", name);
                    let chain = vec![name.clone()];
                    dropped_reasons.insert(name.clone(), DropReason::NotInstalled);
                    cascade_remove(
                        &name,
                        &mut active_manifests,
                        &mut active_states,
                        &mut dropped_reasons,
                        &chain,
                        None,
                    );
                }
            }
        }

        // 6. In update mode, leaving out a module marked to install, upgrade or remove is an
        // error: it would keep its transient state while the operation reports
        // success. Every such module is listed with its reason.
        if self.mode == LoadMode::Update {
            let mut scheduled_drops: Vec<(ModuleName, DropReason)> = dropped_reasons
                .iter()
                .filter(|(name, _)| new_states.get(*name).is_some_and(ModuleState::is_transient))
                .map(|(name, reason)| (name.clone(), reason.clone()))
                .collect();
            if !scheduled_drops.is_empty() {
                scheduled_drops.sort_by(|a, b| a.0.cmp(&b.0));
                return Err(Error::DroppedUpdateModules {
                    modules: scheduled_drops,
                });
            }
        }

        // 7. Compute topological depths and order_name
        let (depths, order_names) = compute_depths_and_order_names(&active_manifests)?;

        // 8. Compute phases
        let phases = compute_phases(
            &active_manifests,
            &active_states,
            self.mode,
            &depths,
            &order_names,
        )?;

        // 9. Build ModuleNodes
        let mut nodes_vec = Vec::new();
        for (name, manifest) in active_manifests {
            let state = state_of(&active_states, &name)?;
            let depth = depths
                .get(&name)
                .copied()
                .ok_or_else(|| Error::InvariantViolation {
                    message: format!("missing depth for module {name}"),
                })?;
            let phase = phases
                .get(&name)
                .copied()
                .ok_or_else(|| Error::InvariantViolation {
                    message: format!("missing phase for module {name}"),
                })?;
            let order_name =
                order_names
                    .get(&name)
                    .cloned()
                    .ok_or_else(|| Error::InvariantViolation {
                        message: format!("missing order_name for module {name}"),
                    })?;

            nodes_vec.push(ModuleNode {
                manifest,
                state,
                depth,
                phase,
                order_name,
            });
        }

        // 10. Sort into graph order:
        // In LoadMode::Load, sort by (depth, order_name)
        // In LoadMode::Update, sort by (phase, depth, order_name)
        let mode = self.mode;
        nodes_vec.sort_by(|a, b| {
            if mode == LoadMode::Update {
                a.phase
                    .cmp(&b.phase)
                    .then_with(|| a.depth.cmp(&b.depth))
                    .then_with(|| a.order_name.cmp(&b.order_name))
            } else {
                a.depth
                    .cmp(&b.depth)
                    .then_with(|| a.order_name.cmp(&b.order_name))
            }
        });

        // Commit state only on successful extend
        self.all_manifests = new_manifests;
        self.all_states = new_states;
        self.nodes.clear();
        for node in nodes_vec {
            self.nodes.insert(node.name().clone(), node);
        }

        Ok(DropReport {
            dropped: dropped_reasons,
        })
    }
}

fn cascade_remove(
    target: &ModuleName,
    manifests: &mut IndexMap<ModuleName, Arc<Manifest>>,
    states: &mut IndexMap<ModuleName, ModuleState>,
    dropped: &mut IndexMap<ModuleName, DropReason>,
    parent_chain: &[ModuleName],
    cascade_reason: Option<&DropReason>,
) {
    if manifests.shift_remove(target).is_none() {
        return;
    }
    states.shift_remove(target);

    // Find all modules directly depending on target
    let dependents: Vec<ModuleName> = manifests
        .iter()
        .filter(|(_, m)| m.depends().iter().any(|d| d == target))
        .map(|(k, _)| k.clone())
        .collect();

    for dep in dependents {
        if manifests.contains_key(&dep) {
            let mut chain = vec![dep.clone()];
            chain.extend_from_slice(parent_chain);

            let reason = match cascade_reason {
                Some(DropReason::DependencyCycle { members, .. }) => DropReason::DependencyCycle {
                    members: members.clone(),
                    cascade_chain: chain.clone(),
                },
                // Any other reason (or none given): for the dependent, `target` is missing.
                None
                | Some(
                    DropReason::NotInstallable
                    | DropReason::NotInstalled
                    | DropReason::Unavailable
                    | DropReason::MissingDependency { .. },
                ) => DropReason::MissingDependency {
                    dependency: target.clone(),
                    cascade_chain: chain.clone(),
                },
            };

            dropped.insert(dep.clone(), reason.clone());
            info!(
                "module {}: its direct/indirect dependency is skipped, skipped",
                dep
            );
            cascade_remove(&dep, manifests, states, dropped, &chain, Some(&reason));
        }
    }
}

struct Tarjan<'a> {
    manifests: &'a IndexMap<ModuleName, Arc<Manifest>>,
    index: usize,
    indices: HashMap<ModuleName, usize>,
    lowlinks: HashMap<ModuleName, usize>,
    on_stack: HashSet<ModuleName>,
    stack: Vec<ModuleName>,
    sccs: Vec<Vec<ModuleName>>,
}

impl<'a> Tarjan<'a> {
    fn new(manifests: &'a IndexMap<ModuleName, Arc<Manifest>>) -> Self {
        Self {
            manifests,
            index: 0,
            indices: HashMap::new(),
            lowlinks: HashMap::new(),
            on_stack: HashSet::new(),
            stack: Vec::new(),
            sccs: Vec::new(),
        }
    }

    fn run(mut self) -> Result<Vec<Vec<ModuleName>>> {
        for node in self.manifests.keys() {
            if !self.indices.contains_key(node) {
                self.strongconnect(node)?;
            }
        }
        Ok(self.sccs)
    }

    fn lowlink(&self, v: &ModuleName) -> Result<usize> {
        self.lowlinks
            .get(v)
            .copied()
            .ok_or_else(|| Error::InvariantViolation {
                message: format!("dependency scan: no lowlink for module {v}"),
            })
    }

    fn index_of(&self, v: &ModuleName) -> Result<usize> {
        self.indices
            .get(v)
            .copied()
            .ok_or_else(|| Error::InvariantViolation {
                message: format!("dependency scan: no index for module {v}"),
            })
    }

    fn strongconnect(&mut self, v: &ModuleName) -> Result<()> {
        self.indices.insert(v.clone(), self.index);
        self.lowlinks.insert(v.clone(), self.index);
        self.index += 1;
        self.stack.push(v.clone());
        self.on_stack.insert(v.clone());

        if let Some(m) = self.manifests.get(v) {
            for w in m.depends() {
                if !self.manifests.contains_key(w) {
                    continue;
                }
                if !self.indices.contains_key(w) {
                    self.strongconnect(w)?;
                    let w_low = self.lowlink(w)?;
                    let v_low = self.lowlink(v)?;
                    self.lowlinks.insert(v.clone(), v_low.min(w_low));
                } else if self.on_stack.contains(w) {
                    let w_idx = self.index_of(w)?;
                    let v_low = self.lowlink(v)?;
                    self.lowlinks.insert(v.clone(), v_low.min(w_idx));
                }
            }
        }

        if self.lowlink(v)? == self.index_of(v)? {
            let mut scc = Vec::new();
            while let Some(w) = self.stack.pop() {
                self.on_stack.remove(&w);
                scc.push(w.clone());
                if &w == v {
                    break;
                }
            }
            self.sccs.push(scc);
        }
        Ok(())
    }
}

/// Detect strongly connected components that form cycles (size > 1 or self-loops).
fn detect_cycles(manifests: &IndexMap<ModuleName, Arc<Manifest>>) -> Result<Vec<Vec<ModuleName>>> {
    let mut cycles: Vec<Vec<ModuleName>> = Vec::new();
    for mut scc in Tarjan::new(manifests).run()? {
        match scc.as_slice() {
            [] => {
                return Err(Error::InvariantViolation {
                    message: "dependency scan produced an empty component".to_owned(),
                });
            }
            [name] => {
                let manifest = manifests
                    .get(name)
                    .ok_or_else(|| Error::InvariantViolation {
                        message: format!("dependency scan: no manifest for module {name}"),
                    })?;
                if manifest.depends().iter().any(|d| d == name) {
                    cycles.push(scc);
                }
            }
            _ => {
                scc.sort();
                cycles.push(scc);
            }
        }
    }
    // Components are disjoint, so ordering them as sequences orders them by first member.
    cycles.sort();
    Ok(cycles)
}

/// The state of an active module; every module in the graph has one (checked on entry).
fn state_of(states: &IndexMap<ModuleName, ModuleState>, name: &ModuleName) -> Result<ModuleState> {
    states
        .get(name)
        .copied()
        .ok_or_else(|| Error::MissingModuleState {
            module: name.clone(),
        })
}

fn compute_depths_and_order_names(
    manifests: &IndexMap<ModuleName, Arc<Manifest>>,
) -> Result<(HashMap<ModuleName, usize>, HashMap<ModuleName, String>)> {
    let mut depths: HashMap<ModuleName, usize> = HashMap::new();
    let mut order_names: HashMap<ModuleName, String> = HashMap::new();

    let mut resolved: HashSet<ModuleName> = HashSet::new();

    while resolved.len() < manifests.len() {
        let mut progress = false;
        for (name, manifest) in manifests {
            if resolved.contains(name) {
                continue;
            }

            // Can resolve if all dependencies in the graph are resolved
            let deps_ready = manifest
                .depends()
                .iter()
                .all(|d| resolved.contains(d) || !manifests.contains_key(d));

            if !deps_ready {
                continue;
            }

            let is_test = name.starts_with("test_");
            let available_deps: Vec<&ModuleName> = manifest
                .depends()
                .iter()
                .filter(|d| manifests.contains_key(*d))
                .collect();

            if is_test {
                // The dependency resolved last: highest depth, then highest order_name.
                let mut last: Option<(&ModuleName, usize, &String)> = None;
                for &dep in &available_deps {
                    let depth = depths.get(dep.as_str()).copied().ok_or_else(|| {
                        Error::InvariantViolation {
                            message: format!("missing depth for dependency {dep} of {name}"),
                        }
                    })?;
                    let order_name =
                        order_names
                            .get(dep.as_str())
                            .ok_or_else(|| Error::InvariantViolation {
                                message: format!(
                                    "missing order_name for dependency {dep} of {name}"
                                ),
                            })?;
                    let better = match last {
                        None => true,
                        Some((_, d, o)) => (depth, order_name) >= (d, o),
                    };
                    if better {
                        last = Some((dep, depth, order_name));
                    }
                }
                if let Some((last_dep, _, _)) = last {
                    let last_depth = depths.get(last_dep.as_str()).copied().ok_or_else(|| {
                        Error::InvariantViolation {
                            message: format!("missing depth for dependency {last_dep} of {name}"),
                        }
                    })?;
                    let last_order_name =
                        order_names.get(last_dep.as_str()).cloned().ok_or_else(|| {
                            Error::InvariantViolation {
                                message: format!(
                                    "missing order_name for dependency {last_dep} of {name}"
                                ),
                            }
                        })?;

                    depths.insert(name.clone(), last_depth);
                    order_names.insert(name.clone(), format!("{} {}", last_order_name, name));
                } else {
                    depths.insert(name.clone(), 0);
                    order_names.insert(name.clone(), name.to_string());
                }
            } else if available_deps.is_empty() {
                depths.insert(name.clone(), 0);
                order_names.insert(name.clone(), name.to_string());
            } else {
                let mut max_dep_depth = 0;
                for d in &available_deps {
                    let d_depth = depths.get(d.as_str()).copied().ok_or_else(|| {
                        Error::InvariantViolation {
                            message: format!("missing depth for dependency {d} of {name}"),
                        }
                    })?;
                    max_dep_depth = max_dep_depth.max(d_depth);
                }

                let depth =
                    max_dep_depth
                        .checked_add(1)
                        .ok_or_else(|| Error::InvariantViolation {
                            message: format!("dependency depth of {name} overflows"),
                        })?;
                depths.insert(name.clone(), depth);
                order_names.insert(name.clone(), name.to_string());
            }

            resolved.insert(name.clone());
            progress = true;
        }

        if !progress {
            let unresolved: Vec<ModuleName> = manifests
                .keys()
                .filter(|k| !resolved.contains(*k))
                .cloned()
                .collect();
            let Some(first) = unresolved.first().cloned() else {
                return Err(Error::InvariantViolation {
                    message: "topological depth resolution made no progress with nothing left"
                        .to_owned(),
                });
            };
            return Err(Error::InvalidManifest {
                name: first,
                message: format!(
                    "topological depth resolution failed to make progress; unresolved: {:?}",
                    unresolved
                ),
            });
        }
    }

    Ok((depths, order_names))
}

fn compute_phases(
    manifests: &IndexMap<ModuleName, Arc<Manifest>>,
    states: &IndexMap<ModuleName, ModuleState>,
    mode: LoadMode,
    depths: &HashMap<ModuleName, usize>,
    order_names: &HashMap<ModuleName, String>,
) -> Result<HashMap<ModuleName, usize>> {
    let mut phases: HashMap<ModuleName, usize> = HashMap::new();

    if mode == LoadMode::Load {
        for name in manifests.keys() {
            if name.as_str() == "base" {
                phases.insert(name.clone(), 0);
            } else {
                phases.insert(name.clone(), 1);
            }
        }
        return Ok(phases);
    }

    // In Update mode:
    // Sort topologically by (depth, order_name) to calculate phase progressively
    let mut keyed: Vec<(usize, &String, &ModuleName)> = Vec::with_capacity(manifests.len());
    for name in manifests.keys() {
        let depth = depths
            .get(name)
            .copied()
            .ok_or_else(|| Error::InvariantViolation {
                message: format!("missing depth for module {name}"),
            })?;
        let order_name = order_names
            .get(name)
            .ok_or_else(|| Error::InvariantViolation {
                message: format!("missing order_name for module {name}"),
            })?;
        keyed.push((depth, order_name, name));
    }
    keyed.sort();
    let sorted_names: Vec<ModuleName> = keyed.into_iter().map(|(_, _, n)| n.clone()).collect();

    for name in sorted_names {
        if name.as_str() == "base" {
            phases.insert(name, 0);
            continue;
        }

        let manifest = manifests
            .get(&name)
            .ok_or_else(|| Error::InvariantViolation {
                message: format!("manifest missing for module {name}"),
            })?;

        let my_state = states
            .get(&name)
            .copied()
            .ok_or_else(|| Error::MissingModuleState {
                module: name.clone(),
            })?;
        let available_deps: Vec<&ModuleName> = manifest
            .depends()
            .iter()
            .filter(|d| manifests.contains_key(*d))
            .collect();

        if available_deps.is_empty() {
            phases.insert(name, 1);
        } else {
            let mut dep_phases = Vec::with_capacity(available_deps.len());
            for dep_name in available_deps {
                let dep_phase =
                    phases
                        .get(dep_name)
                        .copied()
                        .ok_or_else(|| Error::InvariantViolation {
                            message: format!("missing phase for dependency {dep_name} of {name}"),
                        })?;
                let dep_state =
                    states
                        .get(dep_name)
                        .copied()
                        .ok_or_else(|| Error::MissingModuleState {
                            module: dep_name.clone(),
                        })?;

                let not_in_same_phase =
                    (my_state == ModuleState::ToInstall) ^ (dep_state == ModuleState::ToInstall);

                dep_phases.push(
                    dep_phase
                        + (if not_in_same_phase { 1 } else { 0 })
                        + (if dep_name.as_str() == "base" { 1 } else { 0 }),
                );
            }

            let max_phase = dep_phases.into_iter().max().unwrap_or(1);
            phases.insert(name, max_phase);
        }
    }

    Ok(phases)
}
