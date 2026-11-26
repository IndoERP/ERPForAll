// SPDX-License-Identifier: MIT OR Apache-2.0
//! Optional stored-coordinate maps. The host authorizes both coordinate fields.

use erp_sdk::FieldType;
use erp_sdk::prelude::*;
use erp_sdk::views::{
    CheckedViewInput, DescriptionBudget, DescriptionValue, DescriptionValueBuilder,
    MetadataFieldSet, ViewError, ViewResult, ViewTypeContribution, ViewTypeDecl, map_schema,
};

erp_sdk::addon!(
    name = "web_map",
    exact_only = true,
    depends = ["base", "web", "contacts"]
);
erp_sdk::model_ref!(pub View = "ir.ui.view");
erp_sdk::model_ref!(pub ActionView = "ir.actions.act_window.view");
erp_sdk::model_ref!(pub Partner = "res.partner");

#[extend(View, addon = "web_map")]
pub struct MapViewType {
    #[field(selection, selection_add = [("map", "Map")], ondelete = [("map", "cascade")])]
    pub r#type: Option<Arc<str>>,
}
#[extend(ActionView, addon = "web_map")]
pub struct MapActionViewType {
    #[field(selection, selection_add = [("map", "Map")], ondelete = [("map", "cascade")])]
    pub view_mode: Option<Arc<str>>,
}
#[extend(Partner, addon = "web_map")]
pub struct PartnerLocation {
    #[field(boolean, string = "Has map coordinates")]
    pub map_located: bool,
    #[field(float, digits = (10, 6), string = "Latitude")]
    pub map_latitude: Decimal,
    #[field(float, digits = (10, 6), string = "Longitude")]
    pub map_longitude: Decimal,
}

fn coordinates<'a>(input: &'a CheckedViewInput<'_>) -> ViewResult<[&'a str; 2]> {
    let fail = |s: &str| ViewError::InvalidDescription(s.to_owned());
    let root = input
        .root()
        .ok_or_else(|| fail("Map root is unavailable."))?;
    if root.tag() != "map" || root.children().next().is_some() || !root.text().trim().is_empty() {
        return Err(fail("A map needs one <map> root with no children."));
    }
    for (key, _) in root.attributes() {
        if !matches!(key, "string" | "latitude" | "longitude") {
            return Err(fail(&format!("Unsupported map attribute '{key}'.")));
        }
    }
    let coordinate = |key: &str| {
        let name = root
            .attr(key)
            .ok_or_else(|| fail(&format!("Map requires '{key}'.")))?;
        if !input
            .field_spec(name)
            .is_some_and(|f| f.has_column() && matches!(f.ty, FieldType::Float { .. }))
        {
            return Err(fail(&format!(
                "Map {key} '{name}' must be a stored float field."
            )));
        }
        Ok(name)
    };
    let pair = [coordinate("latitude")?, coordinate("longitude")?];
    if pair[0] == pair[1] {
        return Err(fail("Latitude and longitude must use distinct fields."));
    }
    Ok(pair)
}

fn validate(input: &CheckedViewInput<'_>, budget: &DescriptionBudget) -> ViewResult<()> {
    budget.check()?;
    coordinates(input).map(|_| ())
}

fn build(
    input: &CheckedViewInput<'_>,
    output: DescriptionValueBuilder<'_>,
) -> ViewResult<DescriptionValue> {
    let [lat, lon] = coordinates(input)?;
    output.object(|root| {
        root.set("root", |slot| slot.integer(0))?;
        root.set("nodes", |nodes| {
            nodes.array(1, |_, node| {
                node.variant("map", |value| {
                    value.object(|map| {
                        map.set("latitude", |v| v.field_handle(input.root_model(), lat))?;
                        map.set("longitude", |v| v.field_handle(input.root_model(), lon))
                    })
                })
            })
        })
    })
}

fn map_view_type() -> ViewResult<ViewTypeDecl> {
    ViewTypeDecl::builder("web_map", "map")
        .wire_major(1)
        .renderer("web.map")
        .schema(&map_schema()?)
        .validator(validate)
        .builder(build)
        .metadata_fields(MetadataFieldSet::builder().arch_references().finish()?)
        .methods(&[])
        .finish()
}

/// Published when this addon is installed.
pub fn view_type_contributions() -> [ViewTypeContribution; 1] {
    [ViewTypeContribution::new(&ADDON, &[map_view_type])]
}
