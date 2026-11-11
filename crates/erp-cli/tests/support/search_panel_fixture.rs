//! Native search-panel fixtures: real addon installation, session login and API-key ingress.
//! All credentials are fixture-only values held in the test's isolated database/directory.
use crate::native_http::{self, NativeFixture, Reply};
use serde_json::{Value, json};
use std::{fs, path::Path};

pub const ITEM: &str = "panel_http.item";
pub const CATEGORY: &str = "panel_http.category";
pub const TAG: &str = "panel_http.tag";
pub const RANGE: &str = "search_panel_select_range";
pub const MULTI: &str = "search_panel_select_multi_range";

pub struct PanelIds {
    pub root: i64,
    pub child: i64,
    pub other: i64,
    pub hidden_category: i64,
    pub cool_tag: i64,
    pub warm_tag: i64,
    pub unused_tag: i64,
    pub hidden_tag: i64,
    pub items: Vec<i64>,
    pub company: i64,
}

pub struct PanelFixture {
    pub native: NativeFixture,
    pub ids: PanelIds,
    pub user_id: i64,
    pub user_cookies: String,
}

pub fn decoded(reply: &Reply) -> Value {
    serde_json::from_str(&reply.body).expect("native JSON response")
}

pub fn success(reply: &Reply) -> Value {
    assert_eq!(reply.status, 200, "HTTP response: {}", reply.body);
    decoded(reply)
}

impl PanelFixture {
    pub async fn start() -> Self {
        Self::start_with(|_| {}).await
    }

    /// Opt-in test preparation after the default addon is written, before catalogue/install.
    pub async fn start_with(prepare: impl FnOnce(&Path)) -> Self {
        let native = NativeFixture::start_with_addons(|addons| {
            write_addon(addons);
            prepare(addons);
        })
        .await;
        let install = native_http::json_post(
            native.port,
            &native.cookies,
            "/erp/module/install",
            r#"{"modules":["panel_http"]}"#,
        );
        let installed = success(&install);
        assert!(
            installed["installed"]
                .as_array()
                .unwrap()
                .iter()
                .any(|name| name == "panel_http")
        );
        let company = native
            .client
            .query_one(
                "SELECT company_id FROM res_users WHERE login='fixture-admin'",
                &[],
            )
            .await
            .unwrap()
            .get::<_, i64>(0);
        let ids = PanelIds {
            root: xmlid(&native, "root").await,
            child: xmlid(&native, "child").await,
            other: xmlid(&native, "other").await,
            hidden_category: xmlid(&native, "hidden_category").await,
            cool_tag: xmlid(&native, "cool_tag").await,
            warm_tag: xmlid(&native, "warm_tag").await,
            unused_tag: xmlid(&native, "unused_tag").await,
            hidden_tag: xmlid(&native, "hidden_tag").await,
            items: vec![
                xmlid(&native, "item_one").await,
                xmlid(&native, "item_two").await,
                xmlid(&native, "item_three").await,
                xmlid(&native, "hidden_item").await,
            ],
            company,
        };
        // Create an ordinary subject through the installed server's normal ORM boundary.
        let user_group: i64 = native
            .client
            .query_one(
                "SELECT res_id FROM ir_model_data WHERE module='base' AND name='group_user'",
                &[],
            )
            .await
            .unwrap()
            .get(0);
        let partner = native_http::json_post(
            native.port,
            &native.cookies,
            "/erp/json/2/res.partner/create",
            &json!({"vals_list":[{"name":"Panel Reader","company_id":company}]}).to_string(),
        );
        let partner_id = success(&partner)[0].as_i64().unwrap();
        let user = native_http::json_post(native.port, &native.cookies,
            "/erp/json/2/res.users/create", &json!({"vals_list":[{
                "name":"Panel Reader", "login":"panel-reader", "password":"fixture-panel-reader-password",
                "partner_id":partner_id, "company_id":company, "company_ids":[[6,0,[company]]],
                "group_ids":[[6,0,[user_group]]], "lang":"en_US", "tz":"UTC"
            }]}).to_string());
        let user_id = success(&user)[0].as_i64().unwrap();
        let login = native_http::request(native.port, "GET", "/erp/login", &[], "");
        let prefs = native_http::cookie(&login, "prefs");
        let login_data = native_http::embedded(&login, "erp-login");
        let logged = native_http::request(
            native.port,
            "POST",
            "/erp/login",
            &[
                ("cookie", &prefs),
                ("content-type", "application/x-www-form-urlencoded"),
            ],
            &format!(
                "db={}&login=panel-reader&password=fixture-panel-reader-password&csrf_token={}",
                native.db.name(),
                native_http::encoded(login_data["csrf"].as_str().unwrap().as_bytes())
            ),
        );
        assert_eq!(logged.status, 303, "ordinary fixture login failed");
        let user_cookies = format!("{prefs}; {}", native_http::cookie(&logged, "session_id"));
        Self {
            native,
            ids,
            user_id,
            user_cookies,
        }
    }

    pub fn session(&self, method: &str, body: Value) -> Reply {
        native_http::json_post(
            self.native.port,
            &self.native.cookies,
            &format!("/erp/json/2/{ITEM}/{method}"),
            &body.to_string(),
        )
    }

    pub fn user_session(&self, method: &str, body: Value) -> Reply {
        native_http::json_post(
            self.native.port,
            &self.user_cookies,
            &format!("/erp/json/2/{ITEM}/{method}"),
            &body.to_string(),
        )
    }

    pub fn bearer(&self, method: &str, body: Value, token: &str) -> Reply {
        native_http::request(
            self.native.port,
            "POST",
            &format!("/erp/json/2/{ITEM}/{method}"),
            &[
                ("authorization", token),
                ("content-type", "application/json"),
            ],
            &body.to_string(),
        )
    }

    pub async fn key(&self, uid: i64) -> String {
        let secret: String = self
            .native
            .client
            .query_one(
                "SELECT value FROM ir_config_parameter WHERE key='database.secret'",
                &[],
            )
            .await
            .unwrap()
            .get(0);
        let secret = erp_security::auth::secret::DatabaseSecret::from_stored(&secret).unwrap();
        let hasher = erp_security::auth::apikey::ApiKeyHasher::derive(&secret).unwrap();
        let key = erp_security::auth::apikey::GeneratedApiKey::generate().unwrap();
        let digest = hasher.digest(&key.token());
        self.native.client.execute(
            "INSERT INTO res_users_apikeys(name,user_id,created_by,kind,scope,digest,digest_kid,prefix,read_only,capabilities,create_date) VALUES('panel-http-fixture',$1,$1,'user','rpc',$2,$3,$4,false,'subject',now() AT TIME ZONE 'utc')",
            &[&uid, &digest.bytes().as_slice(), &i32::from(digest.kid()), &key.token().display_prefix()],
        ).await.unwrap();
        format!("Bearer {}", key.expose())
    }

    pub async fn cleanup(self) {
        self.native.cleanup().await;
    }
}

async fn xmlid(native: &NativeFixture, name: &str) -> i64 {
    native
        .client
        .query_one(
            "SELECT res_id FROM ir_model_data WHERE module='panel_http' AND name=$1",
            &[&name],
        )
        .await
        .unwrap()
        .get(0)
}

fn write_addon(addons: &Path) {
    let root = addons.join("panel_http");
    fs::create_dir_all(root.join("models")).unwrap();
    fs::create_dir_all(root.join("security")).unwrap();
    fs::create_dir_all(root.join("data")).unwrap();
    fs::write(root.join("manifest.toml"), "name='Panel HTTP Fixture'\nversion='1.0.0'\nlicense='MIT OR Apache-2.0'\ndepends=['base']\ndata=['security/ir.access.csv','data/fixtures.xml']\n").unwrap();
    let mut models = String::new();
    for (name, label) in [
        ("category", "Panel Category"),
        ("tag", "Panel Tag"),
        ("item", "Panel Item"),
    ] {
        models.push_str(&format!(
            "[[model]]\nname='panel_http.{name}'\ndescription='{label}'\norder='id'\n"
        ));
        if name == "item" {
            models.push_str(
                "[model.company_access]\ndirect={field='company_id',branch='exact',empty='deny'}\n",
            );
        }
        models.push_str("[[model.field]]\nname='name'\ntype='char'\nrequired=true\n[[model.field]]\nname='visible'\ntype='boolean'\ndefault=true\n");
        match name {
            "category" => models.push_str("[[model.field]]\nname='parent_id'\ntype='many2one'\ncomodel='panel_http.category'\n[[model.field]]\nname='secret_name'\ntype='char'\ngroups='base.group_system'\n"),
            "tag" => models.push_str("[[model.field]]\nname='family'\ntype='selection'\nselection=[['cool','Cool'],['warm','Warm']]\n[[model.field]]\nname='secret_family'\ntype='selection'\nselection=[['cool','Cool'],['warm','Warm']]\ngroups='base.group_system'\n"),
            "item" => models.push_str("[[model.field]]\nname='state'\ntype='selection'\nselection=[['alpha','Alpha'],['beta','Beta']]\n[[model.field]]\nname='secret_state'\ntype='selection'\nselection=[['alpha','Alpha'],['beta','Beta']]\ngroups='base.group_system'\n[[model.field]]\nname='category_id'\ntype='many2one'\ncomodel='panel_http.category'\n[[model.field]]\nname='tag_ids'\ntype='many2many'\ncomodel='panel_http.tag'\nrelation='panel_http_item_tag_rel'\ncolumn1='item_id'\ncolumn2='tag_id'\n[[model.field]]\nname='company_id'\ntype='many2one'\ncomodel='res.company'\nrequired=true\n"),
            _ => unreachable!("fixed fixture model vocabulary"),
        }
    }
    fs::write(root.join("models/panel.toml"), models).unwrap();
    let mut access = String::from("id,name,model_id/id,group_id/id,operation,domain\n");
    for model in ["item", "category", "tag"] {
        access.push_str(&format!("{model}_user,Visible {model},panel_http.model_panel_http_{model},base.group_user,crud,\"[('visible', '=', True)]\"\n{model}_system,All {model},panel_http.model_panel_http_{model},base.group_system,crud,\n"));
    }
    fs::write(root.join("security/ir.access.csv"), access).unwrap();
    fs::write(root.join("data/fixtures.xml"), FIXTURES).unwrap();
}

const FIXTURES: &str = r#"<erp>
<record id="root" model="panel_http.category"><field name="name">Root</field></record>
<record id="child" model="panel_http.category"><field name="name">Child</field><field name="parent_id" ref="root"/></record>
<record id="other" model="panel_http.category"><field name="name">Other</field></record>
<record id="hidden_category" model="panel_http.category"><field name="name">Hidden category</field><field name="visible" eval="False"/></record>
<record id="cool_tag" model="panel_http.tag"><field name="name">Cool tag</field><field name="family">cool</field><field name="secret_family">cool</field></record>
<record id="warm_tag" model="panel_http.tag"><field name="name">Warm tag</field><field name="family">warm</field><field name="secret_family">warm</field></record>
<record id="unused_tag" model="panel_http.tag"><field name="name">Unused tag</field><field name="family">cool</field></record>
<record id="hidden_tag" model="panel_http.tag"><field name="name">Hidden tag</field><field name="family">warm</field><field name="visible" eval="False"/></record>
<record id="item_one" model="panel_http.item"><field name="name">One</field><field name="state">alpha</field><field name="secret_state">alpha</field><field name="category_id" ref="child"/><field name="tag_ids" eval="[(6,0,[ref('cool_tag'),ref('warm_tag')])]"/><field name="company_id" ref="base.main_company"/></record>
<record id="item_two" model="panel_http.item"><field name="name">Two</field><field name="state">alpha</field><field name="secret_state">alpha</field><field name="category_id" ref="root"/><field name="tag_ids" eval="[(6,0,[ref('cool_tag')])]"/><field name="company_id" ref="base.main_company"/></record>
<record id="item_three" model="panel_http.item"><field name="name">Three</field><field name="state">beta</field><field name="secret_state">beta</field><field name="category_id" ref="other"/><field name="tag_ids" eval="[(6,0,[ref('warm_tag')])]"/><field name="company_id" ref="base.main_company"/></record>
<record id="hidden_item" model="panel_http.item"><field name="name">Hidden item</field><field name="state">beta</field><field name="secret_state">beta</field><field name="category_id" ref="hidden_category"/><field name="tag_ids" eval="[(6,0,[ref('hidden_tag')])]"/><field name="visible" eval="False"/><field name="company_id" ref="base.main_company"/></record>
</erp>"#;
