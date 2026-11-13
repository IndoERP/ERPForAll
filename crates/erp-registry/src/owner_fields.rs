// SPDX-License-Identifier: MIT OR Apache-2.0
//! `owner_only_stored_fields`.
//!
//! A model whose creating declaration sets the policy admits materialized columns only from its
//! owner (that declaration's addon) and the named co-owners. Every declaration in the model's MRO
//! carries the set of addons that brought it in (its provenance), so a mixin, a runtime model file
//! or an overlay cannot launder another addon's column into an owner-looking model. Generation
//! metadata decides physical storage: a VIRTUAL generated field has `store = true` and no storage.

use erp_fields::{Error, FieldType, Result};
use rustc_hash::{FxHashMap, FxHashSet};

use crate::compose::{decl_fields, merge_declarations};
use crate::decl::{AddonDescriptor, FieldDecl, KindDecl, ModelDecl, ModelKind};
use crate::drop::{DropReason, Sink};
use crate::overlay::MANUAL_ADDON;
use crate::spec::{FieldSpec, ModelSpec};

/// Sorted, duplicate-free addon names.
type Addons = Vec<&'static str>;
/// The provenance of every declaration a model's MRO holds, by declaration address.
type Provenance = FxHashMap<*const ModelDecl, Addons>;
/// Declarations by model name, in load order.
type ByName = FxHashMap<&'static str, Vec<&'static ModelDecl>>;

/// One declaration of a field on a model, with its provenance.
struct Entry<'p> {
    fd: &'static FieldDecl,
    decl: &'static ModelDecl,
    magic: bool,
    by: &'p [&'static str],
}

/// A physical table of a protected model (its own table, or the relation table of one of its
/// stored many2many fields) and who may use it.
struct Guard {
    table: String,
    model: &'static str,
    /// The many2many field whose relation table this is; `None` for the model's own table.
    field: Option<&'static str>,
    owner: &'static str,
    admitted: Addons,
}

impl Guard {
    fn describe(&self) -> String {
        match self.field {
            Some(field) => format!(
                "relation table '{}' belongs to '{}.{field}'",
                self.table, self.model
            ),
            None => format!("table '{}' belongs to '{}'", self.table, self.model),
        }
    }
}

/// What physically exists for a field: compared before and after unauthorized declarations.
#[derive(PartialEq)]
enum Outcome {
    /// The declarations do not merge.
    Refused,
    /// No column and no relation table.
    Open,
    /// A column (VIRTUAL included) or a stored many2many.
    Physical(Box<Shape>),
}

#[derive(PartialEq)]
struct Shape {
    ty: FieldType,
    store: bool,
    translate: bool,
    company_dependent: bool,
    generated: Option<crate::generated::GeneratedSpec>,
    relation: (
        Option<&'static str>,
        Option<&'static str>,
        Option<&'static str>,
    ),
}

/// Check every policy and every protected model of a composed registry.
///
/// # Errors
///
/// `Programming` for an invalid policy declaration, and for a declaration that adds or changes a
/// materialized column (or a guarded relation table) without being in the owner group. A custom
/// field goes through `sink`, so `ManualFieldPolicy::DropBroken` drops and reports it.
pub(crate) fn check(
    addons: &[&'static AddonDescriptor],
    known: &FxHashSet<&'static str>,
    decls: &[&'static ModelDecl],
    models: &[ModelSpec],
    sink: &mut Sink,
) -> Result<()> {
    let mut by_name = ByName::default();
    for decl in decls {
        by_name.entry(decl.name).or_default().push(decl);
    }
    for decl in decls {
        if let Some(co_owners) = decl.owner_only_stored_fields {
            validate_policy(addons, known, &by_name, decl, co_owners)?;
        }
    }
    let mut guards: Vec<Guard> = Vec::new();
    for model in models {
        let Some(owner) = creator(&by_name, model.name) else {
            continue;
        };
        let Some(co_owners) = owner.owner_only_stored_fields else {
            continue;
        };
        let group = union(&[owner.addon], co_owners);
        if model.has_table() {
            guards.push(Guard {
                table: model.table.clone(),
                model: model.name,
                field: None,
                owner: owner.addon,
                admitted: group.clone(),
            });
        }
        let provenance = provenance(&by_name, model.name);
        let row_version = crate::keys::row_version_origin(model.name, &model.mro)?;
        for field in model.fields.iter().filter(|f| f.inherited.is_none()) {
            let entries = entries(model, field.name, row_version, &provenance)?;
            let Some(first) = entries.first() else {
                continue;
            };
            let admitted = union(&group, first.by);
            if let Some((addon, decl)) = field_offender(model.name, &entries, &group, &admitted)? {
                refuse(sink, model, field, addon, decl, owner.addon, None)?;
                continue;
            }
            if let FieldType::Many2many {
                relation: Some(rel),
                ..
            } = &field.ty
            {
                guards.push(Guard {
                    table: rel.table.clone(),
                    model: model.name,
                    field: Some(field.name),
                    owner: owner.addon,
                    admitted,
                });
            }
        }
    }
    if guards.is_empty() {
        return Ok(());
    }
    // another model whose table is a protected table would add its columns to it
    for model in models.iter().filter(|m| m.has_table()) {
        let Some(guard) = guards
            .iter()
            .find(|g| g.table == model.table && g.model != model.name)
        else {
            continue;
        };
        let Some(decl) = creator(&by_name, model.name) else {
            continue;
        };
        if is_framework(decl.addon) || guard.admitted.contains(&decl.addon) {
            continue;
        }
        return Err(Error::programming(format!(
            "addon '{}' cannot store model '{}' in table '{}'; owner is '{}'; declared by {}; {}",
            decl.addon,
            model.name,
            model.table,
            guard.owner,
            decl.describe(),
            guard.describe()
        )));
    }
    // a field elsewhere naming a guarded relation table needs that relation's owner group
    for model in models {
        let mut provenance: Option<Provenance> = None;
        for field in model.fields.iter().filter(|f| f.inherited.is_none()) {
            let FieldType::Many2many {
                relation: Some(rel),
                ..
            } = &field.ty
            else {
                continue;
            };
            let Some(guard) = guards.iter().find(|g| {
                g.table == rel.table && !(g.model == model.name && g.field == Some(field.name))
            }) else {
                continue;
            };
            let provenance =
                provenance.get_or_insert_with(|| self::provenance(&by_name, model.name));
            let entries = entries(model, field.name, None, provenance)?;
            let taking = entries.iter().enumerate().find(|(i, e)| {
                (*i == 0 || declares_relation(e.fd)) && !subset(e.by, &guard.admitted)
            });
            if let Some((_, e)) = taking {
                let addon = outside(e.by, &guard.admitted);
                refuse(sink, model, field, addon, e.decl, guard.owner, Some(guard))?;
            }
        }
    }
    Ok(())
}

fn validate_policy(
    addons: &[&'static AddonDescriptor],
    known: &FxHashSet<&'static str>,
    by_name: &ByName,
    decl: &'static ModelDecl,
    co_owners: &[&'static str],
) -> Result<()> {
    let at = decl.describe();
    if decl.is_extension() {
        let owner = creator(by_name, decl.name).map_or("<unknown>", |d| d.addon);
        return Err(Error::programming(format!(
            "{at} cannot set owner_only_stored_fields on model '{}'; only its owner '{owner}' \
             declares the policy",
            decl.name
        )));
    }
    if decl.runtime {
        return Err(Error::programming(format!(
            "{at} cannot set owner_only_stored_fields: the policy is declared only in native \
             addon code"
        )));
    }
    if decl.kind == ModelKind::Abstract {
        return Err(Error::programming(format!(
            "{at} sets owner_only_stored_fields on an abstract model; only a concrete model has \
             stored columns"
        )));
    }
    for (i, co) in co_owners.iter().enumerate() {
        let reason = if co.is_empty()
            || *co == MANUAL_ADDON
            || !co
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
        {
            Some("is not an addon name".to_owned())
        } else if *co == decl.addon {
            Some("is the owner itself".to_owned())
        } else if co_owners[..i].contains(co) {
            Some("is listed twice".to_owned())
        } else if !known.contains(co) {
            Some(
                "is not a known addon (neither in the addon set nor linked into this build)"
                    .to_owned(),
            )
        } else {
            addons
                .iter()
                .find(|a| a.name == *co)
                .filter(|a| !depends_on(addons, a, decl.addon))
                .map(|_| format!("does not depend on the owner '{}'", decl.addon))
        };
        if let Some(reason) = reason {
            return Err(Error::programming(format!(
                "{at}: co-owner '{co}' of owner_only_stored_fields {reason}"
            )));
        }
    }
    Ok(())
}

/// The declaration that creates `model` (not an in-place extension).
fn creator(by_name: &ByName, model: &str) -> Option<&'static ModelDecl> {
    by_name
        .get(model)?
        .iter()
        .copied()
        .find(|d| !d.is_extension())
}

/// Whether `from` depends on `target`, directly or through its dependencies.
fn depends_on(addons: &[&'static AddonDescriptor], from: &AddonDescriptor, target: &str) -> bool {
    let mut stack = vec![from.depends];
    let mut seen: FxHashSet<&str> = FxHashSet::default();
    while let Some(depends) = stack.pop() {
        for dep in depends {
            if *dep == target {
                return true;
            }
            if seen.insert(dep)
                && let Some(next) = addons.iter().find(|a| a.name == *dep)
            {
                stack.push(next.depends);
            }
        }
    }
    false
}

fn is_framework(addon: &str) -> bool {
    addon == crate::BASE_ADDON.name || addon == crate::WEB_ADDON.name
}

/// Which addons brought each declaration of `model`'s MRO in. The model's own declarations carry
/// their addon; a parent's creating declaration takes the set of the declaration that named the
/// parent, and an in-place extension of a parent adds its addon. The implicit `base` parent and
/// the framework addons add nothing. Paths are unioned.
fn provenance(by_name: &ByName, model: &'static str) -> Provenance {
    let mut out = Provenance::default();
    let mut work: Vec<(&'static str, Addons)> = Vec::new();
    let mut seen: FxHashSet<(&'static str, Addons)> = FxHashSet::default();
    let mut visit =
        |decl: &'static ModelDecl, by: Addons, work: &mut Vec<(&'static str, Addons)>| {
            for parent in decl.inherit.iter().filter(|p| **p != decl.name) {
                work.push((parent, by.clone()));
            }
            if decl.name != "base" {
                work.push(("base", Vec::new()));
            }
            let slot = out.entry(std::ptr::from_ref(decl)).or_default();
            *slot = union(slot, &by);
        };
    for decl in by_name.get(model).into_iter().flatten() {
        let by = if is_framework(decl.addon) {
            Vec::new()
        } else {
            vec![decl.addon]
        };
        visit(decl, by, &mut work);
    }
    while let Some((class, by)) = work.pop() {
        if !seen.insert((class, by.clone())) {
            continue;
        }
        for decl in by_name.get(class).into_iter().flatten() {
            let by = if decl.is_extension() && !is_framework(decl.addon) {
                union(&by, &[decl.addon])
            } else {
                by.clone()
            };
            visit(decl, by, &mut work);
        }
    }
    out
}

/// The declarations of `field` on `model` in merge order (as `build_model` meets them).
fn entries<'p>(
    model: &ModelSpec,
    field: &str,
    row_version: Option<&'static ModelDecl>,
    provenance: &'p Provenance,
) -> Result<Vec<Entry<'p>>> {
    let mut out: Vec<Entry<'p>> = Vec::new();
    for decl in model.mro.iter().rev().copied() {
        for (fd, magic) in decl_fields(decl, row_version) {
            if fd.name != field {
                continue;
            }
            // a custom declaration of an addon field is refused by composition, never merged
            if decl.addon == MANUAL_ADDON
                && out.first().is_some_and(|e| e.decl.addon != MANUAL_ADDON)
            {
                continue;
            }
            let Some(by) = provenance.get(&std::ptr::from_ref(decl)) else {
                return Err(Error::programming(format!(
                    "{} is in the MRO of model '{}' but has no provenance",
                    decl.describe(),
                    model.name
                )));
            };
            out.push(Entry {
                fd,
                decl,
                magic,
                by,
            });
        }
    }
    Ok(out)
}

/// The addon (and its declaration) that adds or changes a materialized column of the field
/// without being admitted, if any.
fn field_offender(
    model: &str,
    entries: &[Entry<'_>],
    group: &[&'static str],
    admitted: &[&'static str],
) -> Result<Option<(&'static str, &'static ModelDecl)>> {
    let merge = |selected: &[&Entry<'_>]| {
        merge_declarations(model, selected.iter().map(|e| (e.fd, e.decl, e.magic)))
    };
    let all: Vec<&Entry<'_>> = entries.iter().collect();
    let Some(full) = merge(&all)? else {
        return Ok(None);
    };
    // the declaration that first makes the field a materialized column must be in the group
    if materialized(&full) {
        let k = (0..all.len())
            .find(|&k| {
                merge(&all[..=k])
                    .ok()
                    .flatten()
                    .is_some_and(|s| materialized(&s))
            })
            .unwrap_or(all.len() - 1);
        if !subset(all[k].by, group) {
            return Ok(Some((outside(all[k].by, group), all[k].decl)));
        }
    }
    let ok = |e: &Entry<'_>| subset(e.by, admitted);
    if all.iter().all(|e| ok(e)) {
        return Ok(None);
    }
    let shape = |selected: Vec<&Entry<'_>>| outcome(merge(&selected), &selected);
    let base = shape(all.iter().copied().filter(|e| ok(e)).collect());
    if shape(all.clone()) == base {
        return Ok(None);
    }
    // name the first unadmitted declaration whose addition changes what the admitted ones give
    let unadmitted: Vec<usize> = (0..all.len()).filter(|&i| !ok(all[i])).collect();
    let changing = unadmitted.iter().copied().find(|&k| {
        let prefix = (0..all.len())
            .filter(|&i| i <= k || ok(all[i]))
            .map(|i| all[i])
            .collect();
        shape(prefix) != base
    });
    let k = changing.or(unadmitted.last().copied()).unwrap_or(0);
    Ok(Some((outside(all[k].by, admitted), all[k].decl)))
}

fn outcome(spec: Result<Option<FieldSpec>>, entries: &[&Entry<'_>]) -> Outcome {
    let spec = match spec {
        Ok(Some(spec)) => spec,
        Ok(None) => return Outcome::Open,
        Err(_) => return Outcome::Refused,
    };
    let stored_m2m = matches!(spec.ty, FieldType::Many2many { .. }) && spec.store;
    if !spec.has_column() && !stored_m2m {
        return Outcome::Open;
    }
    let mut ty = spec.ty.clone();
    // selection labels are labels; their values are the column's domain
    if let FieldType::Selection {
        values: Some(values),
        ..
    } = &mut ty
    {
        for (_, label) in values {
            label.clear();
        }
    }
    // the relation table is named after composition: compare what the declarations say
    let mut relation = (None, None, None);
    for e in entries {
        match e.fd.kind {
            KindDecl::Many2many {
                relation: r,
                column1,
                column2,
                ..
            } => {
                relation = (
                    r.or(relation.0),
                    column1.or(relation.1),
                    column2.or(relation.2),
                );
            }
            _ => relation = (None, None, None),
        }
    }
    Outcome::Physical(Box::new(Shape {
        ty,
        store: spec.store,
        translate: spec.translate,
        company_dependent: spec.company_dependent,
        generated: spec.generated().cloned(),
        relation,
    }))
}

/// A column that holds data: stored and not VIRTUAL generated (`store` alone does not say).
fn materialized(spec: &FieldSpec) -> bool {
    spec.has_column() && !spec.is_virtual()
}

fn declares_relation(fd: &FieldDecl) -> bool {
    matches!(
        fd.kind,
        KindDecl::Many2many {
            relation: Some(_),
            ..
        }
    )
}

fn union(a: &[&'static str], b: &[&'static str]) -> Addons {
    let mut out: Addons = a.iter().chain(b).copied().collect();
    out.sort_unstable();
    out.dedup();
    out
}

fn subset(a: &[&str], of: &[&str]) -> bool {
    a.iter().all(|x| of.contains(x))
}

/// The first addon of `by` outside `of` (callers only ask when there is one).
fn outside(by: &[&'static str], of: &[&str]) -> &'static str {
    by.iter()
        .copied()
        .find(|a| !of.contains(a))
        .unwrap_or("<unknown>")
}

fn refuse(
    sink: &mut Sink,
    model: &ModelSpec,
    field: &FieldSpec,
    addon: &str,
    decl: &ModelDecl,
    owner: &str,
    relation: Option<&Guard>,
) -> Result<()> {
    let mut message = format!(
        "addon '{addon}' cannot add or change stored field '{}.{}'; owner is '{owner}'; declared \
         by {}",
        model.name,
        field.name,
        decl.describe()
    );
    if let Some(guard) = relation {
        message = format!("{message}; {}", guard.describe());
    }
    sink.field(
        &model.mro,
        field,
        DropReason::Refused,
        Error::programming(message),
    )
}
