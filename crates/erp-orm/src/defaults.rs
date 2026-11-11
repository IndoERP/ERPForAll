//! `default_get` and the normalization of default values (CRUD §4.5, to 008).
//!
//! The `base` implementation resolves each name in the order of
//! CRUD §4.5 and normalizes the values; `create` turns them back into cells and commands
//! ([`add_missing_defaults_batch`]).

use crate::html::input_cell as to_cell;
use erp_fields::convert::FieldRef;
use erp_fields::{Cell, Error, FieldType, RecId, Result, Value, ValueMap};
use erp_registry::{FieldDefault, FieldSpec, ModelId, ModelSpec};
use std::any::Any;

use crate::Transaction;
use crate::access::Operation;
use crate::commands::Command;
use crate::dispatch::{BoxFuture, ImplKind, MethodFn, MethodImpl, Super, call_method};
use crate::field_access::FieldOp;
use crate::records::Records;
use crate::vals::RawVals;

impl Records {
    /// `default_get(fields)` (CRUD §4.5): the default value of each requested name that has one,
    /// through the model's `default_get` override chain. A name absent from the result has no
    /// default. Steps, numbered as in CRUD §4.5, first match wins:
    ///
    /// 1. the context key `default_<name>`, returned even when `<name>` is not a field
    ///    ;
    /// 2. other names that are not fields are skipped (never an error);
    /// 3. the persisted `ir.default` user default, when that model is installed;
    /// 4. the field's `default` (static value or function of the environment);
    /// 5. the persisted `ir.default` fallback of company-dependent fields;
    /// 6. a field delegated to an `_inherits` parent: the parent model's `default_get` (its own
    ///    chain), unless the environment may not write the field (its groups).
    ///
    /// Outside superuser mode, default x2many commands that
    /// `UPDATE`, `DELETE`, or (one2many) `LINK`/`UNLINK` records need the matching model access on
    /// the comodel, and field write access to the fields they update.
    ///
    /// Without the `ir.default` model there are no persisted defaults. Generated outputs have
    /// no default: an explicit context/persisted default, or an output returned by an override,
    /// returns `Validation` with the generated read-only message.
    pub async fn default_get(&self, tx: &mut Transaction, names: &[&str]) -> Result<ValueMap> {
        // Overrides and persisted ir.default lookups use the transaction capability.
        tx.inc_default_get_count();
        let names: Vec<String> = names.iter().map(|n| (*n).to_owned()).collect();
        let values =
            call_method::<(Vec<String>,), ValueMap>(tx, self, "default_get", (names,)).await?;
        crate::database_generated::check_write_names(
            tx,
            self.spec(),
            values.keys().map(String::as_str),
        )?;
        Ok(values)
    }
}

impl<M: crate::records::Model> crate::records::RecordSet<M> {
    /// [`Records::default_get`] on the typed surface (names from field handles, e.g.
    /// `Partner::NAME.name()`).
    pub async fn default_get(&self, tx: &mut Transaction, names: &[&str]) -> Result<ValueMap> {
        self.as_records().default_get(tx, names).await
    }
}

/// The `base` implementation of `default_get`, on the model of `records`.
async fn base_default_get(
    tx: &mut Transaction,
    records: &Records,
    names: &[String],
) -> Result<ValueMap> {
    tx.check_records(records)?;
    // `may_write_field` and the x2many checks below read the binding: bring it up to date first
    if !records.env().su() {
        tx.prepare_access(records.env()).await?;
    }
    let empty = Records::empty(records.env().clone(), records.model_id());
    // Base not installed => no stored defaults in ir.default.
    let model_defaults = if crate::ir_default::is_available(records.env().registry()) {
        crate::ir_default::get_model_defaults(tx, records.env(), records.model_name(), None).await?
    } else {
        Default::default()
    };
    let (mut out, parents) = resolve_own(tx, &empty, names, &model_defaults)?;
    // step 6: each parent model's own `default_get`, overrides included
    for (parent, list) in parents {
        let parent_records = Records::empty(empty.env().clone(), parent);
        out.extend(parent_records.default_get(tx, &list).await?);
    }
    Ok(out)
}

/// Fields of `_inherits` parents to ask their model for, per parent in first-seen order.
type ParentNames = Vec<(ModelId, Vec<&'static str>)>;

/// Steps 1 to 5 on the empty recordset `records`, and the names step 6 defers to parents.
fn resolve_own(
    tx: &mut Transaction,
    records: &Records,
    names: &[String],
    model_defaults: &rustc_hash::FxHashMap<String, Value>,
) -> Result<(ValueMap, ParentNames)> {
    let spec = records.spec();
    let env = records.env();
    let mut out = ValueMap::new();
    let mut parents: ParentNames = Vec::new();
    let has_default_keys = env.context().has_default_keys();
    for name in names {
        let field = spec.field(name);
        if let Some(field) = field
            && field.is_generated()
        {
            if env.context().get(&format!("default_{name}")).is_some()
                || model_defaults.contains_key(name.as_str())
            {
                crate::database_generated::check_assignment(spec, field)?;
            }
            continue;
        }
        if has_default_keys && let Some(value) = env.context().get(&format!("default_{name}")) {
            let value = match field {
                Some(field) => normalize(tx, records, field, &value)?,
                None => value,
            };
            out.insert(name.clone(), value);
            continue;
        }
        let Some(field) = field else {
            continue;
        };
        // Step 3: ir.default user default for non-company-dependent fields (CRUD §4.5 step 3)
        if !field.company_dependent
            && let Some(value) = model_defaults.get(name.as_str())
        {
            let value = normalize(tx, records, field, value)?;
            out.insert(name.clone(), value);
            continue;
        }
        // Step 4: field.default (static value or function of the environment)
        if let Some(value) = crate::write::field_default(records, field) {
            let value = normalize(tx, records, field, &value)?;
            out.insert(name.clone(), value);
            continue;
        }
        // Step 5: ir.default fallback for company-dependent fields (CRUD §4.5 step 5)
        if field.company_dependent
            && let Some(value) = model_defaults.get(name.as_str())
        {
            let value = normalize(tx, records, field, value)?;
            out.insert(name.clone(), value);
            continue;
        }
        // Step 6: delegated to parent
        if let Some(inherited) = field.inherited
            && may_write_field(tx, records, field)?
        {
            let parent_field = env
                .registry()
                .model_by_id(inherited.parent)
                .field_by_id(inherited.parent_field)
                .name;
            match parents.iter_mut().find(|(m, _)| *m == inherited.parent) {
                Some((_, list)) => list.push(parent_field),
                None => parents.push((inherited.parent, vec![parent_field])),
            }
        }
    }
    Ok((out, parents))
}

fn may_write_field(tx: &Transaction, records: &Records, field: &FieldSpec) -> Result<bool> {
    Ok(tx
        .field_mask(records.env(), records.model_id())?
        .writable(field.id))
}

fn normalize(
    tx: &mut Transaction,
    records: &Records,
    field: &FieldSpec,
    value: &Value,
) -> Result<Value> {
    if field.ty.is_x2many() {
        let default = x2many_default(tx, records, field, value)?;
        let mut commands = Vec::with_capacity(default.creates.len() + 1);
        commands.push(Command::Set(default.links).to_value());
        commands.extend(
            default
                .creates
                .into_iter()
                .map(|vals| Command::Create(vals).to_value()),
        );
        return Ok(Value::list(commands));
    }
    let fref = FieldRef {
        model: records.model_name(),
        field: field.name,
        label: &field.string,
    };
    let cell = to_cell(fref, &field.ty, value)?;
    tx.admit_html_cell(records.spec(), field, cell)
        .map(|cell| Value::from_cell(&cell))
}

/// An x2many default once normalized: the existing records it links, in order, and the values of
/// the lines it creates, in order.
struct X2manyDefault {
    links: Vec<RecId>,
    creates: Vec<RawVals>,
}

/// One element of the value an x2many default builds on an empty record.
enum Line {
    Existing(RecId),
    New(RawVals),
}

/// One2many `CREATE` values lose the inverse
/// many2one, which the create sets anyway. Outside superuser mode the commands are first
/// access-checked (CRUD §4.5).
fn x2many_default(
    tx: &Transaction,
    records: &Records,
    field: &FieldSpec,
    value: &Value,
) -> Result<X2manyDefault> {
    let env = records.env();
    let registry = env.registry();
    let comodel_spec = crate::relational::comodel_of(registry, field)?;
    let comodel = Records::empty(env.clone(), comodel_spec.id);
    let commands = records.commands_from_value(&comodel, field.name, value)?;
    let inverse = match &field.ty {
        FieldType::One2many { inverse, .. } => Some(inverse.as_str()),
        _ => None,
    };
    if !env.su() {
        for command in &commands {
            let op = match command {
                Command::Update(..) => Some(Operation::Write),
                Command::Link(_) | Command::Unlink(_) if inverse.is_some() => {
                    Some(Operation::Write)
                }
                Command::Delete(_) => Some(Operation::Unlink),
                _ => None,
            };
            if let Some(op) = op {
                tx.check_model(env, comodel_spec.id, op)?;
            }
            let updated: Vec<&str> = match command {
                Command::Update(_, vals) => vals.names().collect(),
                Command::Link(_) | Command::Unlink(_) => inverse.into_iter().collect(),
                _ => Vec::new(),
            };
            tx.check_field_names(env, comodel_spec.id, updated, FieldOp::Write)?;
        }
    }
    let mut lines: Vec<Line> = Vec::new();
    let link = |lines: &mut Vec<Line>, id: RecId| {
        if !lines
            .iter()
            .any(|l| matches!(l, Line::Existing(e) if *e == id))
        {
            lines.push(Line::Existing(id));
        }
    };
    for command in commands {
        match command {
            Command::Create(mut vals) => {
                if let Some(inverse) = inverse {
                    vals.remove(inverse);
                }
                lines.push(Line::New(vals));
            }
            Command::Update(id, _) | Command::Link(id) => link(&mut lines, id),
            Command::Delete(id) | Command::Unlink(id) => {
                lines.retain(|l| !matches!(l, Line::Existing(e) if *e == id));
            }
            Command::Clear => lines.clear(),
            Command::Set(ids) => {
                lines.clear();
                for id in ids {
                    link(&mut lines, id);
                }
            }
        }
    }
    let mut out = X2manyDefault {
        links: Vec::new(),
        creates: Vec::new(),
    };
    for line in lines {
        match line {
            Line::Existing(id) => out.links.push(id),
            Line::New(vals) => out.creates.push(vals),
        }
    }
    Ok(out)
}

/// A resolved static default value stored for the fast batch creation path.
struct CachedDefault {
    id: erp_registry::FieldId,
    name: &'static str,
    cell: Cell,
}

/// Batch default resolution (CRUD §4.2 step 3): resolves defaults across an entire batch of records.
///
/// When the model's `default_get` chain is base-only, no `default_*` context keys are present,
/// and all defaults in the hierarchy are static values, the defaults are resolved once for the
/// entire batch and cloned for each record's missing fields without per-record method dispatch.
/// Otherwise, dispatches `default_get` per record using hoisted candidate field names.
pub async fn add_missing_defaults_batch(
    tx: &mut Transaction,
    records: &Records,
    vals_list: &mut [RawVals],
) -> Result<()> {
    if vals_list.is_empty() {
        return Ok(());
    }

    let env = records.env();
    let registry = std::sync::Arc::clone(env.registry());
    let spec = registry.model_by_id(records.model_id());
    let model_graph = tx.shared.graph.model(records.model_id());

    // Exclude id, magic, parent_path, and readonly precomputed fields upfront
    let candidate_fields: Vec<&FieldSpec> = spec
        .fields
        .iter()
        .filter(|f| {
            !matches!(f.ty, FieldType::Id)
                && !f.magic
                // filled by the server at insert
                && !f.server_set
                && spec.parent_path != Some(f.id)
                && !model_graph
                    .compute(f.id)
                    .is_some_and(|fc| fc.precompute() && !fc.editable && f.inverse.is_none())
        })
        .collect();

    if candidate_fields.is_empty() {
        return Ok(());
    }

    let is_base_chain = is_base_default_get_hierarchy(&registry, &tx.shared.dispatch, spec);
    let has_ctx_defaults = env.context().has_default_keys();
    let all_static = all_defaults_static_in_hierarchy(&registry, spec);

    if is_base_chain && !has_ctx_defaults && all_static {
        // Fast path: resolve static defaults once for the batch
        let candidate_names: Vec<&str> = candidate_fields.iter().map(|f| f.name).collect();
        let defaults = records.default_get(tx, &candidate_names).await?;
        if defaults.is_empty() {
            return Ok(());
        }

        let mut cached_defaults = Vec::with_capacity(defaults.len());
        for (name, value) in defaults {
            let field = spec.field(&name).ok_or_else(|| {
                Error::programming(format!(
                    "default_get of '{}' returned a value for '{name}', which is not a field of the \
                     model",
                    spec.name
                ))
            })?;
            if field.ty.is_x2many() {
                // StaticValue cannot express non-empty x2many commands; any x2many default
                // with commands uses FieldDefault::Fn, which takes the per-record path.
                continue;
            }
            let fref = FieldRef {
                model: spec.name,
                field: field.name,
                label: &field.string,
            };
            let cell = to_cell(fref, &field.ty, &value)?;
            let cell = tx.admit_html_cell(spec, field, cell)?;
            cached_defaults.push(CachedDefault {
                id: field.id,
                name: field.name,
                cell,
            });
        }

        for vals in vals_list.iter_mut() {
            let avoided = avoided_parents(&registry, spec, vals);
            for item in &cached_defaults {
                let field = spec.field_by_id(item.id);
                if is_given(vals, field.name)
                    || (!avoided.is_empty() && inherited_from(&registry, field, &avoided))
                {
                    continue;
                }
                vals.insert(item.name.into(), item.cell.clone());
            }
        }
        return Ok(());
    }

    // Fallback path: dynamic defaults, overrides, or default_* context keys
    for vals in vals_list.iter_mut() {
        let avoided = avoided_parents(&registry, spec, vals);
        let missing: Vec<&str> = candidate_fields
            .iter()
            .filter(|f| {
                !is_given(vals, f.name)
                    && (avoided.is_empty() || !inherited_from(&registry, f, &avoided))
            })
            .map(|f| f.name)
            .collect();
        if missing.is_empty() {
            continue;
        }
        let defaults = records.default_get(tx, &missing).await?;
        apply_defaults(records, spec, &registry, vals, defaults)?;
    }

    Ok(())
}

fn apply_defaults(
    records: &Records,
    spec: &ModelSpec,
    registry: &erp_registry::Registry,
    vals: &mut RawVals,
    defaults: ValueMap,
) -> Result<()> {
    for (name, value) in defaults {
        if is_given(vals, &name) {
            continue;
        }
        let field = spec.field(&name).ok_or_else(|| {
            Error::programming(format!(
                "default_get of '{}' returned a value for '{name}', which is not a field of the \
                 model",
                spec.name
            ))
        })?;
        if field.ty.is_x2many() {
            let comodel = crate::relational::comodel_of(registry, field)?;
            let comodel = Records::empty(records.env().clone(), comodel.id);
            let mut commands = records.commands_from_value(&comodel, field.name, &value)?;
            commands.retain(|c| !matches!(c, Command::Set(ids) if ids.is_empty()));
            if !commands.is_empty() {
                vals.insert_commands(field.name.into(), commands);
            }
        } else {
            let fref = FieldRef {
                model: spec.name,
                field: field.name,
                label: &field.string,
            };
            vals.insert(field.name.into(), to_cell(fref, &field.ty, &value)?);
        }
    }
    Ok(())
}

fn is_base_default_get_hierarchy(
    registry: &erp_registry::Registry,
    dispatch: &crate::Dispatch,
    model: &ModelSpec,
) -> bool {
    if !dispatch.is_base_method(model.id, "default_get") {
        return false;
    }
    for &(parent_id, _) in &model.inherits {
        let parent_spec = registry.model_by_id(parent_id);
        if !is_base_default_get_hierarchy(registry, dispatch, parent_spec) {
            return false;
        }
    }
    true
}

fn all_defaults_static_in_hierarchy(registry: &erp_registry::Registry, spec: &ModelSpec) -> bool {
    for f in &spec.fields {
        let mut cur = f;
        if matches!(cur.default, Some(FieldDefault::Fn(_))) {
            return false;
        }
        while let Some(inherited) = cur.inherited {
            cur = registry
                .model_by_id(inherited.parent)
                .field_by_id(inherited.parent_field);
            if matches!(cur.default, Some(FieldDefault::Fn(_))) {
                return false;
            }
        }
    }
    true
}

fn is_given(vals: &RawVals, name: &str) -> bool {
    vals.contains(name) || vals.commands(name).is_some()
}

fn avoided_parents(
    registry: &erp_registry::Registry,
    spec: &ModelSpec,
    vals: &RawVals,
) -> Vec<ModelId> {
    if spec.inherits.is_empty() {
        return Vec::new();
    }
    let mut avoided = Vec::new();
    let mut stack = vec![spec];
    while let Some(model) = stack.pop() {
        for &(parent, delegate) in &model.inherits {
            if is_given(vals, model.field_by_id(delegate).name) {
                avoided.push(parent);
            } else {
                stack.push(registry.model_by_id(parent));
            }
        }
    }
    avoided
}

/// Whether `field` is inherited, at any depth, from one of `parents`.
fn inherited_from(
    registry: &erp_registry::Registry,
    field: &FieldSpec,
    parents: &[ModelId],
) -> bool {
    let mut field = field;
    while let Some(inherited) = field.inherited {
        if parents.contains(&inherited.parent) {
            return true;
        }
        field = registry
            .model_by_id(inherited.parent)
            .field_by_id(inherited.parent_field);
    }
    false
}

type DefaultGetFn = MethodFn<(Vec<String>,), ValueMap>;

fn base_default_get_impl<'a>(
    tx: &'a mut Transaction,
    records: &'a Records,
    args: (Vec<String>,),
    _sup: Super<(Vec<String>,), ValueMap>,
) -> BoxFuture<'a, Result<ValueMap>> {
    Box::pin(async move { base_default_get(tx, records, &args.0).await })
}

fn default_get_entry() -> &'static (dyn Any + Send + Sync) {
    static F: DefaultGetFn = base_default_get_impl;
    &F
}

/// The `base` implementation of `default_get`, registered by [`crate::Dispatch::build`] like the
/// CRUD built-ins.
pub(crate) static BUILTIN_IMPLS: [&MethodImpl; 1] = [&BASE_DEFAULT_GET];

static BASE_DEFAULT_GET: MethodImpl = MethodImpl {
    addon: "base",
    model: "base",
    method: "default_get",
    kind: ImplKind::Define,
    sig_name: std::any::type_name::<DefaultGetFn>,
    entry: default_get_entry,
    file: file!(),
    line: line!(),
};
