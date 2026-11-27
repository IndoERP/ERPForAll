// SPDX-License-Identifier: MIT OR Apache-2.0
//! Native session routes for browser list uploads, rolled-back tests and scoped downloads.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "integration assertions"
)]
#[path = "support/multiuser_fixture.rs"]
mod multiuser_fixture;
#[path = "support/native_http.rs"]
mod native_http;

use native_http::{BinaryReply, NativeFixture};
use serde_json::{Value, json};
use std::time::Duration;

const FORM: &str = "multipart/form-data; boundary=erp-file";
const TAG: &str = "res.partner.category";

fn form(options: &Value, file: &[u8]) -> Vec<u8> {
    let mut bytes = format!("--erp-file\r\nContent-Disposition: form-data; name=\"options\"\r\n\r\n{options}\r\n--erp-file\r\nContent-Disposition: form-data; name=\"file\"; filename=\"upload\"\r\n\r\n").into_bytes();
    bytes.extend_from_slice(file);
    bytes.extend_from_slice(b"\r\n--erp-file--\r\n");
    bytes
}

struct Browser<'a> {
    fixture: &'a NativeFixture,
    csrf: String,
}
impl Browser<'_> {
    fn post(&self, action: &str, content_type: &str, body: &[u8]) -> BinaryReply {
        native_http::request_bytes(
            self.fixture.port,
            "POST",
            &format!("/erp/impex/{action}"),
            &[
                ("cookie", &self.fixture.cookies),
                ("x-csrf-token", &self.csrf),
                ("content-type", content_type),
                ("sec-fetch-site", "same-origin"),
            ],
            body,
            Duration::from_secs(60),
        )
    }
    fn upload(&self, action: &str, options: &Value, file: &[u8]) -> BinaryReply {
        self.post(action, FORM, &form(options, file))
    }
    fn download(&self, action: &str, options: &Value) -> BinaryReply {
        self.post(
            action,
            "application/json",
            &serde_json::to_vec(options).unwrap(),
        )
    }
}

fn ok(reply: &BinaryReply) {
    assert_eq!(
        reply.status,
        200,
        "{}",
        String::from_utf8_lossy(&reply.body)
    );
}
fn json_ok(reply: &BinaryReply) -> Value {
    ok(reply);
    serde_json::from_slice(&reply.body).unwrap()
}

#[test]
fn browser_files_honor_the_canonical_import_and_export_policies() {
    use multiuser_fixture::{Subject, success};
    multiuser_fixture::run(|fixture| {
        let native = fixture.native;
        let page = native_http::request(
            native.port,
            "GET",
            "/erp",
            &[("cookie", &native.cookies)],
            "",
        );
        let csrf = native_http::embedded(&page, "erp-bootstrap")["csrf"]["logout"]
            .as_str()
            .unwrap()
            .to_owned();
        let browser = Browser {
            fixture: native,
            csrf,
        };
        let reviewer = fixture.user(Subject::Admin);
        let id = success(&fixture.rpc(
            Subject::Admin,
            TAG,
            "create",
            json!({"vals_list":[{"name":"Transfer policy tag"}]}),
        ))[0]
            .as_i64()
            .unwrap();
        let upload = json!({"model":TAG,"format":"csv","fields":["name"]});
        let download = json!({"model":TAG,"format":"csv","fields":[{"name":"name","label":"Name"}],"ids":[id]});
        let template =
            json!({"model":TAG,"format":"csv","fields":[{"name":"name","label":"Name"}]});
        let source = b"name\nPending file review\n";
        for (method, outcome) in [("load", "approve"), ("export_data", "deny")] {
            let mut policy = json!({"name":method,"principal_kind":"user","outcome":outcome,
                "model":TAG,"method":method,"effect":"write"});
            if outcome == "approve" {
                policy["approver_user_ids"] = json!([[6, 0, [reviewer.id]]]);
            }
            success(&fixture.rpc(
                Subject::Admin,
                "ir.approval.policy",
                "create",
                json!({"vals_list":[policy]}),
            ));
        }
        ok(&browser.upload("preview", &upload, source));
        ok(&browser.download("template", &template));
        for action in ["test", "import"] {
            let response = browser.upload(action, &upload, source);
            assert_eq!(response.status, 422);
            assert!(
                String::from_utf8_lossy(&response.body)
                    .contains("res.partner.category.load through JSON-2")
            );
        }
        let remaining = success(&fixture.rpc(
            Subject::Admin,
            TAG,
            "search_count",
            json!({"domain":[["name","=","Pending file review"]]}),
        ));
        assert_eq!(remaining, 0);
        assert_eq!(browser.download("export", &download).status, 403);
    });
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bounded_preview_and_transactional_import_share_the_browser_session() {
    let fixture = NativeFixture::start().await;
    let page = native_http::request(
        fixture.port,
        "GET",
        "/erp",
        &[("cookie", &fixture.cookies)],
        "",
    );
    assert_eq!(page.status, 200, "bootstrap unavailable");
    let bootstrap = native_http::embedded(&page, "erp-bootstrap");
    let browser = Browser {
        fixture: &fixture,
        csrf: bootstrap["csrf"]["logout"].as_str().unwrap().into(),
    };
    let options = json!({"model":TAG,"format":"csv"});
    let denied = Browser {
        fixture: &fixture,
        csrf: String::from("invalid"),
    }
    .upload("preview", &options, b"name\ntest\n");
    assert_eq!(denied.status, 400);
    let refused = browser.upload(
        "preview",
        &json!({"model":"ir.model","format":"csv"}),
        b"name\na\n",
    );
    assert_eq!(
        refused.status,
        403,
        "{}",
        String::from_utf8_lossy(&refused.body)
    );
    let bad_context = browser.upload(
        "preview",
        &json!({"model":TAG, "format":"csv", "context":{"install_mode":true}}),
        b"name\na\n",
    );
    assert_eq!(bad_context.status, 400);
    let cross_site = native_http::request(
        fixture.port,
        "POST",
        "/erp/impex/preview",
        &[
            ("cookie", &fixture.cookies),
            ("x-csrf-token", &browser.csrf),
            ("sec-fetch-site", "cross-site"),
        ],
        "",
    );
    assert_eq!(cross_site.status, 403);

    // A file over the normal JSON limit only returns a short preview.
    let long_cell = "x".repeat(30_000);
    let mut large = String::from("name\n");
    for _ in 0..73 {
        large.push_str(&long_cell);
        large.push('\n');
    }
    assert!(large.len() > 2 * 1024 * 1024);
    let reply = json_ok(&browser.upload("preview", &options, large.as_bytes()));
    assert_eq!(reply["row_count"], 73);
    assert_eq!(reply["rows"].as_array().unwrap().len(), 10);
    let too_many = format!("name\n{}", "some tag\n".repeat(10_001));
    let refused = browser.upload("preview", &options, too_many.as_bytes());
    assert!(refused.status >= 400);
    assert!(String::from_utf8_lossy(&refused.body).contains("10000"));

    // Simulate a tag addon that schedules follow-up work on creation. A held job is
    // inspectable after the request and is never picked up by the fixture's worker.
    fixture
        .client
        .batch_execute(
            "
        CREATE FUNCTION queue_imported_tag() RETURNS trigger LANGUAGE plpgsql AS $$
        BEGIN
            INSERT INTO erp_job
                (kind, kind_version, state, max_attempts, run_as_kind, run_as_user,
                 enqueued_by_kind, enqueued_by_user)
            VALUES ('fixture.imported_tag', 1, 'held', 1, 'user', NEW.create_uid,
                    'user', NEW.create_uid);
            RETURN NEW;
        END $$;
        CREATE TRIGGER queue_imported_tag AFTER INSERT ON res_partner_category
            FOR EACH ROW EXECUTE FUNCTION queue_imported_tag();",
        )
        .await
        .unwrap();
    let jobs = || async {
        fixture
            .client
            .query_one(
                "SELECT count(*) FROM erp_job WHERE kind = 'fixture.imported_tag'",
                &[],
            )
            .await
            .unwrap()
            .get::<_, i64>(0)
    };
    let options = json!({"model":TAG,"format":"csv","fields":["id",null,"name"]});
    let source = b"identifier,unused,title\n,Notes for this file,\n__import__.browser_tag,,=literal\n,End of file,\n";
    for action in ["test", "import"] {
        let empty = browser.upload(
            action,
            &options,
            b"identifier,unused,title\n,Only a note,\n",
        );
        assert_eq!(empty.status, 422);
        assert!(String::from_utf8_lossy(&empty.body).contains("no data rows"));
    }
    let test = json_ok(&browser.upload("test", &options, source));
    assert_eq!(test["count"], 1, "{test}");
    assert!(test.get("ids").is_none());
    let count = || async {
        fixture.client.query_one("SELECT count(*) FROM ir_model_data WHERE module='__import__' AND name='browser_tag'", &[]).await.unwrap().get::<_,i64>(0)
    };
    assert_eq!(count().await, 0, "Test left an external ID");
    assert_eq!(jobs().await, 0, "Test left a queued job");
    let imported = json_ok(&browser.upload("import", &options, source));
    assert_eq!(imported["ids"].as_array().unwrap().len(), 1);
    let id = imported["ids"][0].as_i64().unwrap();
    assert_eq!(count().await, 1);
    assert_eq!(jobs().await, 1, "Import should commit the queued job");
    let bad_options = json!({"model":TAG, "format":"csv", "fields":["id","color","name",null]});
    let bad = b"identifier,color,title,notes\n__import__.browser_other,1,new,\n,,,Only a note\n\n__import__.browser_tag,invalid,modified,\n";
    let test = json_ok(&browser.upload("test", &bad_options, bad));
    assert_eq!(test["count"], false);
    assert_eq!(test["messages"][0]["file_rows"], json!({"from":5,"to":5}));
    let result = json_ok(&browser.upload("import", &bad_options, bad));
    assert_eq!(result["ids"], false);
    assert_eq!(result["messages"][0]["file_rows"], json!({"from":5,"to":5}));
    assert_eq!(jobs().await, 1, "A failed file must roll back queued jobs");
    assert_eq!(fixture.client.query_one("SELECT count(*) FROM ir_model_data WHERE module='__import__' AND name='browser_other'", &[]).await.unwrap().get::<_,i64>(0), 0);

    // An ID-only export must still pass the selected records' read rule before creating
    // any external identifiers. This transient record has no owning user.
    let hidden: i64 = fixture
        .client
        .query_one(
            "INSERT INTO change_password_wizard DEFAULT VALUES RETURNING id",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    let hidden_export = browser.download(
        "export",
        &json!({
            "model":"change.password.wizard", "format":"csv",
            "fields":[{"name":"id","label":"External ID"}], "ids":[hidden]
        }),
    );
    assert_eq!(
        hidden_export.status,
        403,
        "{}",
        String::from_utf8_lossy(&hidden_export.body)
    );
    let absent_export = browser.download("export", &json!({
        "model":TAG, "format":"csv", "fields":[{"name":"id","label":"External ID"}], "ids":[i64::MAX]
    }));
    assert!(absent_export.status >= 400);

    let ordered = json!({"model":TAG,"format":"csv","fields":[{"name":"name","label":"Forged label"},{"name":"id","label":"Ignored"}],"ids":[id]});
    let csv = browser.download("export", &ordered);
    ok(&csv);
    let csv = String::from_utf8(csv.body).unwrap();
    assert!(!csv.contains("Forged label"));
    assert!(csv.contains("'=literal,__import__.browser_tag"), "{csv}");
    let no_xid = json_ok(&browser.upload(
        "import",
        &json!({"model": TAG, "format":"csv", "fields":["name"]}),
        b"name\nExport creates an ID\n",
    ));
    let no_xid = no_xid["ids"][0].as_i64().unwrap();
    let generated = browser.download("export", &json!({
        "model": TAG, "format":"csv", "fields":[{"name":"id","label":"External ID"}], "ids":[no_xid]
    }));
    ok(&generated);
    assert!(
        String::from_utf8(generated.body)
            .unwrap()
            .contains("__export__.")
    );
    assert_eq!(fixture.client.query_one(
        "SELECT count(*) FROM ir_model_data WHERE module='__export__' AND model=$1 AND res_id=$2",
        &[&TAG, &no_xid]).await.unwrap().get::<_, i64>(0), 1);

    let mut ordered = ordered;
    ordered["format"] = json!("xlsx");
    let xlsx = browser.download("export", &ordered);
    ok(&xlsx);
    let file = erp_data::read_import_file(
        &xlsx.body,
        erp_data::ImportFileType::Xlsx,
        &Default::default(),
    )
    .unwrap();
    assert_eq!(file.rows[0], ["=literal", "__import__.browser_tag"]);
    let contacts = json_ok(&browser.upload(
        "import",
        &json!({"model":"res.partner", "format":"csv", "fields":["name","category_id/id"]}),
        b"name,tag\nTagged Alpha,__import__.browser_tag\nTagged Beta,__import__.browser_tag\n",
    ));
    assert_eq!(contacts["ids"].as_array().unwrap().len(), 2);
    for (format, kind) in [
        ("csv", erp_data::ImportFileType::Csv),
        ("xlsx", erp_data::ImportFileType::Xlsx),
    ] {
        let reply = browser.download("export", &json!({
            "model": TAG, "format":format, "ids":[id], "fields":[{"name":"partner_ids","label":"Partners"}]
        }));
        ok(&reply);
        let table = erp_data::read_import_file(&reply.body, kind, &Default::default()).unwrap();
        assert_eq!(table.rows.len(), 1);
        let mut names: Vec<_> = table.rows[0][0].split(',').collect();
        names.sort_unstable();
        assert_eq!(names, ["Tagged Alpha", "Tagged Beta"]);
    }
    let template = browser.download(
        "template",
        &json!({"model":"res.partner","format":"xlsx","fields":[{"name":"phone","label":"Phone"},{"name":"email","label":"Email"},{"name":"city","label":"City"}]}),
    );
    ok(&template);
    assert_eq!(
        erp_data::impex::import_sheet_names(&template.body).unwrap(),
        ["Data", "Guide"]
    );
    let preview = json_ok(&browser.upload(
        "preview",
        &json!({"model":"res.partner","format":"xlsx"}),
        &template.body,
    ));
    assert_eq!(preview["sheet"], "Data");
    assert_eq!(preview["header"][0], "name");
    assert!(
        preview["header"]
            .as_array()
            .unwrap()
            .contains(&json!("email"))
    );
    assert_eq!(preview["row_count"], 0);
    let preview = json_ok(&browser.upload(
        "preview",
        &json!({"model":"res.partner","format":"xlsx","sheet":"Guide"}),
        &template.body,
    ));
    assert!(preview["rows"].to_string().contains("Harbor Paper Company"));
    let first_sheet_has_invalid_headers = erp_addon_web::table_xlsx::write_record_table(
        &[String::new(), "Second column".into()],
        &[],
        &[vec!["name".into()], vec!["Use this sheet".into()]],
    )
    .unwrap();
    let preview = json_ok(&browser.upload(
        "preview",
        &json!({"model": TAG, "format":"xlsx"}),
        &first_sheet_has_invalid_headers,
    ));
    assert_eq!(preview["sheet"], "Data");
    assert!(preview["sheet_error"].as_str().unwrap().contains("empty"));
    assert_eq!(preview["sheets"], json!(["Data", "Guide"]));
    let preview = json_ok(&browser.upload(
        "preview",
        &json!({"model": TAG, "format":"xlsx", "sheet":"Guide"}),
        &first_sheet_has_invalid_headers,
    ));
    assert_eq!(preview["rows"], json!([["Use this sheet"]]));
    fixture.cleanup().await;
}
