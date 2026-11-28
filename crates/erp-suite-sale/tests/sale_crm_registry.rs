// SPDX-License-Identifier: MIT OR Apache-2.0
//! A registry-only test of the bridge's optional contribution to both native forms.
#![allow(clippy::expect_used, reason = "registry test assertions")]

use erp_sdk::{AddonDescriptor, RegistryBuilder};

const CONTACTS: AddonDescriptor = AddonDescriptor {
    name: "contacts",
    depends: &["base", "web"],
    exact_only: false,
};

fn crm_and_sales(with_bridge: bool) -> RegistryBuilder {
    let mut addons = vec![
        &erp_addon_base::ADDON,
        &erp_addon_web::ADDON,
        &CONTACTS,
        &erp_addon_uom::ADDON,
        &erp_addon_product::ADDON,
        &erp_addon_analytic::ADDON,
        &erp_addon_account::ADDON,
        &erp_addon_mail::ADDON,
        &erp_addon_reporting::ADDON,
        &erp_addon_sales_team::ADDON,
        &erp_addon_sale::ADDON,
        &erp_addon_crm::ADDON,
    ];
    if with_bridge {
        addons.push(&erp_addon_sale_crm::ADDON);
    }
    RegistryBuilder::new(addons)
        .require_sql_objects([&erp_addon_account::reconcile::history_guard::MANIFEST])
}

#[test]
fn crm_sales_links_and_booking_fields_are_only_contributed_by_the_bridge() {
    let separate = crm_and_sales(false).build().expect("native registry");
    assert!(
        separate
            .model("sale.order")
            .expect("sales")
            .field("opportunity_id")
            .is_none()
    );
    assert!(
        separate
            .model("crm.lead")
            .expect("crm")
            .field("order_ids")
            .is_none()
    );
    let registry = crm_and_sales(true).build().expect("CRM/Sales composition");
    let order = registry.model("sale.order").expect("sales");
    let link = order.field("opportunity_id").expect("link");
    assert!(link.index.is_some());
    assert!(!link.copy);
    assert!(
        order
            .field("crm_booking_amount")
            .expect("booked revenue")
            .store
    );
    let lead = registry.model("crm.lead").expect("CRM");
    assert!(lead.field("order_ids").expect("order links").readonly);
    assert!(
        !lead
            .field("quotation_count")
            .expect("count under caller rights")
            .store
    );
}

#[test]
fn shipped_forecast_and_conversion_aggregates_use_stored_company_scoped_data()
-> Result<(), Box<dyn std::error::Error>> {
    use erp_data::{DataOperation, FieldValue, parse_xml_str};
    let registry = crm_and_sales(true).build()?;
    let mut sources = std::collections::BTreeSet::new();
    for (addon, xml) in [
        ("crm", include_str!("../../../addons/crm/data/reports.xml")),
        (
            "sale_crm",
            include_str!("../../../addons/sale_crm/data/forecast_bookings.xml"),
        ),
    ] {
        for operation in parse_xml_str(xml, "report.xml", addon, false)? {
            let DataOperation::Record(row) = operation else {
                continue;
            };
            if row.model() != "reporting.report.expression" {
                continue;
            }
            let text = |name| match row.field(name).map(|field| field.value()) {
                Some(FieldValue::Text(value)) => Some(value.as_str()),
                _ => None,
            };
            if text("engine") == Some("formula") {
                continue;
            }
            let model_name = text("model").ok_or("missing aggregate model")?;
            let model = registry.model(model_name).ok_or("unknown report source")?;
            let date = text("date_field").ok_or("aggregate missing period filter")?;
            let field = model
                .field(date)
                .ok_or_else(|| format!("unknown {model_name}.{date}"))?;
            assert!(field.store && matches!(field.ty.type_name(), "date" | "datetime"));
            assert!(model.field("company_id").is_some_and(|field| field.store));
            let domain = serde_json::from_str(text("domain").ok_or("missing aggregate domain")?)?;
            erp_sdk::Domain::from_json(&domain)?;
            if let Some(name) = text("field") {
                let measure = model
                    .field(name)
                    .ok_or_else(|| format!("unknown {model_name}.{name}"))?;
                assert!(measure.store && measure.ty.type_name() == "monetary");
                sources.insert((model_name.to_owned(), name.to_owned(), date.to_owned()));
            }
        }
    }
    for (model, field, date) in [
        ("crm.lead", "won_amount", "closed_date"),
        ("crm.lead", "weighted_revenue", "date_deadline"),
        ("crm.quota", "amount", "month"),
        ("sale.order", "crm_booking_amount", "date_order"),
    ] {
        assert!(sources.contains(&(model.into(), field.into(), date.into())));
    }
    Ok(())
}

/// Import the addon's actual data files with the installer XML reader. Combine every inherited
/// form with its real parent before passing the layouts through the native metadata builder.
#[test]
fn crm_combined_layouts_render_with_the_native_view_contract()
-> Result<(), Box<dyn std::error::Error>> {
    use erp_data::{DataOperation, FieldValue, parse_xml_str};
    use erp_views::{OwnerBoundViewTypes, ViewCatalogue, ViewDescriptor};
    use std::{
        collections::{BTreeMap, BTreeSet},
        fs,
        path::Path,
        sync::Arc,
    };

    let registry = Arc::new(crm_and_sales(true).build()?);
    let renderers = OwnerBoundViewTypes::bind(
        Arc::clone(&registry),
        &erp_addon_web::view_type_contributions(),
    )?;
    let directory = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../addons");
    let mut ids = BTreeMap::new();
    let mut views = Vec::new();
    let mut roots = Vec::new();
    for (addon, file, only) in [
        ("base", "data/partner_views.xml", Some("partner_form")),
        (
            "sales_team",
            "views/crm_team_views.xml",
            Some("crm_team_view_form"),
        ),
        (
            "sale",
            "views/sale_order_views.xml",
            Some("view_order_form"),
        ),
        ("crm", "views/leads.xml", None),
        ("crm", "views/configuration.xml", None),
        ("crm", "views/qualification.xml", None),
        ("sale_crm", "views/sale_crm_views.xml", None),
    ] {
        let xml = fs::read_to_string(directory.join(addon).join(file))?;
        for op in parse_xml_str(&xml, file, addon, false)? {
            let DataOperation::Record(record) = op else {
                continue;
            };
            if record.model() != "ir.ui.view" {
                continue;
            }
            let xid = record.id().ok_or("Expected an XML id for the view")?;
            if only.is_some_and(|id| id != xid.name()) {
                continue;
            }
            let name = xid.complete_name(addon);
            let field = |key| record.field(key).map(|f| f.value());
            let (Some(FieldValue::Xml(arch)), Some(FieldValue::Text(model))) =
                (field("arch"), field("model"))
            else {
                return Err(format!("Missing model or arch on {name}").into());
            };
            let id = i64::try_from(ids.len())? + 1;
            let mut view = ViewDescriptor::new(id, &name, model, arch);
            if let Some(FieldValue::Ref(parent)) = field("inherit_id") {
                let parent = parent.complete_name(addon);
                view = view.extension_of(
                    *ids.get(&parent)
                        .ok_or_else(|| format!("{name} refers to missing {parent}"))?,
                );
            } else {
                roots.push((id, name.clone(), model.clone()));
            }
            ids.insert(name, id);
            views.push(view);
        }
    }
    for required in [
        "crm.lead_board",
        "crm.lead_list",
        "crm.lead_form",
        "crm.qualify_lead_form",
        "crm.partner_opportunity_button",
        "crm.partner_business_account",
        "crm.team_crm_pipelines",
        "sale_crm.lead_sales_form",
        "sale_crm.sale_order_opportunity_form",
    ] {
        assert!(
            ids.contains_key(required),
            "Missing regression coverage for {required}"
        );
    }
    let catalogue = ViewCatalogue::new(views)?;
    let mut applied = BTreeSet::new();
    let mut errors = Vec::new();
    for (id, name, model) in roots {
        let combined = catalogue
            .combine(id)
            .map_err(|err| format!("{name}: {err}"))?;
        applied.extend(combined.applied_view_ids());
        let arch = combined.arch();
        let kind = arch.root().tag();
        let decl = renderers
            .declarations()
            .find(|(_, decl)| decl.key() == kind)
            .ok_or_else(|| format!("No native renderer for {kind} on {name}"))?
            .1;
        let spec = registry
            .model(&model)
            .ok_or_else(|| format!("Unknown model {model} on {name}"))?;
        if let Err(err) = erp_addon_web::metadata_test_support::test_native_layout(arch, spec, decl)
        {
            errors.push(format!("{name}: {err}"));
        }
    }
    assert!(errors.is_empty(), "{}", errors.join("\n"));
    assert_eq!(
        applied,
        ids.into_values().collect::<BTreeSet<i64>>(),
        "An extension did not reach its native renderer"
    );
    Ok(())
}

#[test]
fn assistant_offers_the_native_quotation_workflow_without_customer_signature() {
    let registry = std::sync::Arc::new(crm_and_sales(true).build().expect("CRM/Sales composition"));
    let pool = erp_orm::pool::create_pool(erp_testkit::tokio_postgres::Config::new(), 1)
        .expect("lazy pool");
    let orm = erp_orm::Orm::builder(registry, pool)
        .build()
        .expect("dispatch");
    let offered = |model: &str, method: &str| {
        let id = orm.registry().model_id(model).expect("installed model");
        orm.exposure().assistant_offered(id, method).is_ok()
    };
    assert!(offered("crm.lead", "action_create_quotation"));
    assert!(offered("sale.order", "action_draft_quotation"));
    assert!(offered("sale.order", "action_add_quotation_product"));
    assert!(offered("sale.order", "action_prepare_quotation_email"));
    assert!(offered("sale.quotation.send", "action_send_mail"));
    assert!(offered("sale.order", "action_quotation_email_status"));
    assert!(!offered(
        "sale.order.sign.wizard",
        "action_sign_and_confirm"
    ));
}
