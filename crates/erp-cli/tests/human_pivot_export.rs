// SPDX-License-Identifier: MIT OR Apache-2.0
//! The pivot view's spreadsheet download through the real server binary: the documents
//! the web client produces (`addons/web/static/tests/fixtures/pivot-export-*.json`, compared with the
//! client's own output by `client-pivot-export.test.ts`) are posted with a browser session; the route's
//! refusals (session, origin, content type, each declared bound by the name of its setting, a malformed
//! document by path, a model the caller cannot read) and the workbook it answers are checked on the wire.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "integration assertions"
)]
#[path = "support/native_http.rs"]
mod native_http;
#[path = "support/runtime_addons.rs"]
mod runtime_addons;

use erp_testkit::TestDb;
use serde_json::{Value, json};
use std::{
    fs,
    io::{Cursor, Read, Write},
    net::{TcpListener, TcpStream},
    path::Path,
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

struct Server(Child);
impl Drop for Server {
    fn drop(&mut self) {
        if self.0.try_wait().unwrap().is_none() {
            self.0.kill().unwrap();
            self.0.wait().unwrap();
        }
    }
}

struct Raw {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}
impl Raw {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
}

/// One exchange with the body kept as bytes (the answer is a zip archive).
fn raw(port: u16, method: &str, path: &str, headers: &[(&str, &str)], body: &[u8]) -> Raw {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(60)))
        .unwrap();
    let mut wire = format!(
        "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\nContent-Length: {}\r\n",
        body.len()
    );
    for (name, value) in headers {
        wire.push_str(&format!("{name}: {value}\r\n"));
    }
    wire.push_str("\r\n");
    stream.write_all(wire.as_bytes()).unwrap();
    // a refusal may be written before the whole body is read: ignore a closed connection here
    let _ = stream.write_all(body);
    let mut bytes = Vec::new();
    let _ = stream.read_to_end(&mut bytes);
    let split = bytes
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .expect("an HTTP answer");
    let head = String::from_utf8(bytes[..split].to_vec()).unwrap();
    let mut lines = head.split("\r\n");
    let status = lines
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .unwrap()
        .parse()
        .unwrap();
    let headers = lines
        .map(|line| {
            let (name, value) = line.split_once(':').unwrap();
            (name.to_ascii_lowercase(), value.trim().to_owned())
        })
        .collect();
    Raw {
        status,
        headers,
        body: bytes[split + 4..].to_vec(),
    }
}

fn start(config: &Path, stderr: &Path, port: u16) -> Server {
    let mut server = Server(
        Command::new(env!("CARGO_BIN_EXE_erp"))
            .args(["server", "--config"])
            .arg(config)
            .stdout(Stdio::null())
            .stderr(fs::File::create(stderr).unwrap())
            .spawn()
            .unwrap(),
    );
    let until = Instant::now() + Duration::from_secs(180);
    loop {
        if let Some(status) = server.0.try_wait().unwrap() {
            panic!(
                "server exited {status}: {}",
                fs::read_to_string(stderr).unwrap_or_default()
            );
        }
        if TcpStream::connect(("127.0.0.1", port)).is_ok()
            && raw(port, "GET", "/erp/health/ready", &[], b"").status == 200
        {
            return server;
        }
        assert!(
            Instant::now() < until,
            "server did not become ready: {}",
            fs::read_to_string(stderr).unwrap_or_default()
        );
        std::thread::sleep(Duration::from_millis(200));
    }
}

fn login(port: u16, db: &str, user: &str, password: &str) -> String {
    let page = native_http::request(port, "GET", "/erp/login", &[], "");
    assert_eq!(page.status, 200);
    let prefs = native_http::cookie(&page, "prefs");
    let csrf = native_http::embedded(&page, "erp-login")["csrf"]
        .as_str()
        .unwrap()
        .to_owned();
    let logged = native_http::request(
        port,
        "POST",
        "/erp/login",
        &[
            ("cookie", &prefs),
            ("content-type", "application/x-www-form-urlencoded"),
        ],
        &format!(
            "db={db}&login={user}&password={password}&redirect=%2Ferp&csrf_token={}",
            native_http::encoded(csrf.as_bytes())
        ),
    );
    assert_eq!(logged.status, 303, "login of {user}: {}", logged.body);
    format!("{prefs}; {}", native_http::cookie(&logged, "session_id"))
}

fn ok(reply: &native_http::Reply, what: &str) -> Value {
    assert_eq!(reply.status, 200, "{what}: {}", reply.body);
    serde_json::from_str(&reply.body).unwrap()
}

/// A client fixture, with the model swapped for one this database has (the fixtures name the test
/// model of the client suite); every other byte is what the client produced.
fn fixture(name: &str, model: &str) -> Value {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("addons/web/static/tests/fixtures")
        .join(name);
    let mut document: Value = serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap();
    document["model"] = json!(model);
    document
}

const OK_HEADERS: [(&str, &str); 2] = [
    ("content-type", "application/json"),
    ("sec-fetch-site", "same-origin"),
];

fn post(port: u16, cookies: Option<&str>, extra: &[(&str, &str)], body: &[u8]) -> Raw {
    let mut headers = OK_HEADERS.to_vec();
    if let Some(cookies) = cookies {
        headers.push(("cookie", cookies));
    }
    headers.extend_from_slice(extra);
    // a later header of the same name replaces the default one
    let mut unique: Vec<(&str, &str)> = Vec::new();
    for (name, value) in headers.into_iter().rev() {
        if !unique.iter().any(|(seen, _)| *seen == name) {
            unique.push((name, value));
        }
    }
    raw(port, "POST", "/erp/pivot/export", &unique, body)
}

fn part(bytes: &[u8], name: &str) -> String {
    let mut archive =
        zip::ZipArchive::new(Cursor::new(bytes)).expect("the answer is a zip archive");
    let mut text = String::new();
    archive
        .by_name(name)
        .unwrap_or_else(|_| panic!("the workbook has {name}"))
        .read_to_string(&mut text)
        .unwrap();
    text
}

#[test]
fn the_pivot_download_answers_a_workbook_and_refuses_what_it_must() {
    std::thread::Builder::new()
        .stack_size(16 * 1024 * 1024)
        .spawn(|| {
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .unwrap()
                .block_on(acceptance());
        })
        .unwrap()
        .join()
        .unwrap();
}

async fn acceptance() {
    let db = TestDb::create().await.unwrap();
    let role = format!("{}_web", db.name());
    let client = db.client().await.unwrap();
    let temporary = std::env::temp_dir().canonicalize().unwrap();
    let dir = tempfile::tempdir_in(temporary).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    let source = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("addons");
    let addons = dir.path().join("runtime-addons");
    runtime_addons::stage_runtime_addons(&source, &addons).unwrap();
    let addons = addons.canonicalize().unwrap();
    let config = native_http::config(dir.path(), &db, port, &role, &addons);
    client
        .batch_execute(&format!(
            "CREATE ROLE {} LOGIN PASSWORD 'fixture-runtime-password'; CREATE ROLE {} LOGIN PASSWORD 'fixture-runtime-password' NOSUPERUSER NOCREATEDB NOCREATEROLE NOREPLICATION",
            erp_sql::quote(&role),
            erp_sql::quote(&format!("{role}_node"))
        ))
        .await
        .unwrap();
    native_http::private(
        &dir.path().join("password"),
        "fixture-administrator-password",
    );
    let init = Command::new(env!("CARGO_BIN_EXE_erp"))
        .args(["db", "init", "--config"])
        .arg(&config)
        .args(["--admin-login", "fixture-admin", "--admin-password-file"])
        .arg(dir.path().join("password"))
        .output()
        .unwrap();
    assert!(
        init.status.success(),
        "{}",
        String::from_utf8_lossy(&init.stderr)
    );
    native_http::login_rollout::prepare(&db, &role).await;
    let _server = start(&config, &dir.path().join("server.stderr"), port);
    let admin = login(
        port,
        db.name(),
        "fixture-admin",
        "fixture-administrator-password",
    );
    let call = |cookies: &str, path: &str, body: Value| {
        native_http::json_post(port, cookies, path, &body.to_string())
    };

    // an internal user with no read right on module records
    let xid = async |name: &str| -> i64 {
        client
            .query_one(
                "SELECT res_id::int8 FROM ir_model_data WHERE module='base' AND name=$1",
                &[&name],
            )
            .await
            .unwrap()
            .get(0)
    };
    let (main, user_group) = (xid("main_company").await, xid("group_user").await);
    let partner = ok(
        &call(
            &admin,
            "/erp/json/2/res.partner/create",
            json!({"vals_list":[{"name":"Pivot Reader","company_id":main}]}),
        ),
        "partner",
    )[0]
    .as_i64()
    .unwrap();
    ok(
        &call(
            &admin,
            "/erp/json/2/res.users/create",
            json!({"vals_list":[{
                "name":"pivot-reader", "login":"pivot-reader", "password":"fixture-pivot-reader",
                "partner_id":partner, "company_id":main, "company_ids":[[6,0,[main]]],
                "group_ids":[[6,0,[user_group]]], "lang":"en_US", "tz":"UTC"
            }]}),
        ),
        "reader",
    );
    let reader = login(port, db.name(), "pivot-reader", "fixture-pivot-reader");

    // a portal user is refused before the body is parsed: a body that would fail parsing still gets the access refusal
    let portal_group = xid("group_portal").await;
    let portal_partner = ok(
        &call(
            &admin,
            "/erp/json/2/res.partner/create",
            json!({"vals_list":[{"name":"Pivot Portal","company_id":main}]}),
        ),
        "portal partner",
    )[0]
    .as_i64()
    .unwrap();
    ok(
        &call(
            &admin,
            "/erp/json/2/res.users/create",
            json!({"vals_list":[{
                "name":"pivot-portal", "login":"pivot-portal", "password":"fixture-pivot-portal",
                "partner_id":portal_partner, "company_id":main, "company_ids":[[6,0,[main]]],
                "group_ids":[[6,0,[portal_group]]], "lang":"en_US", "tz":"UTC"
            }]}),
        ),
        "portal",
    );
    let portal = login(port, db.name(), "pivot-portal", "fixture-pivot-portal");
    let refused = post(port, Some(&portal), &[], b"{\"model\":");
    assert_eq!(refused.status, 403, "{}", refused.text());

    let nested = fixture("pivot-export-nested.json", "res.partner");
    let flat = fixture("pivot-export-flat.json", "res.partner");
    let body = |document: &Value| document.to_string().into_bytes();

    // the table the client produced comes back as a workbook, for the administrator and for an ordinary reader
    for (who, cookies) in [("administrator", &admin), ("reader", &reader)] {
        let answered = post(port, Some(cookies), &[], &body(&nested));
        assert_eq!(answered.status, 200, "{who}: {}", answered.text());
        assert_eq!(
            answered.header("content-type"),
            Some("application/vnd.openxmlformats-officedocument.spreadsheetml.sheet")
        );
        assert_eq!(
            answered.header("content-disposition"),
            Some("attachment; filename=\"Notes.xlsx\"; filename*=UTF-8''Notes.xlsx")
        );
        assert_eq!(answered.header("cache-control"), Some("private, no-store"));
        assert_eq!(answered.header("x-content-type-options"), Some("nosniff"));
    }
    let workbook = post(port, Some(&admin), &[], &body(&nested)).body;
    let sheet = part(&workbook, "xl/worksheets/sheet1.xml");
    for merged in ["A1:A3", "B1:E1", "F1:G2", "B2:C2", "D2:E2"] {
        assert!(
            sheet.contains(&format!("<mergeCell ref=\"{merged}\"/>")),
            "{merged} in {sheet}"
        );
    }
    assert!(!sheet.contains("<f>"), "no cell is a formula");
    assert!(part(&workbook, "xl/workbook.xml").contains("name=\"Notes\""));
    assert!(part(&workbook, "xl/sharedStrings.xml").contains(">Priority<"));
    assert!(sheet.contains("<v>1234.5</v>"), "a float is a numeric cell");
    let flat_book = post(port, Some(&admin), &[], &body(&flat));
    assert_eq!(flat_book.status, 200, "{}", flat_book.text());
    assert!(
        part(&flat_book.body, "xl/worksheets/sheet1.xml").contains("<mergeCell ref=\"A1:A2\"/>")
    );

    // text that could be a formula is stored as text
    let mut risky = flat.clone();
    risky["rows"][1]["label"] = json!("=HYPERLINK(\"http://example.invalid\",\"x\")");
    let risky = post(port, Some(&admin), &[], &body(&risky));
    assert_eq!(risky.status, 200, "{}", risky.text());
    let risky_sheet = part(&risky.body, "xl/worksheets/sheet1.xml");
    assert!(
        !risky_sheet.contains("<f>") && !risky_sheet.contains("<f "),
        "{risky_sheet}"
    );
    assert!(part(&risky.body, "xl/sharedStrings.xml").contains("=HYPERLINK("));
    assert!(part(&risky.body, "xl/styles.xml").contains("quotePrefix=\"1\""));

    // no session, no cookie of this server, a foreign origin, a cross-site fetch, the wrong content type, the wrong method
    let nobody = post(port, None, &[], &body(&nested));
    assert_eq!(
        (nobody.status, nobody.text().as_str()),
        (401, "user is not connected")
    );
    let foreign = post(
        port,
        Some(&admin),
        &[("origin", "http://evil.example")],
        &body(&nested),
    );
    assert_eq!(foreign.status, 403, "{}", foreign.text());
    let cross = post(
        port,
        Some(&admin),
        &[("sec-fetch-site", "cross-site")],
        &body(&nested),
    );
    assert_eq!(cross.status, 403, "{}", cross.text());
    let own_origin = post(
        port,
        Some(&admin),
        &[("origin", &format!("http://127.0.0.1:{port}"))],
        &body(&nested),
    );
    assert_eq!(
        own_origin.status, 200,
        "the server's own origin is accepted"
    );
    let form = post(
        port,
        Some(&admin),
        &[("content-type", "text/plain")],
        &body(&nested),
    );
    assert_eq!(form.status, 415);
    let get = raw(
        port,
        "GET",
        "/erp/pivot/export",
        &[("cookie", &admin), ("sec-fetch-site", "same-origin")],
        b"",
    );
    assert_eq!((get.status, get.header("allow")), (405, Some("POST")));
    let bearer = post(
        port,
        Some(&admin),
        &[("authorization", "Bearer nothing")],
        &body(&nested),
    );
    assert_eq!(bearer.status, 401);

    // a malformed document is refused by path
    let wrong = |mutate: &dyn Fn(&mut Value)| {
        let mut document = nested.clone();
        mutate(&mut document);
        post(port, Some(&admin), &[], &body(&document))
    };
    let refused = wrong(&|d| d["rows"][1]["cells"][2]["kind"] = json!("text"));
    assert_eq!(refused.status, 422, "{}", refused.text());
    assert!(
        refused.text().contains("$.rows[1].cells[2].kind"),
        "{}",
        refused.text()
    );
    let refused = wrong(&|d| d["headers"][0][1]["colspan"] = json!("4"));
    assert!(
        refused.text().contains("$.headers[0][1].colspan"),
        "{}",
        refused.text()
    );
    let refused = wrong(&|d| d["surprise"] = json!(1));
    assert!(refused.text().contains("$.surprise"), "{}", refused.text());
    let not_json = post(port, Some(&admin), &[], b"{\"model\":");
    assert_eq!(not_json.status, 422, "{}", not_json.text());
    assert!(not_json.text().contains('$'), "{}", not_json.text());
    let unknown = wrong(&|d| d["model"] = json!("no.such.model"));
    assert_eq!(unknown.status, 422, "{}", unknown.text());
    assert!(unknown.text().contains("$.model"), "{}", unknown.text());

    // a caller who cannot read the model gets the ordinary access refusal; whoever can, gets the file
    let mut modules = nested.clone();
    modules["model"] = json!("ir.module.module");
    let denied = post(port, Some(&reader), &[], &body(&modules));
    assert_eq!(denied.status, 403, "{}", denied.text());
    let allowed = post(port, Some(&admin), &[], &body(&modules));
    assert_eq!(
        allowed.status,
        200,
        "the same document by an administrator: {}",
        allowed.text()
    );

    // the bounds are settings: each is refused by the name of its setting, and none goes below its floor
    let read = ok(
        &call(
            &admin,
            "/erp/json/2/ir.config.setting/erp_read_values",
            json!({"scope":{"kind":"global"},"keys":["web.pivot_export_max_cells"]}),
        ),
        "read the cells bound",
    );
    assert_eq!(read["values"][0]["source"], "default");
    assert_eq!(read["values"][0]["value"], 100_000);
    let write = |changes: Value| -> native_http::Reply {
        let revision = ok(
            &call(
                &admin,
                "/erp/json/2/ir.config.setting/erp_read_values",
                json!({"scope":{"kind":"global"},"keys":[]}),
            ),
            "revision",
        )["revision"]
            .clone();
        call(
            &admin,
            "/erp/json/2/ir.config.setting/erp_write_values",
            json!({"scope":{"kind":"global"},"expected_revision":revision,"changes":changes}),
        )
    };
    let below = write(json!([{"key":"web.pivot_export_max_cells","value":99}]));
    assert_ne!(
        below.status, 200,
        "a value under the floor is refused: {}",
        below.body
    );
    let above = write(json!([{"key":"web.pivot_export_max_columns","value":16_384}]));
    assert_ne!(
        above.status, 200,
        "a value over the hard bound is refused: {}",
        above.body
    );
    let floors = write(json!([
        {"key":"web.pivot_export_max_cells","value":100},
        {"key":"web.pivot_export_max_header_depth","value":2},
        {"key":"web.pivot_export_max_columns","value":2},
        {"key":"web.pivot_export_max_label_bytes","value":64},
        {"key":"web.pivot_export_max_request_bytes","value":65_536}
    ]));
    ok(&floors, "every bound set to its floor");
    let over = |document: &Value, setting: &str| {
        let answered = post(port, Some(&admin), &[], &body(document));
        assert!(
            answered.status == 422 || answered.status == 413,
            "{setting}: {} {}",
            answered.status,
            answered.text()
        );
        assert!(
            answered.text().contains(setting),
            "{setting} in {}",
            answered.text()
        );
    };
    over(&nested, "web.pivot_export_max_header_depth"); // three header rows against two
    let mut two_levels = flat.clone();
    two_levels["rows"] = json!((0..60).map(|i| json!({"label": format!("r{i}"), "cells": [{"kind":"integer","value":i},{"kind":"integer","value":i}]})).collect::<Vec<_>>());
    two_levels["headers"] = json!([[{"label":"x"},{"label":"a"},{"label":"b"}]]);
    over(&two_levels, "web.pivot_export_max_cells"); // 183 cells (3 columns x 61 rows) against 100
    let mut wide = two_levels.clone();
    wide["rows"] =
        json!([{"label":"r","cells":[{"kind":"empty"},{"kind":"empty"},{"kind":"empty"}]}]);
    wide["headers"] = json!([[{"label":"x"},{"label":"a"},{"label":"b"},{"label":"c"}]]);
    over(&wide, "web.pivot_export_max_columns"); // three columns against two
    let mut long = flat.clone();
    long["rows"][1]["label"] = json!("l".repeat(65));
    over(&long, "web.pivot_export_max_label_bytes");
    let mut huge = flat.clone();
    huge["rows"][1]["label"] = json!("l".repeat(70_000));
    over(&huge, "web.pivot_export_max_request_bytes"); // refused before the document is parsed
    assert_eq!(
        post(port, Some(&admin), &[], &body(&flat)).status,
        200,
        "a table inside every bound still downloads"
    );
}
