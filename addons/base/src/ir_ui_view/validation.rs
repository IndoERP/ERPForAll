//! Checking written views: lock the trees'
//! roots, fetch the trees, refuse recursion, combine the written views and the primary views their
//! change can break, validate the written views' archs (archived ones on their own), and map the
//! record layer's errors to ORM errors.

use std::collections::{HashMap, HashSet};

use erp_sdk::prelude::*;
use erp_sdk::views::Inheritance;
use erp_sdk::views::record::{
    Alone, InvalidView, RecordLimit, ViewLabel, ViewRecordError, ViewRow, inheritance_cycle,
    parse_alone, validation_targets, widens_validation,
};
use erp_sdk::views::validate::{
    ArchWarning, FULL_VALIDATION_TASK, RegistryLookup, Scope, group_refs_with_limits, reads_fields,
    validate_combined_in,
};
use erp_sdk::views::{Arch, LimitKind, ViewCatalogue};
use erp_sdk::{Context, Error, Registry, ValueMap};

use super::tree::{self, FetchedView};
use crate::group_graph::Graph;

// The context keys of the data loader's load state, shared with the loader through erp-sdk
// (`erp_data::load::load_state` documents their layout; review T-4); `INSTALL_MODULE` names the
// module loading now.
use erp_sdk::load_state::{
    BATCH_NAMES, BATCH_POSITIONS, BATCH_WRITTEN, INSTALL_MODE, INSTALL_MODULE, LOADED_NAMES,
    PENDING_MODULES,
};

/// The typed error is the source.
pub fn view_error(error: ViewRecordError) -> Error {
    let invariant = match &error {
        ViewRecordError::InvariantViolation(_) | ViewRecordError::InvalidMode { .. } => true,
        // InvalidView::source deliberately exposes the leaf beneath the engine's InView
        // wrappers. A broken engine contract is never the view author's validation error.
        ViewRecordError::Invalid { source, .. } => matches!(
            std::error::Error::source(source.as_ref())
                .and_then(|leaf| leaf.downcast_ref::<erp_sdk::views::ViewError>()),
            Some(erp_sdk::views::ViewError::InvariantViolation(_))
        ),
        // The remaining record variants describe invalid authored data, as;
        // only the contract variants and a wrapped engine invariant carry the invariant mark.
        _ => false,
    };
    let mut mapped = if invariant {
        Error::invariant(error.to_string())
    } else {
        Error::validation(error.to_string())
    };
    if let ViewRecordError::Invalid { view, written, .. } = &error {
        let text = |v: Option<&str>| v.map_or(Value::False, Value::str);
        let context: ValueMap = [
            ("name", Value::str(written.as_str())),
            ("xmlid", text(view.view_xmlid())),
            ("view.model", text(view.view_model())),
            ("file", text(view.view_file())),
            (
                "line",
                view.view_line()
                    .map_or(Value::False, |l| Value::Int(i64::from(l))),
            ),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_owned(), v))
        .collect();
        mapped = mapped.with_context(Value::Map(Arc::new(context)));
    }
    mapped.with_source(error)
}

/// Most group external ids the implication check loads (`res.groups` records with an external
/// id); more is a resource-limit refusal, never a truncated answer.
const MAX_IMPLICATION_GROUPS: usize = 65_536;

/// The group external ids and the implication graph, loaded for the expression check
/// : whether a node's guard implies a restricted field's groups. A group with no
/// external id implies nothing, so an unknown pair is refused (fail closed).
struct Implications {
    ids: HashMap<String, i64>,
    graph: Graph,
}

impl Implications {
    fn none() -> Self {
        Self {
            ids: HashMap::new(),
            graph: Graph::default(),
        }
    }

    /// Every external id of a `res.groups` record (one statement) and the `implied_ids` relation
    /// (one statement). Without `ir.model.data` or `res.groups` there is nothing to load.
    async fn load(tx: &mut Transaction) -> Result<Self> {
        if !tree::has_external_ids(tx) || tx.registry().model("res.groups").is_none() {
            return Ok(Self::none());
        }
        tree::flush_external_ids(tx).await?;
        let limit = i64::try_from(MAX_IMPLICATION_GROUPS + 1)
            .map_err(|_| Error::invariant("the implication group bound fits an i64"))?;
        let rows = tx
            .query(
                "SELECT module || '.' || name, res_id FROM ir_model_data \
                  WHERE model = 'res.groups' AND res_id IS NOT NULL LIMIT $1",
                &[&limit],
            )
            .await?;
        if rows.len() > MAX_IMPLICATION_GROUPS {
            return Err(view_error(ViewRecordError::Limit {
                limit: RecordLimit::ImplicationGroups,
                actual: rows.len(),
                max: MAX_IMPLICATION_GROUPS,
            }));
        }
        let mut ids = HashMap::with_capacity(rows.len());
        for row in &rows {
            let xmlid: String = row
                .try_get(0)
                .map_err(|e| Error::db(format!("unexpected value in ir_model_data: {e}")))?;
            let id: i64 = row
                .try_get(1)
                .map_err(|e| Error::db(format!("unexpected value in ir_model_data: {e}")))?;
            ids.insert(xmlid, id);
        }
        Ok(Self {
            ids,
            graph: Graph::load(tx).await?,
        })
    }

    fn implies(&self, group: &str, implied: &str) -> bool {
        match (self.ids.get(group), self.ids.get(implied)) {
            (Some(&group), Some(&implied)) => self.graph.up([group]).contains(&implied),
            _ => false,
        }
    }
}

/// The registry's models and fields, the group references that exist, and the group
/// implications.
struct RegistryFields<'a> {
    registry: &'a Registry,
    groups: &'a HashSet<String>,
    implications: &'a Implications,
}

impl RegistryLookup for RegistryFields<'_> {
    fn has_model(&self, model: &str) -> bool {
        self.registry.model(model).is_some()
    }

    fn has_field(&self, model: &str, field: &str) -> bool {
        self.registry
            .model(model)
            .is_some_and(|m| m.field(field).is_some())
    }

    fn has_group(&self, xmlid: &str) -> bool {
        self.groups.contains(xmlid)
    }

    fn field_groups(&self, model: &str, field: &str) -> Option<&str> {
        self.registry.model(model)?.field(field)?.groups
    }

    fn comodel(&self, model: &str, field: &str) -> Option<&str> {
        self.registry.model(model)?.field(field)?.ty.comodel()
    }

    fn implies(&self, group: &str, implied: &str) -> bool {
        self.implications.implies(group, implied)
    }
}

/// The load state of a module operation, read from the context the data loader writes with.
/// The lists are the context's own: nothing is copied per check
/// (review A-R2).
struct LoadState {
    /// The module loading now; `None` after a module finished, when every module of the
    /// operation that is not pending counts as loaded (`_validate_module_views`, review F-2).
    current: Option<Arc<str>>,
    /// The modules of the operation whose data has not loaded yet.
    pending: Arc<[Value]>,
    /// The names the current module loaded before this write, sorted chunks.
    chunks: Arc<[Value]>,
    /// The batch being written, when the loader tracks it.
    batch: Option<Batch>,
}

/// The batch of an `ir.ui.view` window: its names sorted, each name's write position at the same
/// index, and how many records were written before this write.
struct Batch {
    names: Arc<[Value]>,
    positions: Arc<[Value]>,
    written: usize,
}

/// A load-state key without the loader's layout: a broken contract between the loader and the
/// view model, marked so the demo savepoint does not take it for a data mistake (review F-1).
fn context_error(key: &str, value: &Value, expected: &str) -> Error {
    Error::invariant(format!(
        "context['{key}'] is {value:?}; it must be {expected} (erp_data::load::load_state)"
    ))
}

fn list(context: &Context, key: &str, module: &str) -> Result<Arc<[Value]>> {
    match context.get(key) {
        Some(Value::List(items)) => Ok(items),
        Some(other) => Err(context_error(key, &other, "a list")),
        None => Err(Error::invariant(format!(
            "the context names install_module '{module}' without '{key}': a write in a data \
             load must carry the loader's load state (erp_data::load::load_state)"
        ))),
    }
}

/// The text of a list item of the load state: every list holds names, never `False`
/// (review A2-S4).
fn item<'v>(value: &'v Value, key: &str) -> Result<&'v str> {
    match value {
        Value::Str(s) => Ok(s),
        other => Err(context_error(key, other, "a list of names")),
    }
}

/// The load state of the context, `None` outside a data load.
///
/// # Errors
///
/// `Programming` when `install_module` is set without `install_mode` or without the load-state
/// keys, or a key does not have the loader's layout: a view check that took a missing state for "nothing loaded" would
/// skip the views of the module silently (review A-S2).
fn load_state(records: &Records) -> Result<Option<LoadState>> {
    let context = records.env().context();
    let Some(module) = context.get(INSTALL_MODULE) else {
        return Ok(None);
    };
    let Value::Str(current) = module else {
        return Err(context_error(INSTALL_MODULE, &module, "a module name"));
    };
    // the loader always sets both; a state without `install_mode` was not written by it
    // (review A3-S1; the keys are server-only over RPC too)
    if !context.get(INSTALL_MODE).is_some_and(|v| v.is_truthy()) {
        return Err(Error::invariant(format!(
            "the context names install_module '{current}' without install_mode: the load state \
             is the data loader's, which always sets install_mode (erp_data::load::load_state)"
        )));
    }
    let pending = list(context, PENDING_MODULES, &current)?;
    let chunks = list(context, LOADED_NAMES, &current)?;
    let batch = match (
        context.get(BATCH_NAMES),
        context.get(BATCH_POSITIONS),
        context.get(BATCH_WRITTEN),
    ) {
        (None, None, None) => None,
        (Some(Value::List(names)), Some(Value::List(positions)), Some(Value::Int(written)))
            if names.len() == positions.len() =>
        {
            let written = usize::try_from(written).map_err(|_| {
                context_error(BATCH_WRITTEN, &Value::Int(written), "a count of the batch")
            })?;
            Some(Batch {
                names,
                positions,
                written,
            })
        }
        (names, positions, written) => {
            return Err(Error::invariant(format!(
                "context['{BATCH_NAMES}'] is {names:?}, context['{BATCH_POSITIONS}'] is \
                 {positions:?} and context['{BATCH_WRITTEN}'] is {written:?}; they come \
                 together: two lists of one length and a count"
            )));
        }
    };
    Ok(Some(LoadState {
        current: Some(current),
        pending,
        chunks,
        batch,
    }))
}

impl LoadState {
    fn after_module(pending: &[String]) -> Self {
        LoadState {
            current: None,
            pending: pending.iter().map(|m| Value::str(m.as_str())).collect(),
            chunks: Arc::from(Vec::<Value>::new()),
            batch: None,
        }
    }

    /// Whether the data of `module.name` is loaded in this operation: the module is not pending
    /// and, for the module loading now, the name was loaded before this write (an earlier
    /// window, or an earlier write of this batch: review A-R1).
    fn is_loaded(&self, module: &str, name: &str) -> Result<bool> {
        // the modules of one operation: a handful, scanned
        for m in self.pending.iter() {
            if item(m, PENDING_MODULES)? == module {
                return Ok(false);
            }
        }
        if self.current.as_deref() != Some(module) {
            return Ok(true);
        }
        for chunk in self.chunks.iter() {
            let Value::List(names) = chunk else {
                return Err(context_error(LOADED_NAMES, chunk, "a list of name lists"));
            };
            if sorted_position(names, name, LOADED_NAMES, Order::Sorted)?.is_some() {
                return Ok(true);
            }
        }
        if let Some(batch) = &self.batch
            && let Some(at) = sorted_position(&batch.names, name, BATCH_NAMES, Order::Strict)?
        {
            let position = match &batch.positions[at] {
                Value::Int(p) => usize::try_from(*p).map_err(|_| {
                    context_error(BATCH_POSITIONS, &Value::Int(*p), "write positions")
                })?,
                other => return Err(context_error(BATCH_POSITIONS, other, "write positions")),
            };
            return Ok(position < batch.written);
        }
        Ok(false)
    }
}

/// Whether a load-state list may hold a name twice: the loaded-name chunks may (a name marked
/// loaded twice), the batch names may not (one entry per name, review A4-S5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Order {
    /// Non-decreasing.
    Sorted,
    /// Strictly increasing.
    Strict,
}

/// The index of `name` in `names` (the load-state list `key`, ordered bytewise as `order`
/// says), by binary search. The order is the loader's invariant, asserted by its producers in
/// debug builds; that assertion is the guard. Here only the neighbours of the place the search
/// ends at are checked, in O(1): a list out of order there is refused rather than read as "not
/// loaded"; disorder elsewhere in the list is not seen (reviews A3-S2, A3-R4, A4-R5).
///
/// # Errors
///
/// `Programming` for an item that is not text, or neighbours out of order.
fn sorted_position(names: &[Value], name: &str, key: &str, order: Order) -> Result<Option<usize>> {
    let (mut low, mut high) = (0usize, names.len());
    let mut found = None;
    while low < high {
        let mid = low + (high - low) / 2;
        #[cfg(test)]
        probes::count();
        match item(&names[mid], key)?.cmp(name) {
            std::cmp::Ordering::Equal => {
                found = Some(mid);
                break;
            }
            std::cmp::Ordering::Less => low = mid + 1,
            std::cmp::Ordering::Greater => high = mid,
        }
    }
    // the item before the place must be below `name`, the one after above it (or equal, for a
    // list that may repeat a name)
    let at = found.unwrap_or(low);
    let after = if found.is_some() { at + 1 } else { at };
    let repeats = order == Order::Sorted;
    let before_ok = match at.checked_sub(1) {
        Some(b) => {
            let before = item(&names[b], key)?;
            before < name || (repeats && before == name)
        }
        None => true,
    };
    let after_ok = match names.get(after) {
        Some(a) => {
            let after = item(a, key)?;
            after > name || (repeats && after == name)
        }
        None => true,
    };
    if !(before_ok && after_ok) {
        return Err(Error::invariant(format!(
            "context['{key}'] is out of order around '{name}': the data loader's lists are \
             sorted bytewise (erp_data::load::load_state)"
        )));
    }
    Ok(found)
}

/// Comparisons made by [`sorted_position`] on this thread (the complexity pin of the reader).
#[cfg(test)]
mod probes {
    use std::cell::Cell;

    thread_local!(static PROBES: Cell<u64> = const { Cell::new(0) });

    pub(super) fn count() {
        PROBES.with(|p| p.set(p.get() + 1));
    }

    pub(super) fn take() -> u64 {
        PROBES.with(|p| p.replace(0))
    }
}

/// During a module operation, the sibling views whose external ids all belong to data not
/// loaded yet in it are not combined: they still hold the arch of before the upgrade.
/// A view without external id is combined.
async fn loaded_only(
    tx: &mut Transaction,
    state: &LoadState,
    candidates: Vec<i64>,
) -> Result<Vec<i64>> {
    let xmlids = tree::external_ids(tx, &candidates).await?;
    let mut kept = Vec::with_capacity(candidates.len());
    for id in candidates {
        let loaded = match xmlids.get(&id) {
            None => true,
            Some(names) => {
                let mut any = false;
                for (module, name) in names {
                    if state.is_loaded(module, name)? {
                        any = true;
                        break;
                    }
                }
                any
            }
        };
        if loaded {
            kept.push(id);
        }
    }
    Ok(kept)
}

/// The fetched view `id`.
///
/// # Errors
///
/// `Programming` when the view was not fetched: every view a check names is in its trees.
fn fetched<'v>(views: &HashMap<i64, &'v FetchedView>, id: i64) -> Result<&'v FetchedView> {
    views.get(&id).copied().ok_or_else(|| {
        Error::invariant(format!(
            "ir.ui.view {id} is named by a view check but was not fetched with its tree"
        ))
    })
}

/// The labels of the views `ids` for errors and warnings: name and model, first external id,
/// data file (one statement each for the external ids and the files).
async fn labels(
    tx: &mut Transaction,
    views: &HashMap<i64, &FetchedView>,
    ids: &[i64],
) -> Result<HashMap<i64, ViewLabel>> {
    let mut xmlids = tree::external_ids(tx, ids).await?;
    let mut files = tree::arch_files(tx, ids).await?;
    let mut labels = HashMap::with_capacity(ids.len());
    for &id in ids {
        let view = fetched(views, id)?;
        let xmlid = xmlids
            .remove(&id)
            .and_then(|names| names.into_iter().next())
            .map(|(module, name)| format!("{module}.{name}"));
        labels.insert(
            id,
            ViewLabel::new(id, view.name.clone())
                .model(view.model.clone())
                .xmlid(xmlid)
                .file(files.remove(&id).flatten()),
        );
    }
    Ok(labels)
}

/// The error of an invalid view: labelled with the view whose spec or node failed (or `fallback`
/// when the error does not say), the written view's name in the context.
async fn invalid(
    tx: &mut Transaction,
    views: &HashMap<i64, &FetchedView>,
    fallback: i64,
    written: &[i64],
    source: InvalidView,
) -> Result<Error> {
    // an error that names no view (a catalogue limit) is reported against `fallback`
    let at = source.origin_view().unwrap_or(fallback);
    let line = source.origin().map(|o| o.line());
    let label = labels(tx, views, &[at])
        .await?
        .remove(&at)
        .ok_or_else(|| Error::invariant(format!("no label was built for ir.ui.view {at}")))?
        .line(line);
    // the written view the error is about: the failing view itself when it was written, else
    // the first written view of the check
    let written_id = if written.contains(&at) {
        at
    } else {
        *written
            .first()
            .ok_or_else(|| Error::invariant("a view check without written views failed"))?
    };
    let written_name = fetched(views, written_id)?.name.clone();
    Ok(view_error(ViewRecordError::Invalid {
        view: Box::new(label),
        written: written_name,
        source: Box::new(source),
    }))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Written {
    /// `create`: partial validation.
    Created,
    /// `write`, `_validate_module_views`: full validation.
    Changed,
}

/// Where a check reads the load state of a module operation from.
#[derive(Debug, Clone, Copy)]
pub(crate) enum Loading<'a> {
    /// The written records' context (`create`, `write`, [`check_views`]).
    Context,
    /// The end of a module's data: the modules named have not loaded theirs yet
    /// (`_validate_module_views`, review F-2).
    AfterModule(&'a [String]),
}

/// What a check found besides errors: its warnings and the fetched names the log needs.
struct Checked {
    warnings: Vec<ArchWarning>,
    fetched: Vec<FetchedView>,
}

/// # Errors
///
/// `Validation` with a [`ViewRecordError`] source: [`ViewRecordError::RecursiveInheritance`],
/// [`ViewRecordError::Invalid`] (combination, parse or arch), [`ViewRecordError::Limit`];
/// `Programming` in a read-only transaction (the check writes the roots), and for a load state
/// the context does not hold in the loader's layout; `Concurrency` (`40001`, `40P01`) when a
/// concurrent transaction wrote a root of the same tree: retry the whole transaction; another
/// database error.
pub async fn check_views(
    tx: &mut Transaction,
    records: &Records,
    ids: &[i64],
) -> Result<Vec<ArchWarning>> {
    Ok(check(tx, records, ids, Written::Changed, Loading::Context)
        .await?
        .warnings)
}

/// [`check_views`], then post its warnings to the transaction notice channel ([`Notice`]) and
/// log them with the labels of their views: one line per view for its unknown groups (review S-9),
/// one line for all sub-views not validated (review A-S6) ([`warning_lines`]).
pub(crate) async fn check_and_log(
    tx: &mut Transaction,
    records: &Records,
    ids: &[i64],
    written: Written,
    loading: Loading<'_>,
) -> Result<()> {
    let checked = check(tx, records, ids, written, loading).await?;
    if checked.warnings.is_empty() {
        return Ok(());
    }
    let views: HashMap<i64, &FetchedView> = checked.fetched.iter().map(|v| (v.id, v)).collect();
    let mut warned: Vec<i64> = checked
        .warnings
        .iter()
        .map(|w| w.origin().view_id())
        .collect();
    warned.sort_unstable();
    warned.dedup();
    let labels = labels(tx, &views, &warned).await?;
    for (kind, line) in warning_lines(&checked.warnings, &labels)? {
        tracing::warn!(target: "erp::views", "{line}");
        tx.post_notice(Notice::new(kind, "erp::views", line));
    }
    Ok(())
}

/// The log lines of a check's warnings (review S-9): one line per view for its unknown groups,
/// each distinct reference once with its first arch line and how often it occurs, and one line
/// for every sub-view not validated (they are expected in every x2many form until the full
/// validation lands, a line each would flood the log). The references come from the arch and the
/// labels from the records, so control characters are escaped: no line can fake another.
///
/// # Errors
///
/// `Programming` when a warning names a view `labels` has no label for.
fn warning_lines(
    warnings: &[ArchWarning],
    labels: &HashMap<i64, ViewLabel>,
) -> Result<Vec<(NoticeKind, String)>> {
    let label = |id: i64| -> Result<&ViewLabel> {
        labels
            .get(&id)
            .ok_or_else(|| Error::invariant(format!("no label was built for ir.ui.view {id}")))
    };
    // per view in the order first warned: each reference in the order first written, with its
    // first line and its count (looked up through maps: a check can carry many warnings)
    let mut groups: Vec<(i64, Vec<GroupRef<'_>>)> = Vec::new();
    let mut view_at: HashMap<i64, usize> = HashMap::new();
    let mut ref_at: HashMap<(i64, &str), usize> = HashMap::new();
    let mut sub_views: Vec<(i64, u32)> = Vec::new();
    let mut others: Vec<(NoticeKind, String)> = Vec::new();
    for warning in warnings {
        match warning {
            ArchWarning::UnknownGroup { xmlid, origin } => {
                let view = origin.view_id();
                let at = *view_at.entry(view).or_insert_with(|| {
                    groups.push((view, Vec::new()));
                    groups.len() - 1
                });
                let refs = &mut groups[at].1;
                match ref_at.entry((view, xmlid.as_str())) {
                    std::collections::hash_map::Entry::Occupied(e) => refs[*e.get()].count += 1,
                    std::collections::hash_map::Entry::Vacant(e) => {
                        e.insert(refs.len());
                        refs.push(GroupRef {
                            xmlid: xmlid.as_str(),
                            line: origin.line(),
                            count: 1,
                        });
                    }
                }
            }
            ArchWarning::SubViewNotValidated { origin, .. } => {
                sub_views.push((origin.view_id(), origin.line()));
            }
            // a kind added to the engine later: one line each, until it gets its own grouping
            other => others.push((
                NoticeKind::Other,
                escape_control(&format!("{}: {other}", label(other.origin().view_id())?)),
            )),
        }
    }
    let mut lines = Vec::with_capacity(groups.len() + others.len() + 1);
    for (view, refs) in &groups {
        let total: usize = refs.iter().map(|r| r.count).sum();
        let mut line = format!(
            "{}: {total} reference(s) to unknown groups in groups attributes, which never match: ",
            label(*view)?
        );
        for (
            i,
            GroupRef {
                xmlid,
                line: at,
                count,
            },
        ) in refs.iter().enumerate()
        {
            if i > 0 {
                line.push_str(", ");
            }
            line.push_str(&format!("'{xmlid}' (arch line {at}"));
            if *count > 1 {
                line.push_str(&format!(", {count} times"));
            }
            line.push(')');
        }
        lines.push((NoticeKind::UnknownGroup, escape_control(&line)));
    }
    if !sub_views.is_empty() {
        let mut places = String::new();
        let mut last: Option<i64> = None;
        for (view, line) in &sub_views {
            if last == Some(*view) {
                places.push_str(&format!(", {line}"));
            } else {
                if last.is_some() {
                    places.push_str("; ");
                }
                places.push_str(&format!("{}, arch lines {line}", label(*view)?));
                last = Some(*view);
            }
        }
        lines.push((
            NoticeKind::SubViewNotValidated,
            escape_control(&format!(
                "{} x2many sub-view(s) are not validated until {FULL_VALIDATION_TASK}: {places}",
                sub_views.len()
            )),
        ));
    }
    lines.extend(others);
    Ok(lines)
}

/// One unknown group reference of a view in [`warning_lines`]: first arch line, occurrences.
struct GroupRef<'a> {
    xmlid: &'a str,
    line: u32,
    count: usize,
}

/// `text` with every control character (newlines included) written as its Rust escape.
fn escape_control(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        if c.is_control() {
            out.extend(c.escape_default());
        } else {
            out.push(c);
        }
    }
    out
}

#[allow(
    clippy::too_many_lines,
    reason = "one pass in the reference ERP's order: lock, fetch, recursion, combine, validate"
)]
async fn check(
    tx: &mut Transaction,
    records: &Records,
    ids: &[i64],
    written: Written,
    loading: Loading<'_>,
) -> Result<Checked> {
    // the check writes (the trees' roots): refused up front in a read-only transaction, with
    // the reason, rather than by the raw-statement guard halfway (reviews A3-S4, A3-R1)
    if tx.intent().is_read() {
        return Err(Error::invariant(
            "ir.ui.view: the view check locks and writes the roots of the trees it checks, so \
             that concurrent writers of a tree fail; it needs a write transaction, not a \
             read-only one",
        ));
    }
    let Some(&first) = ids.first() else {
        return Ok(Checked {
            warnings: Vec::new(),
            fetched: Vec::new(),
        });
    };
    // concurrent writes to one tree are serialized on its root (reviews A-D4, A2-D1)
    let tree::Trees {
        views: fetched_views,
        rows,
        ..
    } = tree::lock_and_fetch_trees(tx, ids).await?;
    let views: HashMap<i64, &FetchedView> = fetched_views.iter().map(|v| (v.id, v)).collect();
    // the rows move into the catalogue: each arch is held once (review S-3)
    let descriptors = rows
        .into_iter()
        .map(ViewRow::into_descriptor)
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(view_error)?;
    let catalogue = match ViewCatalogue::new(descriptors) {
        Ok(catalogue) => catalogue,
        Err(e) => {
            return Err(invalid(tx, &views, first, ids, InvalidView::Combine(e)).await?);
        }
    };
    for &id in ids {
        match inheritance_cycle(&catalogue, id) {
            Ok(None) => {}
            Ok(Some(cycle)) => {
                return Err(view_error(ViewRecordError::RecursiveInheritance {
                    views: cycle,
                }));
            }
            Err(e) => {
                return Err(invalid(tx, &views, id, ids, InvalidView::Combine(e)).await?);
            }
        }
    }
    let targets = match validation_targets(&catalogue, ids) {
        Ok(targets) => targets,
        Err(e) => {
            return Err(invalid(tx, &views, first, ids, InvalidView::Combine(e)).await?);
        }
    };
    let state = match loading {
        Loading::Context => load_state(records)?,
        Loading::AfterModule(pending) => Some(LoadState::after_module(pending)),
    };
    let combined = match state {
        Some(state) => loaded_only(tx, &state, targets.combined).await?,
        None => targets.combined,
    };
    let mut all: Vec<i64> = targets.validated.clone();
    all.extend(combined);
    let results = match catalogue.combine_many(&all) {
        Ok(results) => results,
        Err(e) => {
            return Err(invalid(tx, &views, first, ids, InvalidView::Combine(e)).await?);
        }
    };
    // the archived written views, each parsed alone; a root primary one is validated
    let mut alone = Vec::with_capacity(targets.standalone.len());
    for &id in &targets.standalone {
        match parse_alone(&catalogue, id) {
            Ok(Alone::WholeView(view)) => alone.push((id, view)),
            // the specs of a view with a parent: parsing them is the whole check
            Ok(Alone::Specs(_)) => {}
            Err(e) => {
                return Err(invalid(tx, &views, id, ids, InvalidView::Combine(e)).await?);
            }
        }
    }
    let created: HashSet<i64> = match written {
        Written::Created => ids.iter().copied().collect(),
        Written::Changed => HashSet::new(),
    };
    let mut widening: HashSet<i64> = HashSet::new();
    if written == Written::Created {
        for &id in ids {
            let is_root = catalogue
                .get(id)
                .is_some_and(|v| v.inheritance() == Inheritance::Root);
            if is_root {
                continue;
            }
            match widens_validation(&catalogue, id) {
                Ok(true) => {
                    widening.insert(id);
                }
                Ok(false) => {}
                Err(e) => {
                    return Err(invalid(tx, &views, id, ids, InvalidView::Combine(e)).await?);
                }
            }
        }
    }
    let scope_of = |applied: &[i64]| -> Scope<'_> {
        if written == Written::Changed
            || applied.first().is_some_and(|root| created.contains(root))
            || applied.iter().any(|v| widening.contains(v))
        {
            Scope::Whole
        } else {
            Scope::Views(&created)
        }
    };
    let checked: Vec<(i64, &Arch, Scope<'_>)> = targets
        .validated
        .iter()
        .copied()
        .zip(
            results
                .iter()
                .map(|c| (c.arch(), scope_of(c.applied_view_ids()))),
        )
        .map(|(id, (arch, scope))| (id, arch, scope))
        .chain(
            alone
                .iter()
                .map(|(id, view)| (*id, view.arch(), Scope::Whole)),
        )
        .collect();
    // every attribute bounded by `group_refs`, the check's distinct references bounded here,
    // both before the lookup (review R-1/S-1); a set, so the union is linear
    let max_refs = catalogue.limits().get(LimitKind::CheckGroupRefs);
    let mut refs: Vec<&str> = Vec::new();
    let mut seen: HashSet<&str> = HashSet::new();
    let mut refused: Option<(i64, InvalidView)> = None;
    for (id, arch, _) in &checked {
        match group_refs_with_limits(arch, catalogue.limits()) {
            Ok(found) => refs.extend(found.into_iter().filter(|&r| seen.insert(r))),
            Err(issue) => {
                refused = Some((*id, InvalidView::Arch(issue)));
                break;
            }
        }
        if refs.len() > max_refs {
            return Err(view_error(ViewRecordError::Limit {
                limit: RecordLimit::GroupRefs,
                actual: refs.len(),
                max: max_refs,
            }));
        }
    }
    if let Some((id, source)) = refused {
        return Err(invalid(tx, &views, id, ids, source).await?);
    }
    let groups = tree::existing_groups(tx, &refs).await?;
    // the implication data is read only for an arch whose expressions can read a restricted field
    let implications = if checked.iter().any(|(_, arch, _)| reads_fields(arch)) {
        Implications::load(tx).await?
    } else {
        Implications::none()
    };
    let fields = RegistryFields {
        registry: tx.registry(),
        groups: &groups,
        implications: &implications,
    };
    let mut warnings = Vec::new();
    let mut failure: Option<(i64, InvalidView)> = None;
    for (id, arch, scope) in &checked {
        let view = fetched(&views, *id)?;
        let view_type = view.view_type.as_deref();
        match validate_combined_in(arch, view_type, view.model.as_deref(), &fields, *scope) {
            Ok(found) => warnings.extend(found),
            Err(issue) => {
                failure = Some((*id, InvalidView::Arch(issue)));
                break;
            }
        }
    }
    if let Some((id, source)) = failure {
        return Err(invalid(tx, &views, id, ids, source).await?);
    }
    drop(views);
    Ok(Checked {
        warnings,
        fetched: fetched_views,
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "tests")]

    use super::*;
    use erp_sdk::ErrorKind;

    #[test]
    fn wrapped_engine_invariants_keep_their_marker() {
        use erp_sdk::views::ViewError;
        let error = view_error(ViewRecordError::Invalid {
            view: Box::new(ViewLabel::new(7, "broken")),
            written: "written".into(),
            source: Box::new(InvalidView::Combine(ViewError::InView {
                view_id: 7,
                view_name: "broken".into(),
                source: Box::new(ViewError::InvariantViolation(
                    "parser lost its open element",
                )),
            })),
        });
        assert_eq!(error.kind(), ErrorKind::Programming);
        assert!(error.is_invariant_violation());
        assert_eq!(
            error.message(),
            "Error while validating view broken:\ninternal error in the view engine: parser lost its open element"
        );
    }

    #[test]
    fn impossible_stored_modes_keep_their_invariant_marker() {
        let error = view_error(ViewRecordError::InvalidMode {
            value: "broken".into(),
        });
        assert_eq!(error.kind(), ErrorKind::Programming);
        assert!(error.is_invariant_violation());
        assert_eq!(
            error.message(),
            "invalid view mode 'broken': expected 'primary' or 'extension'"
        );
    }

    fn names(list: &[&str]) -> Arc<[Value]> {
        list.iter().map(Value::str).collect()
    }

    fn state(pending: Arc<[Value]>, chunks: Vec<Arc<[Value]>>, batch: Option<Batch>) -> LoadState {
        LoadState {
            current: Some(Arc::from("m")),
            pending,
            chunks: chunks.into_iter().map(Value::List).collect(),
            batch,
        }
    }

    /// The reader's complexity pin (review A2-R4): every name of a batch of 4,096 looked up at
    /// every written count costs at most `log2(4096) + 1 = 13` comparisons per lookup; the scan of
    /// the batch prefix it replaces made about `b² / 2` (8.4 million here, per written count).
    #[test]
    #[ignore = "DEFERRED_PERFORMANCE: milestone-end product cost; semantic coverage runs in ordinary CI"]
    fn batch_lookups_are_logarithmic() {
        const B: usize = 4_096;
        let sorted: Vec<String> = (0..B).map(|i| format!("view_{i:05}")).collect();
        // write order is the reverse of the name order
        let positions: Arc<[Value]> = (0..B)
            .map(|i| Value::Int(i64::try_from(B - 1 - i).unwrap()))
            .collect();
        let refs: Vec<&str> = sorted.iter().map(String::as_str).collect();
        let list = names(&refs);
        probes::take();
        let mut lookups = 0u64;
        for written in [0, 1, B / 2, B] {
            let load = state(
                names(&[]),
                Vec::new(),
                Some(Batch {
                    names: Arc::clone(&list),
                    positions: Arc::clone(&positions),
                    written,
                }),
            );
            for (i, name) in sorted.iter().enumerate() {
                let loaded = load.is_loaded("m", name).unwrap();
                assert_eq!(loaded, B - 1 - i < written, "{name} at {written}");
                lookups += 1;
            }
            assert!(!load.is_loaded("m", "absent").unwrap());
            lookups += 1;
        }
        let probes = probes::take();
        assert!(
            probes <= lookups * 13,
            "{probes} comparisons for {lookups} lookups"
        );
    }

    /// Membership follows write positions without a comparison-count budget.
    #[test]
    fn batch_lookups_preserve_written_membership() {
        const B: usize = 4_096;
        let sorted: Vec<String> = (0..B).map(|i| format!("view_{i:05}")).collect();
        // write order is the reverse of the name order
        let positions: Arc<[Value]> = (0..B)
            .map(|i| Value::Int(i64::try_from(B - 1 - i).unwrap()))
            .collect();
        let refs: Vec<&str> = sorted.iter().map(String::as_str).collect();
        let list = names(&refs);
        for written in [0, 1, B / 2, B] {
            let load = state(
                names(&[]),
                Vec::new(),
                Some(Batch {
                    names: Arc::clone(&list),
                    positions: Arc::clone(&positions),
                    written,
                }),
            );
            for (i, name) in sorted.iter().enumerate() {
                let loaded = load.is_loaded("m", name).unwrap();
                assert_eq!(loaded, B - 1 - i < written, "{name} at {written}");
            }
            assert!(!load.is_loaded("m", "absent").unwrap());
        }
    }

    /// Chunks are searched too; another module's name is loaded unless that module is pending.
    #[test]
    fn chunks_and_pending_modules() {
        let load = state(
            names(&["later"]),
            vec![names(&["a", "c"]), names(&["b"])],
            None,
        );
        assert!(load.is_loaded("m", "b").unwrap());
        assert!(load.is_loaded("m", "c").unwrap());
        assert!(!load.is_loaded("m", "d").unwrap());
        assert!(load.is_loaded("done", "x").unwrap());
        assert!(!load.is_loaded("later", "x").unwrap());
    }

    /// A list out of order where the search ends is refused, not read as "not loaded"
    /// (reviews A3-S2, A3-R4).
    #[test]
    fn a_list_out_of_order_is_refused() {
        let load = state(names(&[]), vec![names(&["b", "a"])], None);
        let err = load.is_loaded("m", "a").unwrap_err();
        assert_eq!(err.kind(), ErrorKind::Programming);
        assert_eq!(
            err.to_string(),
            "context['install_loaded_names'] is out of order around 'a': the data loader's lists \
             are sorted bytewise (erp_data::load::load_state)"
        );
        // a chunk may repeat a name, the batch names may not (review A4-S5)
        let load = state(names(&[]), vec![names(&["a", "a", "b"])], None);
        assert!(load.is_loaded("m", "a").unwrap());
        let twice = Batch {
            names: names(&["a", "a"]),
            positions: vec![Value::Int(0), Value::Int(1)].into(),
            written: 2,
        };
        let err = state(names(&[]), Vec::new(), Some(twice))
            .is_loaded("m", "a")
            .unwrap_err();
        assert_eq!(err.kind(), ErrorKind::Programming);
        assert_eq!(
            err.to_string(),
            "context['install_batch_names'] is out of order around 'a': the data loader's lists \
             are sorted bytewise (erp_data::load::load_state)"
        );
        // in order, found and not found
        let load = state(names(&[]), vec![names(&["a", "b", "c"])], None);
        assert!(load.is_loaded("m", "b").unwrap());
        assert!(!load.is_loaded("m", "bb").unwrap());
    }

    /// `False` is no module name and no loaded name (review A2-S4): the lists hold names only.
    #[test]
    fn false_in_a_name_list_is_refused() {
        let pending: Arc<[Value]> = vec![Value::False].into();
        let err = state(pending, Vec::new(), None)
            .is_loaded("m", "x")
            .unwrap_err();
        assert_eq!(err.kind(), ErrorKind::Programming);
        assert_eq!(
            err.to_string(),
            "context['install_pending_modules'] is False; it must be a list of names \
             (erp_data::load::load_state)"
        );
        let chunk: Arc<[Value]> = vec![Value::False].into();
        let err = state(names(&[]), vec![chunk], None)
            .is_loaded("m", "x")
            .unwrap_err();
        assert_eq!(
            err.to_string(),
            "context['install_loaded_names'] is False; it must be a list of names \
             (erp_data::load::load_state)"
        );
    }

    /// One warning line per view for its unknown groups, each reference once with its first
    /// line and count, and control characters escaped in references and labels (review S-9).
    #[test]
    fn unknown_groups_are_one_escaped_line_per_view() {
        use erp_sdk::views::NodeOrigin;
        let unknown = |view: i64, line: u32, xmlid: &str| ArchWarning::UnknownGroup {
            xmlid: xmlid.to_owned(),
            origin: NodeOrigin::new(view, std::num::NonZeroU32::new(line).unwrap()),
        };
        let sub = |view: i64, line: u32| ArchWarning::SubViewNotValidated {
            tag: "list".to_owned(),
            field: "line_ids".to_owned(),
            origin: NodeOrigin::new(view, std::num::NonZeroU32::new(line).unwrap()),
        };
        let mut warnings = vec![
            unknown(2, 3, "m.a"),
            unknown(1, 4, "m.fake\nWARN forged line"),
            sub(1, 9),
        ];
        // many elements naming the same groups
        for line in 5..1_005 {
            warnings.push(unknown(2, line, "m.b"));
            warnings.push(unknown(2, line, "m.a"));
        }
        let labels: HashMap<i64, ViewLabel> = [
            (1, ViewLabel::new(1, "one\u{1b}[31m")),
            (2, ViewLabel::new(2, "two").xmlid(Some("m.two".into()))),
        ]
        .into_iter()
        .collect();
        let lines = warning_lines(&warnings, &labels).unwrap();
        assert_eq!(
            lines,
            vec![
                (
                    NoticeKind::UnknownGroup,
                    "two (m.two): 2001 reference(s) to unknown groups in groups attributes, which \
                     never match: 'm.a' (arch line 3, 1001 times), 'm.b' (arch line 5, 1000 times)"
                        .to_owned()
                ),
                (
                    NoticeKind::UnknownGroup,
                    "one\\u{1b}[31m: 1 reference(s) to unknown groups in groups attributes, which \
                     never match: 'm.fake\\nWARN forged line' (arch line 4)"
                        .to_owned()
                ),
                (
                    NoticeKind::SubViewNotValidated,
                    format!(
                        "1 x2many sub-view(s) are not validated until {FULL_VALIDATION_TASK}: \
                         one\\u{{1b}}[31m, arch lines 9"
                    )
                ),
            ]
        );
        assert!(lines.iter().all(|(_, l)| !l.chars().any(char::is_control)));
        // a warning of a view without a label is a broken contract
        let err = warning_lines(&[unknown(3, 1, "m.a")], &labels).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::Programming);
        assert_eq!(err.to_string(), "no label was built for ir.ui.view 3");
    }
}
