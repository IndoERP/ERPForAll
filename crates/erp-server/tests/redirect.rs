#![allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]
use erp_server::redirect::{RedirectPolicy, SHELL_VIEW_TYPES};

/// SPEC-VIEW-273 (partial: the kanban board's folded and opened columns in the page address are admitted on reload with their closed shape; the board itself is the client's)
#[test]
fn kanban_fold_state_is_admitted_with_its_closed_shape() {
    let policy = RedirectPolicy::new("https://erp.example.test", SHELL_VIEW_TYPES).unwrap();
    let encoded =
        |json: &str| url::form_urlencoded::byte_serialize(json.as_bytes()).collect::<String>();
    let address = |json: &str| {
        format!(
            "/erp/action-28?view_type=kanban&kanban_fold={}",
            encoded(json)
        )
    };
    let many = |count: usize| {
        (0..count)
            .map(|n| n.to_string())
            .collect::<Vec<_>>()
            .join(",")
    };
    // Fourteen 64-character keys and one of `last` characters: 1,024 UTF-16 units when `last` is 60.
    let long = |last: usize| {
        let keys = (0..14)
            .map(|n| format!(r#""{n:064}""#))
            .chain([format!(r#""{}""#, "y".repeat(last))])
            .collect::<Vec<_>>()
            .join(",");
        format!(r#"{{"by":"n","f":[{keys}],"o":[]}}"#)
    };
    assert_eq!(long(60).encode_utf16().count(), 1024);
    for good in [
        r#"{"by":"stage_id","f":[7,false],"o":["draft"]}"#.to_owned(),
        r#"{"by":"deadline:week","f":["2026-11-09"],"o":[]}"#.to_owned(),
        format!(r#"{{"by":"stage_id","f":[{}],"o":[]}}"#, many(64)),
        format!(r#"{{"by":"name","f":["{}"],"o":[]}}"#, "x".repeat(64)),
        format!(r#"{{"by":"{}","f":[1],"o":[]}}"#, "a".repeat(128)),
        long(60),
    ] {
        assert!(good.encode_utf16().count() <= 1024, "{good}");
        let address = address(&good);
        assert_eq!(policy.shell(&address).unwrap(), address, "{good}");
    }
    for bad in [
        r#"[]"#.to_owned(),
        r#"{"by":"stage_id"}"#.to_owned(),
        r#"{"by":"stage_id","f":[],"o":[],"x":1}"#.to_owned(),
        r#"{"by":"Stage","f":[],"o":[]}"#.to_owned(),
        r#"{"by":"deadline:hour","f":[],"o":[]}"#.to_owned(),
        r#"{"by":"stage_id","f":[null],"o":[]}"#.to_owned(),
        r#"{"by":"stage_id","f":[1.5],"o":[]}"#.to_owned(),
        r#"{"by":"stage_id","f":[[1]],"o":[]}"#.to_owned(),
        r#"{"by":"stage_id","f":[1,1],"o":[]}"#.to_owned(),
        r#"{"by":"stage_id","f":[1],"o":[1]}"#.to_owned(),
        r#"{"by":"stage_id","f":["a\"b"],"o":[]}"#.to_owned(),
        r#"{"by":"stage_id","f":["a\\b"],"o":[]}"#.to_owned(),
        format!(r#"{{"by":"stage_id","f":[{}],"o":[]}}"#, many(65)),
        format!(r#"{{"by":"name","f":["{}"],"o":[]}}"#, "x".repeat(65)),
        format!(r#"{{"by":"{}","f":[1],"o":[]}}"#, "a".repeat(129)),
        long(61),
        // a text key an address cannot carry: a slash pair, a control character
        r#"{"by":"name","f":["a//b"],"o":[]}"#.to_owned(),
        "{\"by\":\"name\",\"f\":[\"a\u{85}b\"],\"o\":[]}".to_owned(),
        // other spellings of the same state: only the text the client writes is admitted
        r#"{"by":"stage_id","f":[1.0],"o":[]}"#.to_owned(),
        r#"{"by":"stage_id","f":[-0],"o":[]}"#.to_owned(),
        r#"{"by":"stage_id","f":[1e2],"o":[]}"#.to_owned(),
        r#"{"f":[1],"o":[],"by":"stage_id"}"#.to_owned(),
        r#"{"by":"stage_id", "f":[1],"o":[]}"#.to_owned(),
        r#"{"by":"stage_id","f":[1],"o":[],"o":[]}"#.to_owned(),
    ] {
        assert!(policy.shell(&address(&bad)).is_err(), "{bad}");
    }
}

#[test]
fn shell_url_contract_corpus() {
    let corpus: serde_json::Value =
        serde_json::from_str(include_str!("../../../testdata/shell-url-contract.json")).unwrap();
    assert_eq!(corpus["schema"], 1);
    let policy = RedirectPolicy::new("https://erp.example.test", &["list", "form"]).unwrap();
    for case in corpus["cases"].as_array().unwrap() {
        let result = policy.shell(case["input"].as_str().unwrap());
        if let Some(expected) = case["canonical"].as_str() {
            assert_eq!(result.unwrap(), expected, "{}", case["name"]);
        } else {
            let error = result.expect_err(case["name"].as_str().unwrap());
            assert_eq!(
                error,
                erp_server::boundary::BoundaryError::Redirect,
                "{}",
                case["name"]
            );
            assert_eq!(
                error.to_string(),
                "redirect must be an allowed local shell URL",
                "{}",
                case["name"]
            );
        }
    }
}
#[test]
fn redirects_are_same_origin_but_shell_state_has_its_own_closed_grammar() {
    let policy =
        RedirectPolicy::new("https://erp.example.test", &["list", "form", "addon_view"]).unwrap();
    for (input, expected) in [
        ("/web/login_successful", "/web/login_successful"),
        ("https://erp.example.test/odoo/action-42", "/odoo/action-42"),
        ("/odoo", "/odoo"),
    ] {
        assert_eq!(policy.normalize(input).unwrap(), expected);
    }
    for (input, expected) in [
        ("/web", "/erp"),
        ("/odoo?action=42", "/erp/action-42"),
        (
            "/web?action=42&view_type=addon_view",
            "/erp/action-42?view_type=addon_view",
        ),
        (
            "/erp/action-42?cids=1%2C2&view_type=form&id=%34&menu=7",
            "/erp/action-42?menu=7&id=4&view_type=form&cids=1%2C2",
        ),
    ] {
        assert_eq!(policy.shell(input).unwrap(), expected);
    }
    for input in [
        "https://evil.test/odoo",
        "https://user@erp.example.test/odoo",
        "//erp.example.test/odoo",
        "/%2f%2fevil.test",
        "/odoo%0d%0aX:y",
        "/odoo%5cfoo",
        "/odoo?x=%",
        "/odoo?x=%zz",
    ] {
        assert!(policy.normalize(input).is_err(), "{input}");
    }
    for input in [
        "/odoo?action=1&action=2",
        "/odoo/action-42?action=42",
        "/odoo/action-42?view_type=unknown",
        "/odoo/action-42?id=3",
        "/odoo?model=res.users",
        "/odoo#x",
        "/odoo/action-42?cids=1,1",
        "/odoo?action=%2534%2532",
    ] {
        assert!(policy.shell(input).is_err(), "{input}");
    }
}

#[test]
fn erp_shell_routes_and_legacy_bookmarks_are_canonical() {
    let p = RedirectPolicy::new("https://erp.example.test", &["list", "form"]).unwrap();
    for input in ["/erp", "/odoo", "/web"] {
        assert_eq!(p.shell(input).unwrap(), "/erp");
    }
    for input in [
        "/erp/action-8?menu=9&id=8&view_type=form",
        "/odoo/action-8?menu=9&id=8&view_type=form",
        "/web?action=8&menu=9&id=8&view_type=form",
    ] {
        assert_eq!(
            p.shell(input).unwrap(),
            "/erp/action-8?menu=9&id=8&view_type=form"
        );
    }
    for bad in [
        "/erp/action-8?action=8",
        "/erp/action-8?menu=0",
        "//evil.test/erp",
        "/erp/action-8?view_type=unknown",
    ] {
        assert!(p.shell(bad).is_err());
    }
}

#[test]
fn dot_segments_and_double_slash_paths_never_resolve_to_another_host() {
    let policy = RedirectPolicy::new("https://erp.example.test", &["list", "form"]).unwrap();
    let refused = [
        "/.//evil.test/x",
        "/erp/..//evil.test",
        "/%2e//evil.test/x",
        "/erp/%2e%2e//evil.test",
        "/%2E//evil.test",
        "/erp/.%2e//evil.test",
        "/erp/%2E%2E//evil.test",
        "/erp/a/../..//evil.test",
        "/./%2fevil.test",
        "https://erp.example.test/.//evil.test/x",
        "https://erp.example.test/erp/..//evil.test",
        "https://erp.example.test/%2e//evil.test",
        "https://erp.example.test//evil.test/x",
    ];
    let accepted: Vec<_> = refused
        .iter()
        .filter_map(|input| Some((*input, policy.normalize(input).ok()?)))
        .collect();
    assert!(
        accepted.is_empty(),
        "resolved to a local string: {accepted:?}"
    );
    for input in refused {
        let error = policy.normalize(input).unwrap_err();
        assert_eq!(
            error,
            erp_server::boundary::BoundaryError::Redirect,
            "{input}"
        );
        assert_eq!(
            error.to_string(),
            "redirect must be an allowed local shell URL",
            "{input}"
        );
        assert!(policy.shell(input).is_err(), "{input}");
    }
    for (input, expected) in [
        ("/erp/action-8?search=a", "/erp/action-8?search=a"),
        ("/erp/./action-8", "/erp/action-8"),
        ("/erp/x/../action-8", "/erp/action-8"),
        ("https://erp.example.test/erp", "/erp"),
    ] {
        assert_eq!(policy.normalize(input).unwrap(), expected, "{input}");
    }
}

/// The dev/testing server answered 400 to `/erp/action-28?menu=38&view_type=pivot` (2026-11-13): the client routed
/// a view type the server's address policy did not list, so a reload of the pivot failed.
#[test]
fn every_view_type_the_client_routes_is_an_address_the_server_admits() {
    let route = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../addons/web/static/src/client/route.ts"
    ))
    .unwrap();
    let declared = route
        .lines()
        .find_map(|line| line.trim().strip_prefix("view_type?: "))
        .expect("route.ts declares the view_type union");
    let client: Vec<&str> = declared
        .trim_end_matches(';')
        .split('|')
        .map(|part| part.trim().trim_matches('"'))
        .collect();
    assert!(
        client.contains(&"list") && client.contains(&"form"),
        "{client:?}"
    );
    let policy = RedirectPolicy::new("https://erp.example.test", SHELL_VIEW_TYPES).unwrap();
    for view in client {
        assert!(
            SHELL_VIEW_TYPES.contains(&view),
            "the client routes {view:?}; add it to SHELL_VIEW_TYPES"
        );
        let address = format!("/erp/action-28?menu=38&view_type={view}");
        assert_eq!(policy.shell(&address).unwrap(), address);
    }
}

#[test]
fn calendar_address_state_is_admitted_with_closed_shapes() {
    let policy = RedirectPolicy::new("https://erp.example.test", SHELL_VIEW_TYPES).unwrap();
    for address in [
        "/erp/action-28?view_type=calendar&calendar_scale=month&calendar_date=2026-11-15",
        "/erp/action-28?view_type=calendar&calendar_scale=year&calendar_date=2024-02-29",
    ] {
        assert_eq!(policy.shell(address).unwrap(), address);
    }
    for address in [
        "/erp/action-28?view_type=calendar&calendar_scale=fortnight",
        "/erp/action-28?view_type=calendar&calendar_date=2026-02-30",
        "/erp/action-28?view_type=calendar&calendar_date=2025-02-29",
        "/erp/action-28?view_type=calendar&calendar_date=2026-13-01",
        "/erp/action-28?view_type=calendar&calendar_date=0999-01-01",
        "/erp/action-28?view_type=calendar&calendar_date=2026-1-01",
        "/erp/action-28?view_type=calendar&calendar_date=%EF%BC%92026-01-01",
    ] {
        assert!(policy.shell(address).is_err(), "{address}");
    }
}

#[test]
fn the_graph_state_in_the_address_survives_a_reload_and_a_malformed_value_is_refused() {
    let policy = RedirectPolicy::new("https://erp.example.test", SHELL_VIEW_TYPES).unwrap();
    let address = "/erp/action-28?view_type=graph&graph_mode=line&graph_measure=size&graph_groupby=published%3Amonth%2Cstate&graph_order=none&graph_stacked=0&graph_cumulated=1";
    assert_eq!(policy.shell(address).unwrap(), address);
    for bad in [
        "graph_mode=area",
        "graph_measure=Size",
        "graph_groupby=a%2Cb%2Cc",
        "graph_groupby=state%2Cstate",
        "graph_groupby=published%3Ahour",
        "graph_order=up",
        "graph_stacked=true",
        "graph_cumulated=2",
    ] {
        let address = format!("/erp/action-28?view_type=graph&{bad}");
        assert!(policy.shell(&address).is_err(), "{address}");
    }
}

#[test]
fn the_search_panel_selection_in_the_address_is_admitted_with_a_closed_shape() {
    let policy = RedirectPolicy::new("https://erp.example.test", SHELL_VIEW_TYPES).unwrap();
    let address = "/erp/action-28?view_type=list&search_panel=%7B%221%22%3A%5B7%5D%2C%222%22%3A%5B%22draft%22%2C3%5D%7D";
    assert_eq!(policy.shell(address).unwrap(), address);
    for bad in [
        "%5B%5D",
        "%7B%220%22%3A%5B1%5D%7D",
        "%7B%224294967296%22%3A%5B1%5D%7D",
        "%7B%221%22%3A7%7D",
        "%7B%221%22%3A%5B0%5D%7D",
        "%7B%221%22%3A%5B1%2C1%5D%7D",
        "%7B%221%22%3A%5B%22%22%5D%7D",
        "%7B%221%22%3A%5Bnull%5D%7D",
    ] {
        let address = format!("/erp/action-28?view_type=list&search_panel={bad}");
        assert!(policy.shell(&address).is_err(), "{address}");
    }
}

#[test]
fn escaped_pivot_headers_survive_an_http_reload() {
    let policy = RedirectPolicy::new("https://erp.example.test", SHELL_VIEW_TYPES).unwrap();
    let path = serde_json::json!([{"text":"A%2F%2FB%20%22C%22%5CD%0A%CE%A9"}]);
    for (name, state) in [
        ("pivot_open", serde_json::json!({"r":[path],"c":[]})),
        ("pivot_sort", serde_json::json!(["size", "desc", path])),
    ] {
        let encoded =
            url::form_urlencoded::byte_serialize(state.to_string().as_bytes()).collect::<String>();
        let url = format!("/erp/action-28?view_type=pivot&{name}={encoded}");
        assert_eq!(policy.shell(&url).unwrap(), url);
    }
}

#[test]
fn escaped_pivot_headers_require_the_client_encoding() {
    let policy = RedirectPolicy::new("https://erp.example.test", SHELL_VIEW_TYPES).unwrap();
    for text in ["%41", "%2f", "unescaped/slash", "%FE", "%", "Ω"] {
        let state = serde_json::json!(["size", "desc", [{"text": text}]]);
        let encoded =
            url::form_urlencoded::byte_serialize(state.to_string().as_bytes()).collect::<String>();
        assert!(
            policy
                .shell(&format!(
                    "/erp/action-28?view_type=pivot&pivot_sort={encoded}"
                ))
                .is_err(),
            "{text}"
        );
    }
}
