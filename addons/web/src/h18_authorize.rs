//! Host-only authorization and pruning of independently visited builtin description references.

use erp_sdk::prelude::*;
use erp_sdk::views::{
    DescriptionBudget, DescriptionPayload, DescriptionReservation, FieldPredicate, FieldReference,
    ViewTypeDecl, wire,
};
use erp_sdk::{ErrorKind, FieldType};
use std::mem::size_of;

#[path = "h18_panel_authorize.rs"]
mod sections;

fn failure(message: impl Into<String>) -> Error {
    Error::validation(message)
}
fn view_error(error: erp_sdk::views::ViewError) -> Error {
    failure(error.to_string())
}

/// Root catalogue names with the request reservation that owns their retained storage.
#[derive(Debug)]
pub(crate) struct AuthorizedFields {
    names: Vec<String>,
    _storage: DescriptionReservation,
}
impl AuthorizedFields {
    pub(crate) fn into_parts(self) -> (Vec<String>, DescriptionReservation) {
        (self.names, self._storage)
    }
    fn text(&mut self, text: &str) -> Result<String> {
        self._storage.grow(text.len()).map_err(view_error)?;
        let mut value = String::new();
        value
            .try_reserve_exact(text.len())
            .map_err(|_| failure("Authorized text allocation failed."))?;
        if value.capacity() > text.len() {
            self._storage
                .grow(value.capacity() - text.len())
                .map_err(view_error)?;
        }
        value.push_str(text);
        Ok(value)
    }
    fn insert(&mut self, name: &str) -> Result<()> {
        if let Err(index) = self
            .names
            .binary_search_by(|value| value.as_str().cmp(name))
        {
            if self.names.len() == self.names.capacity() {
                return Err(failure(
                    "Authorized field catalogue exceeds its admitted root registry fields.",
                ));
            }
            self.names.insert(index, name.to_owned());
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Decision {
    Keep,
    Prune,
}
fn decision(node: &wire::ViewNode, allowed: bool) -> Result<Decision> {
    if allowed {
        return Ok(Decision::Keep);
    }
    match node {
        wire::ViewNode::Dashboard { .. }
        | wire::ViewNode::Field { .. }
        | wire::ViewNode::SearchField { .. }
        | wire::ViewNode::PivotField { .. }
        | wire::ViewNode::KanbanProgressbar { .. }
        | wire::ViewNode::CalendarField { .. }
        | wire::ViewNode::GraphField { .. }
        | wire::ViewNode::SearchFilter { .. } => Ok(Decision::Prune),
        _ => Err(Error::access(
            "A nonprunable builtin node references a field the caller cannot read.",
        )),
    }
}

struct Pruned {
    mapping: Vec<usize>,
    _storage: DescriptionReservation,
}
fn children(node: &wire::ViewNode) -> Result<&[u32]> {
    use wire::ViewNode::*;
    match node {
        Form { children, .. }
        | List { children, .. }
        | Search { children, .. }
        | Pivot { children, .. }
        | Kanban { children, .. }
        | KanbanCard { children }
        | KanbanSection { children, .. }
        | Calendar { children, .. }
        | Graph { children, .. }
        | Sheet { children }
        | Aside { children }
        | Title { children }
        | Heading { children, .. }
        | Header { children }
        | ButtonBox { children }
        | StatButton { children, .. }
        | Footer { children }
        | Group { children, .. }
        | Notebook { children }
        | Page { children, .. }
        | SearchGroup { children, .. } => Ok(children),
        Dashboard { .. }
        | Field { .. }
        | Button { .. }
        | CancelButton { .. }
        | ActionButton { .. }
        | Ribbon { .. }
        | NotificationAlert {}
        | Chatter { .. }
        | Widget { .. }
        | Separator { .. }
        | SearchField { .. }
        | PivotField { .. }
        | KanbanProgressbar { .. }
        | CalendarField { .. }
        | GraphField { .. }
        | SearchFilter { .. }
        | SearchSeparator {} => Ok(&[]),
        _ => Err(failure(
            "Unknown builtin node kind during host authorization.",
        )),
    }
}
fn children_mut(node: &mut wire::ViewNode) -> Result<Option<&mut Vec<u32>>> {
    use wire::ViewNode::*;
    match node {
        Form { children, .. }
        | List { children, .. }
        | Search { children, .. }
        | Pivot { children, .. }
        | Kanban { children, .. }
        | KanbanCard { children }
        | KanbanSection { children, .. }
        | Calendar { children, .. }
        | Graph { children, .. }
        | Sheet { children }
        | Aside { children }
        | Title { children }
        | Heading { children, .. }
        | Header { children }
        | ButtonBox { children }
        | StatButton { children, .. }
        | Footer { children }
        | Group { children, .. }
        | Notebook { children }
        | Page { children, .. }
        | SearchGroup { children, .. } => Ok(Some(children)),
        Dashboard { .. }
        | Field { .. }
        | Button { .. }
        | CancelButton { .. }
        | ActionButton { .. }
        | Ribbon { .. }
        | NotificationAlert {}
        | Chatter { .. }
        | Widget { .. }
        | Separator { .. }
        | SearchField { .. }
        | PivotField { .. }
        | KanbanProgressbar { .. }
        | CalendarField { .. }
        | GraphField { .. }
        | SearchFilter { .. }
        | SearchSeparator {} => Ok(None),
        _ => Err(failure(
            "Unknown builtin node kind during host authorization.",
        )),
    }
}
fn prune(
    view: &mut wire::View,
    decisions: &[Decision],
    budget: &DescriptionBudget,
) -> Result<Pruned> {
    let n = view.nodes.len();
    if n > 4096 || n != decisions.len() {
        return Err(failure(
            "Builtin authorization decisions do not match the bounded arena.",
        ));
    }
    let bytes = n
        .checked_mul(size_of::<usize>() + size_of::<u8>())
        .ok_or_else(|| failure("Builtin pruning storage overflow."))?;
    let storage = budget.reserve(bytes).map_err(view_error)?;
    let mut states = Vec::new();
    states
        .try_reserve_exact(n)
        .map_err(|_| failure("Builtin pruning allocation failed."))?;
    states.resize(n, 0u8);
    let mut mapping = Vec::new();
    mapping
        .try_reserve_exact(n)
        .map_err(|_| failure("Builtin pruning allocation failed."))?;
    mapping.resize(n, usize::MAX);
    fn mark(
        index: usize,
        depth: usize,
        nodes: &[wire::ViewNode],
        decisions: &[Decision],
        states: &mut [u8],
    ) -> Result<bool> {
        if depth > 32 {
            return Err(failure(
                "Builtin authorization graph exceeds the 32-level limit.",
            ));
        }
        let node = nodes
            .get(index)
            .ok_or_else(|| failure("Builtin authorization child index is out of bounds."))?;
        if states[index] != 0 {
            return Err(failure(
                "Builtin authorization graph repeats a node or contains a cycle.",
            ));
        }
        if decisions[index] == Decision::Prune {
            states[index] = 3;
            return Ok(false);
        }
        states[index] = 1;
        let mut visible = 0;
        for child in children(node)? {
            if mark(*child as usize, depth + 1, nodes, decisions, states)? {
                visible += 1;
            }
        }
        if matches!(node, wire::ViewNode::StatButton { .. }) && visible == 0 {
            return Err(failure(
                "A smart button requires at least one readable statistic field. Unsupported form layout requires a later release.",
            ));
        }
        if matches!(node, wire::ViewNode::SearchGroup { .. }) && visible == 0 {
            states[index] = 3;
            return Ok(false);
        }
        states[index] = 2;
        Ok(true)
    }
    let root = view.root as usize;
    if !mark(root, 1, &view.nodes, decisions, &mut states)? {
        return Err(failure("Builtin view root cannot be pruned."));
    }
    let mut next = 0;
    for (index, state) in states.iter().enumerate() {
        if *state == 2 {
            mapping[index] = next;
            next += 1;
        }
    }
    // a pruned measure must not stay named by the table's default order
    let denied: Vec<&str> = view
        .nodes
        .iter()
        .zip(decisions)
        .filter_map(|(node, decision)| match node {
            wire::ViewNode::PivotField {
                field,
                role: wire::PivotRole::Measure,
                ..
            } if *decision == Decision::Prune => Some(field.name.as_str()),
            _ => None,
        })
        .collect();
    let denied: Vec<String> = denied.into_iter().map(str::to_owned).collect();
    for node in &mut view.nodes {
        if let wire::ViewNode::Pivot { default_order, .. } = node
            && default_order
                .as_deref()
                .and_then(|order| order.split_whitespace().next())
                .is_some_and(|measure| denied.iter().any(|name| name == measure))
        {
            *default_order = None;
        }
    }
    let mut old = 0;
    view.nodes.retain(|_| {
        let keep = states[old] == 2;
        old += 1;
        keep
    });
    for node in &mut view.nodes {
        if let Some(edges) = children_mut(node)? {
            edges.retain(|index| mapping[*index as usize] != usize::MAX);
            for index in edges {
                *index = u32::try_from(mapping[*index as usize])
                    .map_err(|_| failure("Builtin arena remapping overflow."))?;
            }
        }
    }
    view.root =
        u32::try_from(mapping[root]).map_err(|_| failure("Builtin root remapping overflow."))?;
    Ok(Pruned {
        mapping,
        _storage: storage,
    })
}

/// Drop the optional root member that names a field the caller cannot read: the search panel, or a
/// kanban's default grouping (the board opens ungrouped). False when the root has
/// no such member, so the view is refused.
fn relieve_root(node: &mut wire::ViewNode) -> bool {
    match node {
        wire::ViewNode::Search { search_panel, .. } | wire::ViewNode::List { search_panel, .. }
            if search_panel.is_some() =>
        {
            *search_panel = None;
            true
        }
        wire::ViewNode::Kanban {
            default_group_by, ..
        } if default_group_by.is_some() => {
            *default_group_by = None;
            true
        }
        _ => false,
    }
}

fn require_builtin_root_field(model: &str, kind: &str, field: &wire::FieldRef) -> Result<()> {
    if field.model != model {
        return Err(failure(format!(
            "Builtin {kind} reference '{}.{}' does not match root model '{model}'. Cross-model builtin fields require a later release.",
            field.model, field.name
        )));
    }
    // The client catalogue resolves builtin handles as direct field names; relation paths stay
    // legal only in expression and generic core reference slots (HOST-D01).
    if field.name.contains('.') {
        return Err(failure(format!(
            "Builtin {kind} reference '{}.{}' must name a direct field of root model '{model}'. Relation paths in builtin field handles require a later release.",
            field.model, field.name
        )));
    }
    Ok(())
}
fn validate_builtin_field_models(model: &str, node: &wire::ViewNode) -> Result<()> {
    match node {
        wire::ViewNode::Field { field, .. } => require_builtin_root_field(model, "field", field),
        wire::ViewNode::SearchField { field, .. } => {
            require_builtin_root_field(model, "search_field", field)
        }
        wire::ViewNode::PivotField { field, .. } => {
            require_builtin_root_field(model, "pivot_field", field)
        }
        wire::ViewNode::Kanban {
            default_group_by: Some(field),
            ..
        } => require_builtin_root_field(model, "kanban", field),
        wire::ViewNode::KanbanProgressbar {
            field, sum_field, ..
        } => {
            require_builtin_root_field(model, "kanban_progressbar", field)?;
            match sum_field {
                Some(sum) => require_builtin_root_field(model, "kanban_progressbar", sum),
                None => Ok(()),
            }
        }
        wire::ViewNode::CalendarField { field, .. } => {
            require_builtin_root_field(model, "calendar_field", field)
        }
        wire::ViewNode::Calendar {
            date_start,
            date_stop,
            all_day,
            color,
            ..
        } => [
            Some(date_start),
            date_stop.as_ref(),
            all_day.as_ref(),
            color.as_ref(),
        ]
        .into_iter()
        .flatten()
        .try_for_each(|field| require_builtin_root_field(model, "calendar", field)),
        wire::ViewNode::GraphField { field, .. } => {
            require_builtin_root_field(model, "graph_field", field)
        }
        wire::ViewNode::Dashboard { items, .. } => {
            for item in items {
                if let wire::DashboardItem::Kpi {
                    field, date_field, ..
                } = item
                {
                    require_builtin_root_field(model, "dashboard", field)?;
                    require_builtin_root_field(model, "dashboard", date_field)?;
                }
            }
            Ok(())
        }
        wire::ViewNode::List {
            search_panel: Some(wire::ListSearchPanel::Selection { field, .. }),
            ..
        }
        | wire::ViewNode::Search {
            search_panel: Some(wire::ListSearchPanel::Selection { field, .. }),
            ..
        } => require_builtin_root_field(model, "search_panel", field),
        _ => Ok(()),
    }
}

/// Literal relation criteria have a comodel LHS and root-record RHS. The generic value
/// visitor handles RHS names; this independent walk prevents root/comodel scope confusion.
fn relation_domain_paths(document: &erp_sdk::expr::wire::WireDocument) -> Result<Vec<&str>> {
    use erp_sdk::expr::wire::{WireLiteral, WireNode};
    fn text(document: &erp_sdk::expr::wire::WireDocument, index: u32) -> Option<&str> {
        match document.nodes.get(index as usize)? {
            WireNode::Literal {
                value: WireLiteral::Str { value },
            } => Some(value),
            _ => None,
        }
    }
    fn visit<'a>(
        document: &'a erp_sdk::expr::wire::WireDocument,
        index: usize,
        depth: usize,
        paths: &mut Vec<&'a str>,
    ) -> Result<()> {
        if depth > 100 {
            return Err(failure("Relation domain exceeds its logical depth."));
        }
        let Some(WireNode::List { items } | WireNode::Tuple { items }) = document.nodes.get(index)
        else {
            return Err(failure(
                "Relation domains require literal criterion structure. Dynamic domain structure requires a later release.",
            ));
        };
        if let [lhs, operator, _rhs] = items.as_slice()
            && let Some(operator) = text(document, *operator)
            && !matches!(operator, "&" | "|" | "!")
        {
            let path = text(document, *lhs)
                .ok_or_else(|| failure("Relation domain criterion fields must be literal text."))?;
            if !matches!(
                operator,
                "=" | "!="
                    | ">"
                    | "<"
                    | ">="
                    | "<="
                    | "=?"
                    | "in"
                    | "not in"
                    | "like"
                    | "not like"
                    | "ilike"
                    | "not ilike"
                    | "=like"
                    | "=ilike"
                    | "child_of"
                    | "parent_of"
            ) {
                return Err(failure(format!(
                    "Unsupported relation domain operator '{operator}'. Additional operators require a later release."
                )));
            }
            paths.push(path);
            return Ok(());
        }
        for item in items {
            if text(document, *item).is_some_and(|value| matches!(value, "&" | "|" | "!")) {
                continue;
            }
            visit(document, *item as usize, depth + 1, paths)?;
        }
        Ok(())
    }
    // The schema value-expression visitor has already validated the complete wire graph.
    let mut paths = Vec::new();
    paths
        .try_reserve_exact(document.nodes.len())
        .map_err(|_| failure("Relation domain reference allocation failed."))?;
    visit(
        document,
        document
            .nodes
            .len()
            .checked_sub(1)
            .ok_or_else(|| failure("Relation domain has no root."))?,
        1,
        &mut paths,
    )?;
    Ok(paths)
}

/// Relation context keys are literal; comodel grouping scopes are not yet admitted.
fn relation_context_shape(document: &erp_sdk::expr::wire::WireDocument) -> Result<()> {
    use erp_sdk::expr::wire::{WireLiteral, WireNode};
    let Some(WireNode::Dict { items }) = document.nodes.last() else {
        return Err(failure(
            "Relation contexts require literal dictionary structure. Dynamic context structure requires a later release.",
        ));
    };
    for (key, _) in items {
        let Some(WireNode::Literal {
            value: WireLiteral::Str { value },
        }) = document.nodes.get(*key as usize)
        else {
            return Err(failure("Relation context keys must be literal text."));
        };
        if value == "group_by" {
            return Err(failure(
                "Relation group_by context requires a later release.",
            ));
        }
    }
    Ok(())
}

fn reference<'a>(field: FieldReference<'a>, root: &'a str) -> Result<(&'a str, &'a str)> {
    match field {
        FieldReference::RootPath(path) => Ok((root, path)),
        FieldReference::ModelPath { model, path } => Ok((model, path)),
        _ => Err(failure(
            "Unknown scoped field reference during host authorization.",
        )),
    }
}
fn validate_path<'a>(env: &'a Env, model: &'a str, path: &str) -> Result<()> {
    let mut current = model;
    let mut segments = path.split('.').peekable();
    while let Some(segment) = segments.next() {
        let spec = env
            .registry()
            .model(current)
            .ok_or_else(|| failure(format!("Description references unknown model '{current}'.")))?;
        let field = spec.field(segment).ok_or_else(|| {
            failure(format!(
                "Description references unknown field '{current}.{segment}'."
            ))
        })?;
        if segments.peek().is_some() {
            current = match &field.ty {
                FieldType::Many2one { comodel, .. }
                | FieldType::One2many { comodel, .. }
                | FieldType::Many2many { comodel, .. } => comodel,
                _ => {
                    return Err(failure(format!(
                        "Description non-relational field '{current}.{segment}' cannot be traversed."
                    )));
                }
            };
        }
    }
    if path.is_empty() {
        return Err(failure("Description references an empty field path."));
    }
    Ok(())
}
async fn path_allowed(env: &Env, tx: &mut Transaction, model: &str, path: &str) -> Result<bool> {
    validate_path(env, model, path)?;
    let mut current = model;
    let mut segments = path.split('.').peekable();
    let mut allowed = true;
    while let Some(segment) = segments.next() {
        let spec = env
            .registry()
            .model(current)
            .ok_or_else(|| Error::invariant("validated description model disappeared"))?;
        let field = spec
            .field(segment)
            .ok_or_else(|| Error::invariant("validated description field disappeared"))?;
        if let Some(groups) = field.groups
            && !tx.has_groups(env, groups).await?
        {
            allowed = false;
        }
        // Every segment uses caller-bound ORM model+field read checks, including comodels.
        match env.records(current)?.read(tx, &[field.name]).await {
            Ok(_) => {}
            Err(error) if error.kind() == ErrorKind::Access => allowed = false,
            Err(error) => return Err(error),
        }
        if segments.peek().is_some() {
            current = match &field.ty {
                FieldType::Many2one { comodel, .. }
                | FieldType::One2many { comodel, .. }
                | FieldType::Many2many { comodel, .. } => comodel,
                _ => {
                    return Err(Error::invariant(
                        "validated description relation disappeared",
                    ));
                }
            };
        }
    }
    Ok(allowed)
}
fn root_name<'a>(model: &str, scope: &str, path: &'a str) -> Option<&'a str> {
    if model == scope {
        path.split('.').next()
    } else {
        None
    }
}
fn predicate_matches(predicate: FieldPredicate, field: &erp_sdk::FieldSpec) -> Result<bool> {
    let column = field.has_column() || field.is_generated();
    Ok(match predicate {
        FieldPredicate::AllReadable => true,
        FieldPredicate::Numeric => matches!(
            field.ty,
            FieldType::Integer | FieldType::Float { .. } | FieldType::Monetary { .. }
        ),
        FieldPredicate::Groupable => {
            column
                && !matches!(
                    field.ty,
                    FieldType::Json
                        | FieldType::Binary { .. }
                        | FieldType::DateRange { .. }
                        | FieldType::DatetimeRange { .. }
                )
        }
        _ => {
            return Err(failure(
                "Unknown metadata field predicate during host authorization.",
            ));
        }
    })
}

/// Host-only context for independently derived builtin references and metadata declarations.
/// The caller first admits the wire view through the exact registered builtin schema. Its
/// closed `root`/`nodes` shape guarantees that node walks cover all whole-payload references.
/// Neither this context nor its descriptive payload grants access; operations use caller-bound
/// ORM checks in the transaction passed to each operation.
pub(crate) struct Request<'a> {
    payload: &'a DescriptionPayload,
    declaration: &'a ViewTypeDecl,
    model: &'a str,
    env: &'a Env,
    budget: &'a DescriptionBudget,
}
impl<'a> Request<'a> {
    pub(crate) fn new(
        payload: &'a DescriptionPayload,
        declaration: &'a ViewTypeDecl,
        model: &'a str,
        env: &'a Env,
        budget: &'a DescriptionBudget,
    ) -> Self {
        Self {
            payload,
            declaration,
            model,
            env,
            budget,
        }
    }
    /// Authorize nodes plus the declaration's explicit and predicate metadata fields.
    pub(crate) async fn authorize(
        &self,
        view: &mut wire::View,
        tx: &mut Transaction,
    ) -> Result<AuthorizedFields> {
        self.execute(view, tx, true).await
    }
    /// Authorize only emitted panel references, excluding custom-filter metadata expansion.
    pub(crate) async fn panel(
        &self,
        view: &mut wire::View,
        tx: &mut Transaction,
    ) -> Result<AuthorizedFields> {
        self.execute(view, tx, false).await
    }
    async fn execute(
        &self,
        view: &mut wire::View,
        tx: &mut Transaction,
        metadata: bool,
    ) -> Result<AuthorizedFields> {
        let payload = self.payload;
        let decl = self.declaration;
        let model = self.model;
        let env = self.env;
        let budget = self.budget;
        let spec = env
            .registry()
            .model(model)
            .ok_or_else(|| failure(format!("Description references unknown model '{model}'.")))?;
        let names_bytes = spec
            .fields
            .iter()
            .try_fold(0usize, |sum, field| {
                sum.checked_add(field.name.len())
                    .and_then(|sum| sum.checked_add(size_of::<String>()))
            })
            .ok_or_else(|| failure("Authorized catalogue storage overflow."))?;
        let storage = budget.reserve(names_bytes).map_err(view_error)?;
        let mut names = Vec::new();
        names
            .try_reserve_exact(spec.fields.len())
            .map_err(|_| failure("Authorized catalogue allocation failed."))?;
        let mut fields = AuthorizedFields {
            names,
            _storage: storage,
        };
        for node in &view.nodes {
            if let wire::ViewNode::Field {
                options: wire::FieldOptions::Presentation(options),
                ..
            } = node
                && let Some(context) = &options.context
            {
                relation_context_shape(context)?;
            }
        }
        for (index, node) in view.nodes.iter().enumerate() {
            if index != view.root as usize
                && matches!(
                    node,
                    wire::ViewNode::Search {
                        search_panel: Some(wire::ListSearchPanel::Sections { .. }),
                        ..
                    } | wire::ViewNode::List {
                        search_panel: Some(wire::ListSearchPanel::Sections { .. }),
                        ..
                    }
                )
            {
                return Err(failure(
                    "Search panel section 0 has an invalid panel placement.",
                ));
            }
        }
        let panel_root = view.root as usize;
        let extended_panel = matches!(
            view.nodes.get(panel_root),
            Some(
                wire::ViewNode::Search {
                    search_panel: Some(wire::ListSearchPanel::Sections { .. }),
                    ..
                } | wire::ViewNode::List {
                    search_panel: Some(wire::ListSearchPanel::Sections { .. }),
                    ..
                }
            )
        );
        if extended_panel {
            let node = view
                .nodes
                .get_mut(panel_root)
                .ok_or_else(|| failure("Search panel root is outside its arena."))?;
            sections::authorize(self, node, tx, &mut fields, !metadata).await?;
        }
        // This independent whole-payload walk validates all slots. Denied paths are handled per node,
        // so a prunable search filter cannot turn into a blanket response denial.
        let all = decl
            .visitor()
            .payload_references(payload, budget)
            .map_err(view_error)?;
        for field in all.fields() {
            let (scope, path) = reference(field, model)?;
            validate_path(env, scope, path)?;
        }
        drop(all);
        let decision_bytes = view
            .nodes
            .len()
            .checked_mul(size_of::<Decision>())
            .ok_or_else(|| failure("Builtin decisions storage overflow."))?;
        let _decision_storage = budget.reserve(decision_bytes).map_err(view_error)?;
        let mut decisions = Vec::new();
        decisions
            .try_reserve_exact(view.nodes.len())
            .map_err(|_| failure("Builtin decision allocation failed."))?;
        for (index, node) in view.nodes.iter_mut().enumerate() {
            // The exact builtin List/Search root schemas have references only in search_panel.
            // Extended sections above receive independent complete admission and whole-section pruning.
            if extended_panel && index == panel_root {
                decisions.push(Decision::Keep);
                continue;
            }
            // Builtin client field handles are root-scoped. Core schema visitors continue to
            // support explicit foreign model scopes for other/custom description contracts.
            validate_builtin_field_models(model, node)?;
            // A custom callback can bypass builtin lowering. Validate the actual output slot,
            // independently of its schema's bounded-text shape and the callback's validator.
            if let wire::ViewNode::SearchField {
                field,
                operator: Some(operator),
                ..
            } = node
            {
                crate::h18_lower::validate_search_operator(&field.model, &field.name, operator)
                    .map_err(view_error)?;
            }
            let refs = decl
                .visitor()
                .payload_node_references(payload, index, budget)
                .map_err(view_error)?;
            let mut allowed = true;
            for field in refs.fields() {
                let (scope, path) = reference(field, model)?;
                if !path_allowed(env, tx, scope, path).await? {
                    allowed = false;
                }
            }
            if let wire::ViewNode::Field {
                field,
                options: wire::FieldOptions::Presentation(options),
                ..
            } = node
                && let Some(domain) = &options.domain
            {
                let relation = spec
                    .fields
                    .iter()
                    .find(|candidate| candidate.name == field.name)
                    .and_then(|candidate| match &candidate.ty {
                        FieldType::Many2one { comodel, .. }
                        | FieldType::Many2many { comodel, .. } => Some(comodel.as_str()),
                        _ => None,
                    })
                    .ok_or_else(|| failure("Relation domain requires a relational field."))?;
                let bytes = domain
                    .nodes
                    .len()
                    .checked_mul(size_of::<&str>())
                    .ok_or_else(|| failure("Relation domain reference storage overflow."))?;
                let _storage = budget.reserve(bytes).map_err(view_error)?;
                for path in relation_domain_paths(domain)? {
                    validate_path(env, relation, path)?;
                    if !path_allowed(env, tx, relation, path).await? {
                        allowed = false;
                    }
                }
            }
            if !allowed && index == view.root as usize {
                if !relieve_root(node) {
                    return Err(Error::access(
                        "The builtin view root references a field the caller cannot read.",
                    ));
                }
                allowed = true;
            }
            if allowed
                && let wire::ViewNode::Button { name, details, .. }
                | wire::ViewNode::StatButton { name, details, .. } = node
            {
                let exposure = env
                    .registry()
                    .model_id(model)
                    .and_then(|id| tx.exposure().get(id, name));
                let Some(erp_sdk::rpc::Exposure::Exposed(exposed)) = exposure else {
                    return Err(failure(format!(
                        "Button '{name}' names a method that '{model}' does not expose over RPC. Unsupported form layout requires a later release."
                    )));
                };
                if let Some(prompt) = details.as_ref().and_then(|details| details.prompt.as_ref()) {
                    prompt_parameter(model, name, exposed.params(), prompt.required)
                        .map_err(failure)?;
                }
            }
            decisions.push(decision(node, allowed)?);
        }
        let pruned = prune(view, &decisions, budget)?;
        if decl.metadata_fields().includes_arch_references() || !metadata {
            for (index, mapped) in pruned.mapping.iter().enumerate() {
                if *mapped == usize::MAX || (extended_panel && index == panel_root) {
                    continue;
                }
                let refs = decl
                    .visitor()
                    .payload_node_references(payload, index, budget)
                    .map_err(view_error)?;
                for field in refs.fields() {
                    let (scope, path) = reference(field, model)?;
                    if path_allowed(env, tx, scope, path).await?
                        && let Some(name) = root_name(model, scope, path)
                    {
                        fields.insert(name)?;
                    }
                }
            }
        }
        if metadata {
            for field in decl.metadata_fields().explicit() {
                let (scope, path) = reference(field, model)?;
                if path_allowed(env, tx, scope, path).await?
                    && let Some(name) = root_name(model, scope, path)
                {
                    fields.insert(name)?;
                }
            }
            for predicate in decl.metadata_fields().predicates() {
                for field in &spec.fields {
                    // Predicate-added catalogue entries are optional extras for custom filters: a
                    // field type the human client has no renderer for (a dynamic selection, binary
                    // data) is left out here. An unsupported field that a view references itself
                    // still fails loudly through the explicit reference path.
                    if predicate_matches(*predicate, field)?
                        && !matches!(field.ty, FieldType::Binary { .. })
                        && crate::metadata::field_kind(model, field).is_ok()
                        && path_allowed(env, tx, model, field.name).await?
                    {
                        fields.insert(field.name)?;
                    }
                }
            }
        }
        Ok(fields)
    }
}

/// The answer of a prompting button reaches its method as the `prompt_value` keyword: the method must take it as
/// text, and as optional text (`Option<String>`) when an empty answer is allowed, since that one is sent as `null`.
/// A parameter without a declared shape (a WebAssembly method) is left to the binder.
fn prompt_parameter(
    model: &str,
    method: &str,
    params: &[erp_sdk::rpc::ParamSpec],
    required: bool,
) -> Result<(), String> {
    use erp_sdk::rpc::ArgShape;
    let Some(param) = params.iter().find(|param| param.name() == "prompt_value") else {
        return Err(format!(
            "Button '{method}' asks for a value, but '{model}.{method}' takes no 'prompt_value' parameter."
        ));
    };
    let fits = match param.shape() {
        ArgShape::Open(_) => true,
        ArgShape::OrNull(ArgShape::String) => true,
        ArgShape::String => required,
        _ => false,
    };
    if !fits {
        return Err(format!(
            "Button '{method}' sends its answer as text{}, but '{model}.{method}' takes 'prompt_value' as {}.",
            if required { "" } else { " or null" },
            param.expected()
        ));
    }
    Ok(())
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "fixture assertions"
)]
mod tests {
    use erp_sdk::views::{DescriptionBudget, wire};

    /// A prompting button's method takes `prompt_value` as text, or as optional text when the answer may be empty.
    #[test]
    fn prompt_parameter_matches_the_answer() {
        use erp_sdk::rpc::{ArgShape, ParamSpec};
        static TEXT: ArgShape = ArgShape::String;
        let text = [ParamSpec::with_shape(
            "prompt_value",
            true,
            "a string",
            ArgShape::String,
        )];
        let optional = [ParamSpec::with_shape(
            "prompt_value",
            false,
            "a string or null",
            ArgShape::OrNull(&TEXT),
        )];
        let number = [ParamSpec::with_shape(
            "prompt_value",
            true,
            "an integer",
            ArgShape::Integer { min: None },
        )];
        let open = [ParamSpec::new("prompt_value", true, "a value")];
        let check = |params: &[ParamSpec], required| {
            super::prompt_parameter("m", "action_refuse", params, required)
        };
        assert_eq!(check(&text, true), Ok(()));
        assert_eq!(check(&optional, true), Ok(()));
        assert_eq!(check(&optional, false), Ok(()));
        assert_eq!(check(&open, false), Ok(()));
        assert_eq!(
            check(&text, false),
            Err("Button 'action_refuse' sends its answer as text or null, but 'm.action_refuse' takes 'prompt_value' as a string.".to_owned())
        );
        assert_eq!(
            check(&number, true),
            Err("Button 'action_refuse' sends its answer as text, but 'm.action_refuse' takes 'prompt_value' as an integer.".to_owned())
        );
        assert_eq!(
            check(&[], true),
            Err("Button 'action_refuse' asks for a value, but 'm.action_refuse' takes no 'prompt_value' parameter.".to_owned())
        );
    }

    #[test]
    fn chatter_is_a_childless_leaf_for_host_authorization() {
        let mut node = wire::ViewNode::Chatter {
            reload_on_post: false,
            reload_on_attachment: false,
            reload_on_activity: false,
            reload_on_follower: false,
            open_attachments: false,
            has_activities: false,
        };
        assert!(super::children(&node).unwrap().is_empty());
        assert!(super::children_mut(&mut node).unwrap().is_none());
    }

    #[test]
    fn relation_domain_criterion_paths_do_not_capture_rhs_or_nested_values() {
        let expression = erp_sdk::expr::parse(
            "['|', ('currency_id', '!=', id), ('name', 'in', ['name', '=', 'data'])]",
        )
        .unwrap();
        let document = erp_sdk::expr::wire::WireDocument::from_expr(&expression).unwrap();
        assert_eq!(
            super::relation_domain_paths(&document).unwrap(),
            ["currency_id", "name"]
        );
        let expression = erp_sdk::expr::parse("name").unwrap();
        let document = erp_sdk::expr::wire::WireDocument::from_expr(&expression).unwrap();
        let error = super::relation_domain_paths(&document).unwrap_err();
        assert_eq!(error.kind(), erp_sdk::ErrorKind::Validation);
        assert_eq!(
            error.message(),
            "Relation domains require literal criterion structure. Dynamic domain structure requires a later release."
        );
    }

    #[test]
    fn relation_expression_structure_refusals_are_explicit() {
        for (source, message) in [
            (
                "[(name, '=', 1)]",
                "Relation domain criterion fields must be literal text.",
            ),
            (
                "[('company_id', 'any', [('name', '=', 'x')])]",
                "Unsupported relation domain operator 'any'. Additional operators require a later release.",
            ),
            (
                "[('name', 'unknown', 1)]",
                "Unsupported relation domain operator 'unknown'. Additional operators require a later release.",
            ),
        ] {
            let expression = erp_sdk::expr::parse(source).unwrap();
            let document = erp_sdk::expr::wire::WireDocument::from_expr(&expression).unwrap();
            let error = super::relation_domain_paths(&document).unwrap_err();
            assert_eq!(error.kind(), erp_sdk::ErrorKind::Validation);
            assert_eq!(error.message(), message);
        }
        let expression = erp_sdk::expr::parse("{'group_by':'currency_id'}").unwrap();
        let document = erp_sdk::expr::wire::WireDocument::from_expr(&expression).unwrap();
        let error = super::relation_context_shape(&document).unwrap_err();
        assert_eq!(error.kind(), erp_sdk::ErrorKind::Validation);
        assert_eq!(
            error.message(),
            "Relation group_by context requires a later release."
        );
    }

    #[test]
    fn h18_builtin_foreign_field_and_panel_handles_refuse_precisely() {
        let field = wire::FieldRef::new("res.company".to_owned(), "name".to_owned());
        let nodes = [
            (
                "field",
                wire::ViewNode::Field {
                    field: field.clone(),
                    widget: "char".to_owned(),
                    options: wire::FieldOptions::Empty(wire::EmptyOptions::new()),
                    modifiers: wire::Modifiers::empty(),
                    label: "Name".to_owned(),
                    force_save: false,
                    width: None,
                },
            ),
            (
                "search_field",
                wire::ViewNode::SearchField {
                    field: field.clone(),
                    label: "Name".to_owned(),
                    filter_domain: None,
                    operator: None,
                },
            ),
            (
                "search_panel",
                wire::ViewNode::Search {
                    children: vec![],
                    search_panel: Some(wire::ListSearchPanel::Selection {
                        field,
                        label: "Name".to_owned(),
                        enable_counters: false,
                        expand: false,
                        default_value: None,
                        notice: None,
                    }),
                },
            ),
        ];
        for (kind, node) in nodes {
            let error =
                super::validate_builtin_field_models("acceptance_notes.note", &node).unwrap_err();
            assert_eq!(error.kind(), erp_sdk::ErrorKind::Validation);
            assert_eq!(
                error.message(),
                format!(
                    "Builtin {kind} reference 'res.company.name' does not match root model 'acceptance_notes.note'. Cross-model builtin fields require a later release."
                )
            );
        }
    }

    #[test]
    fn h18_authorize_refuses_smart_button_after_last_statistic_is_pruned() {
        let mut view = wire::View::new(
            wire::ViewIdentity::generated(),
            wire::ViewType::Form,
            "web.form".to_owned(),
            0,
            vec![
                wire::ViewNode::Form {
                    record_mode: wire::RecordMode::Record,
                    children: vec![1],
                    create: None,
                    edit: None,
                    delete: None,
                },
                wire::ViewNode::StatButton {
                    name: "open".to_owned(),
                    button_type: wire::ButtonType::Object,
                    label: "Open".to_owned(),
                    variant: wire::ButtonVariant::Secondary,
                    confirm: None,
                    modifiers: wire::Modifiers::empty(),
                    children: vec![2],
                    details: None,
                },
                wire::ViewNode::Field {
                    field: wire::FieldRef::new("h18.fixture".to_owned(), "count".to_owned()),
                    widget: "integer".to_owned(),
                    options: wire::FieldOptions::Empty(wire::EmptyOptions::new()),
                    modifiers: wire::Modifiers::empty(),
                    label: "Count".to_owned(),
                    force_save: false,
                    width: None,
                },
            ],
        );
        let error = super::prune(
            &mut view,
            &[
                super::Decision::Keep,
                super::Decision::Keep,
                super::Decision::Prune,
            ],
            &DescriptionBudget::new(),
        )
        .err()
        .expect("empty statistic must fail");
        assert_eq!(error.kind(), erp_sdk::ErrorKind::Validation);
        assert_eq!(
            error.message(),
            "A smart button requires at least one readable statistic field. Unsupported form layout requires a later release."
        );
    }

    #[test]
    fn h18_authorize_denied_nonprunable_nodes_fail_with_access_kind_and_message() {
        let button = wire::ViewNode::Button {
            name: "confirm".to_owned(),
            button_type: wire::ButtonType::Object,
            label: "Confirm".to_owned(),
            variant: wire::ButtonVariant::Secondary,
            confirm: None,
            modifiers: wire::Modifiers::empty(),
            details: None,
        };
        for node in [
            button,
            wire::ViewNode::Group {
                label: "Layout".to_owned(),
                columns: 2,
                colspan: 2,
                children: vec![],
            },
        ] {
            let error = super::decision(&node, false).unwrap_err();
            assert_eq!(error.kind(), erp_sdk::ErrorKind::Access);
            assert_eq!(
                error.message(),
                "A nonprunable builtin node references a field the caller cannot read."
            );
        }
        let filter = wire::ViewNode::SearchFilter {
            name: "filter".to_owned(),
            label: "Filter".to_owned(),
            domain: None,
            context: None,
        };
        assert!(matches!(
            super::decision(&filter, false).unwrap(),
            super::Decision::Prune
        ));
    }

    #[test]
    fn h18_authorize_prunes_denied_facets_and_empty_groups_and_remaps_order() {
        let mut view = wire::View::new(
            wire::ViewIdentity::generated(),
            wire::ViewType::Search,
            "web.search".to_owned(),
            0,
            vec![
                wire::ViewNode::Search {
                    children: vec![1, 3],
                    search_panel: None,
                },
                wire::ViewNode::SearchGroup {
                    label: "Hidden".to_owned(),
                    expand: true,
                    children: vec![2],
                },
                wire::ViewNode::SearchFilter {
                    name: "denied".to_owned(),
                    label: "Denied".to_owned(),
                    domain: None,
                    context: None,
                },
                wire::ViewNode::SearchGroup {
                    label: "Visible".to_owned(),
                    expand: true,
                    children: vec![4],
                },
                wire::ViewNode::SearchFilter {
                    name: "kept".to_owned(),
                    label: "Kept".to_owned(),
                    domain: None,
                    context: None,
                },
            ],
        );
        let budget = DescriptionBudget::new();
        super::prune(
            &mut view,
            &[
                super::Decision::Keep,
                super::Decision::Keep,
                super::Decision::Prune,
                super::Decision::Keep,
                super::Decision::Keep,
            ],
            &budget,
        )
        .unwrap();
        assert_eq!(view.nodes.len(), 3);
        match &view.nodes[0] {
            wire::ViewNode::Search { children, .. } => assert_eq!(children, &[1]),
            _ => panic!("root"),
        }
        match &view.nodes[1] {
            wire::ViewNode::SearchGroup {
                children, expand, ..
            } => {
                assert_eq!(children, &[2]);
                assert!(*expand)
            }
            _ => panic!("group"),
        }
        match &view.nodes[2] {
            wire::ViewNode::SearchFilter { name, .. } => assert_eq!(name, "kept"),
            _ => panic!("filter"),
        }
    }

    #[test]
    fn h18_authorize_prunes_denied_pivot_fields_and_keeps_the_table() {
        let node = |name: &str, role: wire::PivotRole, interval: Option<&str>| {
            wire::ViewNode::PivotField {
                field: wire::FieldRef::new("h18.fixture".to_owned(), name.to_owned()),
                label: name.to_owned(),
                role,
                interval: interval.map(str::to_owned),
            }
        };
        let measure = node("count", wire::PivotRole::Measure, None);
        assert!(matches!(
            super::decision(&measure, false).unwrap(),
            super::Decision::Prune
        ));
        assert!(matches!(
            super::decision(&measure, true).unwrap(),
            super::Decision::Keep
        ));
        let foreign = wire::ViewNode::PivotField {
            field: wire::FieldRef::new("res.company".to_owned(), "name".to_owned()),
            label: "Name".to_owned(),
            role: wire::PivotRole::Row,
            interval: None,
        };
        assert_eq!(
            super::validate_builtin_field_models("h18.fixture", &foreign)
                .unwrap_err()
                .kind(),
            erp_sdk::ErrorKind::Validation
        );
        let mut view = wire::View::new(
            wire::ViewIdentity::generated(),
            wire::ViewType::Pivot,
            "web.pivot".to_owned(),
            0,
            vec![
                wire::ViewNode::Pivot {
                    children: vec![1, 2, 3, 4, 5, 6],
                    disable_linking: false,
                    display_quantity: false,
                    default_order: Some("count desc".to_owned()),
                },
                node("state", wire::PivotRole::Row, None),
                node("stage", wire::PivotRole::Col, None),
                node("deadline", wire::PivotRole::Row, Some("month")),
                node("seen_at", wire::PivotRole::Col, Some("year")),
                node("partner_id", wire::PivotRole::Col, None),
                measure,
            ],
        );
        // The caller may not read the row field `state`, the date column `seen_at` nor the measure `count`.
        super::prune(
            &mut view,
            &[
                super::Decision::Keep,
                super::Decision::Prune,
                super::Decision::Keep,
                super::Decision::Keep,
                super::Decision::Prune,
                super::Decision::Keep,
                super::Decision::Prune,
            ],
            &DescriptionBudget::new(),
        )
        .unwrap();
        assert_eq!(view.nodes.len(), 4);
        let names: Vec<_> = view.nodes[1..]
            .iter()
            .map(|node| match node {
                wire::ViewNode::PivotField {
                    field, interval, ..
                } => (field.name.clone(), interval.clone()),
                _ => panic!("pivot field"),
            })
            .collect();
        assert_eq!(
            names,
            [
                ("stage".to_owned(), None),
                ("deadline".to_owned(), Some("month".to_owned())),
                ("partner_id".to_owned(), None)
            ]
        );
        match &view.nodes[0] {
            wire::ViewNode::Pivot {
                children,
                default_order,
                ..
            } => {
                assert_eq!(children, &[1, 2, 3]);
                assert_eq!(
                    default_order, &None,
                    "the order named the pruned measure `count`"
                );
            }
            _ => panic!("pivot"),
        }
    }
    #[test]
    fn h18_authorize_prunes_calendar_fields_and_refuses_an_unreadable_root() {
        let reference =
            |model: &str, name: &str| wire::FieldRef::new(model.to_owned(), name.to_owned());
        let calendar = |color: wire::FieldRef| wire::ViewNode::Calendar {
            children: vec![1],
            date_start: reference("h18.fixture", "deadline"),
            date_stop: None,
            all_day: None,
            color: Some(color),
            mode: wire::CalendarScale::Week,
            scales: vec![wire::CalendarScale::Week],
            create: true,
            edit: true,
            delete: true,
            quick_create: true,
            hide_date: false,
            hide_time: false,
            month_overflow: true,
            event_limit: 5,
        };
        let field = wire::ViewNode::CalendarField {
            field: reference("h18.fixture", "name"),
            label: "Name".to_owned(),
            invisible: false,
            filters: false,
        };
        assert!(matches!(
            super::decision(&field, false).unwrap(),
            super::Decision::Prune
        ));
        let root = calendar(reference("h18.fixture", "partner_id"));
        assert_eq!(
            super::decision(&root, false).unwrap_err().kind(),
            erp_sdk::ErrorKind::Access
        );
        assert!(matches!(
            super::decision(&root, true).unwrap(),
            super::Decision::Keep
        ));
        super::validate_builtin_field_models("h18.fixture", &root).unwrap();
        for foreign in [
            calendar(reference("res.company", "name")),
            calendar(reference("h18.fixture", "partner_id.name")),
        ] {
            assert_eq!(
                super::validate_builtin_field_models("h18.fixture", &foreign)
                    .unwrap_err()
                    .kind(),
                erp_sdk::ErrorKind::Validation
            );
        }
        let mut view = wire::View::new(
            wire::ViewIdentity::generated(),
            wire::ViewType::Calendar,
            "web.calendar".to_owned(),
            0,
            vec![root, field],
        );
        super::prune(
            &mut view,
            &[super::Decision::Keep, super::Decision::Prune],
            &DescriptionBudget::new(),
        )
        .unwrap();
        assert_eq!(view.nodes.len(), 1);
        match &view.nodes[0] {
            wire::ViewNode::Calendar { children, .. } => assert!(children.is_empty()),
            _ => panic!("calendar"),
        }
    }

    #[test]
    fn h18_authorize_relieves_kanban_grouping_and_prunes_its_progress_bar() {
        let field = |name: &str| wire::FieldRef::new("h18.fixture".to_owned(), name.to_owned());
        let bar = wire::ViewNode::KanbanProgressbar {
            field: field("state"),
            colors: Vec::new(),
            sum_field: Some(field("count")),
            help: None,
        };
        assert!(matches!(
            super::decision(&bar, false).unwrap(),
            super::Decision::Prune
        ));
        let mut root = wire::ViewNode::Kanban {
            children: vec![1],
            page_limit: None,
            default_group_by: Some(field("stage")),
            quick_create: true,
            records_draggable: true,
            groups_draggable: true,
            archivable: true,
            can_open: true,
            create: None,
            edit: None,
            delete: None,
        };
        assert!(super::relieve_root(&mut root));
        assert!(matches!(
            &root,
            wire::ViewNode::Kanban {
                default_group_by: None,
                ..
            }
        ));
        assert!(
            !super::relieve_root(&mut root),
            "a root without a grouping has nothing to drop"
        );
        let foreign = wire::ViewNode::KanbanProgressbar {
            field: field("state"),
            colors: Vec::new(),
            sum_field: Some(wire::FieldRef::new(
                "res.company".to_owned(),
                "name".to_owned(),
            )),
            help: None,
        };
        assert_eq!(
            super::validate_builtin_field_models("h18.fixture", &foreign)
                .unwrap_err()
                .kind(),
            erp_sdk::ErrorKind::Validation
        );
    }

    #[test]
    fn h18_authorize_prunes_denied_graph_fields_and_keeps_the_chart() {
        let node = |name: &str, role: wire::GraphRole| wire::ViewNode::GraphField {
            field: wire::FieldRef::new("h18.fixture".to_owned(), name.to_owned()),
            label: name.to_owned(),
            role,
            interval: None,
        };
        assert!(matches!(
            super::decision(&node("count", wire::GraphRole::Measure), false).unwrap(),
            super::Decision::Prune
        ));
        let foreign = wire::ViewNode::GraphField {
            field: wire::FieldRef::new("res.company".to_owned(), "name".to_owned()),
            label: "Name".to_owned(),
            role: wire::GraphRole::Group,
            interval: None,
        };
        let error = super::validate_builtin_field_models("h18.fixture", &foreign).unwrap_err();
        assert_eq!(error.kind(), erp_sdk::ErrorKind::Validation);
        assert_eq!(
            error.message(),
            "Builtin graph_field reference 'res.company.name' does not match root model 'h18.fixture'. Cross-model builtin fields require a later release."
        );
        let mut view = wire::View::new(
            wire::ViewIdentity::generated(),
            wire::ViewType::Graph,
            "web.graph".to_owned(),
            0,
            vec![
                wire::ViewNode::Graph {
                    children: vec![1, 2, 3],
                    mode: wire::GraphMode::Pie,
                    stacked: true,
                    cumulated: false,
                    order: Some(wire::GraphOrder::Desc),
                    disable_linking: false,
                },
                node("state", wire::GraphRole::Group),
                node("stage", wire::GraphRole::Group),
                node("count", wire::GraphRole::Measure),
            ],
        );
        // The caller may not read the first grouping `state`.
        super::prune(
            &mut view,
            &[
                super::Decision::Keep,
                super::Decision::Prune,
                super::Decision::Keep,
                super::Decision::Keep,
            ],
            &DescriptionBudget::new(),
        )
        .unwrap();
        let names: Vec<_> = view.nodes[1..]
            .iter()
            .map(|node| match node {
                wire::ViewNode::GraphField { field, role, .. } => (field.name.as_str(), *role),
                _ => panic!("graph field"),
            })
            .collect();
        assert_eq!(
            names,
            [
                ("stage", wire::GraphRole::Group),
                ("count", wire::GraphRole::Measure)
            ]
        );
        match &view.nodes[0] {
            wire::ViewNode::Graph {
                children,
                mode,
                order,
                ..
            } => {
                assert_eq!(children, &[1, 2]);
                assert_eq!(
                    (*mode, *order),
                    (wire::GraphMode::Pie, Some(wire::GraphOrder::Desc))
                );
            }
            _ => panic!("graph"),
        }
    }
}
