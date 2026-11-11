//! Final host checks of section descriptors, independent of XML lowering and callbacks.
use super::*;

pub(super) const COUNTER_NOTICE: &str =
    "Category counters are unavailable while a filter domain is active.";
fn invalid(id: u32, kind: &str) -> Error {
    failure(format!("Search panel section {id} {kind}."))
}
fn category(section: &wire::PanelSection) -> bool {
    matches!(section, wire::PanelSection::Category { .. })
}
fn details(
    section: &wire::PanelSection,
) -> Result<(Option<&wire::PanelDomain>, Option<&wire::FieldRef>)> {
    match section {
        wire::PanelSection::Category { .. } => Ok((None, None)),
        wire::PanelSection::Filter {
            domain, group_by, ..
        } => Ok((domain.as_ref(), group_by.as_ref())),
        _ => Err(invalid(
            section.id(),
            "has an unsupported kind; additional syntax requires a later release",
        )),
    }
}
fn relation(field: &erp_sdk::FieldSpec) -> Option<&str> {
    match &field.ty {
        FieldType::Many2one { comodel, .. } | FieldType::Many2many { comodel, .. } => Some(comodel),
        _ => None,
    }
}
fn root_field<'a>(
    request: &'a Request<'_>,
    section: &wire::PanelSection,
) -> Result<&'a erp_sdk::FieldSpec> {
    let id = section.id();
    let field = section.field();
    if field.model != request.model || field.name.contains('.') {
        return Err(invalid(id, "has an invalid root field scope"));
    }
    let spec = request
        .env
        .registry()
        .model(request.model)
        .ok_or_else(|| invalid(id, "references an invalid root model"))?;
    let field = spec
        .field(&field.name)
        .ok_or_else(|| invalid(id, "references an invalid root field"))?;
    if !field.store {
        return Err(invalid(id, "requires a stored field"));
    }
    match &field.ty {
        FieldType::Selection {
            values: Some(values),
            ..
        } => {
            if values.is_empty() {
                return Err(invalid(id, "requires static selection choices"));
            }
            let bytes = values
                .len()
                .checked_mul(std::mem::size_of::<&str>())
                .ok_or_else(|| invalid(id, "exceeds selection storage limit"))?;
            let mut storage = request.budget.reserve(bytes).map_err(view_error)?;
            let mut keys = Vec::new();
            keys.try_reserve_exact(values.len())
                .map_err(|_| invalid(id, "exceeds selection storage limit"))?;
            if keys.capacity() > values.len() {
                storage
                    .grow(
                        (keys.capacity() - values.len())
                            .checked_mul(std::mem::size_of::<&str>())
                            .ok_or_else(|| invalid(id, "exceeds selection storage limit"))?,
                    )
                    .map_err(view_error)?;
            }
            for (key, _) in values {
                if key.is_empty() || key.len() > 256 {
                    return Err(invalid(id, "has an invalid selection key"));
                }
                keys.push(key.as_str());
            }
            keys.sort_unstable();
            if keys.windows(2).any(|pair| pair[0] == pair[1]) {
                return Err(invalid(id, "has duplicate selection keys"));
            }
        }
        FieldType::Many2one { .. } | FieldType::Many2many { .. } => {}
        _ => {
            return Err(invalid(
                id,
                "has an unsupported field type; additional syntax requires a later release",
            ));
        }
    }
    Ok(field)
}
fn common(section: &wire::PanelSection) -> Result<()> {
    let id = section.id();
    let (label, icon, defaults) = match section {
        wire::PanelSection::Category {
            label,
            icon,
            default_values,
            ..
        }
        | wire::PanelSection::Filter {
            label,
            icon,
            default_values,
            ..
        } => (label, icon, default_values),
        _ => {
            return Err(invalid(
                id,
                "has an unsupported kind; additional syntax requires a later release",
            ));
        }
    };
    if id == 0 {
        return Err(invalid(id, "has an invalid identity"));
    }
    if label.len() > 256 {
        return Err(invalid(id, "exceeds label byte limit"));
    }
    if label.trim().is_empty() {
        return Err(invalid(id, "has an invalid label"));
    }
    if icon.as_ref().is_some_and(|icon| icon.len() > 64) {
        return Err(invalid(id, "exceeds icon byte limit"));
    }
    if let Some(icon) = icon
        && (icon.is_empty()
            || !icon.as_bytes()[0].is_ascii_alphabetic()
            || !icon
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-')))
    {
        return Err(invalid(id, "has an invalid icon"));
    }
    if !defaults.is_empty() {
        return Err(invalid(id, "has structural defaults"));
    }
    Ok(())
}
fn domain_refs<'a>(
    domain: &'a wire::PanelDomain,
    budget: &DescriptionBudget,
    id: u32,
) -> Result<erp_sdk::views::SchemaReferences<'a>> {
    erp_sdk::views::panel_domain_references(&domain.model, &domain.ast, budget).map_err(|error| {
        match error {
            erp_sdk::views::ViewError::LimitExceeded { .. } => failure(format!(
                "Search panel section {id} domain admission failed: {error}"
            )),
            _ => invalid(
                id,
                "has an unsupported domain syntax; additional syntax requires a later release",
            ),
        }
    })
}
fn dependency(sections: &[wire::PanelSection], path: &str, id: u32) -> Result<usize> {
    if path.contains('.') {
        return Err(invalid(id, "has an undeclared category dependency"));
    }
    sections
        .iter()
        .position(|s| category(s) && s.field().name == path)
        .ok_or_else(|| invalid(id, "has an undeclared category dependency"))
}

/// Validate every section and every path before any denied section is removed. The complete
/// independent AST traversal below grants no authority; each path then receives caller checks.
pub(super) async fn authorize(
    request: &Request<'_>,
    node: &mut wire::ViewNode,
    tx: &mut Transaction,
    fields: &mut AuthorizedFields,
    list_companion: bool,
) -> Result<()> {
    let is_list = matches!(node, wire::ViewNode::List { .. });
    let panel = match node {
        wire::ViewNode::List { search_panel, .. } | wire::ViewNode::Search { search_panel, .. } => {
            search_panel
        }
        _ => return Ok(()),
    };
    let Some(wire::ListSearchPanel::Sections {
        view_types,
        sections,
        notice,
        ..
    }) = panel
    else {
        return Ok(());
    };
    if sections.len() > 64 {
        return Err(invalid(0, "exceeds section count limit"));
    }
    if view_types.is_empty() || view_types.len() > 64 {
        return Err(invalid(0, "exceeds view key count limit"));
    }
    for (i, key) in view_types.iter().enumerate() {
        if key.len() > 129 || key.split('.').any(|part| part.len() > 64) {
            return Err(invalid(0, "exceeds view key byte limit"));
        }
        if !erp_sdk::views::is_valid_view_type_key(key) || view_types[..i].contains(key) {
            return Err(invalid(0, "has invalid view applicability"));
        }
    }
    let mut dependencies = [0u64; 64];
    let mut keep = [true; 64];
    // Complete shape/path admission runs before access decisions, including later-denied sections.
    for (index, section) in sections.iter().enumerate() {
        let id = section.id();
        common(section)?;
        let field = root_field(request, section)?;
        if sections[..index].iter().any(|s| s.id() == id) {
            return Err(invalid(id, "has a duplicate identity"));
        }
        if category(section)
            && sections[..index]
                .iter()
                .any(|s| category(s) && s.field().name == section.field().name)
        {
            return Err(invalid(id, "has an ambiguous category binding"));
        }
        let (domain, group) = details(section)?;
        let comodel = relation(field);
        if let Some(domain) = domain {
            if Some(domain.model.as_str()) != comodel {
                return Err(invalid(id, "has an invalid domain model"));
            }
            let refs = domain_refs(domain, request.budget, id)?;
            for reference in refs.fields() {
                match reference {
                    FieldReference::RootPath(path) => {
                        let dependency = dependency(sections, path, id)?;
                        dependencies[index] |= 1u64 << dependency;
                        validate_path(request.env, request.model, path)
                            .map_err(|_| invalid(id, "references an invalid category field"))?;
                    }
                    FieldReference::ModelPath { model, path } => {
                        if Some(model) != comodel {
                            return Err(invalid(id, "has an invalid domain model"));
                        }
                        validate_path(request.env, model, path)
                            .map_err(|_| invalid(id, "references an invalid domain field"))?;
                    }
                    _ => {
                        return Err(invalid(
                            id,
                            "has an unsupported reference kind; additional syntax requires a later release",
                        ));
                    }
                }
            }
        }
        if let Some(group) = group {
            if Some(group.model.as_str()) != comodel {
                return Err(invalid(id, "has an invalid grouping scope"));
            }
            if group.name.contains('.') {
                return Err(invalid(id, "references an invalid grouping field"));
            }
            let field = request
                .env
                .registry()
                .model(&group.model)
                .and_then(|m| m.field(&group.name))
                .ok_or_else(|| invalid(id, "references an invalid grouping field"))?;
            if !field.store
                || matches!(
                    field.ty,
                    FieldType::One2many { .. }
                        | FieldType::Many2many { .. }
                        | FieldType::Json
                        | FieldType::DateRange { .. }
                        | FieldType::DatetimeRange { .. }
                )
            {
                return Err(invalid(
                    id,
                    "has an unsupported grouping field; additional syntax requires a later release",
                ));
            }
        }
    }
    if (is_list || list_companion) && !view_types.iter().any(|key| key == "list") {
        *panel = None;
        return Ok(());
    }
    for (index, section) in sections.iter().enumerate() {
        let id = section.id();
        keep[index] = path_allowed(request.env, tx, request.model, &section.field().name).await?;
        // Shape admission above proved the field and relation exist. Even a plain relational
        // section requires comodel Read access before publishing its label or defaults.
        let field = request
            .env
            .registry()
            .model(request.model)
            .and_then(|model| model.field(&section.field().name))
            .ok_or_else(|| Error::invariant("Validated panel field disappeared."))?;
        if let Some(comodel) = relation(field) {
            match request
                .env
                .records(comodel)?
                .check_access(tx, erp_sdk::Operation::Read)
                .await
            {
                Ok(()) => {}
                Err(error) if error.kind() == ErrorKind::Access => keep[index] = false,
                Err(error) => return Err(error),
            }
        }
        let (domain, group) = details(section)?;
        if let Some(domain) = domain {
            let refs = domain_refs(domain, request.budget, id)?;
            for field in refs.fields() {
                let (scope, path) = reference(field, request.model)?;
                if !path_allowed(request.env, tx, scope, path).await? {
                    keep[index] = false;
                }
            }
        }
        if let Some(group) = group
            && !path_allowed(request.env, tx, &group.model, &group.name).await?
        {
            keep[index] = false;
        }
    }
    // Category dependencies are explicit; retain fixed-point semantics if future sections add edges.
    for _ in 0..sections.len() {
        let mut changed = false;
        let denied = keep[..sections.len()]
            .iter()
            .enumerate()
            .fold(
                0u64,
                |mask, (i, keep)| if *keep { mask } else { mask | (1u64 << i) },
            );
        for (i, dependent) in dependencies[..sections.len()].iter().enumerate() {
            if keep[i] && dependent & denied != 0 {
                keep[i] = false;
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    let mut index = 0;
    sections.retain(|_| {
        let retain = keep[index];
        index += 1;
        retain
    });
    if sections.is_empty() {
        *panel = None;
        return Ok(());
    }
    let has_domain = sections.iter().any(|s| {
        matches!(
            s,
            wire::PanelSection::Filter {
                domain: Some(_),
                ..
            }
        )
    });
    *notice = if has_domain {
        Some(fields.text(COUNTER_NOTICE)?)
    } else {
        None
    };
    for section in sections {
        fields.insert(&section.field().name)?;
        if has_domain
            && let wire::PanelSection::Category {
                enable_counters, ..
            } = section
        {
            *enable_counters = false;
        }
        // Every RHS reference names a retained category, whose root field was inserted above or
        // will be inserted later. Comodel criteria/grouping never expand the root catalogue.
    }
    Ok(())
}
