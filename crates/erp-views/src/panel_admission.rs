//! Raw panel admission before the authority-bound traversal can remove any section.
use super::*;
use erp_fields::FieldType;
fn invalid(id: usize, reason: &str) -> Error {
    Error::validation(format!("Search panel section {id} {reason}."))
}
#[derive(Debug, thiserror::Error)]
#[error("Search panel section {id} {reason}.")]
struct UnsupportedPanelSyntax {
    id: usize,
    reason: &'static str,
}
fn unsupported(id: usize, reason: &'static str) -> Error {
    let diagnostic = UnsupportedPanelSyntax { id, reason };
    Error::validation(diagnostic.to_string()).with_source(diagnostic)
}
/// Recognize only diagnostics minted by this admission boundary; caller text is never used.
pub(super) fn syntax_notice(error: &Error) -> Option<String> {
    use std::error::Error as _;
    if error.kind() != erp_fields::ErrorKind::Validation {
        return None;
    }
    error
        .source()?
        .downcast_ref::<UnsupportedPanelSyntax>()
        .map(ToString::to_string)
}
fn flag(node: crate::NodeRef<'_>, key: &str, id: usize) -> Result<()> {
    if let Some(value) = node.attr(key)
        && !matches!(value, "0" | "1" | "true" | "false" | "True" | "False")
    {
        return Err(invalid(
            id,
            "has unsupported visibility or boolean syntax; additional syntax requires a later release",
        ));
    }
    Ok(())
}
fn category(node: crate::NodeRef<'_>, id: usize) -> Result<bool> {
    match node.attr("select") {
        None | Some("one") => Ok(true),
        Some("multi") => Ok(false),
        _ => Err(invalid(
            id,
            "has an unsupported selection mode; additional syntax requires a later release",
        )),
    }
}
pub(super) fn validate(
    guarded: &GuardedArch,
    registry: &erp_registry::Registry,
    budget: &crate::DescriptionBudget,
) -> Result<()> {
    let arch = guarded.arch();
    let panels = arch
        .walk(arch.root_id())
        .filter(|(id, _)| arch.tag(*id) == "searchpanel")
        .count();
    if panels > 1 {
        return Err(invalid(0, "exceeds panel count limit"));
    }
    if panels == 0 {
        return Ok(());
    }
    let spec = registry
        .model(&guarded.table().root_model)
        .ok_or_else(|| invalid(0, "references an invalid root model"))?;
    for (panel_id, _) in arch
        .walk(arch.root_id())
        .filter(|(node, _)| arch.tag(*node) == "searchpanel")
    {
        let node = arch
            .node_ref(panel_id.to_idx())
            .ok_or_else(|| Error::programming("Panel node outside arch"))?;
        if node
            .attributes()
            .any(|(key, _)| !matches!(key, "view_types" | "invisible" | "groups"))
        {
            return Err(unsupported(
                0,
                "has unsupported panel syntax; additional syntax requires a later release",
            ));
        }
        flag(node, "invisible", 0)?;
        let applicable = || {
            node.attr("view_types")
                .unwrap_or("list,kanban")
                .split(',')
                .map(str::trim)
        };
        let count = applicable().count();
        if count == 0 || count > 64 {
            return Err(invalid(0, "exceeds view key count limit"));
        }
        for (i, key) in applicable().enumerate() {
            if key.len() > 129 || key.split('.').any(|part| part.len() > 64) {
                return Err(invalid(0, "exceeds view key byte limit"));
            }
            if !crate::is_valid_view_type_key(key) || applicable().take(i).any(|prior| prior == key)
            {
                return Err(invalid(0, "has invalid view applicability"));
            }
        }
        let legacy = node.children().count()==1 && matches!((applicable().next(),applicable().nth(1),applicable().nth(2)),(Some("list"),Some("kanban"),None)|(Some("kanban"),Some("list"),None))
            && node.children().next().is_some_and(|child| child.attr("select")!=Some("multi") && child.attr("icon").is_none() && child.attr("icon_class").is_none() && child.attr("depth").is_none()
                && !child.attr("name").and_then(|name|spec.field(name)).is_some_and(|field|matches!(field.ty,FieldType::Many2one {..}|FieldType::Many2many {..}) || matches!(&field.ty,FieldType::Selection {values:Some(values),..} if values.len()>32)));
        // The established legacy selection/unsupported producer owns its existing shape errors.
        if legacy {
            continue;
        }
        if node.children().count() > 64 {
            return Err(invalid(65, "exceeds section count limit"));
        }
        for (i, section) in node.children().enumerate() {
            let id = i + 1;
            budget
                .charge_work(1)
                .map_err(|e| Error::validation(e.to_string()))?;
            if section.tag() != "field" || section.children().next().is_some() {
                return Err(invalid(id, "requires a field declaration"));
            }
            if section.attributes().any(|(key, _)| {
                !matches!(
                    key,
                    "name"
                        | "string"
                        | "select"
                        | "icon"
                        | "enable_counters"
                        | "expand"
                        | "limit"
                        | "invisible"
                        | "hierarchize"
                        | "domain"
                        | "groupby"
                        | "groups"
                )
            }) {
                return Err(unsupported(
                    id,
                    "has unsupported section syntax; additional syntax requires a later release",
                ));
            }
            for key in ["enable_counters", "expand", "invisible", "hierarchize"] {
                flag(section, key, id)?;
            }
            let field = section
                .attr("name")
                .and_then(|name| spec.field(name))
                .ok_or_else(|| invalid(id, "references an invalid root field"))?;
            if !field.store {
                return Err(invalid(id, "requires a stored field"));
            }
            let comodel = match &field.ty {
                FieldType::Selection {
                    values: Some(values),
                    ..
                } if !values.is_empty() => {
                    let bytes = values
                        .len()
                        .checked_mul(std::mem::size_of::<&str>())
                        .ok_or_else(|| invalid(id, "exceeds selection storage limit"))?;
                    let mut storage = budget
                        .reserve(bytes)
                        .map_err(|e| Error::validation(e.to_string()))?;
                    let mut keys = Vec::new();
                    keys.try_reserve_exact(values.len())
                        .map_err(|_| invalid(id, "exceeds selection storage limit"))?;
                    if keys.capacity() > values.len() {
                        storage
                            .grow(
                                (keys.capacity() - values.len())
                                    .checked_mul(std::mem::size_of::<&str>())
                                    .ok_or_else(|| {
                                        invalid(id, "exceeds selection storage limit")
                                    })?,
                            )
                            .map_err(|e| Error::validation(e.to_string()))?;
                    }
                    for (key, _) in values {
                        if key.is_empty() || key.len() > 256 {
                            return Err(invalid(id, "has invalid selection keys"));
                        }
                        keys.push(key.as_str());
                    }
                    keys.sort_unstable();
                    if keys.windows(2).any(|pair| pair[0] == pair[1]) {
                        return Err(invalid(id, "has duplicate selection keys"));
                    }
                    None
                }
                FieldType::Many2one { comodel, .. } | FieldType::Many2many { comodel, .. } => {
                    Some(comodel.as_str())
                }
                _ => {
                    return Err(invalid(
                        id,
                        "has an unsupported field type; additional syntax requires a later release",
                    ));
                }
            };
            let label = section.attr("string").unwrap_or(&field.string);
            if label.len() > 256 {
                return Err(invalid(id, "exceeds label byte limit"));
            }
            if label.trim().is_empty() {
                return Err(invalid(id, "has an invalid label"));
            }
            if section.attr("icon").is_some_and(|icon| icon.len() > 64) {
                return Err(invalid(id, "exceeds icon byte limit"));
            }
            if let Some(icon) = section.attr("icon")
                && (icon.is_empty()
                    || !icon.as_bytes()[0].is_ascii_alphabetic()
                    || !icon
                        .bytes()
                        .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'-')))
            {
                return Err(invalid(id, "has an invalid icon"));
            }
            let limit = section
                .attr("limit")
                .unwrap_or("200")
                .trim()
                .parse::<u64>()
                .map_err(|_| invalid(id, "has an invalid display limit"))?;
            if limit > 9_007_199_254_740_991 {
                return Err(invalid(id, "exceeds display limit"));
            }
            let is_category = category(section, id)?;
            if is_category
                && node.children().take(i).any(|prior| {
                    prior.attr("name") == section.attr("name")
                        && matches!(prior.attr("select"), None | Some("one"))
                })
            {
                return Err(invalid(id, "has an ambiguous category binding"));
            }
            if is_category
                && (section.attr("domain").is_some() || section.attr("groupby").is_some())
            {
                return Err(invalid(
                    id,
                    "has unsupported category syntax; additional syntax requires a later release",
                ));
            }
            if !is_category && section.attr("hierarchize").is_some() {
                return Err(invalid(
                    id,
                    "has unsupported filter hierarchy syntax; additional syntax requires a later release",
                ));
            }
            if let Some(group) = section.attr("groupby") {
                let field = comodel
                    .and_then(|name| registry.model(name))
                    .and_then(|model| model.field(group))
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
            if section
                .attr("domain")
                .is_some_and(|source| !source.is_empty() && source.trim().is_empty())
            {
                return Err(invalid(id, "has an invalid domain"));
            }
            if let Some(source) = section
                .attr("domain")
                .filter(|source| !source.trim().is_empty() && source.trim() != "[]")
            {
                if comodel.is_none() {
                    return Err(invalid(id, "requires a relational domain field"));
                }
                let limits = erp_expr::EvalLimits::default();
                let bound = erp_expr::source_preflight(source, limits)
                    .map_err(|_| invalid(id, "has an invalid domain"))?;
                let _storage = budget
                    .reserve(bound.reserve_bytes())
                    .map_err(|error| Error::validation(error.to_string()))?;
                let expr = erp_expr::parse_with_limits(source, limits)
                    .map_err(|_| invalid(id, "has an invalid domain"))?;
                let admitted = erp_expr::wire::ValidatedAst::from_expr(&expr)
                    .map_err(|_| invalid(id, "has an invalid domain"))?;
                budget
                    .charge_expression_bytes(admitted.encoded_len())
                    .map_err(|error| Error::validation(error.to_string()))?;
                crate::validate_panel_domain(&admitted)
                    .map_err(|_| invalid(id, "has an unsupported domain syntax; additional syntax requires a later release"))?;
                crate::expression_references::walk_scoped_domain(
                    admitted.as_expr(),
                    &mut crate::expression_references::BudgetMeter(budget),
                    |found| {
                        if !found.criterion {
                            let Some(name) = found.parts.first().copied() else {
                                return Err(crate::ViewError::InvalidDescription(
                                    "Empty panel category".into(),
                                ));
                            };
                            if found.parts.len() != 1
                                || !node.children().any(|candidate| {
                                    candidate.attr("name") == Some(name)
                                        && matches!(candidate.attr("select"), None | Some("one"))
                                })
                            {
                                return Err(crate::ViewError::InvalidDescription(
                                    "Undeclared panel category".into(),
                                ));
                            }
                        }
                        Ok(())
                    },
                )
                .map_err(|error| match error {
                    crate::ViewError::LimitExceeded { .. } => Error::validation(error.to_string()),
                    _ => invalid(id, "has an undeclared category dependency"),
                })?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn notice_recognition_does_not_swallow_unrelated_failures() {
        let error = unsupported(
            2,
            "has unsupported section syntax; additional syntax requires a later release",
        );
        assert_eq!(error.kind(), erp_fields::ErrorKind::Validation);
        assert_eq!(syntax_notice(&error).as_deref(), Some(error.message()));
        assert!(syntax_notice(&Error::validation(error.message())).is_none());
        for other in [
            Error::access("denied"),
            Error::db("failed"),
            Error::invariant("broken"),
            invalid(2, "exceeds section count limit"),
            invalid(2, "references an invalid root field"),
        ] {
            assert!(syntax_notice(&other).is_none());
        }
    }
}
