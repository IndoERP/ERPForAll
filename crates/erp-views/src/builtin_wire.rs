//! A typed, budgeted adapter from closed builtin payloads to the existing strict client wire.
//! No JSON tree or reparsing is needed, and optional schema members regain their wire nulls.
use super::{DescriptionPayload, DescriptionValue, Kind, propagate, refuse};
use crate::{DescriptionBudget, DescriptionReservation, ViewError, ViewResult, wire};
use std::mem::size_of;

/// A builtin wire view together with the request storage that owns every copied buffer.
#[derive(Debug)]
#[non_exhaustive]
pub struct AdmittedBuiltinView {
    view: wire::View,
    reservation: DescriptionReservation,
}
impl AdmittedBuiltinView {
    /// Borrow the admitted view. Presence is not an authorization witness; the host must check
    /// the payload's independently derived field references before returning it to a caller.
    pub fn view(&self) -> &wire::View {
        &self.view
    }
    /// Transfer the view and its storage ownership to the response. The response must retain
    /// the reservation for as long as it retains the view, including serialization.
    pub fn into_parts(self) -> (wire::View, DescriptionReservation) {
        (self.view, self.reservation)
    }
}

impl DescriptionPayload {
    /// Adapt a list/form/search payload to the established builtin wire without a JSON copy.
    ///
    /// Every string, vector and expression copy is reserved before allocation under the same
    /// budget that constructed this payload. The returned owner retains that reservation.
    /// Optional modifiers and confirmation members become the explicit nulls required by wire v1.
    ///
    /// # Errors
    /// A different budget/schema, unsupported builtin vocabulary, invalid arena indexes, cycles,
    /// repeated or unreachable nodes, excessive graph depth, allocation failure or exhausted budget.
    /// This validates structure, not the caller's permission to read the referenced fields.
    pub fn builtin_view(
        &self,
        identity: wire::ViewIdentity,
        kind: wire::ViewType,
        renderer: &str,
        budget: &DescriptionBudget,
    ) -> ViewResult<AdmittedBuiltinView> {
        let result = (|| {
            budget.check()?;
            if !self.value.budget.same_budget(budget) {
                return Err(refuse(
                    budget,
                    "Builtin view belongs to a different construction budget",
                ));
            }
            let root_schema = crate::builtin_schemas::builtin_root(kind).ok_or_else(|| {
                refuse(
                    budget,
                    "Builtin wire adapter supports only list, form, search and pivot",
                )
            })?;
            if !std::ptr::eq(self.schema.root_node(), root_schema) {
                return Err(refuse(
                    budget,
                    "Builtin view payload does not use the registered builtin schema",
                ));
            }
            let expected_renderer = match kind {
                wire::ViewType::List => "web.list_sections_v1",
                wire::ViewType::Form => "web.form",
                wire::ViewType::Search => "web.search_sections_v1",
                wire::ViewType::Pivot => "web.pivot",
                _ => {
                    return Err(refuse(
                        budget,
                        "Builtin wire adapter supports only list, form, search and pivot; later types require a later release",
                    ));
                }
            };
            let legacy_renderer = matches!(
                (kind, renderer),
                (wire::ViewType::List, "web.list") | (wire::ViewType::Search, "web.search")
            );
            if renderer != expected_renderer && !legacy_renderer {
                return Err(refuse(
                    budget,
                    "Builtin view renderer does not match its declared view type",
                ));
            }
            let object = members(&self.value)?;
            let root = unsigned(required(object, "root")?)?;
            let source = array(required(object, "nodes")?)?;
            let mut output = Output {
                reservation: budget.reserve(0)?,
            };
            let renderer = output.text(renderer)?;
            let mut nodes = output.vector(source.len())?;
            for node in source {
                nodes.push(output.node(node)?);
            }
            let expected = match kind {
                wire::ViewType::List => "list",
                wire::ViewType::Form => "form",
                wire::ViewType::Search => "search",
                wire::ViewType::Pivot => "pivot",
                _ => {
                    return Err(refuse(
                        budget,
                        "Builtin wire adapter supports only list, form, search and pivot; later types require a later release",
                    ));
                }
            };
            let root_source = source
                .get(root as usize)
                .ok_or_else(|| invalid("Builtin view root is outside its node arena"))?;
            if variant(root_source)?.0 != expected {
                return Err(invalid(
                    "Builtin view root does not match its declared view type",
                ));
            }
            if legacy_renderer
                && nodes.iter().any(|node| {
                    matches!(
                        node,
                        wire::ViewNode::List {
                            search_panel: Some(wire::ListSearchPanel::Sections { .. }),
                            ..
                        } | wire::ViewNode::Search {
                            search_panel: Some(wire::ListSearchPanel::Sections { .. }),
                            ..
                        }
                    )
                })
            {
                return Err(refuse(
                    budget,
                    "Section descriptions require a versioned section renderer",
                ));
            }
            validate_graph(root, &nodes, budget)?;
            budget.check()?;
            Ok(AdmittedBuiltinView {
                view: wire::View::new(identity, kind, renderer, root, nodes),
                reservation: output.reservation,
            })
        })();
        propagate(budget, result)
    }
}

type Members<'a> = &'a [(&'static str, DescriptionValue)];
fn invalid(message: &str) -> ViewError {
    ViewError::InvalidDescription(message.to_owned())
}
fn members(value: &DescriptionValue) -> ViewResult<Members<'_>> {
    match &value.kind {
        Kind::Object { values, .. } => Ok(values),
        _ => Err(invalid("Builtin view requires an object")),
    }
}
fn optional<'a>(object: Members<'a>, key: &str) -> Option<&'a DescriptionValue> {
    object
        .iter()
        .find(|(name, _)| *name == key)
        .map(|(_, value)| value)
}
fn required<'a>(object: Members<'a>, key: &str) -> ViewResult<&'a DescriptionValue> {
    optional(object, key).ok_or_else(|| invalid("Builtin view is missing a required member"))
}
fn text(value: &DescriptionValue) -> ViewResult<&str> {
    match &value.kind {
        Kind::Text { value, .. } => Ok(value),
        _ => Err(invalid("Builtin view requires text")),
    }
}
fn integer(value: &DescriptionValue) -> ViewResult<i64> {
    match value.kind {
        Kind::Integer(value) => Ok(value),
        _ => Err(invalid("Builtin view requires an integer")),
    }
}
/// Inspect borrowed construction intermediates before any panel member is copied.
fn panel_ceilings(sections: &[DescriptionValue], views: &[DescriptionValue]) -> ViewResult<()> {
    if sections.len() > 64 {
        return Err(invalid(
            "Search panel section 65 exceeds section count limit.",
        ));
    }
    if views.len() > 64 {
        return Err(invalid(
            "Search panel section 0 exceeds view key count limit.",
        ));
    }
    for view in views {
        let key = text(view)?;
        if key.split('.').any(|component| component.len() > 64) {
            return Err(invalid(
                "Search panel section 0 exceeds view key byte limit.",
            ));
        }
        if !crate::is_valid_view_type_key(key) {
            return Err(invalid("Search panel section 0 has an invalid view key."));
        }
    }
    for section in sections {
        let (_, source) = variant(section)?;
        let id = unsigned(required(source, "id")?)?;
        if text(required(source, "label")?)?.len() > 256 {
            return Err(invalid(&format!(
                "Search panel section {id} exceeds label byte limit."
            )));
        }
        if let Some(icon) = optional(source, "icon")
            && text(icon)?.len() > 64
        {
            return Err(invalid(&format!(
                "Search panel section {id} exceeds icon byte limit."
            )));
        }
    }
    Ok(())
}

fn unsigned(value: &DescriptionValue) -> ViewResult<u32> {
    u32::try_from(integer(value)?)
        .map_err(|_| invalid("Builtin view index is outside the u32 range"))
}
fn boolean(value: &DescriptionValue) -> ViewResult<bool> {
    match value.kind {
        Kind::Bool(value) => Ok(value),
        _ => Err(invalid("Builtin view requires a boolean")),
    }
}
fn array(value: &DescriptionValue) -> ViewResult<&[DescriptionValue]> {
    match &value.kind {
        Kind::Array { values, .. } => Ok(values),
        _ => Err(invalid("Builtin view requires an array")),
    }
}
fn variant(value: &DescriptionValue) -> ViewResult<(&str, Members<'_>)> {
    match &value.kind {
        Kind::Variant { tag, branch, .. } => Ok((tag, members(branch)?)),
        _ => Err(invalid("Builtin view requires a tagged node")),
    }
}

struct Output {
    reservation: DescriptionReservation,
}
impl Output {
    fn vector<T>(&mut self, count: usize) -> ViewResult<Vec<T>> {
        let bytes = count
            .checked_mul(size_of::<T>())
            .ok_or_else(|| invalid("Builtin view storage overflow"))?;
        self.reservation.grow(bytes)?;
        let mut values = Vec::new();
        values
            .try_reserve_exact(count)
            .map_err(|_| invalid("Builtin view allocation failed"))?;
        if values.capacity() > count {
            self.reservation.grow(
                (values.capacity() - count)
                    .checked_mul(size_of::<T>())
                    .ok_or_else(|| invalid("Builtin view storage overflow"))?,
            )?;
        }
        Ok(values)
    }
    fn text(&mut self, source: &str) -> ViewResult<String> {
        self.reservation.grow(source.len())?;
        let mut value = String::new();
        value
            .try_reserve_exact(source.len())
            .map_err(|_| invalid("Builtin view text allocation failed"))?;
        if value.capacity() > source.len() {
            self.reservation.grow(value.capacity() - source.len())?;
        }
        value.push_str(source);
        Ok(value)
    }
    fn string(&mut self, object: Members<'_>, key: &str) -> ViewResult<String> {
        self.text(text(required(object, key)?)?)
    }
    fn optional_text(&mut self, object: Members<'_>, key: &str) -> ViewResult<Option<String>> {
        optional(object, key)
            .map(|value| self.text(text(value)?))
            .transpose()
    }
    fn indices(&mut self, object: Members<'_>) -> ViewResult<Vec<u32>> {
        let source = array(required(object, "children")?)?;
        let mut values = self.vector(source.len())?;
        for value in source {
            values.push(unsigned(value)?);
        }
        Ok(values)
    }
    fn field(&mut self, value: &DescriptionValue) -> ViewResult<wire::FieldRef> {
        let Kind::FieldHandle { model, path, .. } = &value.kind else {
            return Err(invalid("Builtin view requires a scoped field handle"));
        };
        Ok(wire::FieldRef::new(self.text(model)?, self.text(path)?))
    }
    fn expression(&mut self, value: &DescriptionValue) -> ViewResult<erp_expr::wire::WireDocument> {
        let Kind::Expression {
            document, retained, ..
        } = &value.kind
        else {
            return Err(invalid("Builtin view requires an admitted expression"));
        };
        // WireDocument is a flat owned arena. Its derived String/Vec clones allocate only their
        // lengths; the already-accounted source capacities bound all clone buffers together.
        self.reservation.grow(*retained)?;
        Ok(document.clone())
    }
    fn optional_expression(
        &mut self,
        object: Members<'_>,
        key: &str,
    ) -> ViewResult<Option<erp_expr::wire::WireDocument>> {
        optional(object, key)
            .map(|value| self.expression(value))
            .transpose()
    }
    fn modifiers(&mut self, object: Members<'_>) -> ViewResult<wire::Modifiers> {
        let source = members(required(object, "modifiers")?)?;
        let mut result = wire::Modifiers::empty();
        result.readonly = self.optional_expression(source, "readonly")?;
        result.required = self.optional_expression(source, "required")?;
        result.invisible = self.optional_expression(source, "invisible")?;
        Ok(result)
    }
    fn strings(&mut self, value: &DescriptionValue) -> ViewResult<Vec<String>> {
        let source = array(value)?;
        let mut values = self.vector(source.len())?;
        for value in source {
            values.push(self.text(text(value)?)?);
        }
        Ok(values)
    }
    fn presentation(&mut self, source: Members<'_>) -> ViewResult<wire::FieldPresentationOptions> {
        let mut result = wire::FieldPresentationOptions::empty();
        result.help = self.optional_text(source, "help")?;
        result.placeholder = self.optional_text(source, "placeholder")?;
        result.autocomplete = self.optional_text(source, "autocomplete")?;
        result.icon = self.optional_text(source, "icon")?;
        result.icon_class = self.optional_text(source, "icon_class")?;
        result.min_date = self.optional_text(source, "min_date")?;
        result.max_date = self.optional_text(source, "max_date")?;
        result.min_precision = self.optional_text(source, "min_precision")?;
        result.max_precision = self.optional_text(source, "max_precision")?;
        result.warn = self.optional_text(source, "warn")?;
        result.r#type = self.optional_text(source, "type")?;
        result.text = self.optional_text(source, "text")?;
        result.password = optional(source, "password").map(boolean).transpose()?;
        result.nolabel = optional(source, "nolabel").map(boolean).transpose()?;
        result.no_open = optional(source, "no_open").map(boolean).transpose()?;
        result.no_create = optional(source, "no_create").map(boolean).transpose()?;
        result.no_quick_create = optional(source, "no_quick_create")
            .map(boolean)
            .transpose()?;
        result.no_create_edit = optional(source, "no_create_edit")
            .map(boolean)
            .transpose()?;
        result.no_symbol = optional(source, "no_symbol").map(boolean).transpose()?;
        result.enable_formatting = optional(source, "enable_formatting")
            .map(boolean)
            .transpose()?;
        result.hide_trailing_zeros = optional(source, "hide_trailing_zeros")
            .map(boolean)
            .transpose()?;
        result.field_digits = optional(source, "field_digits").map(boolean).transpose()?;
        result.show_time = optional(source, "show_time").map(boolean).transpose()?;
        result.show_seconds = optional(source, "show_seconds").map(boolean).transpose()?;
        result.always_range = optional(source, "always_range").map(boolean).transpose()?;
        result.numeric = optional(source, "numeric").map(boolean).transpose()?;
        result.create = optional(source, "create").map(boolean).transpose()?;
        result.can_open = optional(source, "can_open").map(boolean).transpose()?;
        result.horizontal = optional(source, "horizontal").map(boolean).transpose()?;
        result.rows = optional(source, "rows").map(integer).transpose()?;
        result.max_length = optional(source, "max_length").map(integer).transpose()?;
        result.badge_limit = optional(source, "badge_limit").map(integer).transpose()?;
        result.tag_limit = optional(source, "tag_limit").map(integer).transpose()?;
        result.search_threshold = optional(source, "search_threshold")
            .map(integer)
            .transpose()?;
        result.rounding = optional(source, "rounding").map(integer).transpose()?;
        result.placeholder_field = optional(source, "placeholder_field")
            .map(|value| self.field(value))
            .transpose()?;
        result.currency_field = optional(source, "currency_field")
            .map(|value| self.field(value))
            .transpose()?;
        result.start_date_field = optional(source, "start_date_field")
            .map(|value| self.field(value))
            .transpose()?;
        result.end_date_field = optional(source, "end_date_field")
            .map(|value| self.field(value))
            .transpose()?;
        result.whitelist_fname = optional(source, "whitelist_fname")
            .map(|value| self.field(value))
            .transpose()?;
        result.color_field = optional(source, "color_field")
            .map(|value| self.field(value))
            .transpose()?;
        result.whitelisted_values = optional(source, "whitelisted_values")
            .map(|value| self.strings(value))
            .transpose()?;
        result.blacklisted_values = optional(source, "blacklisted_values")
            .map(|value| self.strings(value))
            .transpose()?;
        if let Some(value) = optional(source, "digits") {
            let source = array(value)?;
            let mut values = self.vector(source.len())?;
            for value in source {
                values.push(integer(value)?);
            }
            result.digits = Some(values);
        }
        result.domain = self.optional_expression(source, "domain")?;
        result.context = self.optional_expression(source, "context")?;
        for value in [
            result.rows,
            result.max_length,
            result.badge_limit,
            result.tag_limit,
            result.search_threshold,
            result.rounding,
        ]
        .into_iter()
        .flatten()
        {
            if !(0..=4096).contains(&value) {
                return Err(invalid("Builtin field integer option exceeds 0..4096"));
            }
        }
        if result.rows.is_some_and(|value| !(1..=100).contains(&value))
            || result
                .badge_limit
                .is_some_and(|value| !(1..=256).contains(&value))
            || result
                .tag_limit
                .is_some_and(|value| !(1..=256).contains(&value))
        {
            return Err(invalid("Builtin field presentation limit is invalid"));
        }
        if let Some(digits) = &result.digits
            && (digits.len() != 2
                || digits.iter().any(|value| !(0..=20).contains(value))
                || digits[1] > digits[0])
        {
            return Err(invalid("Builtin field precision is invalid"));
        }
        Ok(result)
    }
    fn panel_section(&mut self, value: &DescriptionValue) -> ViewResult<wire::PanelSection> {
        let (kind, source) = variant(value)?;
        let id = unsigned(required(source, "id")?)?;
        let id = std::num::NonZeroU32::new(id)
            .ok_or_else(|| invalid("Search panel section 0 has an invalid identity."))?;
        let limit = u64::try_from(integer(required(source, "limit")?)?).map_err(|_| {
            invalid(&format!(
                "Search panel section {id} has an invalid display limit."
            ))
        })?;
        let limit = wire::PanelDisplayLimit::try_from(limit)
            .map_err(|_| invalid(&format!("Search panel section {id} exceeds display limit.")))?;
        let field = self.field(required(source, "field")?)?;
        let label = self.string(source, "label")?;
        let icon = self.optional_text(source, "icon")?;
        let enable_counters = boolean(required(source, "enable_counters")?)?;
        let expand = boolean(required(source, "expand")?)?;
        match kind {
            "category" => Ok(wire::PanelSection::Category {
                id,
                field,
                label,
                icon,
                enable_counters,
                expand,
                limit,
                default_values: wire::PanelDefaults::default(),
                hierarchize: boolean(required(source, "hierarchize")?)?,
            }),
            "filter" => {
                let domain = match optional(source, "domain") {
                    None => None,
                    Some(value) => {
                        let Kind::ScopedDomain {
                            model,
                            document,
                            retained,
                            ..
                        } = &value.kind
                        else {
                            return Err(invalid("Builtin panel requires a scoped domain"));
                        };
                        self.reservation.grow(*retained)?;
                        Some(wire::PanelDomain::new(self.text(model)?, document.clone()))
                    }
                };
                let group_by = optional(source, "group_by")
                    .map(|value| self.field(value))
                    .transpose()?;
                Ok(wire::PanelSection::Filter {
                    id,
                    field,
                    label,
                    icon,
                    enable_counters,
                    expand,
                    limit,
                    default_values: wire::PanelDefaults::default(),
                    domain,
                    group_by,
                })
            }
            _ => Err(invalid("Builtin panel has an unknown section kind")),
        }
    }
    fn panel(&mut self, object: Members<'_>) -> ViewResult<Option<wire::ListSearchPanel>> {
        let Some(value) = optional(object, "search_panel") else {
            return Ok(None);
        };
        let (kind, object) = variant(value)?;
        Ok(Some(match kind {
            "sections" => {
                let version = u8::try_from(integer(required(object, "version")?)?)
                    .map_err(|_| invalid("unsupported search panel version"))?;
                let version = wire::PanelVersion::try_from(version).map_err(invalid)?;
                let view_source = required(object, "view_types")?;
                let source = array(required(object, "sections")?)?;
                panel_ceilings(source, array(view_source)?)?;
                let view_types = self.strings(view_source)?;
                let mut sections = self.vector(source.len())?;
                for value in source {
                    sections.push(self.panel_section(value)?);
                }
                wire::ListSearchPanel::Sections {
                    version,
                    view_types,
                    sections,
                    notice: self.optional_text(object, "notice")?,
                }
            }
            "unsupported" => wire::ListSearchPanel::Unsupported {
                message: self.string(object, "message")?,
            },
            "selection" => wire::ListSearchPanel::Selection {
                field: self.field(required(object, "field")?)?,
                label: self.string(object, "label")?,
                enable_counters: boolean(required(object, "enable_counters")?)?,
                expand: boolean(required(object, "expand")?)?,
                default_value: self.optional_text(object, "default_value")?,
                notice: self.optional_text(object, "notice")?,
            },
            _ => return Err(invalid("Builtin view has an unknown search panel kind")),
        }))
    }
    fn button_details(&mut self, object: Members<'_>) -> ViewResult<Option<wire::ButtonDetails>> {
        let Some(value) = optional(object, "details") else {
            return Ok(None);
        };
        let source = members(value)?;
        let size = match text(required(source, "size")?)? {
            "small" => wire::ButtonSize::Small,
            "normal" => wire::ButtonSize::Normal,
            "large" => wire::ButtonSize::Large,
            _ => return Err(invalid("Builtin button size is invalid")),
        };
        let classes_source = array(required(source, "classes")?)?;
        let mut class_bytes = classes_source.len().saturating_sub(1);
        for (index, token) in classes_source.iter().enumerate() {
            let token = text(token)?;
            class_bytes = class_bytes
                .checked_add(token.len())
                .ok_or_else(|| invalid("Builtin button classes are invalid"))?;
            if !wire::valid_button_class_token(token)
                || classes_source[..index].iter().any(
                    |previous| matches!(&previous.kind, Kind::Text { value, .. } if value == token),
                )
            {
                return Err(invalid("Builtin button classes are invalid"));
            }
        }
        if classes_source.len() > 8 || class_bytes > 512 {
            return Err(invalid("Builtin button classes are invalid"));
        }
        let debounce_ms = u32::try_from(integer(required(source, "debounce_ms")?)?)
            .map_err(|_| invalid("Builtin button debounce is invalid"))?;
        if debounce_ms > 60000 {
            return Err(invalid("Builtin button debounce is invalid"));
        }
        let hotkey = self.optional_text(source, "hotkey")?;
        if hotkey
            .as_ref()
            .is_some_and(|key| !wire::valid_button_hotkey(key))
        {
            return Err(invalid("Builtin button hotkey is invalid"));
        }
        let icon = self.optional_text(source, "icon")?;
        if icon.as_ref().is_some_and(|icon| !valid_icon(icon)) {
            return Err(invalid("Builtin button icon is invalid"));
        }
        let details = wire::ButtonDetails {
            size,
            circle: boolean(required(source, "circle")?)?,
            classes: self.strings(required(source, "classes")?)?,
            debounce_ms,
            block_ui: boolean(required(source, "block_ui")?)?,
            icon,
            hotkey,
            title: self.optional_text(source, "title")?,
            help: self.optional_text(source, "help")?,
            confirm_title: self.optional_text(source, "confirm_title")?,
            confirm_label: self.optional_text(source, "confirm_label")?,
            cancel_label: self.optional_text(source, "cancel_label")?,
            context: self.optional_expression(source, "context")?,
        };
        Ok(Some(details))
    }
    fn button_target(&mut self, value: &DescriptionValue) -> ViewResult<wire::ButtonTarget> {
        let (kind, source) = variant(value)?;
        Ok(match kind {
            "action" => wire::ButtonTarget::Action {
                id: wire::WireId::try_from(
                    u64::try_from(integer(required(source, "id")?)?)
                        .map_err(|_| invalid("Builtin button action identity is invalid"))?,
                )
                .map_err(invalid)?,
            },
            "url" => {
                let href = text(required(source, "href")?)?;
                if !wire::valid_button_url(href) {
                    return Err(invalid("Builtin button URL is invalid"));
                }
                wire::ButtonTarget::Url {
                    href: self.text(href)?,
                }
            }
            "inert" => wire::ButtonTarget::Inert,
            _ => return Err(invalid("Builtin button target is invalid")),
        })
    }
    fn dashboard(&mut self, object: Members<'_>) -> ViewResult<wire::ViewNode> {
        if integer(required(object, "version")?)? != 1 {
            return Err(invalid("Unsupported dashboard version"));
        }
        let revision = text(required(object, "revision")?)?;
        if revision.len() != 64
            || !revision
                .bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
        {
            return Err(invalid("Dashboard revision must be lowercase SHA-256 text"));
        }
        let source = array(required(object, "items")?)?;
        if source.is_empty() || source.len() > 32 {
            return Err(invalid("Dashboard item count must be between 1 and 32"));
        }
        let mut items = self.vector(source.len())?;
        for (index, item) in source.iter().enumerate() {
            let (kind, item) = variant(item)?;
            let name = text(required(item, "name")?)?;
            if name.is_empty() || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') {
                return Err(invalid(
                    "Dashboard item name must contain letters, digits or underscores",
                ));
            }
            for previous in &source[..index] {
                if text(required(variant(previous)?.1, "name")?)? == name {
                    return Err(invalid("Duplicate dashboard item name"));
                }
            }
            let name = self.text(name)?;
            let label = self.text(text(required(item, "label")?)?)?;
            if label.trim().is_empty() {
                return Err(invalid("Dashboard item label must be nonempty"));
            }
            let domain = optional(item, "domain")
                .map(|value| self.expression(value))
                .transpose()?;
            items.push(match kind {
                "count" => {
                    let filter = optional(item, "filter")
                        .map(|v| self.text(text(v)?))
                        .transpose()?;
                    if domain.is_some() == filter.is_some()
                        || filter.as_ref().is_some_and(|f| f.trim().is_empty())
                    {
                        return Err(invalid(
                            "Dashboard count requires exactly one domain or named filter",
                        ));
                    }
                    let color = optional(item, "color")
                        .map(|v| self.text(text(v)?))
                        .transpose()?;
                    if color
                        .as_deref()
                        .is_some_and(|v| !matches!(v, "info" | "success" | "warning" | "danger"))
                    {
                        return Err(invalid("Dashboard count color is invalid"));
                    }
                    wire::DashboardItem::Count {
                        name,
                        label,
                        domain,
                        filter,
                        color,
                    }
                }
                "kpi" => {
                    let aggregate = text(required(item, "aggregate")?)?;
                    if !matches!(aggregate, "sum" | "avg" | "min" | "max") {
                        return Err(invalid("Dashboard aggregate is invalid"));
                    }
                    wire::DashboardItem::Kpi {
                        name,
                        label,
                        domain,
                        aggregate: self.text(aggregate)?,
                        field: self.field(required(item, "field")?)?,
                        date_field: self.field(required(item, "date_field")?)?,
                    }
                }
                _ => return Err(invalid("Unknown dashboard item kind")),
            });
        }
        Ok(wire::ViewNode::Dashboard {
            version: 1,
            revision: self.text(text(required(object, "revision")?)?)?,
            items,
        })
    }
    fn node(&mut self, value: &DescriptionValue) -> ViewResult<wire::ViewNode> {
        let (kind, object) = variant(value)?;
        Ok(match kind {
            "dashboard" => self.dashboard(object)?,
            "list" => wire::ViewNode::List {
                children: self.indices(object)?,
                search_panel: self.panel(object)?,
                page_limit: optional(object, "page_limit")
                    .map(|value| {
                        let value = u64::try_from(integer(value)?)
                            .map_err(|_| invalid("Builtin list page limit is invalid"))?;
                        wire::WirePageLimit::try_from(value).map_err(invalid)
                    })
                    .transpose()?,
                create: optional(object, "create").map(boolean).transpose()?,
                edit: optional(object, "edit").map(boolean).transpose()?,
                delete: optional(object, "delete").map(boolean).transpose()?,
            },
            "form" => wire::ViewNode::Form {
                record_mode: match text(required(object, "record_mode")?)? {
                    "record" => wire::RecordMode::Record,
                    "service" => wire::RecordMode::Service,
                    _ => return Err(invalid("Builtin form record mode is invalid")),
                },
                children: self.indices(object)?,
                create: optional(object, "create").map(boolean).transpose()?,
                edit: optional(object, "edit").map(boolean).transpose()?,
                delete: optional(object, "delete").map(boolean).transpose()?,
            },
            "sheet" => wire::ViewNode::Sheet {
                children: self.indices(object)?,
            },
            "title" => wire::ViewNode::Title {
                children: self.indices(object)?,
            },
            "heading" => wire::ViewNode::Heading {
                level: match integer(required(object, "level")?)? {
                    1 => 1,
                    2 => 2,
                    _ => return Err(invalid("Builtin heading level is invalid")),
                },
                children: self.indices(object)?,
            },
            "aside" => wire::ViewNode::Aside {
                children: self.indices(object)?,
            },
            "chatter" => wire::ViewNode::Chatter {
                reload_on_post: boolean(required(object, "reload_on_post")?)?,
                reload_on_attachment: boolean(required(object, "reload_on_attachment")?)?,
                reload_on_activity: boolean(required(object, "reload_on_activity")?)?,
                reload_on_follower: boolean(required(object, "reload_on_follower")?)?,
                open_attachments: boolean(required(object, "open_attachments")?)?,
                has_activities: boolean(required(object, "has_activities")?)?,
            },
            "header" => wire::ViewNode::Header {
                children: self.indices(object)?,
            },
            "footer" => wire::ViewNode::Footer {
                children: self.indices(object)?,
            },
            "button_box" => wire::ViewNode::ButtonBox {
                children: self.indices(object)?,
            },
            "notebook" => wire::ViewNode::Notebook {
                children: self.indices(object)?,
            },
            "group" => wire::ViewNode::Group {
                label: self.string(object, "label")?,
                columns: tracks(required(object, "columns")?)?,
                colspan: tracks(required(object, "colspan")?)?,
                children: self.indices(object)?,
            },
            "page" => wire::ViewNode::Page {
                name: self.string(object, "name")?,
                label: self.string(object, "label")?,
                children: self.indices(object)?,
            },
            "separator" => wire::ViewNode::Separator {
                label: self.string(object, "label")?,
            },
            "field" => {
                let widget = self.string(object, "widget")?;
                let options = members(required(object, "options")?)?;
                let visible = optional(options, "visible")
                    .map(|value| self.strings(value))
                    .transpose()?;
                let mode = optional(options, "mode").map(text).transpose()?;
                if mode.is_some() && widget != "code" {
                    return Err(invalid("Builtin field mode options require code"));
                }
                for (key, _) in options {
                    // Code fields admit only their mode; visibility is refused below
                    // with the statusbar message.
                    if widget == "code" {
                        if *key != "mode" && *key != "visible" {
                            return Err(invalid("Builtin code field accepts its mode option only"));
                        }
                        continue;
                    }
                    if widget == "statusbar" && *key != "visible" {
                        return Err(invalid("Builtin statusbar accepts visibility options only"));
                    }
                    if !(widget == "statusbar" && *key == "visible")
                        && wire::field_option_kind(&widget, key).is_none()
                    {
                        return Err(invalid(
                            "Builtin field option is not supported by its widget",
                        ));
                    }
                }
                let options = if widget == "code" {
                    if visible.is_some() {
                        return Err(invalid(
                            "Builtin field visibility options require statusbar",
                        ));
                    }
                    let source = mode
                        .ok_or_else(|| invalid("Builtin code field requires an explicit mode"))?;
                    let mode = wire::CodeMode::from_wire_str(source).ok_or_else(|| {
                        invalid(&format!("Unsupported builtin code field mode '{source}'"))
                    })?;
                    wire::FieldOptions::Code(wire::CodeOptions::new(mode))
                } else if widget == "statusbar" {
                    wire::FieldOptions::Statusbar(wire::StatusbarOptions::new(visible))
                } else if visible.is_some() {
                    return Err(invalid(
                        "Builtin field visibility options require statusbar",
                    ));
                } else if options.is_empty() {
                    wire::FieldOptions::Empty(wire::EmptyOptions::new())
                } else {
                    self.reservation
                        .grow(size_of::<wire::FieldPresentationOptions>())?;
                    wire::FieldOptions::Presentation(Box::new(self.presentation(options)?))
                };
                wire::ViewNode::Field {
                    field: self.field(required(object, "field")?)?,
                    widget,
                    options,
                    modifiers: self.modifiers(object)?,
                    label: self.string(object, "label")?,
                    force_save: boolean(required(object, "force_save")?)?,
                }
            }
            "button" | "stat_button" => {
                if text(required(object, "button_type")?)? != "object" {
                    return Err(invalid("Builtin button type is invalid"));
                }
                let name = self.string(object, "name")?;
                let label = self.string(object, "label")?;
                let variant = button_variant(required(object, "variant")?)?;
                let confirm = self.optional_text(object, "confirm")?;
                let modifiers = self.modifiers(object)?;
                let details = self.button_details(object)?;
                validate_button_content(&label, confirm.as_deref(), details.as_ref())?;
                if !valid_method(&name) {
                    return Err(invalid("Builtin button method is invalid"));
                }
                if kind == "stat_button" {
                    wire::ViewNode::StatButton {
                        name,
                        button_type: wire::ButtonType::Object,
                        label,
                        variant,
                        confirm,
                        modifiers,
                        details,
                        children: self.indices(object)?,
                    }
                } else {
                    wire::ViewNode::Button {
                        name,
                        button_type: wire::ButtonType::Object,
                        label,
                        variant,
                        confirm,
                        modifiers,
                        details,
                    }
                }
            }
            "action_button" => {
                let label = self.string(object, "label")?;
                let confirm = self.optional_text(object, "confirm")?;
                let details = self.button_details(object)?;
                validate_button_content(&label, confirm.as_deref(), details.as_ref())?;
                wire::ViewNode::ActionButton {
                    target: self.button_target(required(object, "target")?)?,
                    label,
                    variant: button_variant(required(object, "variant")?)?,
                    confirm,
                    modifiers: self.modifiers(object)?,
                    details,
                }
            }
            "ribbon" => {
                let text = self.string(object, "text")?;
                if text.trim().is_empty() {
                    return Err(invalid("Builtin ribbon text is empty"));
                }
                wire::ViewNode::Ribbon {
                    text,
                    tooltip: self.optional_text(object, "tooltip")?,
                    background: match text_value(object, "background")? {
                        "info" => wire::RibbonBackground::Info,
                        "success" => wire::RibbonBackground::Success,
                        "warning" => wire::RibbonBackground::Warning,
                        "danger" => wire::RibbonBackground::Danger,
                        _ => return Err(invalid("Builtin ribbon background is invalid")),
                    },
                }
            }
            "notification_alert" => wire::ViewNode::NotificationAlert {},
            "cancel_button" => wire::ViewNode::CancelButton {
                label: self.string(object, "label")?,
                variant: button_variant(required(object, "variant")?)?,
                modifiers: self.modifiers(object)?,
                details: {
                    let details = self.button_details(object)?;
                    validate_button_content(text_value(object, "label")?, None, details.as_ref())?;
                    if details.as_ref().is_some_and(|d| d.context.is_some()) {
                        return Err(invalid("Builtin cancel button cannot declare context"));
                    }
                    details
                },
            },
            "widget" => {
                let options = members(required(object, "options")?)?;
                let (widget, options) = match text(required(object, "widget")?)? {
                    "web.addons" if options.is_empty() => (
                        wire::ServiceWidget::Addons,
                        wire::WidgetOptions::Empty(wire::EmptyOptions::new()),
                    ),
                    "web.settings" => {
                        let source = array(required(options, "scope_kinds")?)?;
                        let mut scopes = self.vector(source.len())?;
                        for value in source {
                            scopes.push(match text(value)? {
                                "global" => wire::ScopeKind::Global,
                                "company" => wire::ScopeKind::Company,
                                _ => return Err(invalid("Builtin settings scope is invalid")),
                            });
                        }
                        let mut settings = wire::SettingsOptions::new(scopes);
                        if let Some(app) = optional(options, "app") {
                            let app = text(app)?;
                            if !wire::is_settings_app_name(app) {
                                return Err(invalid("Builtin settings app is invalid"));
                            }
                            settings = settings.with_app(self.text(app)?);
                        }
                        (
                            wire::ServiceWidget::Settings,
                            wire::WidgetOptions::Settings(settings),
                        )
                    }
                    _ => return Err(invalid("Builtin service widget is invalid")),
                };
                wire::ViewNode::Widget {
                    widget,
                    options,
                    modifiers: self.modifiers(object)?,
                }
            }
            "search" => wire::ViewNode::Search {
                children: self.indices(object)?,
                search_panel: self.panel(object)?,
            },
            "pivot" => wire::ViewNode::Pivot {
                children: self.indices(object)?,
                disable_linking: boolean(required(object, "disable_linking")?)?,
                display_quantity: boolean(required(object, "display_quantity")?)?,
                default_order: self.optional_text(object, "default_order")?,
            },
            "pivot_field" => wire::ViewNode::PivotField {
                field: self.field(required(object, "field")?)?,
                label: self.string(object, "label")?,
                role: match text_value(object, "role")? {
                    "row" => wire::PivotRole::Row,
                    "col" => wire::PivotRole::Col,
                    "measure" => wire::PivotRole::Measure,
                    _ => return Err(invalid("Builtin pivot role is invalid")),
                },
                interval: self.optional_text(object, "interval")?,
            },
            "search_field" => wire::ViewNode::SearchField {
                field: self.field(required(object, "field")?)?,
                label: self.string(object, "label")?,
                filter_domain: self.optional_expression(object, "filter_domain")?,
                operator: self.optional_text(object, "operator")?,
            },
            "search_filter" => wire::ViewNode::SearchFilter {
                name: self.string(object, "name")?,
                label: self.string(object, "label")?,
                domain: self.optional_expression(object, "domain")?,
                context: self.optional_expression(object, "context")?,
            },
            "search_separator" => wire::ViewNode::SearchSeparator {},
            "search_group" => wire::ViewNode::SearchGroup {
                label: self.string(object, "label")?,
                expand: optional(object, "expand")
                    .map(boolean)
                    .transpose()?
                    .unwrap_or(false),
                children: self.indices(object)?,
            },
            _ => return Err(invalid("Builtin view has an unsupported node kind")),
        })
    }
}
fn text_value<'a>(object: Members<'a>, key: &str) -> ViewResult<&'a str> {
    text(required(object, key)?)
}
fn valid_method(name: &str) -> bool {
    name.len() <= 128
        && name.starts_with(|c: char| c.is_ascii_lowercase())
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}
fn valid_icon(icon: &str) -> bool {
    icon.len() <= 256
        && icon.strip_prefix("lucide:").is_some_and(|name| {
            !name.is_empty()
                && name
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        })
}
fn validate_button_content(
    label: &str,
    confirm: Option<&str>,
    details: Option<&wire::ButtonDetails>,
) -> ViewResult<()> {
    if label.trim().is_empty()
        && details
            .and_then(|d| d.title.as_ref())
            .is_none_or(|title| title.trim().is_empty())
    {
        return Err(invalid("Builtin button requires a caption or title"));
    }
    if confirm.is_some_and(|text| text.trim().is_empty()) {
        return Err(invalid("Builtin button confirmation is empty"));
    }
    if let Some(details) = details {
        for text in [
            &details.confirm_title,
            &details.confirm_label,
            &details.cancel_label,
        ]
        .into_iter()
        .flatten()
        {
            if text.trim().is_empty() {
                return Err(invalid("Builtin button confirmation labels are empty"));
            }
            if confirm.is_none() {
                return Err(invalid("Button confirmation labels require confirm."));
            }
        }
    }
    Ok(())
}
fn tracks(value: &DescriptionValue) -> ViewResult<u32> {
    let value = unsigned(value)?;
    if (2..=12).contains(&value) && value % 2 == 0 {
        Ok(value)
    } else {
        Err(invalid(
            "Builtin group tracks must be even values from 2 through 12",
        ))
    }
}
fn button_variant(value: &DescriptionValue) -> ViewResult<wire::ButtonVariant> {
    match text(value)? {
        "primary" => Ok(wire::ButtonVariant::Primary),
        "secondary" => Ok(wire::ButtonVariant::Secondary),
        "danger" => Ok(wire::ButtonVariant::Danger),
        "link" => Ok(wire::ButtonVariant::Link),
        "success" => Ok(wire::ButtonVariant::Success),
        "info" => Ok(wire::ButtonVariant::Info),
        "warning" => Ok(wire::ButtonVariant::Warning),
        "unstyled" => Ok(wire::ButtonVariant::Unstyled),
        _ => Err(invalid("Builtin button variant is invalid")),
    }
}
fn children(node: &wire::ViewNode) -> &[u32] {
    match node {
        wire::ViewNode::List { children, .. }
        | wire::ViewNode::Form { children, .. }
        | wire::ViewNode::Sheet { children }
        | wire::ViewNode::Title { children }
        | wire::ViewNode::Heading { children, .. }
        | wire::ViewNode::Aside { children }
        | wire::ViewNode::Header { children }
        | wire::ViewNode::Footer { children }
        | wire::ViewNode::ButtonBox { children }
        | wire::ViewNode::StatButton { children, .. }
        | wire::ViewNode::Group { children, .. }
        | wire::ViewNode::Notebook { children }
        | wire::ViewNode::Page { children, .. }
        | wire::ViewNode::Search { children, .. }
        | wire::ViewNode::Pivot { children, .. }
        | wire::ViewNode::SearchGroup { children, .. } => children,
        wire::ViewNode::Dashboard { .. }
        | wire::ViewNode::Field { .. }
        | wire::ViewNode::Chatter { .. }
        | wire::ViewNode::Button { .. }
        | wire::ViewNode::CancelButton { .. }
        | wire::ViewNode::ActionButton { .. }
        | wire::ViewNode::Ribbon { .. }
        | wire::ViewNode::NotificationAlert {}
        | wire::ViewNode::Widget { .. }
        | wire::ViewNode::Separator { .. }
        | wire::ViewNode::PivotField { .. }
        | wire::ViewNode::SearchField { .. }
        | wire::ViewNode::SearchFilter { .. }
        | wire::ViewNode::SearchSeparator {} => &[],
    }
}
fn validate_graph(
    root: u32,
    nodes: &[wire::ViewNode],
    budget: &DescriptionBudget,
) -> ViewResult<()> {
    let edges = nodes.iter().try_fold(1usize, |count, node| {
        count
            .checked_add(children(node).len())
            .ok_or_else(|| invalid("Builtin view graph storage overflow"))
    })?;
    let (mut stack, _stack_storage) =
        crate::description_budget::storage::<(u32, usize, Option<u32>)>(budget, edges)?;
    let (mut seen, _seen_storage) =
        crate::description_budget::storage::<bool>(budget, nodes.len())?;
    seen.resize(nodes.len(), false);
    stack.push((root, 1, None));
    let service = matches!(
        nodes.get(root as usize),
        Some(wire::ViewNode::Form {
            record_mode: wire::RecordMode::Service,
            ..
        })
    );
    let mut visited = 0usize;
    while let Some((index, depth, parent)) = stack.pop() {
        let index = index as usize;
        let slot = seen
            .get_mut(index)
            .ok_or_else(|| invalid("Builtin view child is outside its node arena"))?;
        if *slot {
            return Err(invalid(
                "Builtin view graph contains a cycle or repeated node",
            ));
        }
        if depth > 32 {
            return Err(crate::ViewError::LimitExceeded {
                limit: crate::LimitKind::Depth,
                max: 32,
                actual: Some(depth),
            });
        }
        *slot = true;
        visited += 1;
        validate_layout(
            &nodes[index],
            parent.and_then(|i| nodes.get(i as usize)),
            service,
            budget,
            nodes,
        )?;
        for &child in children(&nodes[index]).iter().rev() {
            stack.push((child, depth + 1, Some(index as u32)));
        }
    }
    if visited != nodes.len() {
        return Err(invalid("Builtin view graph contains unreachable nodes"));
    }
    Ok(())
}

// These are the structural roles of our existing strict client reader. Field/model compatibility
// remains the host metadata authority; this adapter never infers access rights from a node.
fn validate_layout(
    node: &wire::ViewNode,
    parent: Option<&wire::ViewNode>,
    service: bool,
    budget: &DescriptionBudget,
    nodes: &[wire::ViewNode],
) -> ViewResult<()> {
    use wire::ViewNode as N;
    let valid = match node {
        N::Form { .. } | N::List { .. } | N::Search { .. } | N::Pivot { .. } => parent.is_none(),
        N::Page { name, label, .. } => {
            matches!(parent, Some(N::Notebook { .. }))
                && !name.trim().is_empty()
                && !label.trim().is_empty()
        }
        N::Sheet { .. }
        | N::Aside { .. }
        | N::Chatter { .. }
        | N::Header { .. }
        | N::Footer { .. } => matches!(parent, Some(N::Form { .. })) && !service,
        N::Title { .. } => {
            matches!(
                parent,
                Some(N::Form { .. } | N::Sheet { .. } | N::Group { .. } | N::Page { .. })
            ) && !service
        }
        N::Dashboard { .. } => matches!(parent, Some(N::List { .. })),
        N::Heading { .. } => matches!(parent, Some(N::Title { .. })),
        N::ButtonBox { .. } => matches!(parent, Some(N::Sheet { .. })),
        N::StatButton { .. } => matches!(parent, Some(N::ButtonBox { .. })),
        N::Button { .. } | N::ActionButton { .. } => {
            matches!(parent, Some(N::Header { .. } | N::Footer { .. }))
        }
        N::Ribbon { .. } | N::NotificationAlert {} => {
            !service
                && matches!(
                    parent,
                    Some(
                        N::Form { .. }
                            | N::Sheet { .. }
                            | N::Group { .. }
                            | N::Page { .. }
                            | N::Aside { .. }
                    )
                )
        }
        N::CancelButton { .. } => matches!(parent, Some(N::Footer { .. })),
        N::Widget { .. } => service,
        _ => !service,
    };
    let allowed_child = match parent {
        Some(N::List { .. }) => matches!(node, N::Field { .. } | N::Dashboard { .. }),
        Some(N::Heading { .. } | N::StatButton { .. }) => {
            matches!(node, N::Field { .. })
        }
        Some(N::Pivot { .. }) => matches!(node, N::PivotField { .. }),
        Some(N::Search { .. }) => matches!(
            node,
            N::SearchField { .. }
                | N::SearchFilter { .. }
                | N::SearchSeparator {}
                | N::SearchGroup { .. }
        ),
        Some(N::SearchGroup { .. }) => {
            matches!(node, N::SearchFilter { .. } | N::SearchSeparator {})
        }
        Some(N::Notebook { .. }) => matches!(node, N::Page { .. }),
        Some(N::Title { .. }) => matches!(node, N::Heading { .. } | N::Field { .. }),
        Some(N::Aside { .. }) => matches!(
            node,
            N::Field { .. }
                | N::Group { .. }
                | N::Separator { .. }
                | N::Ribbon { .. }
                | N::NotificationAlert {}
        ),
        Some(N::ButtonBox { .. }) => matches!(node, N::StatButton { .. }),
        Some(N::Header { .. }) => matches!(
            node,
            N::Button { .. } | N::ActionButton { .. } | N::Field { .. }
        ),
        Some(N::Footer { .. }) => matches!(
            node,
            N::Button { .. } | N::ActionButton { .. } | N::CancelButton { .. }
        ),
        _ => !matches!(
            node,
            N::SearchField { .. }
                | N::SearchFilter { .. }
                | N::SearchSeparator {}
                | N::SearchGroup { .. }
                | N::PivotField { .. }
        ),
    };
    if !valid || !allowed_child {
        return Err(invalid("Builtin view has an invalid layout hierarchy"));
    }
    match node {
        N::List { children, .. }
            if children
                .iter()
                .filter(|index| matches!(nodes.get(**index as usize), Some(N::Dashboard { .. })))
                .count()
                > 1 =>
        {
            return Err(invalid("Only one dashboard region is allowed"));
        }
        N::Form { children, .. } => {
            let mut asides = 0;
            let mut chatters = 0;
            for (position, index) in children.iter().enumerate() {
                match nodes.get(*index as usize) {
                    Some(N::Header { .. }) if position != 0 => {
                        return Err(invalid("Builtin view has an invalid layout hierarchy"));
                    }
                    Some(N::Footer { .. }) if position + 1 != children.len() => {
                        return Err(invalid("Builtin view has an invalid layout hierarchy"));
                    }
                    Some(N::Aside { .. }) => asides += 1,
                    Some(N::Chatter { .. }) => chatters += 1,
                    _ => {}
                }
            }
            if asides > 1 || chatters > 1 {
                return Err(invalid("Builtin view has an invalid layout hierarchy"));
            }
        }
        N::Notebook { children } => {
            let (mut names, _storage) =
                crate::description_budget::storage::<&str>(budget, children.len())?;
            for index in children {
                if let Some(N::Page { name, .. }) = nodes.get(*index as usize) {
                    names.push(name);
                }
            }
            names.sort_unstable();
            if names.windows(2).any(|pair| pair[0] == pair[1]) {
                return Err(invalid("Builtin notebook contains duplicate page names"));
            }
        }
        N::Footer { children }
            if children
                .iter()
                .filter(|index| matches!(nodes.get(**index as usize), Some(N::CancelButton { .. })))
                .count()
                > 1 =>
        {
            return Err(invalid("Builtin view has an invalid layout hierarchy"));
        }
        N::SearchFilter { name, label, .. }
            if name.trim().is_empty() || label.trim().is_empty() =>
        {
            return Err(invalid("Builtin view has an invalid layout hierarchy"));
        }
        N::Field { widget, .. } => {
            let header = matches!(parent, Some(N::Header { .. }));
            let statistic = matches!(parent, Some(N::StatButton { .. }));
            if (widget == "statusbar") != header
                || (statistic && widget != "statinfo" && widget != "relation_count")
                || (!statistic && widget == "relation_count")
            {
                return Err(invalid("Builtin view has an invalid layout hierarchy"));
            }
        }
        N::Button { modifiers, .. }
        | N::ActionButton { modifiers, .. }
        | N::StatButton { modifiers, .. }
        | N::CancelButton { modifiers, .. }
            if modifiers.readonly.is_some() || modifiers.required.is_some() =>
        {
            return Err(invalid(
                "Builtin button supports only an invisible modifier",
            ));
        }
        N::Widget { modifiers, .. }
            if modifiers.readonly.is_some()
                || modifiers.required.is_some()
                || modifiers.invisible.is_some() =>
        {
            return Err(invalid(
                "Builtin widget does not support expression modifiers",
            ));
        }
        _ => {}
    }
    Ok(())
}
