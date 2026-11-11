//! Module management commands (`install`, `upgrade`, `uninstall`).

use crate::args::{DatabaseName, ModuleAction, ModuleOpArgs};
use crate::error::{CliError, Result};
use erp_modules::{AddonIndex, LoadMode, ModuleGraph, ModuleName, ModuleState};
use std::collections::{BTreeMap, BinaryHeap, HashMap, HashSet, VecDeque};
use std::path::PathBuf;

/// Dedicated operation enum representing module lifecycle commands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModuleOperation {
    /// Install module and its forward dependencies.
    Install,
    /// Upgrade module and its forward dependencies.
    Upgrade,
    /// Uninstall module if no other available addons depend on it.
    Uninstall,
}

/// What a live module operation needs to run: the database, the modules the user named, the
/// resolved plan, and the addon search paths the module system discovers addons from.
#[derive(Debug, Clone, Copy)]
#[non_exhaustive]
pub struct LifecycleRequest<'a> {
    /// The target database (`-d`); the credentials come from `DATABASE_URL`.
    pub database: &'a DatabaseName,
    /// The modules named on the command line, without duplicates, in command-line order. The
    /// module system resolves dependencies (install) and dependents (uninstall) against the
    /// database's own state.
    pub requested: &'a [ModuleName],
    /// The offline plan: the modules in load order as resolved from the addon manifests.
    pub plan: &'a [ModuleName],
    /// The addon search paths (`--addons-path`).
    pub addons_path: &'a [PathBuf],
    /// `--restore-generated-restrictions` (upgrade only): rewrite generated company restrictions
    /// changed since they were generated instead of refusing the update.
    pub restore_generated_restrictions: bool,
    /// Explicitly verified previous-release preimage for forward upgrade only.
    pub identity_previous: Option<&'a std::path::Path>,
}

/// What a live module operation did.
///
/// The operation committed whenever a report is returned; the post-commit fields say whether
/// its follow-up steps (index builds and the like) finished.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct ExecutionReport {
    /// Modules installed, in install order.
    pub installed: Vec<ModuleName>,
    /// Modules upgraded, in update order.
    pub upgraded: Vec<ModuleName>,
    /// Modules uninstalled (the requested ones and their installed dependents).
    pub uninstalled: Vec<ModuleName>,
    /// Installed modules that no addon source provides.
    pub unavailable: Vec<ModuleName>,
    /// Generated company restrictions rewritten by `--restore-generated-restrictions`, one line
    /// each (`model: what`).
    pub restored_restrictions: Vec<String>,
    /// Post-commit steps that ran.
    pub post_commit_done: usize,
    /// Post-commit steps that failed, as `module/key: message`.
    pub post_commit_failed: Vec<String>,
    /// Why the post-commit phase stopped before running every pending step, when it did.
    pub post_commit_stopped: Option<String>,
    /// Column comments a schema step kept because they were changed outside the ORM while it
    /// ran, as `table.column: comment` (`(none)` for a column without one).
    pub schema_kept_comments: Vec<String>,
    /// Schema steps that found nothing left to do, as `module/key`.
    pub schema_nothing_left: Vec<String>,
}

impl ExecutionReport {
    /// The error for a post-commit phase that did not finish cleanly, `None` when it did.
    pub fn post_commit_error(&self) -> Option<CliError> {
        if self.post_commit_failed.is_empty() && self.post_commit_stopped.is_none() {
            None
        } else {
            Some(CliError::PostCommitIncomplete {
                failed: self.post_commit_failed.len(),
                stopped: self.post_commit_stopped.clone(),
            })
        }
    }
}

fn write_modules(
    f: &mut std::fmt::Formatter<'_>,
    label: &str,
    modules: &[ModuleName],
) -> std::fmt::Result {
    if modules.is_empty() {
        return Ok(());
    }
    let names: Vec<&str> = modules.iter().map(ModuleName::as_str).collect();
    writeln!(f, "  {label}: {}", names.join(", "))
}

impl std::fmt::Display for ExecutionReport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write_modules(f, "installed", &self.installed)?;
        write_modules(f, "upgraded", &self.upgraded)?;
        write_modules(f, "uninstalled", &self.uninstalled)?;
        write_modules(
            f,
            "unavailable (installed, no addon source)",
            &self.unavailable,
        )?;
        for restored in &self.restored_restrictions {
            writeln!(f, "  generated restriction restored: {restored}")?;
        }
        writeln!(f, "  post-commit steps run: {}", self.post_commit_done)?;
        for failure in &self.post_commit_failed {
            writeln!(f, "  post-commit step failed: {failure}")?;
        }
        for step in &self.schema_nothing_left {
            writeln!(f, "  schema step found nothing left to do: {step}")?;
        }
        for kept in &self.schema_kept_comments {
            writeln!(f, "  schema step kept the column comment {kept}")?;
        }
        if let Some(reason) = &self.post_commit_stopped {
            writeln!(f, "  post-commit phase stopped early: {reason}")?;
        }
        Ok(())
    }
}

/// Trait for executing module lifecycle operations against a database.
///
/// CLI argument validation, discovery and the offline dependency plan are decoupled from the
/// database: [`DefaultLifecycleExecutor`] runs the operation through
/// [`erp_modules::ModuleSystem`] in one transaction per operation; tests
/// substitute their own executor.
pub trait ModuleLifecycleExecutor: Send + Sync {
    /// Run `operation` on the database of `request`.
    ///
    /// # Errors
    ///
    /// Whatever the executor refuses or fails with; [`DefaultLifecycleExecutor`] returns
    /// [`CliError::ModuleOperation`] for the module system's errors.
    fn execute(
        &self,
        operation: ModuleOperation,
        request: &LifecycleRequest<'_>,
    ) -> Result<ExecutionReport>;
}

pub use crate::live::DefaultLifecycleExecutor;

pub(crate) fn resolve_manifest_path(
    name: &ModuleName,
    addons_paths: &[PathBuf],
) -> Result<Option<PathBuf>> {
    let exists = |path: &PathBuf| {
        path.try_exists().map_err(|source| CliError::Io {
            path: path.clone(),
            source,
        })
    };
    for base in addons_paths {
        let candidate = base.join(name.as_str()).join("manifest.toml");
        if exists(&candidate)? {
            return Ok(Some(candidate));
        }
        if base.file_name().and_then(|s| s.to_str()) == Some(name.as_str()) {
            let direct = base.join("manifest.toml");
            if exists(&direct)? {
                return Ok(Some(direct));
            }
        }
    }
    Ok(None)
}

fn map_single_discovery_error(err: erp_modules::Error, addons_paths: &[PathBuf]) -> CliError {
    match err {
        erp_modules::Error::Io { path, source } => CliError::Io { path, source },
        erp_modules::Error::InvalidManifest { ref name, .. }
        | erp_modules::Error::InvalidVersion { ref name, .. }
        | erp_modules::Error::ManifestNotFound { ref name } => {
            match resolve_manifest_path(name, addons_paths) {
                Ok(Some(path)) => CliError::Manifest {
                    path,
                    source: Box::new(err),
                },
                // The manifest file is not under any search path (it was read from somewhere
                // else): the error text names the module and what is wrong with it.
                Ok(None) => CliError::DependencyResolution(err.to_string()),
                Err(io) => io,
            }
        }
        erp_modules::Error::DiscoveryError { path, message } => CliError::DependencyResolution(
            format!("Discovery error at '{}': {message}", path.display()),
        ),
        // Known non-I/O module-level errors: format as dependency resolution failures (exit 1).
        erp_modules::Error::InvalidModuleName { .. }
        | erp_modules::Error::DuplicateAddon { .. }
        | erp_modules::Error::MissingModuleState { .. }
        | erp_modules::Error::DroppedUpdateModules { .. }
        | erp_modules::Error::SelfDependency { .. }
        | erp_modules::Error::DuplicateDependency { .. }
        | erp_modules::Error::SelfExclusion { .. }
        | erp_modules::Error::DuplicateExclusion { .. }
        | erp_modules::Error::InvariantViolation { .. }
        | erp_modules::Error::AutoInstallTriggerMismatch { .. }
        | erp_modules::Error::ExternalDependencyMissing { .. } => {
            CliError::DependencyResolution(err.to_string())
        }
        // DiscoveryErrors is handled by map_discovery_error; if reached here
        // (e.g. nested), delegate to map_discovery_error.
        erp_modules::Error::DiscoveryErrors { .. } => map_discovery_error(err, addons_paths),
        // Catch-all for future non_exhaustive variants from erp_modules::Error.
        // Uses DependencyResolution so the error is never silently dropped.
        other => CliError::DependencyResolution(other.to_string()),
    }
}

/// The CLI error for a module of the plan that the dependency graph dropped.
fn dropped_module_error(name: &ModuleName, reason: &erp_modules::DropReason) -> CliError {
    match reason {
        erp_modules::DropReason::MissingDependency { dependency, .. } => {
            CliError::MissingDependency {
                module: name.clone(),
                dependency: dependency.clone(),
            }
        }
        erp_modules::DropReason::DependencyCycle { members, .. } => {
            CliError::DependencyCycle(members.clone())
        }
        erp_modules::DropReason::NotInstallable => CliError::DependencyResolution(format!(
            "module '{name}' was dropped from execution plan: not installable"
        )),
        erp_modules::DropReason::NotInstalled => CliError::DependencyResolution(format!(
            "module '{name}' was dropped from execution plan: not installed"
        )),
        erp_modules::DropReason::Unavailable => CliError::DependencyResolution(format!(
            "module '{name}' was dropped from execution plan: unavailable"
        )),
        // DropReason is #[non_exhaustive]: a future reason is still reported, with its text.
        other => CliError::DependencyResolution(format!(
            "module '{name}' was dropped from execution plan: {other}"
        )),
    }
}

/// Map addon discovery errors into typed CLI errors preserving source and distinguishing I/O from manifest issues.
///
/// All underlying errors are preserved: none are dropped. When multiple errors exist,
/// they are collected into [`CliError::MultipleDiscoveryErrors`], which reports exit code 2
/// if any error is I/O and exit code 1 otherwise.
///
/// Resolves real manifest file paths using configured addon search directories.
pub fn map_discovery_error(err: erp_modules::Error, addons_paths: &[PathBuf]) -> CliError {
    match err {
        erp_modules::Error::DiscoveryErrors { mut errors, .. } => {
            if errors.len() == 1 {
                map_single_discovery_error(errors.remove(0), addons_paths)
            } else {
                let mapped: Vec<CliError> = errors
                    .into_iter()
                    .map(|e| map_single_discovery_error(e, addons_paths))
                    .collect();
                CliError::MultipleDiscoveryErrors(mapped)
            }
        }
        other => map_single_discovery_error(other, addons_paths),
    }
}

/// The result of a module command: the offline plan, and what the live operation did.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct ModuleRun {
    /// The modules in load order, resolved from the addon manifests.
    pub plan: Vec<ModuleName>,
    /// What the operation did; `None` for `--dry-run`, which touches no database.
    pub report: Option<ExecutionReport>,
}

/// Execute a module command action with a custom lifecycle executor.
///
/// In dry-run mode (`--dry-run`), dependencies are resolved, topological order is computed,
/// and the plan is returned without invoking the executor.
/// In live mode, the executor is invoked with the target database and resolved plan.
///
/// # Errors
///
/// Returns [`CliError::ModuleNotFound`] if a requested module is not found in the search paths.
/// Returns [`CliError::MissingDependency`] if a required module dependency is not available.
/// Returns [`CliError::DependencyCycle`] if cyclical dependencies are detected.
/// Returns [`CliError::DependencyResolution`] if resolution constraints cannot be satisfied.
/// Returns the executor's error if live execution fails ([`CliError::ModuleOperation`] for
/// [`DefaultLifecycleExecutor`]).
pub fn run_module_command_with_executor<E: ModuleLifecycleExecutor>(
    action: &ModuleAction,
    executor: &E,
) -> Result<ModuleRun> {
    match action {
        ModuleAction::Install(args) => {
            resolve_and_execute(args, ModuleOperation::Install, executor)
        }
        ModuleAction::Upgrade(args) => {
            resolve_and_execute(args, ModuleOperation::Upgrade, executor)
        }
        ModuleAction::Uninstall(args) => {
            resolve_and_execute(args, ModuleOperation::Uninstall, executor)
        }
        ModuleAction::IdentityEnroll(_) | ModuleAction::IdentityActivate(_) => {
            Err(CliError::InvalidInput {
                arg: "module identity".into(),
                reason: "identity administration uses the owner configuration command dispatcher"
                    .into(),
            })
        }
    }
}

/// As [`run_module_command_with_executor`], returning only the plan.
///
/// # Errors
///
/// As [`run_module_command_with_executor`].
pub fn execute_module_command_with_executor<E: ModuleLifecycleExecutor>(
    action: &ModuleAction,
    executor: &E,
) -> Result<Vec<ModuleName>> {
    run_module_command_with_executor(action, executor).map(|run| run.plan)
}

/// Execute a module command action, resolving dependencies and returning the planned execution order.
///
/// In dry-run mode, returns the resolved plan. In live mode (no `--dry-run`), runs the operation
/// through [`DefaultLifecycleExecutor`] on the database named by `-d`, with the credentials of
/// `DATABASE_URL`.
///
/// # Errors
///
/// As [`run_module_command_with_executor`].
pub fn execute_module_command(action: &ModuleAction) -> Result<Vec<ModuleName>> {
    execute_module_command_with_executor(action, &DefaultLifecycleExecutor)
}

/// As [`execute_module_command`], returning the plan and the live operation's report.
///
/// # Errors
///
/// As [`run_module_command_with_executor`].
pub fn run_module_command(action: &ModuleAction) -> Result<ModuleRun> {
    run_module_command_with_executor(action, &DefaultLifecycleExecutor)
}

fn resolve_and_execute<E: ModuleLifecycleExecutor>(
    args: &ModuleOpArgs,
    operation: ModuleOperation,
    executor: &E,
) -> Result<ModuleRun> {
    if args.identity_previous.is_some() && operation != ModuleOperation::Upgrade {
        return Err(CliError::InvalidInput {
            arg: "--identity-previous".into(),
            reason: "only an explicit forward upgrade may admit previous-release identity".into(),
        });
    }
    if args.restore_generated_restrictions && operation != ModuleOperation::Upgrade {
        return Err(CliError::InvalidInput {
            arg: "--restore-generated-restrictions".to_owned(),
            reason: "only the upgrade command rewrites generated company restrictions".to_owned(),
        });
    }
    // 1. Build AddonIndex from configured search paths (scans developer directories)
    let mut index = AddonIndex::new(true);
    for path in &args.addons_path {
        index.add_dev_path(path);
    }

    let discovered = index
        .discover_all()
        .map_err(|e| map_discovery_error(e, &args.addons_path))?;

    // 2. Verify all requested modules exist in discovered addons
    for mod_name in &args.modules {
        if !discovered.contains_key(mod_name) {
            return Err(CliError::ModuleNotFound(mod_name.clone()));
        }
    }

    // 3. Handle uninstall vs install/upgrade
    let plan = match operation {
        ModuleOperation::Uninstall => {
            // Reverse dependency check of the plan-only mode (`--dry-run`): refuse uninstall if
            // other available addons depend on it. Without database state this is a
            // conservative approximation that checks the addons available on disk.
            // Transitive dependents outside the uninstall set are
            // caught because every transitive dependent chain must pass through at least one
            // direct dependent in the available discovered addons.
            for (other_name, manifest) in &discovered {
                if args.dry_run && !args.modules.contains(other_name) {
                    for dep in manifest.depends() {
                        if args.modules.contains(dep) {
                            return Err(CliError::DependencyResolution(format!(
                                "cannot uninstall module '{dep}' because module '{other_name}' depends on it (conservative plan-only check on available addons; a live run cascades to the installed dependents)"
                            )));
                        }
                    }
                }
            }

            // Deduplicate preserving encounter order.
            let mut unique: Vec<ModuleName> = Vec::new();
            let mut unique_set = HashSet::new();
            for m in &args.modules {
                if unique_set.insert(m.clone()) {
                    unique.push(m.clone());
                }
            }

            // Kahn's algorithm for topological ordering over the requested set.
            // Directed edge u -> v means u must precede v in forward load order (v depends on u).
            // in_degree[v]: count of dependencies of v that are in the requested set.
            // dependents_of[u]: modules in the requested set that depend on u.
            let mut in_degree: HashMap<ModuleName, usize> = HashMap::new();
            let mut dependents_of: HashMap<ModuleName, Vec<ModuleName>> = HashMap::new();

            for m in &unique {
                in_degree.insert(m.clone(), 0);
                dependents_of.insert(m.clone(), Vec::new());
            }

            for m in &unique {
                if let Some(manifest) = discovered.get(m) {
                    for dep in manifest.depends() {
                        if unique_set.contains(dep) {
                            *in_degree.entry(m.clone()).or_insert(0) += 1;
                            dependents_of
                                .entry(dep.clone())
                                .or_default()
                                .push(m.clone());
                        }
                    }
                }
            }

            // Min-heap for deterministic alphabetical tie-breaking on zero in-degree nodes.
            let mut ready: BinaryHeap<std::cmp::Reverse<ModuleName>> = BinaryHeap::new();
            for (m, &deg) in &in_degree {
                if deg == 0 {
                    ready.push(std::cmp::Reverse(m.clone()));
                }
            }

            let mut forward_order: Vec<ModuleName> = Vec::with_capacity(unique.len());
            while let Some(std::cmp::Reverse(current)) = ready.pop() {
                forward_order.push(current.clone());
                if let Some(deps) = dependents_of.get(&current) {
                    for dep in deps {
                        if let Some(deg) = in_degree.get_mut(dep) {
                            *deg -= 1;
                            if *deg == 0 {
                                ready.push(std::cmp::Reverse(dep.clone()));
                            }
                        }
                    }
                }
            }

            if forward_order.len() < unique.len() {
                let unvisited: Vec<ModuleName> = unique
                    .into_iter()
                    .filter(|m| !forward_order.contains(m))
                    .collect();
                return Err(CliError::DependencyCycle(unvisited));
            }

            // Uninstall order is reverse of forward load order: dependents first, dependencies last.
            forward_order.reverse();
            forward_order
        }
        ModuleOperation::Install | ModuleOperation::Upgrade => {
            // Compute transitive closure of forward dependencies
            let mut closure = HashSet::new();
            let mut queue = VecDeque::new();
            let mut parent_map: HashMap<ModuleName, ModuleName> = HashMap::new();

            for mod_name in &args.modules {
                closure.insert(mod_name.clone());
                queue.push_back(mod_name.clone());
            }

            while let Some(current) = queue.pop_front() {
                if let Some(manifest) = discovered.get(&current) {
                    for dep in manifest.depends() {
                        parent_map
                            .entry(dep.clone())
                            .or_insert_with(|| current.clone());
                        if closure.insert(dep.clone()) {
                            queue.push_back(dep.clone());
                        }
                    }
                }
            }

            // Build manifests and states maps for the closure
            let mut closure_manifests = BTreeMap::new();
            let mut closure_states = BTreeMap::new();

            let target_state = if operation == ModuleOperation::Upgrade {
                ModuleState::ToUpgrade
            } else {
                ModuleState::ToInstall
            };

            for mod_name in &closure {
                if let Some(manifest) = discovered.get(mod_name) {
                    closure_manifests.insert(mod_name, manifest);

                    // NOTE: The plan has no database state, so dependencies of upgraded modules are
                    // conservatively marked ToInstall. The full S1-backed version inspects database
                    // state to decide between install and upgrade for dependencies.
                    let state = if args.modules.contains(mod_name) {
                        target_state
                    } else {
                        ModuleState::ToInstall
                    };
                    closure_states.insert(mod_name, state);
                } else {
                    let parent = parent_map.get(mod_name).cloned().ok_or_else(|| {
                        CliError::Execution(format!(
                            "Internal error: missing dependency '{mod_name}' has no recorded parent"
                        ))
                    })?;
                    return Err(CliError::MissingDependency {
                        module: parent,
                        dependency: mod_name.clone(),
                    });
                }
            }

            // Resolve topological order using erp-modules::ModuleGraph
            let (graph, drop_report) = ModuleGraph::build(
                closure_manifests.iter().map(|(k, v)| (*k, *v)),
                closure_states.iter().map(|(k, v)| (*k, v)),
                LoadMode::Update,
            )
            .map_err(|e| match e {
                // Update mode refuses a plan that drops a module marked to install: report the
                // first dropped module of the closure with its reason.
                erp_modules::Error::DroppedUpdateModules { modules } => match modules
                    .iter()
                    .find(|(name, _)| closure.contains(name))
                    .or_else(|| modules.first())
                {
                    Some((name, reason)) => dropped_module_error(name, reason),
                    None => CliError::DependencyResolution(
                        "the dependency graph refused the plan without naming a module".to_owned(),
                    ),
                },
                other => map_single_discovery_error(other, &args.addons_path),
            })?;

            // Inspect DropReport: ensure no requested or dependency module was dropped
            if !drop_report.is_empty() {
                for (dropped_name, reason) in drop_report.dropped() {
                    if closure.contains(dropped_name) {
                        return Err(dropped_module_error(dropped_name, reason));
                    }
                }
            }

            graph.phase_names()
        }
    };

    // 4. Execute or dry-run
    if args.dry_run {
        // Architectural guarantee: dry-run performs no database writes and never calls the executor.
        return Ok(ModuleRun { plan, report: None });
    }
    // Live mode always invokes the executor, with the modules as named (without duplicates).
    let mut requested: Vec<ModuleName> = Vec::with_capacity(args.modules.len());
    for module in &args.modules {
        if !requested.contains(module) {
            requested.push(module.clone());
        }
    }
    let request = LifecycleRequest {
        database: &args.database,
        requested: &requested,
        plan: &plan,
        addons_path: &args.addons_path,
        restore_generated_restrictions: args.restore_generated_restrictions,
        identity_previous: args.identity_previous.as_deref(),
    };
    let report = executor.execute(operation, &request)?;
    Ok(ModuleRun {
        plan,
        report: Some(report),
    })
}
