//! The policy snapshot (DN §2.3) and its compiler (DN §3.1): stages 2–4 of
//! the pipeline over [`PolicyRows`] and a [`Registry`], pure (no I/O).
//!
//! A [`PolicySnapshot`] is immutable once built, except its bounded memos (the combined predicate
//! per (model, operation, group set, shape), the interner, the allowing groups per (model,
//! operation)). Every decision is a function of the snapshot, the binding's group set and shape,
//! the model and the operation: principal values are parameters of the predicate, bound per
//! statement by the ORM.

use std::sync::{Arc, Mutex, PoisonError};
use std::time::Instant;

use erp_domain::{Condition, Domain, DomainKind, Operation, Operator, Value};
use erp_fields::{Error, FieldType, Result};
use erp_orm::{
    AccessCtx, AccessCtxParts, AccessDecision, AccessDomain, AccessDomainChains, AccessDomainImpl,
    AccessPolicy, ActiveCompanySet, CAPABILITIES, Capability, ElevationKey, ElevationKind,
    EvidenceColumn, ExplainRequest, Explanation, FieldAccessImpl, FieldAccessTable,
    FieldContextValues, FieldDenialDetail, FieldMask, FieldOp, GRANT_SOURCES, GrantSource, GroupId,
    GroupLookup, GroupSet, GroupSetId, GroupSetInterner, InternedGroups, KindClass, OperationSet,
    PolicyGeneration, PrincipalBinding, PrincipalShape, QualifiedXmlId, RuleKind, RuleOperatorDecl,
    RuleOrigin, RuleRef, SnapshotId, SnapshotOrigin, SnapshotRef,
};
use erp_registry::{FieldId, FieldSpec, ModelId, ModelSpec, Registry};
use erp_sql::reference::ReferenceQuery;
use rustc_hash::{FxHashMap, FxHashSet};

use crate::analyze::{AccessLeaf, analyze, comodel_of};
use crate::bits::BitSet;
use crate::company_access::{self, GeneratedRestriction};
use crate::explain::{self, RecordMessage, blame};
use crate::fields::FieldRules;
use crate::graph;
use crate::group_spec::{GroupSpec, GroupSpecError};
use crate::groups::GroupGraph;
use crate::memo::Lru;
use crate::report::{BuildCounts, BuildReport, CapabilityIssue, GeneratedIssueReport, RuleIssue};
use crate::rows::{FieldGroupsRow, PolicyRows, RuleId, RuleRow};
use crate::rule::{self, RuleError, holds_rule_operator};
use crate::rule_op::{Operators, RuleOperatorLeaf};

/// The group whose members see the debug listing of a record-level denial (in debug mode).
pub const DEBUG_GROUP: &str = "base.group_no_one";

/// Default bound of the combined-predicate memo of one snapshot (entries).
pub const DEFAULT_MEMO_CAPACITY: usize = 16_384;
/// Default bound of the group-set interner of one snapshot (sets).
pub const DEFAULT_INTERNER_CAPACITY: usize = 4_096;

/// One `ir.access` rule after compilation.
#[derive(Debug, Clone)]
pub struct CompiledRule {
    rule: RuleRef,
    id: RuleId,
    xmlid: Option<QualifiedXmlId>,
    model: ModelId,
    group: Option<GroupId>,
    ops: OperationSet,
    domain: std::result::Result<Domain, RuleError>,
    reads: Vec<(ModelId, FieldId)>,
    mentions_company: bool,
    access: Vec<AccessLeaf>,
    /// The models a declared rule operator in the rule selects from (Amendment 1).
    operator_targets: Vec<ModelId>,
    /// The deny variant of a generated company restriction (design `m3-company-a1` §4.3).
    deny: Option<std::result::Result<Domain, RuleError>>,
}

impl CompiledRule {
    /// The domain a reader with the active-company set `active` is decided with: the compiled
    /// domain, or for a generated company restriction under an empty set its deny variant.
    fn domain_for(&self, active: ActiveCompanySet) -> &std::result::Result<Domain, RuleError> {
        match (&self.deny, active) {
            (Some(deny), ActiveCompanySet::Empty) => deny,
            _ => &self.domain,
        }
    }

    /// Whether this is a generated company restriction (marked from the registry).
    pub fn generated(&self) -> bool {
        self.deny.is_some()
    }

    /// The rule (id, name, external id) as predicates and explanations name it.
    pub fn rule(&self) -> &RuleRef {
        &self.rule
    }

    /// The rule id.
    pub fn id(&self) -> RuleId {
        self.id
    }

    /// The external id, if any.
    pub fn xmlid(&self) -> Option<&QualifiedXmlId> {
        self.xmlid.as_ref()
    }

    /// The model.
    pub fn model(&self) -> ModelId {
        self.model
    }

    /// The group of a permission; `None` for a restriction.
    pub fn group(&self) -> Option<GroupId> {
        self.group
    }

    /// The operations it applies to.
    pub fn ops(&self) -> OperationSet {
        self.ops
    }

    /// The compiled domain (parameters for principal values, `access` conditions unresolved),
    /// or why it does not compile.
    pub fn domain(&self) -> std::result::Result<&Domain, &RuleError> {
        self.domain.as_ref()
    }

    /// The (model, field) pairs the domain reads.
    pub fn reads(&self) -> &[(ModelId, FieldId)] {
        &self.reads
    }

    /// Whether the rule names the company anywhere (DN §2.3: a scoped elevation can never lift
    /// it).
    pub fn mentions_company(&self) -> bool {
        self.mentions_company
    }
}

/// Builds [`PolicySnapshot`]s: options and the compile step.
///
/// ```
/// use std::sync::Arc;
/// use erp_registry::RegistryBuilder;
/// use erp_security::rows::PolicyRows;
/// use erp_security::snapshot::Compiler;
///
/// let registry = Arc::new(RegistryBuilder::new([]).linked(false).build()?);
/// let snapshot = Compiler::new(registry).compile(&PolicyRows::builder(3).build()?)?;
/// assert_eq!(erp_orm::AccessPolicy::generation(&snapshot).policy(), 3);
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
#[derive(Clone)]
#[must_use]
pub struct Compiler {
    registry: Arc<Registry>,
    origin: SnapshotOrigin,
    memo_capacity: usize,
    interner_capacity: usize,
    overrides: Vec<&'static AccessDomainImpl>,
    grant_sources: Vec<&'static dyn GrantSource>,
    capabilities: Vec<&'static Capability>,
    field_overrides: Vec<&'static FieldAccessImpl>,
    rule_operators: Vec<&'static RuleOperatorDecl>,
}

impl std::fmt::Debug for Compiler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Compiler")
            .field("origin", &self.origin)
            .field("memo_capacity", &self.memo_capacity)
            .field("interner_capacity", &self.interner_capacity)
            .finish_non_exhaustive()
    }
}

impl Compiler {
    /// A compiler for `registry`: shared snapshots, default memo bounds, the linked
    /// `#[access_domain]` overrides, grant sources and capabilities.
    pub fn new(registry: Arc<Registry>) -> Self {
        Self {
            registry,
            origin: SnapshotOrigin::Shared,
            memo_capacity: DEFAULT_MEMO_CAPACITY,
            interner_capacity: DEFAULT_INTERNER_CAPACITY,
            overrides: Vec::new(),
            grant_sources: Vec::new(),
            capabilities: Vec::new(),
            field_overrides: Vec::new(),
            rule_operators: Vec::new(),
        }
    }

    /// Build snapshots of `origin` (`Private` for a transaction's own uncommitted view, SEC-018).
    pub fn origin(mut self, origin: SnapshotOrigin) -> Self {
        self.origin = origin;
        self
    }

    /// Bound the combined-predicate memo (entries; zero is refused by [`Compiler::compile`]).
    pub fn memo_capacity(mut self, entries: usize) -> Self {
        self.memo_capacity = entries;
        self
    }

    /// Bound the group-set interner (sets; zero is refused by [`Compiler::compile`]).
    pub fn interner_capacity(mut self, sets: usize) -> Self {
        self.interner_capacity = sets;
        self
    }

    /// Add `#[access_domain]` overrides built at runtime (tests), besides the linked ones.
    pub fn overrides(mut self, extra: impl IntoIterator<Item = &'static AccessDomainImpl>) -> Self {
        self.overrides.extend(extra);
        self
    }

    /// Add grant sources (tests), besides the linked [`GRANT_SOURCES`].
    pub fn grant_sources(
        mut self,
        extra: impl IntoIterator<Item = &'static dyn GrantSource>,
    ) -> Self {
        self.grant_sources.extend(extra);
        self
    }

    /// Add registered capabilities (tests), besides the linked [`CAPABILITIES`].
    pub fn capabilities(mut self, extra: impl IntoIterator<Item = &'static Capability>) -> Self {
        self.capabilities.extend(extra);
        self
    }

    /// Add `#[field_access]` overrides built at runtime (tests), besides the linked ones.
    pub fn field_overrides(
        mut self,
        extra: impl IntoIterator<Item = &'static FieldAccessImpl>,
    ) -> Self {
        self.field_overrides.extend(extra);
        self
    }

    /// Add declared rule operators (tests), besides the linked [`erp_orm::RULE_OPERATORS`]
    /// (Amendment 1). Every declaration is validated by [`Compiler::compile`].
    pub fn rule_operators(
        mut self,
        extra: impl IntoIterator<Item = &'static RuleOperatorDecl>,
    ) -> Self {
        self.rule_operators.extend(extra);
        self
    }

    /// Compile `rows` (pure: no I/O). Per-rule problems do not fail the build: the rule denies
    /// the operations it applies to and is listed in [`PolicySnapshot::report`].
    ///
    /// # Errors
    ///
    /// `Programming` for a zero memo bound, a declared rule operator that does not hold up against
    /// the registry (Amendment 1: duplicate or malformed name, a core operator's name, a
    /// field that is not a `many2one_reference`, an unknown model), two grant sources with one name, an
    /// `#[access_domain]` or `#[field_access]` override declared twice or without a declaration
    /// of its model, or rows that refer to missing groups (which [`crate::rows::PolicyRowsBuilder::build`] refuses);
    /// `ResourceExhausted` when the process ran out of snapshot ids.
    pub fn compile(&self, rows: &PolicyRows) -> Result<PolicySnapshot> {
        let start = Instant::now();
        let registry = Arc::clone(&self.registry);
        let groups = GroupGraph::build(rows)?;
        let mut report = BuildReport::default();
        let mut rules: Vec<CompiledRule> = Vec::with_capacity(rows.rules().len());
        let expected = company_access::generate(&registry)?;
        let operators = Operators::new(
            &registry,
            erp_orm::RULE_OPERATORS
                .iter()
                .chain(self.rule_operators.iter().copied()),
        )?;
        for row in rows.rules() {
            let mut rule_ref = rule_ref(row)?;
            let generated = if row.generated() {
                expected.iter().find(|g| g.model_name() == row.model())
            } else {
                None
            };
            if let Some(g) = generated {
                let source = g.source();
                rule_ref = rule_ref.with_origin(RuleOrigin::Generated {
                    model: g.model_name().into(),
                    addon: source.addon().into(),
                    file: source.file().into(),
                    line: source.line(),
                });
            }
            let Some(model) = registry.model(row.model()) else {
                report.rule_errors.push(RuleIssue::new(
                    rule_ref,
                    row.model().to_owned(),
                    row.ops(),
                    RuleError::UnknownModel {
                        model: row.model().to_owned(),
                    },
                ));
                continue;
            };
            let deny = generated
                .map(|g| rule::compile(&registry, model, Some(g.deny_text())).map(|c| c.domain));
            let compiled = rule::compile_with(&registry, model, row.domain(), &operators);
            let (domain, reads, mentions_company, access, operator_targets) = match compiled {
                Ok(c) => (
                    Ok(c.domain),
                    c.reads,
                    c.mentions_company,
                    c.access,
                    c.operator_targets,
                ),
                Err(err) => (Err(err), Vec::new(), false, Vec::new(), Vec::new()),
            };
            rules.push(CompiledRule {
                rule: rule_ref,
                id: row.id(),
                xmlid: row.xmlid().cloned(),
                model: model.id,
                group: row.group(),
                ops: row.ops(),
                domain,
                reads,
                mentions_company,
                access,
                operator_targets,
                deny,
            });
        }
        let findings = company_access::verify_against(&registry, &expected, &rows.generated_rows());
        let mut drift: FxHashMap<ModelId, String> = FxHashMap::default();
        for finding in findings {
            let want = expected
                .iter()
                .find(|g| g.model_name() == finding.model_name());
            let problem = finding.issue().describe();
            let text = match want {
                Some(g) => format!(
                    "generated company restriction of {} ({}, addon '{}') {problem}",
                    g.model_name(),
                    g.spelling(),
                    g.source().addon()
                ),
                None => format!(
                    "generated company restriction row of {} {problem}",
                    finding.model_name()
                ),
            };
            if let (true, Some(model)) = (finding.denies(), finding.model()) {
                let entry = drift.entry(model).or_default();
                if !entry.is_empty() {
                    entry.push_str("; ");
                }
                entry.push_str(&problem);
            }
            report
                .generated_issues
                .push(GeneratedIssueReport::new(finding, text));
        }
        let drift: FxHashMap<ModelId, Box<str>> = drift
            .into_iter()
            .filter_map(|(model, problem)| {
                let g = expected.iter().find(|g| g.model() == model)?;
                Some((model, drift_message(g, &problem).into()))
            })
            .collect();
        mark_access_cycles(&registry, &mut rules);
        for compiled in &rules {
            if let Err(err) = &compiled.domain {
                report.rule_errors.push(RuleIssue::new(
                    compiled.rule.clone(),
                    registry.model_by_id(compiled.model).name.to_owned(),
                    compiled.ops,
                    err.clone(),
                ));
            }
        }
        report.rule_errors.sort_by_key(|issue| issue.rule().id());
        let mut by_model: FxHashMap<ModelId, Vec<usize>> = FxHashMap::default();
        for (i, compiled) in rules.iter().enumerate() {
            by_model.entry(compiled.model).or_default().push(i);
        }
        let reads_index = build_reads_index(&registry, &rules, &by_model);
        let chains = AccessDomainChains::build(&registry, &self.overrides)?;
        let grant_sources = all_grant_sources(&self.grant_sources)?;
        let (capabilities, missing, unknown) = capability_mapping(rows, &self.capabilities);
        report.missing_capabilities = missing;
        report.unknown_capabilities = unknown;
        let capability_specs: FxHashMap<Box<str>, std::result::Result<GroupSpec, GroupSpecError>> =
            capabilities
                .iter()
                .map(|(name, text)| (name.clone(), GroupSpec::parse(text, &groups)))
                .collect();
        report.capability_errors = {
            let mut issues: Vec<CapabilityIssue> = capability_specs
                .iter()
                .filter_map(|(name, spec)| {
                    spec.as_ref()
                        .err()
                        .map(|error| CapabilityIssue::new(name, error.clone()))
                })
                .collect();
            issues.sort_by(|a, b| a.name().cmp(b.name()));
            issues
        };
        let (field_rules, field_issues) =
            FieldRules::compile(&registry, &groups, rows.field_groups());
        report.field_group_issues = field_issues;
        let field_chains = FieldAccessTable::build(&registry, &self.field_overrides)?;
        report.group_cycles = groups.cycles().to_vec();
        report.counts = BuildCounts {
            rules: rows.rules().len(),
            compiled: rules.iter().filter(|r| r.domain.is_ok()).count(),
            groups: rows.groups().len(),
            implications: rows.implied().len(),
            models: by_model.len(),
        };
        let combined = Lru::new(self.memo_capacity, "combined access domain")?;
        let masks = Lru::new(self.memo_capacity, "field mask")?;
        let interner = GroupSetInterner::new(self.interner_capacity)?;
        report.duration = start.elapsed();
        let debug_group = groups.by_xmlid(&QualifiedXmlId::parse(DEBUG_GROUP)?);
        Ok(PolicySnapshot {
            id: SnapshotId::next()?,
            origin: self.origin,
            generation: PolicyGeneration::new(registry.generation(), rows.generation()),
            registry,
            rules,
            by_model,
            reads_index,
            groups,
            debug_group,
            capabilities,
            capability_specs,
            extra_capabilities: self.capabilities.clone(),
            field_groups: rows.field_groups().to_vec(),
            field_rules,
            field_chains,
            masks,
            chains,
            grant_sources,
            interner,
            combined,
            allowing: Mutex::new(FxHashMap::default()),
            drift,
            report,
        })
    }
}

/// Compile `rows` against `registry` with the default options ([`Compiler::new`]).
///
/// # Errors
///
/// As [`Compiler::compile`].
pub fn compile(rows: &PolicyRows, registry: Arc<Registry>) -> Result<PolicySnapshot> {
    Compiler::new(registry).compile(rows)
}

/// The access error of a model whose generated company restriction drifted (design §6.4).
fn drift_message(g: &GeneratedRestriction, problem: &str) -> String {
    format!(
        "The generated company restriction of {model} ({spelling}, declared by addon '{addon}') \
         {problem}; access to {model} is refused until an update of addon '{addon}' restores it.",
        model = g.model_name(),
        spelling = g.spelling(),
        addon = g.source().addon(),
    )
}

fn rule_ref(row: &RuleRow) -> Result<RuleRef> {
    let rule = RuleRef::new(row.id().get(), row.name())?;
    Ok(match row.xmlid() {
        Some(xmlid) => rule.with_xmlid(xmlid.as_str()),
        None => rule,
    })
}

/// Rules whose `access` conditions reach back to a (model, operation) they serve (R-S4): every
/// rule with an edge on a cycle becomes a rule error, so it denies for everyone it applies to
/// and is reported at build time. `_inherits` parents are edges too (a child's predicate holds
/// its parents'). Cycles a principal could only reach through `#[access_domain]` overrides are
/// caught when the predicate is combined (the resolution stack).
fn mark_access_cycles(registry: &Registry, rules: &mut [CompiledRule]) {
    let mut index: FxHashMap<(ModelId, Operation), u32> = FxHashMap::default();
    let mut nodes: Vec<(ModelId, Operation)> = Vec::new();
    let mut adjacency: Vec<Vec<u32>> = Vec::new();
    let mut node = |key: (ModelId, Operation), adjacency: &mut Vec<Vec<u32>>| -> u32 {
        *index.entry(key).or_insert_with(|| {
            nodes.push(key);
            adjacency.push(Vec::new());
            (nodes.len() - 1) as u32
        })
    };
    let mut labelled: Vec<(u32, u32, usize)> = Vec::new();
    for (i, compiled) in rules.iter().enumerate() {
        if compiled.domain.is_err() {
            continue;
        }
        for op in compiled.ops.iter() {
            for leaf in &compiled.access {
                let u = node((compiled.model, op), &mut adjacency);
                let v = node((leaf.target, leaf.op), &mut adjacency);
                adjacency[u as usize].push(v);
                labelled.push((u, v, i));
            }
        }
    }
    if labelled.is_empty() {
        return;
    }
    for spec in registry.models().iter().filter(|m| m.inherits_access) {
        for &(parent, _) in &spec.inherits {
            for op in Operation::ALL {
                let u = node((spec.id, op), &mut adjacency);
                let v = node((parent, op), &mut adjacency);
                adjacency[u as usize].push(v);
            }
        }
    }
    let components = graph::components(&adjacency);
    for (u, v, i) in labelled {
        if components.of[u as usize] != components.of[v as usize] || rules[i].domain.is_err() {
            continue;
        }
        // a path v -> … -> u inside the component closes the cycle u -> v -> … -> u
        let path = path_within(&adjacency, &components.of, v, u);
        let mut cycle = vec![nodes[u as usize]];
        cycle.extend(path.iter().map(|n| nodes[*n as usize]));
        rules[i].domain = Err(RuleError::AccessCycle {
            cycle: cycle
                .into_iter()
                .map(|(model, op)| (registry.model_by_id(model).name.to_owned(), op))
                .collect(),
        });
    }
}

/// The nodes of a shortest path from `from` to `to` (both included) staying in their component.
fn path_within(adjacency: &[Vec<u32>], component: &[u32], from: u32, to: u32) -> Vec<u32> {
    let c = component[from as usize];
    let mut parent: FxHashMap<u32, u32> = FxHashMap::default();
    let mut queue = std::collections::VecDeque::from([from]);
    let mut seen: FxHashSet<u32> = FxHashSet::from_iter([from]);
    while let Some(n) = queue.pop_front() {
        if n == to {
            break;
        }
        for &m in &adjacency[n as usize] {
            if component[m as usize] == c && seen.insert(m) {
                parent.insert(m, n);
                queue.push_back(m);
            }
        }
    }
    let mut path = vec![to];
    let mut current = to;
    while current != from {
        match parent.get(&current) {
            Some(&p) => {
                path.push(p);
                current = p;
            }
            // `to` is in the component of `from`, so the search reached it
            None => break,
        }
    }
    path.reverse();
    path
}

/// Written model → (model, operation) pairs whose rule predicates read one of its fields,
/// following `_inherits` parents and `access` targets (R-S5). Grant domains and
/// `#[access_domain]` overrides are added per combined predicate (`AccessDomain::reads`).
fn build_reads_index(
    registry: &Registry,
    rules: &[CompiledRule],
    by_model: &FxHashMap<ModelId, Vec<usize>>,
) -> FxHashMap<ModelId, Vec<(ModelId, Operation)>> {
    // the read models of each (model, op) and the pairs it includes
    let mut direct: FxHashMap<(ModelId, Operation), FxHashSet<ModelId>> = FxHashMap::default();
    let mut includes: FxHashMap<(ModelId, Operation), Vec<(ModelId, Operation)>> =
        FxHashMap::default();
    for (model, indices) in by_model {
        for &i in indices {
            let compiled = &rules[i];
            if compiled.domain.is_err() {
                continue;
            }
            for op in compiled.ops.iter() {
                let entry = direct.entry((*model, op)).or_default();
                entry.extend(compiled.reads.iter().map(|(m, _)| *m));
                entry.extend(compiled.operator_targets.iter().copied());
                for leaf in &compiled.access {
                    includes
                        .entry((*model, op))
                        .or_default()
                        .push((leaf.target, leaf.op));
                }
            }
        }
    }
    for spec in registry.models().iter().filter(|m| m.inherits_access) {
        for &(parent, _) in &spec.inherits {
            for op in Operation::ALL {
                // the child's predicate reads its delegate field and holds the parent's
                direct.entry((spec.id, op)).or_default().insert(spec.id);
                includes
                    .entry((spec.id, op))
                    .or_default()
                    .push((parent, op));
            }
        }
    }
    let mut index: FxHashMap<ModelId, Vec<(ModelId, Operation)>> = FxHashMap::default();
    let keys: Vec<(ModelId, Operation)> = direct.keys().copied().collect();
    for key in keys {
        // everything the predicate of `key` reads: its own reads and the included predicates'
        let mut seen: FxHashSet<(ModelId, Operation)> = FxHashSet::from_iter([key]);
        let mut stack = vec![key];
        let mut models: FxHashSet<ModelId> = FxHashSet::default();
        while let Some(current) = stack.pop() {
            if let Some(read) = direct.get(&current) {
                models.extend(read.iter().copied());
            }
            for next in includes.get(&current).into_iter().flatten() {
                if seen.insert(*next) {
                    stack.push(*next);
                }
            }
        }
        for model in models {
            index.entry(model).or_default().push(key);
        }
    }
    for pairs in index.values_mut() {
        pairs.sort_unstable();
        pairs.dedup();
    }
    index
}

fn all_grant_sources(extra: &[&'static dyn GrantSource]) -> Result<Vec<&'static dyn GrantSource>> {
    let sources: Vec<&'static dyn GrantSource> = GRANT_SOURCES
        .iter()
        .copied()
        .chain(extra.iter().copied())
        .collect();
    let mut names: FxHashSet<&str> = FxHashSet::default();
    for source in &sources {
        if !names.insert(source.name()) {
            return Err(Error::programming(format!(
                "two grant sources are named '{}': grant source names are unique",
                source.name()
            )));
        }
    }
    Ok(sources)
}

/// Capability name → group spec text (`ir_capability`).
type CapabilityMap = FxHashMap<Box<str>, Box<str>>;

/// The capability mapping, the registered capabilities without a row, and the rows naming no
/// registered capability (both sorted).
fn capability_mapping(
    rows: &PolicyRows,
    extra: &[&'static Capability],
) -> (CapabilityMap, Vec<&'static str>, Vec<String>) {
    let mapping: CapabilityMap = rows
        .capabilities()
        .iter()
        .map(|c| (Box::from(c.name()), Box::from(c.groups())))
        .collect();
    let registered: Vec<&'static Capability> =
        CAPABILITIES.iter().chain(extra.iter().copied()).collect();
    let mut missing: Vec<&'static str> = registered
        .iter()
        .filter(|cap| !mapping.contains_key(cap.name()))
        .map(|cap| cap.name())
        .collect();
    missing.sort_unstable();
    missing.dedup();
    let mut unknown: Vec<String> = rows
        .capabilities()
        .iter()
        .filter(|row| !registered.iter().any(|cap| cap.name() == row.name()))
        .map(|row| row.name().to_owned())
        .collect();
    unknown.sort();
    (mapping, missing, unknown)
}

/// Memo key of a combined predicate (DN §2.3): by value, with the snapshot implied (the memo
/// belongs to one snapshot, so a `GroupSetId` is never read under another one).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct CombinedKey {
    model: ModelId,
    op: Operation,
    groups: GroupSetId,
    shape: PrincipalShape,
}

/// Memo key of a field mask (DN §2.3): everything the mask depends on, by value. The declared
/// context values count only for models whose `#[field_access]` overrides declared keys, so
/// others share one entry whatever the context holds.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct MaskKey {
    model: ModelId,
    groups: GroupSetId,
    class: KindClass,
    elevation: ElevationKind,
    debug: bool,
    declared: FieldContextValues,
}

/// A compiled access policy: the rules, the group graph and the capability mapping of one
/// database at one `policy` generation, with bounded memos. Implements
/// [`AccessPolicy`] (`erp-orm` pins one per transaction).
pub struct PolicySnapshot {
    id: SnapshotId,
    origin: SnapshotOrigin,
    generation: PolicyGeneration,
    registry: Arc<Registry>,
    rules: Vec<CompiledRule>,
    by_model: FxHashMap<ModelId, Vec<usize>>,
    reads_index: FxHashMap<ModelId, Vec<(ModelId, Operation)>>,
    groups: GroupGraph,
    /// `base.group_no_one` when the snapshot has it, resolved once at build.
    debug_group: Option<GroupId>,
    capabilities: CapabilityMap,
    capability_specs: FxHashMap<Box<str>, std::result::Result<GroupSpec, GroupSpecError>>,
    extra_capabilities: Vec<&'static Capability>,
    field_groups: Vec<FieldGroupsRow>,
    field_rules: FieldRules,
    field_chains: FieldAccessTable,
    masks: Lru<MaskKey, Arc<FieldMask>>,
    chains: AccessDomainChains,
    grant_sources: Vec<&'static dyn GrantSource>,
    interner: GroupSetInterner,
    combined: Lru<CombinedKey, AccessDomain>,
    allowing: Mutex<FxHashMap<(ModelId, Operation), BitSet>>,
    /// Models whose generated company restriction is not as the declaration gives it: refused to
    /// every non-`Full` principal with this text (design `m3-company-a1` §6.4).
    drift: FxHashMap<ModelId, Box<str>>,
    report: BuildReport,
}

impl std::fmt::Debug for PolicySnapshot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PolicySnapshot")
            .field("id", &self.id)
            .field("origin", &self.origin)
            .field("generation", &self.generation)
            .field("rules", &self.rules.len())
            .field("groups", &self.groups.len())
            .finish_non_exhaustive()
    }
}

impl PolicySnapshot {
    /// What the build found (rule errors, cycles, capability mappings, counts).
    pub fn report(&self) -> &BuildReport {
        &self.report
    }

    /// The group graph.
    pub fn group_graph(&self) -> &GroupGraph {
        &self.groups
    }

    /// The compiled rules of `model`, by id.
    pub fn rules_of(&self, model: ModelId) -> impl Iterator<Item = &CompiledRule> + '_ {
        self.by_model
            .get(&model)
            .into_iter()
            .flatten()
            .map(|i| &self.rules[*i])
    }

    /// The (model, operation) predicates that read fields of `written` through their rules,
    /// `_inherits` parents and `access` targets (R-S5): a write to `written` drops their record
    /// memo entries. Grants and overrides add reads per combined predicate
    /// ([`AccessDomain::reads`]).
    pub fn reads_index(&self, written: ModelId) -> &[(ModelId, Operation)] {
        self.reads_index
            .get(&written)
            .map_or(&[], |pairs| pairs.as_slice())
    }

    /// The group spec text mapped to a capability name (`ir_capability`).
    pub fn capability_spec(&self, name: &str) -> Option<&str> {
        self.capabilities.get(name).map(|spec| &**spec)
    }

    /// The field-groups overlay of custom fields (rows as loaded; the masks merge them with the
    /// registry's `groups`).
    pub fn field_groups(&self) -> &[FieldGroupsRow] {
        &self.field_groups
    }

    /// The registry the snapshot was built for.
    pub fn registry(&self) -> &Arc<Registry> {
        &self.registry
    }

    /// Entries the combined-predicate memo and the interner evicted
    /// (`access_memo_evictions{memo}`).
    pub fn memo_evictions(&self) -> (u64, u64) {
        (self.combined.evictions(), self.interner.evictions())
    }

    /// Entries the field-mask memo evicted (`access_memo_evictions{memo="masks"}`).
    pub fn mask_evictions(&self) -> u64 {
        self.masks.evictions()
    }

    fn compiled_rules(&self, model: ModelId) -> impl Iterator<Item = &CompiledRule> + '_ {
        self.rules_of(model)
    }

    /// The rules of `op` on `model` that apply to the binding (its groups' permissions and every
    /// restriction), each with its predicate compiled for the binding (`access` conditions
    /// resolved), by rule id: the columns of the evidence statement of a record-level denial
    /// (SEC-009).
    ///
    /// # Errors
    ///
    /// The error of an applicable rule that does not compile; the binding's elevation errors.
    pub fn evidence_columns(
        &self,
        b: &PrincipalBinding<'_>,
        model: &ModelSpec,
        op: Operation,
    ) -> Result<Vec<EvidenceColumn>> {
        let mut combine = Combine {
            snapshot: self,
            groups: b.resolution().groups(),
            groups_id: b.groups(),
            shape: b.shape(),
            stack: vec![(model.id, op)],
        };
        let mut columns = Vec::new();
        for compiled in self.compiled_rules(model.id) {
            if !compiled.ops.contains(op) {
                continue;
            }
            if let Some(group) = compiled.group
                && !combine.groups.contains(group)
            {
                continue;
            }
            let domain = match compiled.domain_for(b.shape().companies()) {
                Ok(domain) => domain,
                Err(err) => return Err(rule_error(compiled, model, err)),
            };
            let kind = if compiled.group.is_some() {
                RuleKind::Permission
            } else {
                RuleKind::Restriction
            };
            let resolved = combine.resolve(model, domain)?;
            columns.push(
                EvidenceColumn::new(compiled.rule.clone(), kind, resolved)
                    .with_mentions_company(compiled.mentions_company),
            );
        }
        Ok(columns)
    }

    /// A snapshot without
    /// that group (only fixtures lack it) never lists records: the listing is presentation, the
    /// decision is the same.
    fn debug_listing(&self, b: &PrincipalBinding<'_>) -> Result<bool> {
        if !b.principal().debug() || b.shape().class() != KindClass::Internal {
            return Ok(false);
        }
        let no_one = QualifiedXmlId::parse(DEBUG_GROUP)?;
        Ok(self
            .groups
            .by_xmlid(&no_one)
            .is_some_and(|group| b.resolution().groups().contains(group)))
    }

    /// # Errors
    ///
    /// The errors of [`AccessPolicy::allowing_groups`].
    fn model_message(&self, model: &ModelSpec, op: Operation) -> Result<String> {
        let groups = self.allowing_groups(model, op)?;
        let names = groups
            .iter()
            .map(|g| self.groups.display_name(*g))
            .collect::<Result<Vec<String>>>()?;
        Ok(explain::model_message(model, op, &names))
    }

    fn allowing_bits(
        &self,
        model: ModelId,
        op: Operation,
        stack: &mut Vec<(ModelId, Operation)>,
    ) -> Result<BitSet> {
        if let Some(hit) = self
            .allowing
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&(model, op))
        {
            return Ok(hit.clone());
        }
        if let Some(position) = stack.iter().position(|e| *e == (model, op)) {
            return Err(self.cycle_error(&stack[position..], model, op));
        }
        stack.push((model, op));
        let result = self.allowing_uncached(model, op, stack);
        stack.pop();
        let bits = result?;
        self.allowing
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert((model, op), bits.clone());
        Ok(bits)
    }

    fn allowing_uncached(
        &self,
        model: ModelId,
        op: Operation,
        stack: &mut Vec<(ModelId, Operation)>,
    ) -> Result<BitSet> {
        let spec = self.registry.model_by_id(model);
        let mut out = BitSet::new(self.groups.len());
        for compiled in self.compiled_rules(model) {
            let Some(group) = compiled.group else {
                continue;
            };
            if !compiled.ops.contains(op) {
                continue;
            }
            // a permission that does not compile allows nobody (it denies, decision 2)
            let Ok(domain) = &compiled.domain else {
                continue;
            };
            let mut groups = self.groups.implied_by_bits(group)?;
            groups.intersect_with(&self.satisfying(spec, domain, stack)?);
            out.union_with(&groups);
        }
        for &(parent, _) in &spec.inherits {
            out.intersect_with(&self.allowing_bits(parent, op, stack)?);
        }
        Ok(out)
    }

    fn satisfying(
        &self,
        model: &ModelSpec,
        domain: &Domain,
        stack: &mut Vec<(ModelId, Operation)>,
    ) -> Result<BitSet> {
        Ok(match domain.kind() {
            DomainKind::True | DomainKind::Custom(_) => self.groups.all_bits(),
            DomainKind::False => BitSet::new(self.groups.len()),
            DomainKind::Not(child) => {
                let mut all = self.groups.all_bits();
                all.difference_with(&self.satisfying(model, child, stack)?);
                all
            }
            DomainKind::And(children) => {
                let mut all = self.groups.all_bits();
                for child in children {
                    all.intersect_with(&self.satisfying(model, child, stack)?);
                }
                all
            }
            DomainKind::Or(children) => {
                let mut none = BitSet::new(self.groups.len());
                for child in children {
                    none.union_with(&self.satisfying(model, child, stack)?);
                }
                none
            }
            DomainKind::Condition(c) if c.operator() == Operator::Access => {
                let (target, op) = access_target(&self.registry, model, c)?;
                self.allowing_bits(target.id, op, stack)?
            }
            DomainKind::Condition(_) => self.groups.all_bits(),
        })
    }

    fn cycle_error(&self, stack: &[(ModelId, Operation)], model: ModelId, op: Operation) -> Error {
        let mut text = String::new();
        for (m, o) in stack.iter().chain([&(model, op)]) {
            if !text.is_empty() {
                text.push_str(" -> ");
            }
            text.push_str(&format!("{} ({o})", self.registry.model_by_id(*m).name));
        }
        Error::access(format!(
            "The access domain of {} ({op}) cannot be computed: its 'access' conditions form a \
             cycle: {text}",
            self.registry.model_by_id(model).name
        ))
    }

    /// `cap` if `capability!` registered it (or the compiler was given it for a test).
    fn registered(&self, cap: &Capability) -> Result<&'static Capability> {
        match erp_orm::registered_capability(cap) {
            Ok(registered) => Ok(registered),
            Err(error) => self
                .extra_capabilities
                .iter()
                .copied()
                .find(|extra| std::ptr::eq(*extra, cap))
                .ok_or_else(|| Error::from(error)),
        }
    }

    fn combine(
        &self,
        b: &PrincipalBinding<'_>,
        model: &ModelSpec,
        op: Operation,
    ) -> Result<AccessDomain> {
        let mut combine = Combine {
            snapshot: self,
            groups: b.resolution().groups(),
            groups_id: b.groups(),
            shape: b.shape(),
            stack: Vec::new(),
        };
        combine.domain(model.id, op)
    }
}

/// The model an `access` condition asks about, and the operation (DOM-035): the model itself for
/// `id`, the comodel of a many2one.
fn access_target<'r>(
    registry: &'r Registry,
    model: &'r ModelSpec,
    condition: &Condition,
) -> Result<(&'r ModelSpec, Operation)> {
    let field = condition.field_expr();
    let op = match condition.value() {
        Value::Str(value) => Operation::parse(value),
        _ => None,
    };
    let Some(op) = op else {
        return Err(Error::programming(format!(
            "Invalid value for 'access' operator in condition on {}.{field}",
            model.name
        )));
    };
    if field == "id" {
        return Ok((model, op));
    }
    match model.field(field).map(|spec| &spec.ty) {
        Some(FieldType::Many2one { comodel, .. }) => match registry.model(comodel) {
            Some(target) => Ok((target, op)),
            None => Err(Error::programming(format!(
                "the comodel {comodel} of {}.{field} is not in the registry",
                model.name
            ))),
        },
        _ => Err(Error::programming(format!(
            "The 'access' operator works only for many2one and 'id' fields (condition on {}.{field})",
            model.name
        ))),
    }
}

/// The model reached by the relational path `field_expr` from `model`.
fn path_comodel<'r>(
    registry: &'r Registry,
    model: &'r ModelSpec,
    field_expr: &str,
) -> Result<&'r ModelSpec> {
    let mut current = model;
    for segment in field_expr.split('.') {
        let comodel = current
            .field(segment)
            .and_then(|spec| comodel_of(&spec.ty))
            .and_then(|comodel| registry.model(comodel));
        current = comodel.ok_or_else(|| {
            Error::programming(format!(
                "access predicate of {}: '{field_expr}' is not a relational path",
                model.name
            ))
        })?;
    }
    Ok(current)
}

/// Whether `domain` holds an `access` condition or a declared rule operator, sub-domains included.
fn has_access(domain: &Domain) -> bool {
    match domain.kind() {
        DomainKind::True | DomainKind::False => false,
        // a declared rule operator is expanded when the predicate is combined (Amendment 1)
        DomainKind::Custom(node) => RuleOperatorLeaf::of(node.as_ref()).is_some(),
        DomainKind::Not(child) => has_access(child),
        DomainKind::And(children) | DomainKind::Or(children) => children.iter().any(has_access),
        DomainKind::Condition(c) => {
            c.operator() == Operator::Access
                || matches!(c.value(), Value::Domain(sub) if has_access(sub))
        }
    }
}

impl GroupLookup for GroupGraph {
    fn group_id(&self, xmlid: &QualifiedXmlId) -> Result<Option<GroupId>> {
        Ok(self.by_xmlid(xmlid))
    }
}

/// Stage 4 for one binding: combination, grants, overrides and `access` resolution, with the
/// resolution stack that refuses cycles.
struct Combine<'s> {
    snapshot: &'s PolicySnapshot,
    groups: &'s GroupSet,
    groups_id: GroupSetId,
    shape: &'s PrincipalShape,
    stack: Vec<(ModelId, Operation)>,
}

impl Combine<'_> {
    fn domain(&mut self, model: ModelId, op: Operation) -> Result<AccessDomain> {
        let key = CombinedKey {
            model,
            op,
            groups: self.groups_id,
            shape: self.shape.clone(),
        };
        if let Some(hit) = self.snapshot.combined.get(&key) {
            return Ok(hit);
        }
        if let Some(position) = self.stack.iter().position(|e| *e == (model, op)) {
            return Err(self
                .snapshot
                .cycle_error(&self.stack[position..], model, op));
        }
        self.stack.push((model, op));
        let result = self.compute(model, op);
        self.stack.pop();
        let domain = result?;
        // a private snapshot's memo dies with it; a shared one serves every transaction
        self.snapshot.combined.insert(key, domain.clone());
        Ok(domain)
    }

    fn ctx<'a>(&'a self, spec: &'a ModelSpec) -> Result<AccessCtx<'a>> {
        let elevation = match self.shape.elevation() {
            ElevationKey::None => ElevationKind::None,
            ElevationKey::Full => ElevationKind::Full,
            ElevationKey::Scoped(_) => ElevationKind::Scoped,
            _ => {
                return Err(Error::programming(
                    "a principal shape has an elevation kind this policy does not know",
                ));
            }
        };
        Ok(AccessCtx::new(AccessCtxParts::new(
            self.groups,
            self.shape.class(),
            elevation,
            spec,
            &self.snapshot.registry,
            &self.snapshot.groups,
        )))
    }

    fn compute(&mut self, model: ModelId, op: Operation) -> Result<AccessDomain> {
        let snapshot = self.snapshot;
        let spec = snapshot.registry.model_by_id(model);
        match self.shape.elevation() {
            ElevationKey::None => {}
            // superuser mode restricts nothing (the engine allows it before asking)
            ElevationKey::Full => return Ok(AccessDomain::allow_all()),
            ElevationKey::Scoped(_) => {
                return Err(Error::programming(
                    "scoped elevation blocks are not supported until a later release",
                ));
            }
            _ => {
                return Err(Error::programming(
                    "a principal shape has an elevation kind this policy does not know",
                ));
            }
        }
        if let Some(message) = snapshot.drift.get(&model) {
            return Err(Error::access(message.to_string()));
        }
        let mut permissions: Vec<Domain> = Vec::new();
        let mut restrictions: Vec<Domain> = Vec::new();
        let mut rules: Vec<RuleRef> = Vec::new();
        // `_inherits` parents: a parent that denies everything denies the child
        // , unless the model does not check its parents'
        // access (`_check_inherits_access = False`)
        let parents: &[(ModelId, FieldId)] = if spec.inherits_access {
            &spec.inherits
        } else {
            &[]
        };
        for &(parent, delegate) in parents {
            let parent_domain = self.domain(parent, op)?;
            if parent_domain.is_false() {
                return Ok(AccessDomain::builder(Domain::FALSE)
                    .rules(parent_domain.rules().iter().cloned())
                    .build());
            }
            let delegate = spec.field_by_id(delegate);
            restrictions.push(condition(
                delegate.name,
                Operator::AnyBypass,
                Value::Domain(parent_domain.domain().clone()),
            )?);
        }
        for compiled in snapshot.compiled_rules(model) {
            if !compiled.ops.contains(op) {
                continue;
            }
            if let Some(group) = compiled.group
                && !self.groups.contains(group)
            {
                continue;
            }
            let domain = match compiled.domain_for(self.shape.companies()) {
                Ok(domain) => domain,
                Err(err) => return Err(rule_error(compiled, spec, err)),
            };
            rules.push(compiled.rule.clone());
            if compiled.group.is_some() {
                permissions.push(domain.clone());
            } else {
                restrictions.push(domain.clone());
            }
        }
        let mut grants = Vec::new();
        {
            let ctx = self.ctx(spec)?;
            for source in &snapshot.grant_sources {
                if source.applies(&ctx, spec, op)? {
                    let grant = source.grant_domain(&ctx, spec, op)?;
                    grants.push(grant.source().clone());
                    permissions.push(grant.domain().clone());
                }
            }
        }
        // OR(permissions, grants) AND AND(restrictions, parents); an empty OR is FALSE (SEC-002)
        let combined = Domain::and([Domain::or(permissions), Domain::and(restrictions)]);
        let overridden = {
            let ctx = self.ctx(spec)?;
            snapshot.chains.apply(&ctx, op, &combined)?
        };
        let resolved = self.resolve(spec, &overridden)?;
        let analysis = analyze(&snapshot.registry, spec, &resolved)?;
        let mut builder = AccessDomain::builder(resolved)
            .reads(analysis.reads)
            .rules(rules)
            .reads_own_table(analysis.subselects.contains(&model));
        for grant in grants {
            builder = builder.grant(grant);
        }
        Ok(builder.build())
    }

    /// Expand a declared rule operator on `model` for the binding (Amendment 1): one
    /// branch `model_column = M AND reference_column any! reference_query(M, <M's combined
    /// predicate>)` per concrete model `M` the operator ranges over whose predicate for the
    /// operation is not `FALSE` for the binding's groups; none is `FALSE`. The branches depend on
    /// the groups and the shape only (the memo key of this predicate) and hold principal values as
    /// parameters, never literals. A target whose predicate is `TRUE` still selects from its
    /// table: the column has no foreign key, so an orphan must not match.
    ///
    /// # Errors
    ///
    /// The error of a target's predicate (a rule that does not compile, a cycle through the
    /// `access` operator), `Programming` for a leaf on a model other than the declared one.
    fn expand_rule_operator(
        &mut self,
        model: &ModelSpec,
        leaf: &RuleOperatorLeaf,
    ) -> Result<Domain> {
        let decl = leaf.decl();
        let registry = Arc::clone(&self.snapshot.registry);
        let FieldType::Many2oneReference { model_field } = model
            .field(decl.field())
            .map(|f| &f.ty)
            .filter(|_| model.name == decl.model())
            .ok_or_else(|| {
                Error::programming(format!(
                    "the '{}' operator is declared for {}.{}, not for {}",
                    decl.name(),
                    decl.model(),
                    decl.field(),
                    model.name
                ))
            })?
        else {
            return Err(Error::programming(format!(
                "{}.{} is not a many2one_reference",
                model.name,
                decl.field()
            )));
        };
        let mut branches = Vec::new();
        for target in Operators::targets(&registry, decl) {
            let predicate = self.domain(target.id, leaf.op())?;
            if predicate.is_false() {
                continue;
            }
            branches.push(Domain::and([
                condition(model_field, Operator::In, Value::list([target.name]))?,
                condition(
                    decl.field(),
                    Operator::AnyBypass,
                    ReferenceQuery::new(target.id, predicate.domain().clone()).into_value(),
                )?,
            ]));
        }
        Ok(Domain::or(branches))
    }

    /// Resolve the `access` conditions of `domain`, a predicate on `model` (DOM-035): `FALSE`
    /// when the target allows nothing, `field != False` when it allows everything,
    /// `field any! <target predicate>` otherwise; for `id`, the model's own predicate.
    fn resolve(&mut self, model: &ModelSpec, domain: &Domain) -> Result<Domain> {
        if !has_access(domain) {
            return Ok(domain.clone());
        }
        Ok(match domain.kind() {
            DomainKind::Custom(node) => match RuleOperatorLeaf::of(node.as_ref()) {
                Some(leaf) => self.expand_rule_operator(model, leaf)?,
                None => domain.clone(),
            },
            DomainKind::True | DomainKind::False => domain.clone(),
            DomainKind::Not(child) => {
                // a negated leaf is refused when the rule compiles; a negation reaching here (an
                // `#[access_domain]` override) could let rows through where the expansion is
                // unknown, so it is refused too
                if holds_rule_operator(child) {
                    return Err(Error::programming(format!(
                        "a declared rule operator under a negation in the predicate of {}",
                        model.name
                    )));
                }
                !self.resolve(model, child)?
            }
            DomainKind::And(children) => Domain::and(
                children
                    .iter()
                    .map(|c| self.resolve(model, c))
                    .collect::<Result<Vec<_>>>()?,
            ),
            DomainKind::Or(children) => Domain::or(
                children
                    .iter()
                    .map(|c| self.resolve(model, c))
                    .collect::<Result<Vec<_>>>()?,
            ),
            DomainKind::Condition(c) if c.operator() == Operator::Access => {
                let registry = &self.snapshot.registry;
                let (target, op) = access_target(registry, model, c)?;
                let target_domain = self.domain(target.id, op)?;
                let field = c.field_expr();
                if field == "id" {
                    target_domain.domain().clone()
                } else if target_domain.is_false() {
                    Domain::FALSE
                } else if target_domain.is_true() {
                    condition(field, Operator::Ne, Value::FALSE)?
                } else {
                    condition(
                        field,
                        Operator::AnyBypass,
                        Value::Domain(target_domain.domain().clone()),
                    )?
                }
            }
            DomainKind::Condition(c) => match c.value() {
                Value::Domain(sub) => {
                    // a leaf under `not any`/`not any!` is a negated leaf: refused as under `!`
                    if matches!(c.operator(), Operator::NotAny | Operator::NotAnyBypass)
                        && holds_rule_operator(sub)
                    {
                        return Err(Error::programming(format!(
                            "a declared rule operator under '{}' in the predicate of {}",
                            c.operator(),
                            model.name
                        )));
                    }
                    let comodel = path_comodel(&self.snapshot.registry, model, c.field_expr())?;
                    let resolved = self.resolve(comodel, sub)?;
                    condition(c.field_expr(), c.operator(), Value::Domain(resolved))?
                }
                _ => domain.clone(),
            },
        })
    }
}

/// The access error of a rule that does not compile (DN §3.1): it denies its model's operation.
fn rule_error(compiled: &CompiledRule, model: &ModelSpec, err: &RuleError) -> Error {
    Error::access(format!(
        "Access rule '{}' ({}) on {} cannot be compiled: {err}",
        compiled.rule.name(),
        compiled.rule.id(),
        model.name
    ))
    .with_source(err.clone())
}

fn condition(field: &str, operator: Operator, value: Value) -> Result<Domain> {
    Domain::condition(field, operator, value).map_err(Error::from)
}

impl AccessPolicy for PolicySnapshot {
    /// An immutable snapshot: what it resolves never changes.
    fn group_refs_are_stable(&self) -> bool {
        true
    }

    fn generation(&self) -> PolicyGeneration {
        self.generation
    }

    fn snapshot(&self) -> SnapshotRef {
        SnapshotRef::new(self.id, self.origin)
    }

    /// A compiled policy decides on the subject's groups and companies.
    fn needs_resolved_principals(&self) -> bool {
        true
    }

    /// Interns the set after checking that every group is in this snapshot (a group deleted
    /// since the subject was resolved is an error, never dropped).
    fn intern_groups(&self, groups: &GroupSet) -> Result<InternedGroups> {
        if let Some(missing) = groups.ids().iter().find(|g| !self.groups.contains(**g)) {
            return Err(Error::access(format!(
                "The group {missing} does not exist (any more): the user's groups cannot be \
                 resolved."
            )));
        }
        let share = self.groups.is_share(groups)?;
        Ok(InternedGroups::new(self.interner.intern(groups)?, share))
    }

    fn check_model(
        &self,
        b: &PrincipalBinding<'_>,
        model: &ModelSpec,
        op: Operation,
    ) -> Result<()> {
        if let Some(scope) = b.principal().scope()
            && scope.read_only()
            && op != Operation::Read
        {
            return Err(Error::access(format!(
                "The API key '{}' is read-only: it may not {} '{}' ({}).",
                scope.name(),
                op.as_str(),
                model.description,
                model.name
            )));
        }
        if self.access_domain(b, model, op)?.is_false() {
            return Err(Error::access(self.model_message(model, op)?));
        }
        Ok(())
    }

    fn access_domain(
        &self,
        b: &PrincipalBinding<'_>,
        model: &ModelSpec,
        op: Operation,
    ) -> Result<AccessDomain> {
        self.combine(b, model, op)
    }

    /// The fields of `model` the binding may read and write: each restricted field's group spec
    /// against the binding's groups (`groups` of the declaration and the custom-field overlay;
    /// `.` forbids for everyone), then the model's `#[field_access]` overrides. Memoized by
    /// (model, group set, class, elevation, debug, declared context values), bounded; a model
    /// with neither restriction nor override gets the shared all-accessible mask without a memo
    /// entry. No statements.
    fn field_context_keys(&self) -> &[&'static str] {
        self.field_chains.all_context_keys()
    }

    fn field_access(&self, b: &PrincipalBinding<'_>, model: &ModelSpec) -> Result<Arc<FieldMask>> {
        let restricted = self.field_rules.restricts(model.id);
        let overridden = self.field_chains.has_overrides(model.id);
        if !restricted && !overridden {
            return Ok(FieldMask::shared_all());
        }
        // only the keys this model's own overrides declared: the memo and the overrides see
        // nothing of the keys other models declared
        let declared = b
            .field_ctx()
            .restricted_to(self.field_chains.context_keys(model.id));
        let key = MaskKey {
            model: model.id,
            groups: b.groups(),
            class: b.shape().class(),
            elevation: ElevationKind::of(b.shape().elevation()),
            debug: b.principal().debug(),
            declared,
        };
        if let Some(hit) = self.masks.get(&key) {
            return Ok(hit);
        }
        let groups = b.resolution().groups();
        let base = if restricted {
            Arc::new(self.field_rules.mask(model, groups, key.debug)?)
        } else {
            FieldMask::shared_all()
        };
        let mask = if overridden {
            let ctx = AccessCtx::new(AccessCtxParts::new(
                groups,
                key.class,
                key.elevation,
                model,
                &self.registry,
                &self.groups,
            ));
            self.field_chains.narrow(ctx, &key.declared, base)?
        } else {
            base
        };
        self.masks.insert(key, Arc::clone(&mask));
        Ok(mask)
    }

    /// Other principals get nothing.
    ///
    /// # Errors
    ///
    /// `Programming` when the field's group spec does not compile (the build report lists it).
    fn field_denial_detail(
        &self,
        b: &PrincipalBinding<'_>,
        model: &ModelSpec,
        field: &FieldSpec,
        _op: FieldOp,
    ) -> Result<Option<FieldDenialDetail>> {
        let no_one = QualifiedXmlId::parse(DEBUG_GROUP)?;
        let member = self
            .groups
            .by_xmlid(&no_one)
            .is_some_and(|group| b.resolution().groups().contains(group));
        if !member {
            return Ok(None);
        }
        self.field_rules
            .detail(model, field.id, &self.groups)
            .map(Some)
    }

    /// Group-spec evaluation over the binding's closed group set ([`crate::group_spec`]).
    fn has_groups(&self, b: &PrincipalBinding<'_>, spec: &str) -> Result<bool> {
        let parsed = GroupSpec::parse(spec, &self.groups)?;
        Ok(parsed.satisfied_by(b.resolution().groups(), b.principal().debug()))
    }

    /// Membership in one of `groups` over the binding's closed group set; `base.group_no_one`
    /// only in debug mode, as [`AccessPolicy::has_groups`] does.
    fn has_any_group(&self, b: &PrincipalBinding<'_>, groups: &[GroupId]) -> Result<bool> {
        let held = b.resolution().groups();
        let debug = b.principal().debug();
        Ok(groups
            .iter()
            .any(|g| held.contains(*g) && (debug || Some(*g) != self.debug_group)))
    }

    fn resolve_group_ref(&self, xmlid: &QualifiedXmlId) -> Result<erp_orm::GroupRefResolution> {
        Ok(match self.groups.by_xmlid(xmlid) {
            Some(id) => erp_orm::GroupRefResolution::Known {
                id,
                debug_only: Some(id) == self.debug_group,
            },
            None => erp_orm::GroupRefResolution::Unknown,
        })
    }

    fn groups(&self, direct: &[GroupId]) -> Result<GroupSet> {
        self.groups.closure(direct)
    }

    fn allowing_groups(&self, model: &ModelSpec, op: Operation) -> Result<Vec<GroupId>> {
        let bits = self.allowing_bits(model.id, op, &mut Vec::new())?;
        let mut groups = self.groups.ids_of(&bits);
        self.groups.sort(&mut groups)?;
        Ok(groups)
    }

    /// Whether the binding's subject holds `cap`: the mapped group spec (`ir_capability`)
    /// against the subject's groups. Capabilities belong to the subject: elevation never grants
    /// one, so a superuser-mode environment is answered like the plain one.
    ///
    /// # Errors
    ///
    /// `Programming` for a value `capability!` did not register and for a registered
    /// capability without a mapping row (the build report lists it; never `false`); `Access`
    /// when the mapped spec does not compile (the build report lists it).
    fn has_capability(&self, b: &PrincipalBinding<'_>, cap: &Capability) -> Result<bool> {
        let cap = self.registered(cap)?;
        match self.capability_specs.get(cap.name()) {
            Some(Ok(spec)) => Ok(spec.satisfied_by(b.resolution().groups(), b.principal().debug())),
            Some(Err(error)) => Err(Error::access(format!(
                "The capability '{}' cannot be evaluated: {error}",
                cap.name()
            ))
            .with_source(error.clone())),
            None => Err(Error::programming(format!(
                "the capability '{}' has no ir_capability row in this policy: map it to groups \
                 (the build report lists it)",
                cap.name()
            ))),
        }
    }

    fn generated_restriction(
        &self,
        b: &PrincipalBinding<'_>,
        model: &ModelSpec,
    ) -> Result<Option<AccessDomain>> {
        // superuser mode restricts nothing (the engine does not ask)
        if matches!(b.shape().elevation(), ElevationKey::Full) {
            return Ok(None);
        }
        if let Some(message) = self.drift.get(&model.id) {
            return Err(Error::access(message.to_string()));
        }
        let Some(compiled) = self
            .compiled_rules(model.id)
            .find(|c| c.generated() && c.group.is_none())
        else {
            return Ok(None);
        };
        let domain = match compiled.domain_for(b.shape().companies()) {
            Ok(domain) => domain,
            Err(err) => return Err(rule_error(compiled, model, err)),
        };
        let mut combine = Combine {
            snapshot: self,
            groups: b.resolution().groups(),
            groups_id: b.groups(),
            shape: b.shape(),
            stack: vec![(model.id, Operation::Write)],
        };
        let resolved = combine.resolve(model, domain)?;
        Ok(Some(
            AccessDomain::builder(resolved)
                .rule(compiled.rule.clone())
                .reads(compiled.reads.iter().copied())
                .build(),
        ))
    }

    /// Model level (no ids): the decision, with the SEC-008 text when denied. Records: without
    /// evidence, the evidence columns to evaluate on the denied ids (decision `Deny`: the engine
    /// asks about records its check refused); with evidence, the rules to blame and the SEC-009
    /// text.
    fn explain(&self, req: &ExplainRequest<'_>) -> Result<Explanation> {
        let (b, model, op) = (req.binding(), req.model(), req.op());
        let domain = self.access_domain(b, model, op)?;
        if domain.is_false() {
            let message = self.model_message(model, op)?;
            return Ok(
                Explanation::new(AccessDecision::Deny, self.generation, Vec::new())
                    .with_message(message),
            );
        }
        if req.record_ids().is_empty() {
            return Ok(Explanation::new(
                AccessDecision::Allow,
                self.generation,
                Vec::new(),
            ));
        }
        let Some(evidence) = req.record_evidence() else {
            let columns = self.evidence_columns(b, model, op)?;
            return Ok(
                Explanation::new(AccessDecision::Deny, self.generation, Vec::new())
                    .with_columns(columns)
                    .with_record_listing(self.debug_listing(b)?),
            );
        };
        if let Some(row) = evidence
            .rows()
            .iter()
            .find(|row| !req.record_ids().contains(&row.id()))
        {
            return Err(Error::programming(format!(
                "access evidence names record {}, which the explanation was not asked about",
                row.id()
            )));
        }
        let debug = self.debug_listing(b)?;
        let text =
            RecordMessage::new(model, op, b.principal().subject().get(), evidence).debug(debug);
        Ok(
            Explanation::new(AccessDecision::Deny, self.generation, blame(evidence))
                .with_message(text.render())
                .with_context(text.context()),
        )
    }
}

#[cfg(test)]
mod field_tests;
#[cfg(test)]
mod tests;
