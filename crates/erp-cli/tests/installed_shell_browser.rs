// SPDX-License-Identifier: MIT OR Apache-2.0
//! Installed session, record authorization, keyboard and request evidence in real Chrome.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    dead_code,
    reason = "integration fixture assertions and shared helpers"
)]
#[path = "support/native_http.rs"]
mod native_http;
use native_http::NativeFixture;
use serde_json::json;
use std::{
    io::Write,
    path::Path,
    process::{Command, Stdio},
};
#[path = "support/form_browser.rs"]
mod form_browser;
use form_browser::{MANY, build_assets, call, create, order_with_lines};

/// Keyboard, permission, revocation and bounded list/large form timings on the installed shell.
#[ignore = "requires Chrome and SHELL_ACCEPTANCE_OUTPUT"]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn installed_shell_keyboard_access_and_request_timings() {
    let assets = client_assets();
    let fixture = NativeFixture::start_with_assets(|addons| {
        let dir = addons.join("shell_view_checks");
        std::fs::create_dir_all(dir.join("views")).unwrap();
        std::fs::write(dir.join("manifest.toml"), "name='Shell view checks'\nversion='1.0.0'\ndepends=['web_map','sale_management']\ndata=['views/views.xml']\n").unwrap();
        std::fs::write(dir.join("views/views.xml"), r#"<erp>
            <record id="order_activity" model="ir.ui.view"><field name="name">Order activities</field><field name="model">sale.order</field>
                <field name="arch" type="xml"><activity><field name="name"/></activity></field></record>
            <record id="order_activity_search" model="ir.ui.view"><field name="name">Search order activities</field><field name="model">sale.order</field>
                <field name="arch" type="xml"><search><field name="name"/></search></field></record>
            <record id="activity_action" model="ir.actions.act_window"><field name="name">Order activities</field><field name="res_model">sale.order</field>
                <field name="view_mode">activity</field><field name="search_view_id" ref="order_activity_search"/></record>
            <record id="order_activity_binding" model="ir.actions.act_window.view"><field name="act_window_id" ref="activity_action"/><field name="view_id" ref="order_activity"/><field name="view_mode">activity</field><field name="sequence" eval="1"/></record>
            <menuitem id="activity_menu" parent="contacts.menu_contacts_root" name="Order activities" action="activity_action" groups="base.group_user"/>
        </erp>"#).unwrap();
    }, Some(assets.path())).await;
    install_fixture(&fixture, "shell_view_checks");
    let company = xid(&fixture, "base", "main_company").await;
    let contacts = xid(&fixture, "contacts", "action_contacts").await;
    let sales = xid(&fixture, "sale", "action_quotations").await;
    let arabic = xid(&fixture, "base", "lang_ar").await;
    call(
        &fixture,
        "res.lang",
        "write",
        company,
        json!({"ids":[arabic],"vals":{"active":true}}),
    );
    let mut partners = Vec::new();
    for batch in 0..6 {
        partners.extend(create(
            &fixture,
            "res.partner",
            company,
            (0..100)
                .map(|i| json!({"name":format!("Acceptance port {:04}",batch*100+i)}))
                .collect(),
        ));
    }
    let products = create(
        &fixture,
        "product.product",
        company,
        vec![json!({"name":"Shell measurement product","type":"consu","list_price":10})],
    );
    let order = order_with_lines(&fixture, company, partners[0], &products, MANY);
    let salesperson = xid(&fixture, "sales_team", "group_sale_salesman").await;
    let reader = create(
        &fixture,
        "res.users",
        company,
        vec![
            json!({"name":"Shell acceptance reader","login":"shell-reader","password":"fixture-shell-reader","partner_id":partners[1],"lang":"ar_001","tz":"Asia/Jakarta","company_id":company,"company_ids":[[6,0,[company]]],"group_ids":[[6,0,[salesperson]]]}),
        ],
    )[0];
    let map = xid(&fixture, "web_map", "contact_map_action").await;
    let activity = xid(&fixture, "shell_view_checks", "activity_action").await;
    let private = partners[2];
    call(
        &fixture,
        "res.partner",
        "write",
        company,
        json!({"ids":[partners[0],private], "vals":{"map_located":true,"map_latitude":-6.2,"map_longitude":106.8}}),
    );
    let todo = xid(&fixture, "mail", "mail_activity_data_call").await;
    let admin: i64 = fixture
        .client
        .query_one(
            "SELECT id::int8 FROM res_users WHERE login=$1",
            &[&"fixture-admin"],
        )
        .await
        .unwrap()
        .get(0);
    let private_order = create(
        &fixture,
        "sale.order",
        company,
        vec![json!({"partner_id":private, "company_id":company, "user_id":admin})],
    )[0];
    let order_name: String = fixture
        .client
        .query_one("SELECT name FROM sale_order WHERE id=$1", &[&order])
        .await
        .unwrap()
        .get(0);
    create(&fixture, "mail.activity", company, [order, private_order].iter().map(|id| json!({"res_model":"sale.order","res_id":id,"activity_type_id":todo,"user_id":reader,"date_deadline":"2026-11-26","summary":format!("Review order {id}")})).collect());
    // The installed sales policy lets a salesperson read their own and unassigned orders.
    call(
        &fixture,
        "sale.order",
        "write",
        company,
        json!({"ids":[order],"vals":{"user_id":reader}}),
    );
    let model = xid(&fixture, "base", "model_res_partner").await;
    create(
        &fixture,
        "ir.access",
        company,
        vec![
            json!({"name":"Shell private port","model_id":model,"operation":"r","domain":format!("[('id','!=',{private})]")}),
        ],
    );
    fixture.client.batch_execute(&format!("CREATE FUNCTION shell_test_refuse_logout() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'fixture revocation failure'; END $$;
        CREATE TRIGGER shell_test_refuse_logout BEFORE DELETE ON erp_session FOR EACH ROW WHEN (OLD.user_id = {reader}) EXECUTE FUNCTION shell_test_refuse_logout()")).await.unwrap();
    let status = run_browser(
        &fixture,
        json!({
            "scenario":"shell","arabic":arabic,"contacts":contacts,"sales":sales,"map":map,"activity":activity,"partner":partners[0],
            "order":order,"order_name":order_name,"lines":MANY,"list_records":partners.len(),
        }),
    );
    fixture.cleanup().await;
    assert!(status.success(), "installed shell browser failed");
}

fn repo_root() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
}
fn client_assets() -> tempfile::TempDir {
    let compiler = Path::new(env!("CARGO_BIN_EXE_erp")).with_file_name("erp-ui-compile");
    build_assets(repo_root(), &compiler)
}
fn run_browser(fixture: &NativeFixture, mut args: serde_json::Value) -> std::process::ExitStatus {
    let erp = Path::new(env!("CARGO_BIN_EXE_erp"));
    args["profile"] = json!(erp.parent().unwrap().file_name().unwrap().to_str().unwrap());
    args["origin"] = json!(format!("http://127.0.0.1:{}", fixture.port));
    let mut child = Command::new("bash").current_dir(repo_root())
        .args(["-ec", "eval \"$(scripts/web-toolchain.sh env)\"; exec node web/scripts/browser-installed-shell.mjs"])
        .stdin(Stdio::piped()).spawn().unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(args.to_string().as_bytes())
        .unwrap();
    child.wait().unwrap()
}
fn install_fixture(fixture: &NativeFixture, name: &str) {
    let installed = native_http::request_within(
        fixture.port,
        "POST",
        "/erp/module/install",
        &[
            ("cookie", &fixture.cookies),
            ("content-type", "application/json"),
            ("sec-fetch-site", "same-origin"),
        ],
        &json!({"modules":[name]}).to_string(),
        std::time::Duration::from_secs(450),
    );
    assert_eq!(installed.status, 200, "install: {}", installed.body);
}
async fn xid(fixture: &NativeFixture, module: &str, name: &str) -> i64 {
    fixture
        .client
        .query_one(
            "SELECT res_id FROM ir_model_data WHERE module=$1 AND name=$2",
            &[&module, &name],
        )
        .await
        .unwrap()
        .get(0)
}

/// A served card projected through the pinned guest and checked independently for two real users.
#[ignore = "requires Chrome, SHELL_ACCEPTANCE_OUTPUT and a pinned ERP_UI_CARD_GUEST"]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn installed_restricted_card_uses_the_viewers_authorized_fields() {
    let assets = client_assets();
    let fixture = NativeFixture::start_with_assets(|addons| {
        let dir = addons.join("served_card_checks");
        std::fs::create_dir_all(dir.join("views")).unwrap();
        std::fs::write(dir.join("manifest.toml"), "name='Served card checks'\nversion='1.0.0'\ndepends=['web']\ndata=['views/cards.xml']\n").unwrap();
        std::fs::write(dir.join("views/cards.xml"), r#"<erp>
            <record id="restricted_cards" model="ir.ui.view"><field name="name">Restricted port cards</field><field name="model">res.partner</field>
                <field name="arch" type="xml"><kanban><templates><t t-name="card"><field name="name"/></t></templates>
                  <restricted_templates><t t-name="card"><strong class="shell-card-name"><t t-esc="record.name.value"/></strong></t></restricted_templates>
                </kanban></field></record>
            <record id="cards_action" model="ir.actions.act_window"><field name="name">Port cards</field><field name="res_model">res.partner</field>
                <field name="view_mode">kanban,form</field><field name="domain">[('name','ilike','WASI port')]</field></record>
            <record id="card_binding" model="ir.actions.act_window.view"><field name="act_window_id" ref="cards_action"/><field name="view_id" ref="restricted_cards"/><field name="view_mode">kanban</field></record>
            <menuitem id="card_menu" name="Port cards" action="cards_action" groups="base.group_user"/>
            <record id="undeclared_card" model="ir.ui.view"><field name="name">Group-protected card projection</field><field name="model">res.partner</field>
                <field name="arch" type="xml"><kanban><templates><t t-name="card"><field name="name"/><field name="email" groups="base.group_system"/></t></templates>
                  <restricted_templates><t t-name="card"><strong><t t-esc="record.email.value"/></strong></t></restricted_templates>
                </kanban></field></record>
        </erp>"#).unwrap();
    }, Some(assets.path())).await;
    install_fixture(&fixture, "served_card_checks");
    let company = xid(&fixture, "base", "main_company").await;
    let group = xid(&fixture, "base", "group_user").await;
    let contacts = create(
        &fixture,
        "res.partner",
        company,
        vec![
            json!({"name":"WASI port & quay"}),
            json!({"name":"WASI port private"}),
            json!({"name":"Card reader"}),
        ],
    );
    create(
        &fixture,
        "res.users",
        company,
        vec![json!({
            "name":"Card reader","login":"shell-reader","password":"fixture-shell-reader","partner_id":contacts[2],
            "lang":"en_US","tz":"UTC","company_id":company,"company_ids":[[6,0,[company]]],"group_ids":[[6,0,[group]]]
        })],
    );
    let model = xid(&fixture, "base", "model_res_partner").await;
    create(
        &fixture,
        "ir.access",
        company,
        vec![
            json!({"name":"Private card", "model_id":model, "operation":"r", "domain":format!("[('id','!=',{})]",contacts[1])}),
        ],
    );
    let action = xid(&fixture, "served_card_checks", "cards_action").await;
    let refused = xid(&fixture, "served_card_checks", "undeclared_card").await;
    let status = run_browser(
        &fixture,
        json!({"scenario":"restricted-cards","cards":action,"refused_card":refused}),
    );
    fixture.cleanup().await;
    assert!(status.success(), "installed restricted card browser failed");
}
