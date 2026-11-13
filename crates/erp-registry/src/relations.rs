//! Relational setup once every model is composed (REL §4.3–4.7, MODEL-020…022).
//!
//! Runs after [`crate::compose`] has built each model on its own, because these rules look across
//! models: many2one `ondelete` defaults depend on the comodel's kind, one2many inverses live on
//! the comodel, many2many relation names are built from both tables, and `_inherits` copies the
//! parent's fields onto the child.

use erp_fields::{Error, FieldType, M2mRelation, OnDelete, Result};
use rustc_hash::FxHashMap;

use crate::decl::{KindDecl, ModelKind, TableObjectKind};
use crate::drop::{DropReason, Sink, declared_on};
use crate::ident::{MAX_IDENTIFIER_LEN, is_valid_pg_name};
use crate::spec::{FieldId, FieldSpec, Inherited, ModelId, ModelSpec};

const NO_RESTRICT_COMODELS: [&str; 7] = [
    "ir.model",
    "ir.model.data",
    "ir.model.fields",
    "ir.model.fields.selection",
    "ir.model.relation",
    "ir.model.constraint",
    "ir.module.module",
];

/// The `ondelete` a many2one declares explicitly, newest declaration first.
fn declared_ondelete(model: &ModelSpec, field: &str) -> Option<OnDelete> {
    model.mro.iter().find_map(|decl| {
        decl.fields
            .iter()
            .filter(|f| f.name == field)
            .find_map(|f| match f.kind {
                KindDecl::Many2one { ondelete, .. } => ondelete,
                _ => None,
            })
    })
}

/// The relation table and columns of a many2many, as declared (newest first per part).
fn declared_relation(
    model: &ModelSpec,
    field: &str,
) -> (
    Option<&'static str>,
    Option<&'static str>,
    Option<&'static str>,
) {
    let mut out = (None, None, None);
    for decl in &model.mro {
        for f in decl.fields.iter().filter(|f| f.name == field) {
            if let KindDecl::Many2many {
                relation,
                column1,
                column2,
                ..
            } = f.kind
            {
                out.0 = out.0.or(relation);
                out.1 = out.1.or(column1);
                out.2 = out.2.or(column2);
            }
        }
    }
    out
}

/// The `ondelete` a many2many declares explicitly.
fn declared_m2m_ondelete(model: &ModelSpec, field: &str) -> Option<OnDelete> {
    model.mro.iter().find_map(|decl| {
        decl.fields
            .iter()
            .filter(|f| f.name == field)
            .find_map(|f| match f.kind {
                KindDecl::Many2many { ondelete, .. } => ondelete,
                _ => None,
            })
    })
}

fn check_table_name(name: &str) -> Result<()> {
    if !is_valid_pg_name(name) {
        return Err(Error::validation(format!(
            "Invalid characters in table name '{name}'"
        )));
    }
    if name.len() > MAX_IDENTIFIER_LEN {
        return Err(Error::validation(format!(
            "Table name '{name}' is too long"
        )));
    }
    Ok(())
}

/// Settle the cross-model relational definitions of a freshly composed registry.
///
/// A refusal of one custom field goes through `sink` (dropped under `DropBroken`, the caller
/// composes again without it); every other refusal is the error.
pub(crate) fn finish(
    models: &mut [ModelSpec],
    by_name: &FxHashMap<&'static str, ModelId>,
    sink: &mut Sink,
) -> Result<()> {
    many2one_rules(models, by_name, sink)?;
    inherits_rules(models, by_name)?;
    one2many_rules(models, by_name, sink)?;
    many2many_rules(models, by_name, sink)?;
    reference_rules(models)?;
    add_inherited_fields(models)?;
    related_groups(models, by_name)?;
    related_secret(models, by_name);
    parent_names(models)?;
    parent_store(models)?;
    signal_trigger_rules(models)?;
    follow_delete_rules(models)?;
    Ok(())
}

/// Follow-delete triggers ([`crate::FollowDeleteDecl`]): only on a model with a table; a relation
/// trigger names a stored many2many field the model declares itself and deletes no key rows; the
/// key table is the table of a model of the registry other than the model's own, with a stored
/// char field `type` naming each key row's model; the cache name fits the trigger's setting.
fn follow_delete_rules(models: &[ModelSpec]) -> Result<()> {
    for model in models {
        for obj in &model.table_objects {
            let TableObjectKind::FollowDelete(decl) = obj.kind else {
                continue;
            };
            let refused = |why: String| {
                Error::programming(format!(
                    "follow-delete trigger '{}' on model '{}': {why}",
                    obj.full_name, model.name
                ))
            };
            if !model.has_table() {
                return Err(refused("the model has no table".to_owned()));
            }
            model.follow_delete_table(&decl)?;
            if !crate::FollowDeleteDecl::valid_cache(decl.cache) {
                return Err(refused(format!(
                    "the cache name '{}' is not 1 to {} characters of [a-z0-9_]",
                    decl.cache.escape_debug(),
                    crate::MAX_FOLLOW_CACHE_LEN
                )));
            }
            let Some(key_table) = decl.key_table else {
                continue;
            };
            if matches!(decl.table, crate::TriggerTable::Relation(_)) {
                return Err(refused(format!(
                    "a trigger on a relation table deletes no rows of '{key_table}' (relation \
                     rows carry no ids of its key space)"
                )));
            }
            if key_table == model.table {
                return Err(refused(format!(
                    "the key table '{key_table}' is the model's own table"
                )));
            }
            let Some(key_model) = models
                .iter()
                .find(|m| m.has_table() && m.table == key_table)
            else {
                return Err(refused(format!(
                    "the key table '{key_table}' is not the table of a model in the registry"
                )));
            };
            // key rows name the model of their typed row in `type`: the trigger deletes only the
            // key rows of its own model
            if !key_model
                .field("type")
                .is_some_and(|f| matches!(f.ty, FieldType::Char { .. }) && f.has_column())
            {
                return Err(refused(format!(
                    "the key table '{key_table}' (model '{}') has no stored char field 'type' \
                     naming the model of each key row",
                    key_model.name
                )));
            }
        }
    }
    Ok(())
}

/// Signal triggers: only on a model with a table, and a
/// relation trigger names a stored many2many field the model declares itself (an `_inherits`
/// copy's relation table belongs to the parent).
fn signal_trigger_rules(models: &[ModelSpec]) -> Result<()> {
    for model in models {
        for obj in &model.table_objects {
            let TableObjectKind::SignalTrigger(decl) = obj.kind else {
                continue;
            };
            if !model.has_table() {
                return Err(Error::programming(format!(
                    "signal trigger '{}' on model '{}': the model has no table",
                    obj.full_name, model.name
                )));
            }
            model.signal_trigger_table(&decl)?;
        }
    }
    Ok(())
}

/// `ondelete` defaults and checks (/010), many2one towards a transient model,
/// `delegate` outside `_inherits`.
fn many2one_rules(
    models: &mut [ModelSpec],
    by_name: &FxHashMap<&'static str, ModelId>,
    sink: &mut Sink,
) -> Result<()> {
    let kinds: Vec<ModelKind> = models.iter().map(|m| m.kind).collect();
    for model in models.iter_mut() {
        let inherits_fields: Vec<&'static str> = model
            .mro
            .iter()
            .flat_map(|d| d.inherits.iter().map(|(_, f)| *f))
            .collect();
        let explicit: Vec<Option<OnDelete>> = model
            .fields
            .iter()
            .map(|f| declared_ondelete(model, f.name))
            .collect();
        let model_kind = model.kind;
        let model_name = model.name;
        for (field, explicit) in model.fields.iter_mut().zip(explicit) {
            let FieldType::Many2one { comodel, ondelete } = &mut field.ty else {
                continue;
            };
            let comodel_kind = by_name
                .get(comodel.as_str())
                .map_or(ModelKind::Regular, |id| kinds[id.index()]);
            *ondelete = match explicit {
                Some(action) => action,
                None if model_kind == ModelKind::Transient
                    && comodel_kind != ModelKind::Transient =>
                {
                    if field.required {
                        OnDelete::Cascade
                    } else {
                        OnDelete::SetNull
                    }
                }
                None if field.required => OnDelete::Restrict,
                None => OnDelete::SetNull,
            };
            // copies, so `field` can be handed to the sink below
            let ondelete = *ondelete;
            let comodel = comodel.clone();
            let refused = if field.required && ondelete == OnDelete::SetNull {
                Some(format!(
                    "The m2o field {} of model {model_name} is required but declares its ondelete \
                     policy as being 'set null'. Only 'restrict' and 'cascade' make sense.",
                    field.name
                ))
            } else if ondelete == OnDelete::Restrict
                && NO_RESTRICT_COMODELS.contains(&comodel.as_str())
            {
                Some(format!(
                    "Field {} of model {model_name} is defined as ondelete='restrict' while having \
                     {comodel} as comodel, the 'restrict' mode is not supported for this type of \
                     field as comodel.",
                    field.name
                ))
            } else if field.store
                && model_kind == ModelKind::Regular
                && comodel_kind == ModelKind::Transient
            {
                Some(format!(
                    "Many2one {model_name}.{} from Model to TransientModel is forbidden",
                    field.name
                ))
            } else {
                None
            };
            if let Some(message) = refused {
                sink.field(
                    &model.mro,
                    field,
                    DropReason::Refused,
                    Error::programming(message),
                )?;
                continue;
            }
            if field.delegate && !inherits_fields.contains(&field.name) {
                return Err(Error::programming(format!(
                    "The delegate field {model_name}.{} must be declared in the model class \
                     e.g.\n_inherits = {{'{comodel}': '{}'}}",
                    field.name, field.name
                )));
            }
        }
    }
    Ok(())
}

/// `_inherits` entries need a required many2one with an explicit `cascade`/`restrict`;
/// that field becomes `delegate` and bypasses search access.
fn inherits_rules(
    models: &mut [ModelSpec],
    by_name: &FxHashMap<&'static str, ModelId>,
) -> Result<()> {
    for model in models.iter_mut() {
        // oldest declaration first, a later one replacing the field of the same parent
        let mut entries: Vec<(&'static str, &'static str)> = Vec::new();
        for decl in model.mro.iter().rev() {
            for (parent, field) in decl.inherits {
                match entries.iter_mut().find(|(p, _)| p == parent) {
                    Some(entry) => entry.1 = field,
                    None => entries.push((parent, field)),
                }
            }
        }
        let mut resolved = Vec::with_capacity(entries.len());
        for (parent, fname) in entries {
            let parent_id = *by_name.get(parent).ok_or_else(|| {
                Error::programming(format!(
                    "Model '{}' inherits from non-existing model '{parent}'.",
                    model.name
                ))
            })?;
            let missing = || {
                Error::programming(format!(
                    "Missing many2one field definition for _inherits reference '{fname}' in model \
                     '{}'. Add a field like: {fname} = fields.Many2one('{parent}', required=True, \
                     ondelete='cascade')",
                    model.name
                ))
            };
            let explicit = declared_ondelete(model, fname);
            let field_id = model.field_id(fname).ok_or_else(missing)?;
            let field = &mut model.fields[field_id.index()];
            let FieldType::Many2one { comodel, .. } = &field.ty else {
                return Err(missing());
            };
            if comodel != parent {
                return Err(missing());
            }
            let valid_ondelete = matches!(explicit, Some(OnDelete::Cascade | OnDelete::Restrict));
            if !field.required || !valid_ondelete || !field.store {
                return Err(Error::programming(format!(
                    "Field definition for _inherits reference '{fname}' in '{}' must be marked as \
                     'delegate', 'required' with ondelete='cascade' or 'restrict'",
                    model.name
                )));
            }
            field.delegate = true;
            field.bypass_search_access = true;
            resolved.push((parent_id, field_id));
        }
        model.inherits = resolved;
    }
    Ok(())
}

/// One2many inverses must exist on the comodel.
fn one2many_rules(
    models: &[ModelSpec],
    by_name: &FxHashMap<&'static str, ModelId>,
    sink: &mut Sink,
) -> Result<()> {
    for model in models.iter() {
        for field in &model.fields {
            let FieldType::One2many { comodel, inverse } = &field.ty else {
                continue;
            };
            let Some(co) = by_name.get(comodel.as_str()).map(|id| &models[id.index()]) else {
                continue;
            };
            let problem = match co.field(inverse) {
                None => Error::programming(format!(
                    "'{inverse}' declared in {}.{} does not exist on '{comodel}'.",
                    model.name, field.name
                )),
                // F2: an integer inverse would read rows of any model
                Some(inv)
                    if !matches!(
                        inv.ty,
                        FieldType::Many2one { .. } | FieldType::Many2oneReference { .. }
                    ) =>
                {
                    Error::programming(format!(
                        "The inverse of {}.{} must be a many2one or a many2one_reference: \
                         '{comodel}.{inverse}' is a {} field.",
                        model.name,
                        field.name,
                        inv.ty.type_name()
                    ))
                }
                Some(_) => continue,
            };
            sink.field(&model.mro, field, DropReason::Refused, problem)?;
        }
    }
    Ok(())
}

/// A many2many field using a relation.
#[derive(Debug)]
struct RelationOwner {
    model: ModelId,
    comodel: String,
    /// `model.field`, for messages.
    label: String,
    /// The relation table is declared explicitly.
    explicit: bool,
    /// `(model, field)` when the field is a pure custom field (it can be dropped).
    custom: Option<(&'static str, &'static str)>,
}

/// Relation naming, `ondelete` and sharing rules of many2many fields (…034).
fn many2many_rules(
    models: &mut [ModelSpec],
    by_name: &FxHashMap<&'static str, ModelId>,
    sink: &mut Sink,
) -> Result<()> {
    let tables: Vec<String> = models.iter().map(|m| m.table.clone()).collect();
    // (relation, column1, column2) -> (model, comodel, field, explicit relation)
    let mut owners: FxHashMap<(String, String, String), Vec<RelationOwner>> = FxHashMap::default();
    for model in models.iter_mut() {
        let names: Vec<&'static str> = model.fields.iter().map(|f| f.name).collect();
        for name in names {
            let declared = declared_relation(model, name);
            let explicit_ondelete = declared_m2m_ondelete(model, name);
            let model_name = model.name;
            let model_id = model.id;
            let model_table = model.table.clone();
            // the custom declaration's model, for a pure custom field
            let owner = model
                .fields
                .iter()
                .find(|f| f.name == name)
                .and_then(|f| declared_on(&model.mro, f));
            let Some(field) = model.fields.iter_mut().find(|f| f.name == name) else {
                continue;
            };
            let FieldType::Many2many {
                comodel,
                relation,
                ondelete,
            } = &mut field.ty
            else {
                continue;
            };
            let mut refuse = |error: Error| match owner {
                Some(declared) => sink.refuse(true, declared, name, DropReason::Refused, error),
                None => Err(error),
            };
            if explicit_ondelete == Some(OnDelete::SetNull) {
                refuse(Error::programming(format!(
                    "The m2m field {name} of model {model_name} declares its ondelete policy as \
                     being 'set null'. Only 'restrict' and 'cascade' make sense."
                )))?;
                continue;
            }
            *ondelete = explicit_ondelete.unwrap_or(OnDelete::Cascade);
            if !field.store {
                *relation = None;
                continue;
            }
            let Some(co_table) = by_name
                .get(comodel.as_str())
                .map(|id| tables[id.index()].clone())
            else {
                continue;
            };
            let table = match declared.0 {
                Some(t) => t.to_owned(),
                None => {
                    if model_table == co_table {
                        refuse(Error::programming(format!(
                            "{model_name}.{name}: Implicit/canonical naming of many2many \
                             relationship table is not possible when source and destination \
                             models are the same"
                        )))?;
                        continue;
                    }
                    let (a, b) = if model_table <= co_table {
                        (&model_table, &co_table)
                    } else {
                        (&co_table, &model_table)
                    };
                    format!("{a}_{b}_rel")
                }
            };
            if let Err(error) = check_table_name(&table) {
                refuse(error)?;
                continue;
            }
            let column1 = declared
                .1
                .map_or_else(|| format!("{model_table}_id"), str::to_owned);
            let column2 = declared
                .2
                .map_or_else(|| format!("{co_table}_id"), str::to_owned);
            if let Some(column) = [&column1, &column2]
                .into_iter()
                .find(|c| !is_valid_pg_name(c) || c.len() > MAX_IDENTIFIER_LEN)
            {
                refuse(Error::validation(format!(
                    "Invalid column name '{column}' for the relation of {model_name}.{name}"
                )))?;
                continue;
            }
            let key = (table.clone(), column1.clone(), column2.clone());
            let entry = RelationOwner {
                model: model_id,
                comodel: comodel.clone(),
                label: format!("{model_name}.{name}"),
                explicit: declared.0.is_some(),
                custom: owner.map(|declared| (declared, name)),
            };
            let list = owners.entry(key).or_default();
            let clash = list.iter().find(|other| {
                let siblings = other.model == entry.model
                    && other.comodel == entry.comodel
                    && other.explicit
                    && entry.explicit;
                !siblings
            });
            if let Some(other) = clash {
                let error = Error::programming(format!(
                    "Many2many fields {} and {} use the same table and columns",
                    other.label, entry.label
                ));
                // drop the custom one of the two; two addon fields stay an error
                match (entry.custom, other.custom) {
                    (Some(_), _) => {
                        refuse(error)?;
                        continue;
                    }
                    (None, Some((m, f))) => {
                        sink.refuse(true, m, f, DropReason::Refused, error)?;
                    }
                    (None, None) => return Err(error),
                }
            }
            list.push(entry);
            *relation = Some(M2mRelation {
                table,
                column1,
                column2,
            });
        }
    }
    Ok(())
}

/// `model_field` of a many2one_reference must exist.
fn reference_rules(models: &[ModelSpec]) -> Result<()> {
    for model in models.iter() {
        for field in &model.fields {
            if let FieldType::Many2oneReference { model_field } = &field.ty
                && model.field(model_field).is_none()
            {
                return Err(Error::programming(format!(
                    "Field {}.{} with unknown model_field '{model_field}'",
                    model.name, field.name
                )));
            }
        }
    }
    Ok(())
}

/// Parents first, so a child copies its parent's inherited fields too.
fn inherits_order(models: &[ModelSpec]) -> Result<Vec<usize>> {
    fn visit(i: usize, models: &[ModelSpec], state: &mut [u8], out: &mut Vec<usize>) -> Result<()> {
        match state[i] {
            2 => return Ok(()),
            1 => {
                return Err(Error::programming(format!(
                    "_inherits of model '{}' forms a cycle",
                    models[i].name
                )));
            }
            _ => {}
        }
        state[i] = 1;
        for (parent, _) in &models[i].inherits {
            visit(parent.index(), models, state, out)?;
        }
        state[i] = 2;
        out.push(i);
        Ok(())
    }
    let mut state = vec![0u8; models.len()];
    let mut out = Vec::with_capacity(models.len());
    for i in 0..models.len() {
        visit(i, models, &mut state, &mut out)?;
    }
    Ok(out)
}

/// Fields of `_inherits` parents not redefined on the child become inherited fields, read and
/// written through the delegate many2one (MODEL §4.4).
fn add_inherited_fields(models: &mut [ModelSpec]) -> Result<()> {
    for i in inherits_order(models)? {
        if models[i].inherits.is_empty() {
            continue;
        }
        let entries = models[i].inherits.clone();
        for (parent, delegate) in entries {
            let parent_fields: Vec<FieldSpec> = models[parent.index()].fields.clone();
            let child = &mut models[i];
            for pf in parent_fields {
                if pf.magic {
                    continue;
                }
                let make_field = |id: FieldId| {
                    let mut field = pf.clone();
                    field.id = id;
                    field.store = false;
                    // the column, and so its database default, is the parent's
                    field.db_default = false;
                    field.index = None;
                    field.prefetch = false;
                    field.default = None;
                    field.compute = None;
                    field.inverse = None;
                    field.related = None;
                    // the copy is read on the parent: no related path, so no company-bound or
                    // superuser read of its own (review TD2)
                    field.compute_read = crate::ComputeRead::AsCaller;
                    field.precompute = None;
                    field.depends = None;
                    field.reads = &[];
                    field.depends_context = &[];
                    field.delegate = false;
                    // the change is captured and reported under the parent model, never on the child
                    field.tracking = None;
                    // the inherited copy of a custom field is not itself custom
                    field.manual = false;
                    field.inherited = Some(Inherited {
                        delegate,
                        parent,
                        parent_field: pf.id,
                    });
                    field
                };
                if let Some(&existing_id) = child.by_name.get(pf.name) {
                    if child.fields[existing_id.index()].inherited.is_some() {
                        child.fields[existing_id.index()] = make_field(existing_id);
                    }
                    // Otherwise locally defined on child: local redefinition wins.
                    continue;
                }
                let id = FieldId(child.fields.len() as u32);
                let field = make_field(id);
                child.by_name.insert(field.name, id);
                child.fields.push(field);
            }
        }
        // `_rec_name` defaults to `name`, which may come from a parent
        let child = &mut models[i];
        let explicit = child.mro.iter().any(|d| d.rec_name.is_some());
        if !explicit && child.rec_name.is_none() {
            child.rec_name = child.field_id("name");
        }
    }
    Ok(())
}

/// A field that declares `groups` keeps its own,
/// and an `_inherits` child's field takes its parent field's effective groups.
///
/// # Errors
///
/// `Programming`, naming `model.field` and the path, for a related path that does not resolve
/// and for a cycle of related fields: either would leave the field without the target's groups
/// (a field the target restricts would be readable through it), so the registry does not build.
fn related_groups(
    models: &mut [ModelSpec],
    by_name: &FxHashMap<&'static str, ModelId>,
) -> Result<()> {
    type Key = (usize, usize);

    fn label(models: &[ModelSpec], at: Key) -> String {
        format!("{}.{}", models[at.0].name, models[at.0].fields[at.1].name)
    }

    fn effective(
        models: &[ModelSpec],
        by_name: &FxHashMap<&'static str, ModelId>,
        at: Key,
        stack: &mut Vec<Key>,
        memo: &mut FxHashMap<Key, Option<&'static str>>,
    ) -> Result<Option<&'static str>> {
        if let Some(done) = memo.get(&at) {
            return Ok(*done);
        }
        if stack.contains(&at) {
            let mut chain: Vec<String> = stack.iter().map(|k| label(models, *k)).collect();
            chain.push(label(models, at));
            return Err(Error::programming(format!(
                "related fields form a cycle: {}",
                chain.join(" -> ")
            )));
        }
        let field = &models[at.0].fields[at.1];
        let result = if let Some(inherited) = field.inherited {
            stack.push(at);
            let parent = (inherited.parent.index(), inherited.parent_field.index());
            let found = effective(models, by_name, parent, stack, memo);
            stack.pop();
            found?
        } else if field.groups.is_some() {
            field.groups
        } else if let Some(path) = field.related {
            let target = match related_target(models, by_name, at.0, path) {
                Ok(target) => target,
                // a custom field with a broken path is refused or dropped by
                // `drop::check_custom_related` in the same build (strict: the build's error;
                // `DropBroken`: left out, and the next round composes without it)
                Err(_) if crate::drop::is_pure_custom(field) => {
                    memo.insert(at, None);
                    return Ok(None);
                }
                Err(reason) => {
                    return Err(Error::programming(format!(
                        "related field {} (related='{path}'): {reason}",
                        label(models, at)
                    )));
                }
            };
            stack.push(at);
            let found = effective(models, by_name, target, stack, memo);
            stack.pop();
            found?
        } else {
            None
        };
        memo.insert(at, result);
        Ok(result)
    }

    /// The (model, field) a related path ends at, or why the path does not resolve.
    fn related_target(
        models: &[ModelSpec],
        by_name: &FxHashMap<&'static str, ModelId>,
        start: usize,
        path: &str,
    ) -> std::result::Result<(usize, usize), String> {
        let mut model = start;
        let mut segments = path.split('.').peekable();
        while let Some(name) = segments.next() {
            let Some(id) = models[model].field_id(name) else {
                return Err(format!(
                    "'{name}' is not a field of model '{}'",
                    models[model].name
                ));
            };
            if segments.peek().is_none() {
                return Ok((model, id.index()));
            }
            let field = &models[model].fields[id.index()];
            let Some(comodel) = field.ty.comodel() else {
                return Err(format!(
                    "the path goes through {}.{}, which is not a relation field",
                    models[model].name, field.name
                ));
            };
            let Some(next) = by_name.get(comodel) else {
                return Err(format!(
                    "the path goes through {}.{}, whose comodel '{comodel}' is not in the registry",
                    models[model].name, field.name
                ));
            };
            model = next.index();
        }
        Err("the path is empty".to_owned())
    }

    let mut memo: FxHashMap<Key, Option<&'static str>> = FxHashMap::default();
    let mut updates: Vec<(usize, usize, &'static str)> = Vec::new();
    for (m, model) in models.iter().enumerate() {
        for (f, field) in model.fields.iter().enumerate() {
            if field.groups.is_none() && (field.related.is_some() || field.inherited.is_some()) {
                let mut stack = Vec::new();
                if let Some(groups) = effective(models, by_name, (m, f), &mut stack, &mut memo)? {
                    updates.push((m, f, groups));
                }
            }
        }
    }
    for (m, f, groups) in updates {
        models[m].fields[f].groups = Some(groups);
    }
    Ok(())
}

/// A related field whose path ends at a `secret` field is `secret` itself, along chains of related
/// fields, so a writable `related = "user_id.password"` is redacted from the audit digest like its
/// target (delta D2). A path that does not resolve is left to `related_groups`, which
/// refuses it.
fn related_secret(models: &mut [ModelSpec], by_name: &FxHashMap<&'static str, ModelId>) {
    fn target(
        models: &[ModelSpec],
        by_name: &FxHashMap<&'static str, ModelId>,
        start: usize,
        path: &str,
    ) -> Option<(usize, usize)> {
        let mut model = start;
        let mut segments = path.split('.').peekable();
        while let Some(name) = segments.next() {
            let id = models[model].field_id(name)?;
            if segments.peek().is_none() {
                return Some((model, id.index()));
            }
            let comodel = models[model].fields[id.index()].ty.comodel()?;
            model = by_name.get(comodel)?.index();
        }
        None
    }
    // a chain is as long as the number of related fields at most: iterate until nothing changes
    loop {
        let mut updates = Vec::new();
        for (m, model) in models.iter().enumerate() {
            for (f, field) in model.fields.iter().enumerate() {
                if field.secret {
                    continue;
                }
                if let Some(path) = field.related
                    && let Some((tm, tf)) = target(models, by_name, m, path)
                    && models[tm].fields[tf].secret
                {
                    updates.push((m, f));
                }
            }
        }
        if updates.is_empty() {
            return;
        }
        for (m, f) in updates {
            models[m].fields[f].secret = true;
        }
    }
}

/// `_parent_store` (CRUD §4.11): the model needs a parent many2one to itself and an indexed
/// `parent_path` char field.
fn parent_store(models: &mut [ModelSpec]) -> Result<()> {
    for model in models.iter_mut() {
        if !model
            .mro
            .iter()
            .find_map(|d| d.parent_store)
            .unwrap_or(false)
        {
            continue;
        }
        let self_parent = model.parent_name.is_some_and(|p| {
            matches!(&model.field_by_id(p).ty, FieldType::Many2one { comodel, .. } if *comodel == model.name)
        });
        if !self_parent {
            return Err(Error::programming(format!(
                "_parent_store on model '{}' needs a many2one parent field to the model itself \
                 (_parent_name, default parent_id)",
                model.name
            )));
        }
        // a computed parent changes without a write the paths could follow
        if model
            .parent_name
            .is_some_and(|p| model.field_by_id(p).is_computed())
        {
            return Err(Error::programming(format!(
                "_parent_store on model '{}' needs a parent field that is not computed \
                 (_parent_name, default parent_id)",
                model.name
            )));
        }
        let path = model
            .field("parent_path")
            .filter(|f| f.store && matches!(f.ty, FieldType::Char { .. }))
            .ok_or_else(|| {
                Error::programming(format!(
                    "add a field parent_path on model '{}': #[field(char, index)] pub parent_path \
                     (_parent_store)",
                    model.name
                ))
            })?;
        if path.index.is_none() {
            return Err(Error::programming(format!(
                "parent_path field on model '{}' should be indexed: #[field(char, index)]",
                model.name
            )));
        }
        model.parent_path = Some(path.id);
    }
    Ok(())
}

/// `_parent_name`: explicit, else `parent_id` when it exists (DOM §4.8).
fn parent_names(models: &mut [ModelSpec]) -> Result<()> {
    for model in models.iter_mut() {
        let declared = model.mro.iter().find_map(|d| d.parent_name);
        model.parent_name = match declared {
            Some(name) => Some(model.field_id(name).ok_or_else(|| {
                Error::programming(format!(
                    "Invalid _parent_name='{name}' for model '{}'",
                    model.name
                ))
            })?),
            None => model
                .field("parent_id")
                .filter(|f| matches!(f.ty, FieldType::Many2one { .. }))
                .map(|f| f.id),
        };
    }
    Ok(())
}
