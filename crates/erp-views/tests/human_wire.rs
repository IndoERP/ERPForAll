#![allow(clippy::expect_used, clippy::unwrap_used, reason = "tests")]
use erp_views::wire::*;
use serde::{Serialize, de::DeserializeOwned};
use serde_json::{Value, json};

fn roundtrip<T: DeserializeOwned + Serialize>(value: &Value) {
    let decoded: T = serde_json::from_value(value.clone()).expect("fixture deserializes");
    assert_eq!(serde_json::to_value(decoded).unwrap(), *value);
    let mut bad = value.clone();
    bad.as_object_mut()
        .unwrap()
        .insert("unknown".into(), Value::Bool(true));
    assert!(serde_json::from_value::<T>(bad).is_err());
}

#[test]
fn action_view_bundle_roundtrips_existing_descriptions_and_rejects_unknown_fields() {
    let fixture: Value =
        serde_json::from_str(include_str!("../../../testdata/contracts/human-ui-v1.json")).unwrap();
    for (action_key, views_key) in [
        ("action", "views"),
        ("settings_action", "settings_views"),
        ("addons_action", "addons_views"),
    ] {
        let action: WindowAction =
            serde_json::from_value(fixture[action_key]["response"].clone()).unwrap();
        let views: ViewDescription =
            serde_json::from_value(fixture[views_key]["response"].clone()).unwrap();
        let bundle = WindowActionViews::new(action, views);
        let encoded = serde_json::to_value(bundle).unwrap();
        assert_eq!(encoded["action"], fixture[action_key]["response"]);
        assert_eq!(encoded["views"], fixture[views_key]["response"]);
        roundtrip::<WindowActionViews>(&encoded);

        let mut unknown = encoded.clone();
        unknown["unknown"] = json!(true);
        let error = serde_json::from_value::<WindowActionViews>(unknown).unwrap_err();
        assert!(error.is_data());
        assert_eq!(
            error.to_string(),
            "unknown field `unknown`, expected `action` or `views`"
        );
        for member in ["action", "views"] {
            let mut missing = encoded.clone();
            missing.as_object_mut().unwrap().remove(member);
            let error = serde_json::from_value::<WindowActionViews>(missing).unwrap_err();
            assert!(error.is_data());
            assert_eq!(error.to_string(), format!("missing field `{member}`"));
            let mut unsupported = encoded.clone();
            unsupported[member]["wire_major"] = json!(2);
            let error = serde_json::from_value::<WindowActionViews>(unsupported).unwrap_err();
            assert!(error.is_data());
            assert_eq!(error.to_string(), "unsupported human UI wire major");
        }
    }
}
#[test]
fn shared_contract_shapes_roundtrip_with_closed_objects() {
    let fixture: Value =
        serde_json::from_str(include_str!("../../../testdata/contracts/human-ui-v1.json")).unwrap();
    roundtrip::<Bootstrap>(&fixture["bootstrap"]);
    roundtrip::<LoginPage>(&fixture["login"]["value"]);
    roundtrip::<MenuTree>(&fixture["menus"]["response"]);
    for key in ["action", "settings_action", "addons_action"] {
        roundtrip::<WindowAction>(&fixture[key]["response"]);
    }
    for key in ["views", "settings_views", "addons_views"] {
        roundtrip::<ViewDescription>(&fixture[key]["response"]);
    }
    roundtrip::<SettingsDescription>(&fixture["settings_describe"]["response"]);
    for key in ["settings_read", "settings_write"] {
        roundtrip::<SettingsValues>(&fixture[key]["response"]);
    }
    roundtrip::<AddonCatalog>(&fixture["addon_catalog"]["response"]);
    roundtrip::<InstallResult>(&fixture["install"]["response"]);
    roundtrip::<UpgradeResult>(&fixture["upgrade"]["response"]);
    roundtrip::<UninstallResult>(&fixture["uninstall"]["response"]);
    roundtrip::<AddonDetail>(&fixture["addon_detail"]["response"]);
    roundtrip::<AddonDetailRequest>(&fixture["addon_detail"]["request"]);
}
#[test]
fn versions_identities_and_scope_shapes_are_closed() {
    for raw in [0_u64, 9_007_199_254_740_992, u64::MAX] {
        assert!(WireId::try_from(raw).is_err());
    }
    for raw in [1_u64, 9_007_199_254_740_991] {
        assert_eq!(u64::from(WireId::try_from(raw).unwrap()), raw);
    }
    for raw in [json!(0), json!(2), json!("1"), json!(1.1)] {
        assert!(serde_json::from_value::<WireMajor>(raw).is_err());
    }
    for raw in [
        json!({"kind":"global","company_id":1}),
        json!({"kind":"company"}),
        json!({"kind":"company","company_id":0}),
    ] {
        assert!(serde_json::from_value::<SettingsScope>(raw).is_err());
    }
    assert!(serde_json::from_value::<ServiceWidget>(json!("arbitrary.component")).is_err());
    assert!(serde_json::from_value::<EmptyOptions>(json!({"execute":"untrusted"})).is_err());
}

#[test]
fn nullable_keys_are_required_and_not_silently_defaulted() {
    let fixture: Value =
        serde_json::from_str(include_str!("../../../testdata/contracts/human-ui-v1.json")).unwrap();
    let mut login = fixture["login"]["value"].clone();
    login.as_object_mut().unwrap().remove("error");
    assert!(serde_json::from_value::<LoginPage>(login).is_err());
    let mut action = fixture["action"]["response"].clone();
    action.as_object_mut().unwrap().remove("domain");
    assert!(serde_json::from_value::<WindowAction>(action).is_err());
    assert!(serde_json::from_value::<Modifiers>(json!({"readonly":null,"required":null})).is_err());
}

#[test]
fn structured_form_wire_variants_are_additive_and_closed() {
    for value in [
        json!({"kind":"sheet","children":[1]}),
        json!({"kind":"group","label":"Details","columns":4,"colspan":2,"children":[2]}),
        json!({"kind":"notebook","children":[3]}),
        json!({"kind":"page","name":"details","label":"Details","children":[4]}),
        json!({"kind":"separator","label":"Notes"}),
    ] {
        roundtrip::<ViewNode>(&value);
    }
}

#[test]
fn header_button_wire_variants_are_additive_and_closed() {
    let modifiers = json!({"readonly":null,"required":null,"invisible":null});
    let button = json!({
        "kind":"button","name":"lend","button_type":"object","label":"Lend",
        "variant":"primary","confirm":null,"modifiers":modifiers
    });
    roundtrip::<ViewNode>(&json!({"kind":"header","children":[1]}));
    roundtrip::<ViewNode>(&button);
    let with_confirm = {
        let mut value = button.clone();
        value["confirm"] = json!("Lend this book?");
        value
    };
    roundtrip::<ViewNode>(&with_confirm);
    // `confirm` is nullable but required, like every other nullable member.
    let mut missing = button.clone();
    missing.as_object_mut().unwrap().remove("confirm");
    assert!(serde_json::from_value::<ViewNode>(missing).is_err());
    for (member, bad) in [("button_type", json!("action")), ("variant", json!("neon"))] {
        let mut value = button.clone();
        value[member] = bad;
        assert!(serde_json::from_value::<ViewNode>(value).is_err());
    }
}

#[test]
fn wizard_footer_and_action_result_shapes_are_closed() {
    let modifiers = json!({"readonly":null,"required":null,"invisible":null});
    roundtrip::<ViewNode>(&json!({"kind":"footer","children":[1,2]}));
    roundtrip::<ViewNode>(&json!({
        "kind":"cancel_button","label":"Cancel","variant":"secondary","modifiers":modifiers
    }));
    // A cancel button has no method name: the member is refused, not ignored.
    let named = json!({
        "kind":"cancel_button","label":"Cancel","variant":"secondary","modifiers":modifiers,
        "name":"lend"
    });
    assert!(serde_json::from_value::<ViewNode>(named).is_err());
    for result in [
        json!({"kind":"none"}),
        json!({"kind":"close"}),
        json!({"kind":"window","name":"Lend","model":"library.loan","res_id":null,
               "views":[{"id":null,"type":"form"}],"context":{"default_book_id":7}}),
        json!({"kind":"notification","title":null,"message":"Done","level":"success","then_close":true}),
    ] {
        roundtrip::<ActionResult>(&result);
    }
    roundtrip::<ActionResultEnvelope>(&json!({"wire_major":1,"result":{"kind":"close"}}));
    for bad in [
        json!({"wire_major":2,"result":{"kind":"close"}}),
        json!({"wire_major":1,"result":{"kind":"reload"}}),
        json!({"wire_major":1,"result":{"kind":"notification","title":null,"message":"x","level":"loud","then_close":false}}),
    ] {
        assert!(serde_json::from_value::<ActionResultEnvelope>(bad).is_err());
    }
}

// The new member is required even when false: a producer/consumer deployment mismatch
// must refuse the document instead of silently changing readonly persistence.
#[test]
fn static_field_force_save_is_required_boolean_and_modifiers_roundtrip() {
    let literal = |value| json!({"schema":1,"nodes":[{"kind":"literal","value":{"kind":"bool","value":value}}]});
    let field = json!({"kind":"field","field":{"model":"acceptance_notes.note","name":"name"},
        "widget":"char","options":{},"label":"Name","force_save":true,
        "modifiers":{"readonly":literal(true),"required":literal(false),"invisible":literal(true)}});
    roundtrip::<ViewNode>(&field);
    let mut disabled = field.clone();
    disabled["force_save"] = json!(false);
    roundtrip::<ViewNode>(&disabled);
    let mut absent = field.clone();
    absent.as_object_mut().unwrap().remove("force_save");
    let error = serde_json::from_value::<ViewNode>(absent).unwrap_err();
    assert_eq!(error.to_string(), "missing field `force_save`");
    for (bad, message) in [
        (json!(null), "invalid type: null, expected a boolean"),
        (json!(0), "invalid type: integer `0`, expected a boolean"),
        (json!(1), "invalid type: integer `1`, expected a boolean"),
        (
            json!("true"),
            "invalid type: string \"true\", expected a boolean",
        ),
    ] {
        let mut malformed = field.clone();
        malformed["force_save"] = bad;
        let error = serde_json::from_value::<ViewNode>(malformed).unwrap_err();
        assert!(error.is_data());
        assert_eq!(error.to_string(), message);
    }
}

#[test]
fn statusbar_options_are_additive_and_closed() {
    roundtrip::<FieldOptions>(&json!({}));
    roundtrip::<FieldOptions>(&json!({"visible":null}));
    roundtrip::<FieldOptions>(&json!({"visible":["draft","done"]}));
    for value in [
        json!({"visible":"draft"}),
        json!({"visible":[1]}),
        json!({"visible":null,"clickable":true}),
    ] {
        let error = serde_json::from_value::<FieldOptions>(value).unwrap_err();
        assert_eq!(
            error.to_string(),
            "data did not match any variant of untagged enum FieldOptions"
        );
    }
}

#[test]
fn list_selection_filter_reference_roundtrips_without_changing_existing_lists() {
    roundtrip::<ViewNode>(&json!({"kind":"list","children":[]}));
    roundtrip::<ViewNode>(
        &json!({"kind":"list","children":[],"search_panel":{"kind":"selection","field":{"model":"test.note","name":"priority"},"label":"Priority","enable_counters":true,"expand":true,"default_value":null,"notice":null},"page_limit":75}),
    );
}

#[test]
fn lifecycle_wire_outcomes_are_closed_and_detail_nulls_are_explicit() {
    roundtrip::<UpgradeResult>(
        &json!({"upgraded":["notes"],"installed":["dependency"],"registry_version":"9","asset_version":"a"}),
    );
    roundtrip::<UninstallResult>(
        &json!({"uninstalled":["notes"],"registry_version":"10","asset_version":"b"}),
    );
    let page = json!({"items":[],"next_cursor":null});
    let detail = json!({"wire_major":1,"registry_version":"9","name":"notes","label":"Notes","summary":"","description":"<b>plain text</b>","author":"","available_version":"1.0","installed_version":null,"state":"uninstalled","dependencies":page,"dependents":page,"menus":page,"views":page,"upgrade_refusal":"not installed","uninstall_refusal":"not installed"});
    roundtrip::<AddonDetail>(&detail);
    let mut populated = detail.clone();
    populated["menus"] = json!({"items":[{"id":17,"name":"notes_menu","label":"Notes"}],"next_cursor":"opaque-menu-cursor"});
    populated["views"] = json!({"items":[{"id":18,"name":"notes_form","model":"test.note","kind":"form"}],"next_cursor":null});
    roundtrip::<AddonDetail>(&populated);
    for member in [
        "available_version",
        "installed_version",
        "upgrade_refusal",
        "uninstall_refusal",
    ] {
        let mut missing = detail.clone();
        missing.as_object_mut().unwrap().remove(member);
        let error = serde_json::from_value::<AddonDetail>(missing).unwrap_err();
        assert!(error.is_data());
        assert_eq!(error.to_string(), format!("missing field `{member}`"));
    }
}

#[test]
fn lifecycle_impact_counts_are_canonical_decimal_text() {
    for raw in ["0", "1", "9007199254740993", "18446744073709551615"] {
        let count: WireDecimalCount = serde_json::from_value(json!(raw)).unwrap();
        assert_eq!(serde_json::to_value(count).unwrap(), json!(raw));
    }
    for raw in [
        json!(1),
        json!(""),
        json!("01"),
        json!("-1"),
        json!("1.0"),
        json!("18446744073709551616"),
    ] {
        let expected = if raw.is_string() {
            "count must be a canonical unsigned 64-bit decimal string"
        } else {
            "invalid type: integer `1`, expected a string"
        };
        let error = serde_json::from_value::<WireDecimalCount>(raw).unwrap_err();
        assert!(error.is_data());
        assert_eq!(error.to_string(), expected);
    }
}

#[test]
fn addon_detail_pagination_is_strict() {
    for limit in [1, 50, 100] {
        roundtrip::<AddonDetailRequest>(
            &json!({"name":"notes","limit":limit,"menus_cursor":"opaque"}),
        );
    }
    for limit in [0, 101] {
        let error =
            serde_json::from_value::<AddonDetailRequest>(json!({"name":"notes","limit":limit}))
                .unwrap_err();
        assert!(error.is_data());
        assert_eq!(
            error.to_string(),
            "addon detail limit must be between 1 and 100"
        );
    }
    let error = serde_json::from_value::<AddonDetailRequest>(json!({"name":"notes","limit":null}))
        .unwrap_err();
    assert!(error.is_data());
    assert_eq!(error.to_string(), "invalid type: null, expected u64");
    let error = serde_json::from_value::<AddonNamePage>(json!({"items":[]})).unwrap_err();
    assert!(error.is_data());
    assert_eq!(error.to_string(), "missing field `next_cursor`");
    let error =
        serde_json::from_value::<AddonNamePage>(json!({"items":[],"next_cursor":1})).unwrap_err();
    assert!(error.is_data());
    assert_eq!(
        error.to_string(),
        "invalid type: integer `1`, expected a string"
    );
    let fixture: Value =
        serde_json::from_str(include_str!("../../../testdata/contracts/human-ui-v1.json")).unwrap();
    let mut detail = fixture["addon_detail"]["response"].clone();
    detail["wire_major"] = json!(2);
    let error = serde_json::from_value::<AddonDetail>(detail).unwrap_err();
    assert!(error.is_data());
    assert_eq!(error.to_string(), "unsupported human UI wire major");
}

#[test]
fn extended_button_wire_is_additive_and_closed() {
    let base = json!({"kind":"button","name":"confirm","button_type":"object","label":"Confirm","variant":"secondary","confirm":null,"modifiers":{"readonly":null,"required":null,"invisible":null}});
    roundtrip::<ViewNode>(&base);
    let details =
        json!({"size":"normal","circle":false,"classes":[],"debounce_ms":0,"block_ui":false});
    let mut extended = base.clone();
    extended["details"] = details.clone();
    for variant in [
        "primary",
        "secondary",
        "link",
        "success",
        "info",
        "warning",
        "danger",
        "unstyled",
    ] {
        extended["variant"] = json!(variant);
        roundtrip::<ViewNode>(&extended);
    }
    extended["confirm"] = json!("Continue?");
    extended["details"] = json!({"size":"small","circle":true,"classes":["addon_hook"],"debounce_ms":60000,"block_ui":true,"icon":"lucide:refresh-cw","title":"Confirm record","help":"Help","hotkey":"shift+x","confirm_title":"Review","confirm_label":"Proceed","cancel_label":"Stay","context":{"schema":1,"nodes":[{"kind":"literal","value":{"kind":"bool","value":true}}]}});
    roundtrip::<ViewNode>(&extended);
    for target in [
        json!({"kind":"action","id":7}),
        json!({"kind":"url","href":"/documentation"}),
        json!({"kind":"inert"}),
    ] {
        roundtrip::<ViewNode>(
            &json!({"kind":"action_button","target":target,"label":"Open","variant":"link","confirm":null,"modifiers":base["modifiers"],"details":details}),
        );
    }
    for (field, value, expected) in [
        (
            "circle",
            json!("yes"),
            "invalid type: string \"yes\", expected a boolean",
        ),
        (
            "block_ui",
            json!(1),
            "invalid type: integer `1`, expected a boolean",
        ),
        (
            "title",
            json!(true),
            "invalid type: boolean `true`, expected a string",
        ),
    ] {
        let mut bad = base.clone();
        bad["details"] = details.clone();
        bad["details"][field] = value;
        let error = serde_json::from_value::<ViewNode>(bad).unwrap_err();
        assert!(error.is_data());
        assert_eq!(error.to_string(), expected);
    }
    let mut bad = base;
    bad["details"] = details;
    bad["details"].as_object_mut().unwrap().remove("size");
    let error = serde_json::from_value::<ViewNode>(bad).unwrap_err();
    assert!(error.is_data());
    assert_eq!(error.to_string(), "missing field `size`");
}

#[test]
fn display_widget_wire_leaves_are_closed() {
    for background in ["info", "success", "warning", "danger"] {
        roundtrip::<ViewNode>(
            &json!({"kind":"ribbon","text":"Archived","tooltip":"Record status","background":background}),
        );
    }
    roundtrip::<ViewNode>(&json!({"kind":"notification_alert"}));
}

#[test]
fn chatter_wire_node_is_closed() {
    let node = json!({"kind":"chatter","reload_on_post":true,"reload_on_attachment":false,
        "reload_on_activity":false,"reload_on_follower":true,"open_attachments":false,"has_activities":true});
    roundtrip::<ViewNode>(&node);
    for key in ["reload_on_post", "has_activities", "open_attachments"] {
        let mut missing = node.clone();
        missing.as_object_mut().unwrap().remove(key);
        assert!(
            serde_json::from_value::<ViewNode>(missing).is_err(),
            "{key}"
        );
    }
    let mut text = node.clone();
    text["reload_on_post"] = json!("1");
    assert!(serde_json::from_value::<ViewNode>(text).is_err());
    let mut children = node;
    children["children"] = json!([]);
    assert!(serde_json::from_value::<ViewNode>(children).is_err());
}

#[test]
fn button_url_declarations_refuse_other_schemes_and_controls() {
    for valid in [
        "/documentation",
        "/records?q=x#y",
        "https://example.com/x",
        "HTTP://example.com",
    ] {
        assert!(valid_button_url(valid), "{valid}");
    }
    for invalid in [
        "",
        "//example.com",
        "javascript:alert(1)",
        "data:text/html,x",
        "mailto:x@y",
        "ftp://example.com",
        "/\\example.com",
        "/a\nb",
        "/a\u{85}b",
        "https://",
        "https:///x",
    ] {
        assert!(!valid_button_url(invalid), "{invalid}");
    }
}

#[test]
fn code_field_options_are_closed_and_roundtrip() {
    for mode in [
        "xml",
        "python",
        "javascript",
        "json",
        "qweb",
        "scss",
        "bash",
        "text",
    ] {
        roundtrip::<FieldOptions>(&json!({"mode":mode}));
    }
    for malformed in [
        json!({"mode":""}),
        json!({"mode":"html"}),
        json!({"mode":null}),
        json!({"mode":false}),
        json!({"mode":1}),
        json!({"mode":[]}),
        json!({"mode":"xml","visible":null}),
        json!({"mode":"xml","wrap":true}),
    ] {
        let error = serde_json::from_value::<FieldOptions>(malformed).unwrap_err();
        assert!(error.is_data());
        assert_eq!(
            error.to_string(),
            "data did not match any variant of untagged enum FieldOptions"
        );
    }
}
