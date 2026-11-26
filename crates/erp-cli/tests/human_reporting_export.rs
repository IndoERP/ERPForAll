// SPDX-License-Identifier: MIT OR Apache-2.0
//! The dynamic report export through the real server binary: `reporting` is
//! installed from the browser route, the server restarts, and `POST /erp/reporting/export` answers the PDF
//! and the workbook of "Contacts by Country" with a browser session. The route's refusals (session,
//! origin, content type, method, bearer, body size by the name of its setting, unknown keys, a caller
//! without read access, the row bound by the name of its setting) are checked on the wire.
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
    collections::BTreeMap,
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
    raw(port, "POST", "/erp/reporting/export", &unique, body)
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

/// The text of every page of a PDF, white space removed, read back with an independent parser.
fn pdf_text(bytes: &[u8]) -> String {
    let pdf = lopdf::Document::load_mem(bytes).expect("the answer is a PDF");
    pdf.get_pages()
        .keys()
        .map(|page| {
            pdf.extract_text(&[*page])
                .unwrap()
                .chars()
                .filter(|c| !c.is_whitespace())
                .collect::<String>()
        })
        .collect()
}

#[test]
fn the_report_export_answers_files_and_refuses_what_it_must() {
    run_native(false);
}

#[test]
#[ignore = "requires Chrome, web dependencies and ERP_UI_COMPILER pointing to erp-ui-compile"]
fn browser_breadcrumb_replays_real_pdf_and_xlsx_as_the_clerk() {
    run_native(true);
}

fn run_native(browser_replay: bool) {
    std::thread::Builder::new()
        .stack_size(16 * 1024 * 1024)
        .spawn(move || {
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .unwrap()
                .block_on(acceptance(browser_replay));
        })
        .unwrap()
        .join()
        .unwrap();
}

async fn acceptance(browser_replay: bool) {
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
    if browser_replay {
        let fixtures = dir.path().join("reporting-fixtures");
        runtime_addons::stage_runtime_addons(
            &Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/reporting"),
            &fixtures,
        )
        .unwrap();
        fs::rename(
            fixtures.join("reporting_fixture"),
            addons.join("reporting_fixture"),
        )
        .unwrap();
        let toolchain =
            run_browser_toolchain("node web/scripts/build-human-ui.mjs --addons web,reporting")
                .env("ERP_UI_OUTPUT_DIR", dir.path())
                .status()
                .unwrap();
        assert!(
            toolchain.success(),
            "Reporting browser asset build failed: {toolchain}"
        );
    }
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
    let stderr = dir.path().join("server.stderr");
    let server = start(&config, &stderr, port);
    let admin = login(
        port,
        db.name(),
        "fixture-admin",
        "fixture-administrator-password",
    );
    let call = |cookies: &str, path: &str, body: Value| {
        native_http::json_post(port, cookies, path, &body.to_string())
    };
    ok(
        &call(
            &admin,
            "/erp/module/install",
            json!({"modules":[if browser_replay { "reporting_fixture" } else { "reporting" }]}),
        ),
        "install reporting",
    );
    // a deployment installs its addons and restarts: serve from the installed registry
    drop(server);
    let _server = start(&config, &stderr, port);

    let xid = async |module: &str, name: &str| -> i64 {
        client
            .query_one(
                "SELECT res_id::int8 FROM ir_model_data WHERE module=$1 AND name=$2",
                &[&module, &name],
            )
            .await
            .unwrap()
            .get(0)
    };
    let main = xid("base", "main_company").await;
    if browser_replay {
        let internal_group = xid("base", "group_user").await;
        let action = xid("reporting_fixture", "action_stock").await;
        let menu = xid("reporting_fixture", "menu_stock").await;
        verify_browser_replay(
            port,
            db.name(),
            &admin,
            (action, menu),
            main,
            internal_group,
            dir.path(),
        );
        return;
    }
    let portal_group = xid("base", "group_portal").await;
    let report = xid("reporting", "report_partner_country").await;
    let open = || {
        let result = ok(
            &call(
                &admin,
                "/erp/json/2/reporting.report/action_open_report",
                json!({"ids":[report]}),
            ),
            "open report",
        );
        let normalized = ok(
            &call(
                &admin,
                "/erp/json/2/reporting.report/erp_normalize_action_result",
                json!({"wire_major":1,"result":result}),
            ),
            "normalize report",
        );
        assert_eq!(normalized["result"]["tag"], "reporting.report");
        assert_eq!(normalized["result"]["params"]["report"], report);
        normalized["result"]["name"].as_str().unwrap().to_owned()
    };
    let original = open();
    let rename = |name: &str| {
        ok(
            &call(
                &admin,
                "/erp/json/2/reporting.report/write",
                json!({"ids":[report],"vals":{"name":name}}),
            ),
            "rename report",
        );
    };
    let long_title = "集計".repeat(80);
    rename(&long_title);
    let title = open();
    assert!(!title.is_empty() && long_title.starts_with(&title) && title.len() < long_title.len());
    rename(&original);
    let company_name: String = client
        .query_one("SELECT name FROM res_company WHERE id = $1", &[&main])
        .await
        .unwrap()
        .get(0);

    // 60 partners in 60 different countries
    let countries: Vec<i64> = client
        .query(
            "SELECT id::int8 FROM res_country ORDER BY id LIMIT 130",
            &[],
        )
        .await
        .unwrap()
        .iter()
        .map(|row| row.get(0))
        .collect();
    assert_eq!(countries.len(), 130, "the base data has the countries");
    let make = |range: std::ops::Range<usize>| {
        let vals: Vec<Value> = range
            .map(|i| json!({"name": format!("Export Partner {i}"), "country_id": countries[i], "company_id": main}))
            .collect();
        ok(
            &call(
                &admin,
                "/erp/json/2/res.partner/create",
                json!({"vals_list": vals}),
            ),
            "partners",
        );
    };
    make(0..60);

    // a portal user cannot read the report definitions
    let portal_partner = ok(
        &call(
            &admin,
            "/erp/json/2/res.partner/create",
            json!({"vals_list":[{"name":"Export Portal","company_id":main}]}),
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
                "name":"export-portal", "login":"export-portal", "password":"fixture-export-portal",
                "partner_id":portal_partner, "company_id":main, "company_ids":[[6,0,[main]]],
                "group_ids":[[6,0,[portal_group]]], "lang":"en_US", "tz":"UTC"
            }]}),
        ),
        "portal",
    );
    let portal = login(port, db.name(), "export-portal", "fixture-export-portal");

    let request = |format: &str| {
        json!({"report": report, "options": {"unfold_all": true}, "format": format})
            .to_string()
            .into_bytes()
    };

    // the workbook: one row per country with its count, the header with the period, the options sheet
    let book = post(port, Some(&admin), &[], &request("xlsx"));
    assert_eq!(book.status, 200, "{}", book.text());
    assert_eq!(
        book.header("content-type"),
        Some("application/vnd.openxmlformats-officedocument.spreadsheetml.sheet")
    );
    assert_eq!(
        book.header("content-disposition"),
        Some(
            "attachment; filename=\"Contacts by Country.xlsx\"; filename*=UTF-8''Contacts%20by%20Country.xlsx"
        )
    );
    assert_eq!(book.header("cache-control"), Some("private, no-store"));
    assert_eq!(book.header("x-content-type-options"), Some("nosniff"));
    let sheet = part(&book.body, "xl/worksheets/sheet1.xml");
    assert!(!sheet.contains("<f>"), "no cell is a formula");
    assert!(part(&book.body, "xl/sharedStrings.xml").contains(">Contacts<"));
    assert!(part(&book.body, "xl/workbook.xml").contains("name=\"Options\""));

    // the PDF: the letterhead, the title and the lines
    let pdf = post(port, Some(&admin), &[], &request("pdf"));
    assert_eq!(pdf.status, 200, "{}", pdf.text());
    assert_eq!(pdf.header("content-type"), Some("application/pdf"));
    assert!(pdf.body.starts_with(b"%PDF"));
    let text = pdf_text(&pdf.body);
    for want in [
        company_name.replace(' ', ""),
        "ContactsbyCountry".to_owned(),
        "Contacts".to_owned(),
    ] {
        assert!(text.contains(&want), "{want} in {text}");
    }

    // no session, a foreign origin, a cross-site fetch, the wrong content type, the wrong method, a bearer
    let nobody = post(port, None, &[], &request("xlsx"));
    assert_eq!(
        (nobody.status, nobody.text().as_str()),
        (401, "user is not connected")
    );
    let foreign = post(
        port,
        Some(&admin),
        &[("origin", "http://evil.example")],
        &request("xlsx"),
    );
    assert_eq!(foreign.status, 403, "{}", foreign.text());
    let cross = post(
        port,
        Some(&admin),
        &[("sec-fetch-site", "cross-site")],
        &request("xlsx"),
    );
    assert_eq!(cross.status, 403, "{}", cross.text());
    let own = post(
        port,
        Some(&admin),
        &[("origin", &format!("http://127.0.0.1:{port}"))],
        &request("xlsx"),
    );
    assert_eq!(own.status, 200, "the server's own origin is accepted");
    let form = post(
        port,
        Some(&admin),
        &[("content-type", "text/plain")],
        &request("xlsx"),
    );
    assert_eq!(form.status, 415);
    let get = raw(
        port,
        "GET",
        "/erp/reporting/export",
        &[("cookie", &admin), ("sec-fetch-site", "same-origin")],
        b"",
    );
    assert_eq!((get.status, get.header("allow")), (405, Some("POST")));
    let bearer = post(
        port,
        Some(&admin),
        &[("authorization", "Bearer nothing")],
        &request("xlsx"),
    );
    assert_eq!(bearer.status, 401);

    // a body over the declared bound is refused by the name of its setting, before it is parsed
    let huge = post(port, Some(&admin), &[], &vec![b' '; 1_048_577]);
    assert_eq!(huge.status, 413, "{}", huge.text());
    assert!(
        huge.text().contains("reporting.export_max_request_bytes"),
        "{}",
        huge.text()
    );

    // the options only: rows are an unknown key; a bad format and a foreign report id are refused
    let rows = post(
        port,
        Some(&admin),
        &[],
        json!({"report": report, "options": {}, "format": "pdf", "rows": [[1]]})
            .to_string()
            .as_bytes(),
    );
    assert_eq!(rows.status, 422, "{}", rows.text());
    assert!(
        rows.text().contains("'rows' is not part"),
        "{}",
        rows.text()
    );
    let csv = post(
        port,
        Some(&admin),
        &[],
        json!({"report": report, "options": {}, "format": "csv"})
            .to_string()
            .as_bytes(),
    );
    assert_eq!(csv.status, 422, "{}", csv.text());

    // a caller who may not read the definition gets the ordinary refusal and no file
    let denied = post(port, Some(&portal), &[], &request("xlsx"));
    assert_eq!(denied.status, 403, "{}", denied.text());

    // the row bound is a setting: refused by name over it, nothing truncated, and under it the file comes
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
    let below = write(json!([{"key":"reporting.export_max_rows","value":99}]));
    assert_ne!(below.status, 200, "under the floor: {}", below.body);
    let above = write(json!([{"key":"reporting.export_max_rows","value":500_001}]));
    assert_ne!(above.status, 200, "over the hard bound: {}", above.body);
    ok(
        &write(json!([{"key":"reporting.export_max_rows","value":100}])),
        "the row bound at its floor",
    );
    let under = post(port, Some(&admin), &[], &request("xlsx"));
    assert_eq!(
        under.status,
        200,
        "about 60 rows are inside 100: {}",
        under.text()
    );
    make(60..120);
    let over = post(port, Some(&admin), &[], &request("pdf"));
    assert_eq!(over.status, 422, "{}", over.text());
    assert!(
        over.text().contains("reporting.export_max_rows"),
        "{}",
        over.text()
    );
}

fn run_browser_toolchain(script: &str) -> Command {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap();
    let mut command = Command::new("bash");
    command.current_dir(root).args([
        "-c",
        &format!("set -euo pipefail\ntoolchain_env=$(scripts/web-toolchain.sh env)\neval \"$toolchain_env\"\nexec {script}"),
    ]);
    command
}

fn verify_browser_replay(
    port: u16,
    database: &str,
    admin: &str,
    (action, menu): (i64, i64),
    company: i64,
    internal_group: i64,
    temp: &Path,
) {
    let create = |model: &str, vals: Vec<Value>| {
        ok(
            &native_http::json_post(
                port,
                admin,
                &format!("/erp/json/2/{model}/create"),
                &json!({"vals_list": vals}).to_string(),
            ),
            model,
        )
    };
    let bins = create(
        "reporting_fixture.bin",
        (1..=12)
            .map(|i| json!({"name":format!("B{i:02}")}))
            .collect(),
    );
    let bins = bins.as_array().unwrap();
    let item = |name: String, bin: &Value, restricted: bool| json!({"name":name,"bin_id":bin,"qty":5,"reserved":1,"day":"2000-01-01","restricted":restricted});
    let mut items: Vec<_> = bins
        .iter()
        .enumerate()
        .filter(|(i, _)| *i != 10)
        .map(|(i, bin)| item(format!("Part{i:02}"), bin, false))
        .collect();
    items.extend((1..=23).map(|i| item(format!("Visible{i:02}"), &bins[10], false)));
    // This would sort within the first loaded child page and add five to the total.
    items.push(item("NoAccess".to_owned(), &bins[10], true));
    create("reporting_fixture.item", items);
    let clerk_partner = create(
        "res.partner",
        vec![json!({"name":"Replay Clerk","company_id":company})],
    );
    create(
        "res.users",
        vec![json!({
            "name":"Replay Clerk", "login":"replay-clerk", "password":"fixture-replay-clerk",
            "partner_id":clerk_partner[0], "company_id":company, "company_ids":[[6,0,[company]]],
            "group_ids":[[6,0,[internal_group]]], "lang":"en_US", "tz":"UTC"
        })],
    );
    let cookies = login(port, database, "replay-clerk", "fixture-replay-clerk");
    let output =
        std::env::var_os("ERP_REPORT_REPLAY_OUTPUT").map_or_else(|| temp.to_owned(), Into::into);
    let mut child = run_browser_toolchain("node web/scripts/browser-reporting-replay.mjs")
        .stdin(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(
            json!({
                "origin":format!("http://127.0.0.1:{port}"), "cookies":cookies,
                "action":action, "menu":menu, "output":output,
            })
            .to_string()
            .as_bytes(),
        )
        .unwrap();
    let status = child.wait().unwrap();
    assert!(
        status.success(),
        "Installed report browser failed: {status}"
    );
    verify_replay_files(&output);
}

fn verify_replay_files(output: &Path) {
    let expected: Vec<String> =
        serde_json::from_slice(&fs::read(output.join("rows.json")).unwrap()).unwrap();
    let book = fs::read(output.join("replay.xlsx")).unwrap();
    let strings = part(&book, "xl/sharedStrings.xml");
    let labels: Vec<_> = strings
        .split("<si>")
        .skip(1)
        .map(|term| {
            let start = term.find("<t").unwrap();
            term[start..]
                .split_once('>')
                .unwrap()
                .1
                .split_once("</t>")
                .unwrap()
                .0
        })
        .collect();
    let sheet = part(&book, "xl/worksheets/sheet1.xml");
    let cells: BTreeMap<_, _> = sheet
        .split("<c r=\"")
        .skip(1)
        .filter_map(|cell| {
            let (position, rest) = cell.split_once('"').unwrap();
            let (attributes, body) = rest.split_once('>')?;
            if attributes.ends_with('/') {
                return None;
            }
            let value = body.split_once("<v>")?.1.split_once("</v>")?.0;
            let value = if attributes.contains("t=\"s\"") {
                labels[value.parse::<usize>().unwrap()]
            } else {
                value
            };
            Some((position, value))
        })
        .collect();
    let xlsx_rows: Vec<_> = (3..)
        .map_while(|row| cells.get(format!("A{row}").as_str()))
        .copied()
        .collect();
    assert_eq!(xlsx_rows, expected);
    assert!(!strings.contains("NoAccess") && !strings.contains("Visible21"));
    let row = expected
        .iter()
        .position(|name| name == "Total On Hand")
        .unwrap()
        + 3;
    for (column, total) in [("B", 170.0), ("C", 34.0), ("D", 136.0)] {
        assert_eq!(
            cells[format!("{column}{row}").as_str()]
                .parse::<f64>()
                .unwrap(),
            total
        );
    }

    let pdf = fs::read(output.join("replay.pdf")).unwrap();
    let text = pdf_text(&pdf);
    assert!(!text.contains("NoAccess") && !text.contains("Visible21"));
    let mut after = text.as_str();
    for row in &expected {
        let label: String = row.chars().filter(|ch| !ch.is_whitespace()).collect();
        after = after
            .split_once(&label)
            .unwrap_or_else(|| panic!("Missing or out-of-order PDF row {row}: {text}"))
            .1;
    }
    assert!(
        text.contains("TotalOnHand170.0034.00136.00"),
        "PDF lost or changed displayed totals: {text}"
    );
}
