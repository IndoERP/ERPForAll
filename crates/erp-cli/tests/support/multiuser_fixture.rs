// SPDX-License-Identifier: MIT OR Apache-2.0
//! Real native multi-user profile. Credentials are disposable fixture values and never formatted.
#![allow(
    dead_code,
    reason = "shared acceptance fixture; consumers use different helpers"
)]

use crate::native_http::{self, NativeFixture, Reply};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    panic::{AssertUnwindSafe, catch_unwind, resume_unwind},
    path::PathBuf,
};

/// Explicit ingress subject: anonymous calls cannot fall back to a setup session.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Subject {
    Admin,
    A,
    B,
    Anonymous,
}

/// Ordinary authenticated subject. Cookies deliberately have no Debug implementation or getter.
pub struct User {
    pub id: i64,
    pub login: String,
    pub company: i64,
    pub companies: Vec<i64>,
    cookies: String,
}

/// Borrowed owner keeps the provisioning session separate from every acceptance subject.
pub struct MultiuserFixture<'a> {
    pub native: &'a NativeFixture,
    users: [User; 3],
    groups: BTreeMap<String, i64>,
    installed: BTreeSet<String>,
}

/// Decode only an explicitly successful response. Bodies are omitted from diagnostics: a regression
/// might return credential material where an error was expected.
pub fn success(reply: &Reply) -> Value {
    assert_eq!(
        reply.status, 200,
        "multiuser request did not succeed (body redacted)"
    );
    serde_json::from_str(&reply.body).expect("multiuser response must be JSON")
}

/// Precise negative assertion without ever printing a successful response or private headers.
pub fn error(reply: &Reply, status: u16, kind: &str, message: &str) {
    assert_eq!(
        reply.status, status,
        "unexpected HTTP status (body redacted)"
    );
    let body: Value = serde_json::from_str(&reply.body).expect("multiuser error must be JSON");
    assert_eq!(body["name"], kind);
    assert_eq!(body["message"], message);
    assert!(
        body.get("values").is_none(),
        "error exposed successful values"
    );
}

fn setup_rpc(native: &NativeFixture, model: &str, method: &str, body: Value) -> Value {
    let reply = native_http::json_post(
        native.port,
        &native.cookies,
        &format!("/erp/json/2/{model}/{method}"),
        &body.to_string(),
    );
    if reply.status != 200 {
        let error: Value = serde_json::from_str(&reply.body).expect("setup error JSON");
        let mut message = error["message"]
            .as_str()
            .expect("setup error message")
            .to_owned();
        if let Some(values) = body.get("vals_list").and_then(Value::as_array) {
            for values in values {
                if let Some(password) = values.get("password").and_then(Value::as_str) {
                    message = message.replace(password, "[redacted]");
                }
            }
        }
        panic!(
            "fixture setup {model}/{method}: HTTP {}, {}: {}",
            reply.status, error["name"], message
        );
    }
    success(&reply)
}

fn create(native: &NativeFixture, model: &str, values: Value) -> i64 {
    setup_rpc(native, model, "create", json!({"vals_list":[values]}))[0]
        .as_i64()
        .expect("created record identity")
}

fn relation_id(value: &Value) -> i64 {
    match value {
        Value::Number(n) => n.as_i64().expect("integer relation identity"),
        Value::Array(values) => values
            .first()
            .expect("relation pair")
            .as_i64()
            .expect("relation identity"),
        _ => panic!("expected an integer or relation pair"),
    }
}

impl<'a> MultiuserFixture<'a> {
    async fn prepare(native: &'a NativeFixture) -> Self {
        let before = native_http::catalogue(native.port, &native.cookies);
        let required = ["base", "contacts", "product", "uom", "mail", "web"];
        let mut missing = Vec::new();
        for name in required {
            let state = native_http::state_of(&before, name)
                .expect("required addon absent from native catalogue");
            if state != "installed" {
                missing.push(name);
            }
        }
        if !missing.is_empty() {
            success(&native_http::json_post(
                native.port,
                &native.cookies,
                "/erp/module/install",
                &json!({"modules":missing}).to_string(),
            ));
        }
        let after = native_http::catalogue(native.port, &native.cookies);
        let installed: BTreeSet<String> = after["addons"]
            .as_array()
            .expect("addon catalogue")
            .iter()
            .filter(|row| row["state"] == "installed")
            .map(|row| row["name"].as_str().expect("addon name").to_owned())
            .collect();
        let expected = BTreeSet::from_iter(
            [
                "base",
                "contacts",
                "mail",
                "product",
                "product_mail",
                "uom",
                "web",
            ]
            .into_iter()
            .map(str::to_owned),
        );
        assert_eq!(
            installed, expected,
            "unexpected installed acceptance profile"
        );
        let mut groups = BTreeMap::new();
        for name in ["group_user", "group_system", "group_erp_manager"] {
            let id: i64 = native.client.query_one(
                "SELECT res_id FROM ir_model_data WHERE module='base' AND name=$1 AND model='res.groups'", &[&name]
            ).await.expect("fixture group XML identity").get(0);
            groups.insert(name.to_owned(), id);
        }
        // Privileged SQL is restricted to locating setup identities; all provisioning mutations
        // and every acceptance operation go through the native application boundary.
        let bootstrap_id: i64 = native
            .client
            .query_one(
                "SELECT id FROM res_users WHERE login=$1",
                &[&"fixture-admin"],
            )
            .await
            .expect("bootstrap identity")
            .get(0);
        let bootstrap = setup_rpc(
            native,
            "res.users",
            "read",
            json!({"ids":[bootstrap_id], "fields":["company_id"]}),
        );
        let main_company = relation_id(&bootstrap[0]["company_id"]);
        let company = setup_rpc(
            native,
            "res.company",
            "read",
            json!({"ids":[main_company], "fields":["currency_id"]}),
        );
        let currency = relation_id(&company[0]["currency_id"]);
        let a = create(
            native,
            "res.company",
            json!({"name":"Multiuser Company A", "currency_id":currency}),
        );
        let b = create(
            native,
            "res.company",
            json!({"name":"Multiuser Company B", "currency_id":currency}),
        );
        setup_rpc(
            native,
            "res.users",
            "write",
            json!({"ids":[bootstrap_id], "vals":{"company_ids":[[6,0,[main_company,a,b]]]}}),
        );
        let specs = [
            (
                "multiuser-admin",
                main_company,
                vec![main_company, a, b],
                groups["group_system"],
            ),
            ("multiuser-a", a, vec![a], groups["group_user"]),
            ("multiuser-b", b, vec![b], groups["group_user"]),
        ];
        let mut users = Vec::new();
        for (login, company, companies, group) in specs {
            // Synthetic fixture-only password intentionally exercises URL form encoding.
            let password = format!("{} & = fixture £ password", native.db.name());
            let partner = create(
                native,
                "res.partner",
                json!({"name":login, "company_id":company}),
            );
            let id = create(
                native,
                "res.users",
                json!({
                    "name":login, "login":login, "password":password, "partner_id":partner,
                    "company_id":company, "company_ids":[[6,0,companies]], "group_ids":[[6,0,[group]]],
                    "lang":"en_US", "tz":"UTC"
                }),
            );
            assert_ne!(
                id,
                erp_orm::SUPERUSER_ID.get(),
                "acceptance subject must not be System"
            );
            let cookies = login_session(native, login, &password);
            users.push(User {
                id,
                login: login.to_owned(),
                company,
                companies,
                cookies,
            });
        }
        let users: [User; 3] = match users.try_into() {
            Ok(users) => users,
            Err(_) => panic!("fixture must provision exactly three authenticated subjects"),
        };
        let fixture = Self {
            native,
            users,
            groups,
            installed,
        };
        fixture.verify_subjects();
        fixture
    }

    fn verify_subjects(&self) {
        for subject in [Subject::Admin, Subject::A, Subject::B] {
            let user = self.user(subject);
            let values = success(&self.rpc(Subject::Admin, "res.users", "read", json!({
                "ids":[user.id], "fields":["active", "share", "group_ids", "all_group_ids", "company_id", "company_ids"]
            })));
            let row = &values[0];
            assert_eq!(row["active"], true, "fixture subject is inactive");
            assert_eq!(
                row["share"], false,
                "internal subject must not be a share user"
            );
            let explicit: BTreeSet<_> = row["group_ids"]
                .as_array()
                .expect("explicit groups")
                .iter()
                .map(|id| id.as_i64().expect("group id"))
                .collect();
            let effective: BTreeSet<_> = row["all_group_ids"]
                .as_array()
                .expect("effective groups")
                .iter()
                .map(|id| id.as_i64().expect("group id"))
                .collect();
            let expected = match subject {
                Subject::Admin => self.group("group_system"),
                Subject::A | Subject::B => self.group("group_user"),
                Subject::Anonymous => unreachable!(),
            };
            assert_eq!(
                explicit,
                BTreeSet::from([expected]),
                "unexpected explicit membership"
            );
            assert!(
                effective.contains(&self.group("group_user")),
                "subject is not internal"
            );
            match subject {
                Subject::Admin => assert!(effective.is_superset(&BTreeSet::from([
                    self.group("group_system"),
                    self.group("group_erp_manager")
                ]))),
                Subject::A | Subject::B => {
                    assert_eq!(effective, BTreeSet::from([self.group("group_user")]))
                }
                Subject::Anonymous => unreachable!(),
            }
            assert_eq!(relation_id(&row["company_id"]), user.company);
            let allowed: BTreeSet<_> = row["company_ids"]
                .as_array()
                .expect("allowed companies")
                .iter()
                .map(|id| id.as_i64().expect("company id"))
                .collect();
            assert_eq!(allowed, BTreeSet::from_iter(user.companies.iter().copied()));
            let bootstrap = self.bootstrap(subject);
            assert_eq!(bootstrap["identity"]["uid"], user.id);
            assert_eq!(bootstrap["current_company_id"], user.company);
            let allowed: BTreeSet<_> = bootstrap["allowed_company_ids"]
                .as_array()
                .expect("resolved companies")
                .iter()
                .map(|id| id.as_i64().expect("company id"))
                .collect();
            assert_eq!(allowed, BTreeSet::from_iter(user.companies.iter().copied()));
        }
        assert!(
            self.sessions_are_distinct(),
            "subject sessions are not independent"
        );
    }

    pub fn user(&self, subject: Subject) -> &User {
        match subject {
            Subject::Admin => &self.users[0],
            Subject::A => &self.users[1],
            Subject::B => &self.users[2],
            Subject::Anonymous => panic!("anonymous subject has no authenticated user"),
        }
    }
    pub fn installed(&self) -> &BTreeSet<String> {
        &self.installed
    }
    pub fn group(&self, name: &str) -> i64 {
        *self.groups.get(name).expect("unknown fixture group")
    }
    pub fn sessions_are_distinct(&self) -> bool {
        let sessions: BTreeSet<_> = self
            .users
            .iter()
            .map(|user| {
                user.cookies
                    .split("; ")
                    .find(|part| part.starts_with("session_id="))
                    .expect("authenticated session cookie")
            })
            .collect();
        sessions.len() == self.users.len()
    }
    pub fn rpc(&self, subject: Subject, model: &str, method: &str, body: Value) -> Reply {
        let path = format!("/erp/json/2/{model}/{method}");
        match subject {
            Subject::Anonymous => native_http::request(
                self.native.port,
                "POST",
                &path,
                &[
                    ("content-type", "application/json"),
                    ("sec-fetch-site", "same-origin"),
                ],
                &body.to_string(),
            ),
            Subject::Admin | Subject::A | Subject::B => native_http::json_post(
                self.native.port,
                &self.user(subject).cookies,
                &path,
                &body.to_string(),
            ),
        }
    }
    pub fn bootstrap(&self, subject: Subject) -> Value {
        let reply = native_http::request(
            self.native.port,
            "GET",
            "/erp",
            &[("cookie", &self.user(subject).cookies)],
            "",
        );
        assert_eq!(
            reply.status, 200,
            "authenticated bootstrap failed (body redacted)"
        );
        native_http::embedded(&reply, "erp-bootstrap")
    }
    pub fn cleanup_probe(&self) -> CleanupProbe {
        CleanupProbe::capture(self.native)
    }
}

fn login_session(native: &NativeFixture, login: &str, password: &str) -> String {
    let login_page = native_http::request(native.port, "GET", "/erp/login", &[], "");
    assert_eq!(login_page.status, 200, "login form unavailable");
    let prefs = native_http::cookie(&login_page, "prefs");
    let data = native_http::embedded(&login_page, "erp-login");
    let fields = [
        ("db", native.db.name()),
        ("login", login),
        ("password", password),
        (
            "csrf_token",
            data["csrf"].as_str().expect("login CSRF token"),
        ),
    ];
    let form = fields
        .into_iter()
        .map(|(name, value)| format!("{name}={}", native_http::encoded(value.as_bytes())))
        .collect::<Vec<_>>()
        .join("&");
    let logged = native_http::request(
        native.port,
        "POST",
        "/erp/login",
        &[
            ("cookie", &prefs),
            ("content-type", "application/x-www-form-urlencoded"),
        ],
        &form,
    );
    assert_eq!(
        logged.status, 303,
        "ordinary subject login failed (body redacted)"
    );
    format!("{prefs}; {}", native_http::cookie(&logged, "session_id"))
}

/// Run the whole fixture on a large-stack thread; always perform explicit cleanup after any
/// failure in the new setup or body. NativeFixture startup's existing failure handling is inherited.
pub fn run<T: Send + 'static>(body: impl FnOnce(&MultiuserFixture<'_>) -> T + Send + 'static) -> T {
    run_with_setup(|_| {}, body)
}
pub fn run_with_setup<T: Send + 'static>(
    setup: impl FnOnce(&NativeFixture) + Send + 'static,
    body: impl FnOnce(&MultiuserFixture<'_>) -> T + Send + 'static,
) -> T {
    let thread = std::thread::Builder::new()
        .stack_size(16 * 1024 * 1024)
        .spawn(move || {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .expect("fixture runtime");
            let native = runtime.block_on(NativeFixture::start());
            let result = catch_unwind(AssertUnwindSafe(|| {
                setup(&native);
                let fixture = runtime.block_on(MultiuserFixture::prepare(&native));
                body(&fixture)
            }));
            runtime.block_on(native.cleanup());
            match result {
                Ok(value) => value,
                Err(failure) => resume_unwind(failure),
            }
        })
        .expect("fixture thread");
    match thread.join() {
        Ok(value) => value,
        Err(failure) => resume_unwind(failure),
    }
}

/// Only nonsecret resource identities leave the fixture; the database connection stays private.
pub struct CleanupProbe {
    pub database: String,
    pub role: String,
    pub directory: PathBuf,
    port: u16,
    config: erp_testkit::tokio_postgres::Config,
}
impl CleanupProbe {
    pub fn capture(native: &NativeFixture) -> Self {
        let mut config = native.db.config();
        config.dbname("postgres");
        Self {
            database: native.db.name().to_owned(),
            role: native.role.clone(),
            directory: native.dir.path().to_owned(),
            port: native.port,
            config,
        }
    }
    pub fn assert_removed(&self) {
        match std::fs::symlink_metadata(&self.directory) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => panic!("could not inspect fixture directory: {error}"),
            Ok(_) => panic!("fixture directory leaked"),
        }
        match std::net::TcpStream::connect(("127.0.0.1", self.port)) {
            Err(error) if error.kind() == std::io::ErrorKind::ConnectionRefused => {}
            Err(error) => panic!("could not inspect fixture listener: {error}"),
            Ok(_) => panic!("fixture process still listens"),
        }
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("cleanup inspection runtime");
        runtime.block_on(async {
            let (client, connection) = self
                .config
                .connect(erp_testkit::tokio_postgres::NoTls)
                .await
                .expect("cleanup inspection connection");
            let driver =
                tokio::spawn(async move { connection.await.expect("cleanup inspection driver") });
            let db: bool = client
                .query_one(
                    "SELECT EXISTS(SELECT FROM pg_database WHERE datname=$1)",
                    &[&self.database],
                )
                .await
                .unwrap()
                .get(0);
            assert!(!db, "fixture database leaked");
            for role in [
                &self.role,
                &format!("{}_node", self.role),
                &format!("{}_feature", self.role),
            ] {
                let exists: bool = client
                    .query_one(
                        "SELECT EXISTS(SELECT FROM pg_roles WHERE rolname=$1)",
                        &[role],
                    )
                    .await
                    .unwrap()
                    .get(0);
                assert!(!exists, "fixture role leaked");
            }
            drop(client);
            driver.await.expect("cleanup inspection driver task");
        });
    }
}
