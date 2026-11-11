//! Canonical ERP data XML shares the legacy reader's semantics and admission limits.

use erp_data::{DataOperation, ParseErrorKind, ReaderLimits, XmlDataReader, parse_xml_str};

#[test]
fn canonical_erp_preserves_legacy_container_semantics() {
    let body = r#"
<record id="outer" model="m"><field name="name">Café</field></record>
<data noupdate="0" uid="base.user_admin" context="{'inner': 2}">
  <menuitem id="menu" name="Menu"/>
  <delete model="m" id="old"/>
</data>"#;
    let document = |root: &str| {
        format!(
            "<{root} noupdate=\"1\" auto_sequence=\"1\" context=\"{{'outer': 1}}\">{body}</{root}>"
        )
    };
    let legacy =
        parse_xml_str(&document("odoo"), "data.xml", "m", false).expect("legacy input parses");
    assert_eq!(legacy.len(), 3);
    for root in ["erp", "odoo", "openerp", "data"] {
        let parsed = parse_xml_str(&document(root), "data.xml", "m", false)
            .unwrap_or_else(|error| panic!("{root}: {error}"));
        assert_eq!(parsed, legacy, "{root}");
        assert!(
            parse_xml_str(&format!("<{root}/>"), "empty.xml", "m", false)
                .expect("empty container parses")
                .is_empty()
        );
    }
    let nested = format!("<odoo><erp>{body}</erp></odoo>");
    let plain = format!("<odoo><data>{body}</data></odoo>");
    assert_eq!(
        parse_xml_str(&nested, "data.xml", "m", false).expect("nested ERP container"),
        parse_xml_str(&plain, "data.xml", "m", false).expect("nested legacy container")
    );
}

#[test]
fn canonical_erp_keeps_schema_and_entity_refusals() {
    let error = parse_xml_str("<erp typo=\"1\"/>", "bad.xml", "m", false)
        .expect_err("unknown attributes remain errors");
    assert_eq!(
        error.kind(),
        &ParseErrorKind::UnknownAttribute {
            tag: "erp".into(),
            attr: "typo".into(),
        }
    );
    assert_eq!(
        error.detail(),
        "unknown attribute 'typo' on <erp>; allowed: noupdate, auto_sequence, uid, context"
    );
    let error = parse_xml_str("<erp><unexpected/></erp>", "bad.xml", "m", false)
        .expect_err("unknown elements remain errors");
    assert_eq!(
        error.kind(),
        &ParseErrorKind::UnknownElement {
            parent: "erp".into(),
            tag: "unexpected".into(),
        }
    );
    assert_eq!(
        error.detail(),
        "element <unexpected> is not allowed in <erp>; allowed: erp, odoo, openerp, data, record, template, asset, menuitem, delete, function"
    );
    let error = parse_xml_str(
        "<!DOCTYPE erp [<!ENTITY secret SYSTEM 'file:///etc/passwd'>]><erp/>",
        "bad.xml",
        "m",
        false,
    )
    .expect_err("DOCTYPE remains forbidden");
    assert_eq!(error.kind(), &ParseErrorKind::XmlSyntax);
    assert_eq!(error.detail(), "DOCTYPE declarations are not permitted");
}

#[test]
fn canonical_erp_and_legacy_aliases_enforce_the_same_limits() {
    for root in ["erp", "odoo", "openerp", "data"] {
        for (xml, limits, expected) in [
            (
                format!("<{root}><data><record model=\"m\"/></data></{root}>"),
                ReaderLimits::default().with_max_depth(2),
                ParseErrorKind::DepthLimitExceeded { limit: 2 },
            ),
            (
                format!("<{root}><record id=\"long_record\" model=\"m\"/></{root}>"),
                ReaderLimits::default().with_max_operation_bytes(16),
                ParseErrorKind::OperationTooLarge { limit: 16 },
            ),
        ] {
            let error = XmlDataReader::new(&xml, "limited.xml", "m", false)
                .with_limits(limits)
                .collect::<Result<Vec<_>, _>>()
                .expect_err("the limit must reject the document");
            assert_eq!(error.kind(), &expected, "{root}");
            assert_eq!(error.file(), "limited.xml");
            assert_eq!(
                error.detail(),
                match expected {
                    ParseErrorKind::DepthLimitExceeded { .. } => {
                        "maximum element nesting depth (2) exceeded"
                    }
                    ParseErrorKind::OperationTooLarge { .. } => "operation is larger than 16 bytes",
                    _ => unreachable!("only depth and byte limits are exercised"),
                }
            );
        }
    }
}

#[test]
fn active_addon_data_parses_and_preserves_application_icons() {
    let files = [
        (
            "base",
            "bootstrap_company.xml",
            include_str!("../../../addons/base/data/bootstrap_company.xml"),
        ),
        (
            "base",
            "bootstrap_groups.xml",
            include_str!("../../../addons/base/data/bootstrap_groups.xml"),
        ),
        (
            "base",
            "settings_views.xml",
            include_str!("../../../addons/base/data/settings_views.xml"),
        ),
        (
            "base",
            "technical_views.xml",
            include_str!("../../../addons/base/data/technical_views.xml"),
        ),
        (
            "web",
            "addons_views.xml",
            include_str!("../../../addons/web/data/addons_views.xml"),
        ),
        (
            "acceptance_notes",
            "acceptance_notes_views.xml",
            include_str!("../../../addons/acceptance_notes/views/acceptance_notes_views.xml"),
        ),
        (
            "acceptance_notes",
            "acceptance_notes_data.xml",
            include_str!("../../../addons/acceptance_notes/data/acceptance_notes_data.xml"),
        ),
    ];
    let mut icons = Vec::new();
    for (module, file, xml) in files {
        let operations = parse_xml_str(xml, file, module, false)
            .unwrap_or_else(|error| panic!("{file}: {error}"));
        for operation in operations {
            if let DataOperation::MenuItem(menu) = operation
                && let Some(icon) = menu.web_icon()
            {
                assert!(
                    menu.parent().is_none(),
                    "application icons belong to root menus"
                );
                icons.push((
                    menu.name().expect("application name").to_owned(),
                    icon.to_owned(),
                ));
            }
        }
    }
    assert!(icons.contains(&("Settings".into(), "lucide:settings".into())));
    assert!(icons.contains(&("Notes".into(), "lucide:notebook-pen".into())));
}
