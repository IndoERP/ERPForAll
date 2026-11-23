//! Field-level group rules of a policy snapshot: the
//! `groups` of the registry's fields and the overlay of custom fields (`ir.model.fields` groups),
//! compiled once into [`GroupSpec`]s, and the [`FieldMask`] a principal's groups give a model.
//!
//! A field with registry groups *and* an overlay row must satisfy both (denying more is the safe
//! direction when an addon takes over the name of a custom field). An administrator's override
//! (`ir.field.access`) either adds its groups the same way (`extend`) or takes the place of the
//! declared groups (`replace`); a `replace` override of a field declared for nobody (`.`) is
//! ignored and reported. A spec that does not compile
//! (an unknown group) denies its field to everyone outside superuser mode and is listed in the
//! build report; in debug mode the denial error names the broken spec.

use erp_fields::{Error, Result};
use erp_orm::{FieldDenialDetail, FieldMask, FieldOp, GroupId, GroupSet};
use erp_registry::{FieldId, ModelId, ModelSpec, Registry};
use rustc_hash::FxHashMap;

use crate::group_spec::{GroupSpec, GroupSpecError, NEVER};
use crate::groups::GroupGraph;
use crate::report::{FieldGroupsIssue, FieldGroupsProblem, GroupsSource};
use crate::rows::{FieldGroupsMode, FieldGroupsRow};

#[derive(Debug)]
struct Part {
    source: GroupsSource,
    spec: Result<GroupSpec, GroupSpecError>,
}

#[derive(Debug)]
struct FieldEntry {
    field: FieldId,
    parts: Vec<Part>,
}

/// The restricted fields of every model.
#[derive(Debug, Default)]
pub(crate) struct FieldRules {
    models: FxHashMap<ModelId, Vec<FieldEntry>>,
}

impl FieldRules {
    /// Compile the registry's field groups and the overlay `rows` against `graph`; problems are
    /// returned (they deny, they do not fail the build).
    pub(crate) fn compile(
        registry: &Registry,
        graph: &GroupGraph,
        rows: &[FieldGroupsRow],
    ) -> (Self, Vec<FieldGroupsIssue>) {
        let mut issues = Vec::new();
        let mut models: FxHashMap<ModelId, Vec<FieldEntry>> = FxHashMap::default();
        for model in registry.models() {
            for field in &model.fields {
                let Some(text) = field.groups.filter(|t| !t.is_empty()) else {
                    continue;
                };
                let spec = GroupSpec::parse(text, graph);
                push(
                    &mut models,
                    &mut issues,
                    model,
                    field.id,
                    field.name,
                    GroupsSource::Registry,
                    spec,
                );
            }
        }
        // rows sort by model, field and mode, after every registry part is in: a `replace`
        // override finds the declared part it takes the place of
        for row in rows {
            let source = if row.mode().is_override() {
                GroupsSource::Override
            } else {
                GroupsSource::Overlay
            };
            let Some(model) = registry.model(row.model()) else {
                issues.push(FieldGroupsIssue::new(
                    row.model(),
                    row.field(),
                    source,
                    FieldGroupsProblem::UnknownModel,
                ));
                continue;
            };
            let Some(field) = model.field(row.field()) else {
                issues.push(FieldGroupsIssue::new(
                    row.model(),
                    row.field(),
                    source,
                    FieldGroupsProblem::UnknownField,
                ));
                continue;
            };
            if row.mode() == FieldGroupsMode::Replace {
                // a field declared for nobody holds secrets (key digests, encrypted material): no
                // stored row may open it, whoever wrote the row and however
                if field.groups.is_some_and(|text| text.trim() == NEVER) {
                    issues.push(FieldGroupsIssue::new(
                        model.name,
                        field.name,
                        source,
                        FieldGroupsProblem::Protected,
                    ));
                    continue;
                }
                drop_registry_part(&mut models, model.id, field.id);
                if row.groups().is_empty() {
                    continue;
                }
            }
            let spec = GroupSpec::any_of(row.groups().iter().copied(), graph);
            push(
                &mut models,
                &mut issues,
                model,
                field.id,
                field.name,
                source,
                spec,
            );
        }
        for entries in models.values_mut() {
            entries.sort_by_key(|e| e.field);
        }
        issues.sort_by(|a, b| (a.model(), a.field()).cmp(&(b.model(), b.field())));
        (Self { models }, issues)
    }

    /// Whether any field of `model` has a group rule.
    pub(crate) fn restricts(&self, model: ModelId) -> bool {
        self.models.contains_key(&model)
    }

    /// The mask of `model` for a principal in `groups` (`debug`: `base.group_no_one` counts).
    pub(crate) fn mask(
        &self,
        model: &ModelSpec,
        groups: &GroupSet,
        debug: bool,
    ) -> Result<FieldMask> {
        let mut mask = FieldMask::full(model.fields.len());
        for entry in self.models.get(&model.id).into_iter().flatten() {
            if entry
                .parts
                .iter()
                .any(|p| matches!(&p.spec, Ok(spec) if spec.is_never()))
            {
                mask.forbid(entry.field)?;
                continue;
            }
            let allowed = entry
                .parts
                .iter()
                .all(|p| matches!(&p.spec, Ok(spec) if spec.satisfied_by(groups, debug)));
            if !allowed {
                mask.deny(entry.field, FieldOp::Read)?
                    .deny(entry.field, FieldOp::Write)?;
            }
        }
        Ok(mask)
    }

    pub(crate) fn detail(
        &self,
        model: &ModelSpec,
        field: FieldId,
        graph: &GroupGraph,
    ) -> Result<FieldDenialDetail> {
        let Some(entry) = self
            .models
            .get(&model.id)
            .and_then(|e| e.iter().find(|e| e.field == field))
        else {
            return Ok(FieldDenialDetail::CustomRules);
        };
        let mut positives: Vec<GroupId> = Vec::new();
        let mut negatives: Vec<GroupId> = Vec::new();
        for part in &entry.parts {
            match &part.spec {
                Err(error) => {
                    return Err(Error::programming(format!(
                        "the groups of field '{}' on {} ({}) do not compile: {error}",
                        model.field_by_id(field).name,
                        model.name,
                        part.source
                    ))
                    .with_source(error.clone()));
                }
                Ok(spec) if spec.is_never() => return Ok(FieldDenialDetail::AlwaysForbidden),
                Ok(spec) => {
                    positives.extend(spec.positives());
                    negatives.extend(spec.negatives());
                }
            }
        }
        positives.sort_unstable();
        positives.dedup();
        negatives.sort_unstable();
        negatives.dedup();
        let mut names = Vec::with_capacity(positives.len() + negatives.len());
        for group in positives {
            names.push(graph.display_name(group)?);
        }
        for group in negatives {
            names.push(format!("!{}", graph.display_name(group)?));
        }
        Ok(FieldDenialDetail::Groups(names))
    }
}

/// Remove the declared part of `field` (a `replace` override takes its place), and the field's
/// entry when nothing else restricts it.
fn drop_registry_part(
    models: &mut FxHashMap<ModelId, Vec<FieldEntry>>,
    model: ModelId,
    field: FieldId,
) {
    let Some(entries) = models.get_mut(&model) else {
        return;
    };
    for entry in entries.iter_mut().filter(|e| e.field == field) {
        entry.parts.retain(|p| p.source != GroupsSource::Registry);
    }
    entries.retain(|e| !e.parts.is_empty());
    if entries.is_empty() {
        models.remove(&model);
    }
}

fn push(
    models: &mut FxHashMap<ModelId, Vec<FieldEntry>>,
    issues: &mut Vec<FieldGroupsIssue>,
    model: &ModelSpec,
    field: FieldId,
    field_name: &str,
    source: GroupsSource,
    spec: Result<GroupSpec, GroupSpecError>,
) {
    if let Err(error) = &spec {
        issues.push(FieldGroupsIssue::new(
            model.name,
            field_name,
            source,
            FieldGroupsProblem::BadSpec(error.clone()),
        ));
    }
    let entries = models.entry(model.id).or_default();
    let part = Part { source, spec };
    match entries.iter_mut().find(|e| e.field == field) {
        Some(entry) => entry.parts.push(part),
        None => entries.push(FieldEntry {
            field,
            parts: vec![part],
        }),
    }
}
