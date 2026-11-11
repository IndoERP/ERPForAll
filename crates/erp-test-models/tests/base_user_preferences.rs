//! The backend of the My Preferences dialog on the production policy: the
//! `ir.access` rows give an internal user their own `res.users` row and nobody else's, language
//! and time zone are checked against what the database offers, and `preference_save` answers as
//! says. The installed-data
//! half (the action and view records of `base`) is `crates/erp-cli/tests/user_preferences.rs`.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

mod users_support;

use std::sync::Arc;

use erp_orm::{Orm, PolicyProvider, UserId};
use erp_sdk::serde_json::json;
use erp_sdk::{Domain, ErrorKind, SearchOptions, Transaction, Value};
use erp_security::resolver::Resolver;
use erp_security::source::PolicySource;
use erp_testkit::Harness;
use erp_testkit::tokio_postgres::types::ToSql;
use erp_testkit::tokio_postgres::{Client, NoTls};
use users_support::{assert_error, create, harness, map, s};

async fn connect(h: &Harness) -> Client {
    let (client, connection) = h.db().config().connect(NoTls).await.unwrap();
    tokio::spawn(async move {
        if let Err(error) = connection.await {
            eprintln!("base_user_preferences: connection closed with an error: {error}");
        }
    });
    client
}

async fn one(client: &Client, sql: &str, params: &[&(dyn ToSql + Sync)]) -> i64 {
    client.query_one(sql, params).await.unwrap().get(0)
}

async fn exec(client: &Client, sql: &str, params: &[&(dyn ToSql + Sync)]) {
    client.execute(sql, params).await.unwrap();
}

async fn group(client: &Client, name: &str, xmlid: &str) -> i64 {
    let id = one(
        client,
        "INSERT INTO res_groups (name) VALUES ($1) RETURNING id::int8",
        &[&name],
    )
    .await;
    let (module, local) = xmlid.split_once('.').unwrap();
    exec(
        client,
        "INSERT INTO ir_model_data (module, name, model, res_id) VALUES ($1, $2, 'res.groups', $3)",
        &[&module, &local, &id],
    )
    .await;
    id
}

async fn model(client: &Client, name: &str, description: &str) -> i64 {
    one(
        client,
        "INSERT INTO ir_model (name, model, state, transient) VALUES ($1, $2, 'base', false) \
         RETURNING id::int8",
        &[&description, &name],
    )
    .await
}

async fn access(
    client: &Client,
    name: &str,
    model: i64,
    group: i64,
    ops: &str,
    domain: Option<&str>,
) {
    exec(
        client,
        "INSERT INTO ir_access (name, model_id, group_id, operation, domain, active) \
         VALUES ($1, $2, $3, $4, $5, true)",
        &[&name, &model, &group, &ops, &domain],
    )
    .await;
}

async fn new_user(client: &Client, login: &str, groups: &[i64]) -> i64 {
    let uid = one(
        client,
        "INSERT INTO res_users (name, login, active, share) VALUES ($1, $1, true, false) \
         RETURNING id::int8",
        &[&login],
    )
    .await;
    for g in groups {
        exec(
            client,
            "INSERT INTO res_groups_users_rel (uid, gid) VALUES ($1, $2)",
            &[&uid, g],
        )
        .await;
    }
    uid
}

fn pool(h: &Harness, size: usize) -> erp_testkit::deadpool_postgres::Pool {
    erp_orm::pool::create_pool(h.db().config(), size).unwrap()
}

async fn production_orm(h: &Harness) -> Orm {
    let source = PolicySource::build(Arc::clone(h.registry()), pool(h, 2))
        .await
        .unwrap();
    let provider: Arc<dyn PolicyProvider> = Arc::new(source);
    Orm::builder(Arc::clone(h.registry()), pool(h, 2))
        .policy(provider)
        .resolver(Resolver::id_order())
        .build()
        .unwrap()
}

async fn begin(orm: &Orm, uid: i64) -> Transaction {
    orm.begin(UserId::new(uid).unwrap()).await.unwrap()
}

/// A database with the access rows the `base` data files give for My Preferences
/// (`access_users_self_internal`, `access_lang_internal`, `access_users_system`) and no others,
/// four users and three languages.
struct Fixture {
    h: Harness,
    client: Client,
    orm: Orm,
    me: i64,
    other: i64,
    /// Internal user in `base.group_erp_manager`, not in `base.group_system`.
    manager: i64,
    /// Internal user in `base.group_system`.
    admin: i64,
    system_group: i64,
    /// `(name, code)` of the active languages created, in creation order.
    active_languages: Vec<(&'static str, &'static str)>,
}

async fn fixture() -> Fixture {
    let h = harness().await;
    let client = connect(&h).await;
    let internal = group(&client, "Internal User", "base.group_user").await;
    // the groups the field mask of res.users looks up by name
    let manager_group = group(&client, "Access Rights", "base.group_erp_manager").await;
    let system_group = group(&client, "Administrator", "base.group_system").await;
    let users_model = model(&client, "res.users", "User").await;
    let lang_model = model(&client, "res.lang", "Language").await;
    let partner_model = model(&client, "res.partner", "Contact").await;
    // the same rows as addons/base/security/ir.access.csv (and no others)
    access(
        &client,
        "internal users read and update their own record",
        users_model,
        internal,
        "ru",
        Some("[('id', '=', user.id)]"),
    )
    .await;
    access(
        &client,
        "internal users read languages",
        lang_model,
        internal,
        "r",
        None,
    )
    .await;
    // the only user-management row the shipped data has (`access_users_system`)
    access(
        &client,
        "administrators manage users",
        users_model,
        system_group,
        "crud",
        None,
    )
    .await;
    access(
        &client,
        "internal users read contacts",
        partner_model,
        internal,
        "r",
        None,
    )
    .await;
    let me = new_user(&client, "me", &[internal]).await;
    let other = new_user(&client, "other", &[internal]).await;
    let manager = new_user(&client, "manager", &[internal, manager_group]).await;
    let admin = new_user(&client, "admin", &[internal, system_group]).await;
    let orm = production_orm(&h).await;
    let active_languages = vec![
        ("Indonesian", "id_ID"),
        ("German", "de_DE"),
        ("English (US)", "en_US"),
    ];
    let mut tx = begin(&orm, erp_orm::SUPERUSER_ID.get()).await;
    for (name, code) in &active_languages {
        create(
            &mut tx,
            "res.lang",
            &[
                ("name", s(name)),
                ("code", s(code)),
                ("url_code", s(&code[..2])),
                ("active", Value::Bool(true)),
            ],
        )
        .await;
    }
    // installed in the catalogue but not active
    create(
        &mut tx,
        "res.lang",
        &[
            ("name", s("French")),
            ("code", s("fr_FR")),
            ("url_code", s("fr")),
            ("active", Value::Bool(false)),
        ],
    )
    .await;
    tx.commit().await.unwrap();
    drop(tx);
    Fixture {
        h,
        client,
        orm,
        me,
        other,
        manager,
        admin,
        system_group,
        active_languages,
    }
}

impl Fixture {
    async fn finish(self) {
        drop(self.client);
        self.h.cleanup().await.unwrap();
    }
}

fn pairs(value: &Value) -> Vec<(String, String)> {
    let Value::List(items) = value else {
        panic!("choices are a list, got {value:?}");
    };
    items
        .iter()
        .map(|item| {
            let Value::List(pair) = item else {
                panic!("a choice is a pair, got {item:?}");
            };
            let [Value::Str(code), Value::Str(label)] = &pair[..] else {
                panic!("a choice is two texts, got {pair:?}");
            };
            (code.to_string(), label.to_string())
        })
        .collect()
}

#[tokio::test]
async fn spec_prefs_003_choices_are_the_installed_languages_and_the_time_zones() {
    let f = fixture().await;
    let mut tx = begin(&f.orm, f.me).await;
    // the dialog's pickers ask the ordinary `fields_get` of `res.partner` for these two selections
    let described = tx
        .env()
        .records("res.partner")
        .unwrap()
        .fields_get(&mut tx, Some(&["lang", "tz"]), Some(&["selection"]))
        .await
        .unwrap();

    let mut expected: Vec<(String, String)> = f
        .active_languages
        .iter()
        .map(|(name, code)| ((*code).to_owned(), (*name).to_owned()))
        .collect();
    expected.sort_by(|a, b| a.1.cmp(&b.1));
    assert_eq!(
        pairs(&described["lang"]["selection"]),
        expected,
        "active languages by name; fr_FR is not active"
    );

    let zones: Vec<String> = pairs(&described["tz"]["selection"])
        .into_iter()
        .map(|(code, label)| {
            assert_eq!(code, label, "a zone is its own label");
            code
        })
        .collect();
    assert!(zones.iter().any(|z| z == "Asia/Jakarta"));
    let first_etc = zones.iter().position(|z| z.starts_with("Etc/")).unwrap();
    assert!(first_etc > 0, "zones other than Etc/* come first");
    assert!(
        zones[first_etc..].iter().all(|z| z.starts_with("Etc/")),
        "Etc/* zones come last"
    );
    let mut rest = zones[..first_etc].to_vec();
    rest.sort();
    assert_eq!(rest, zones[..first_etc], "each group is sorted by name");
    // what the picker offers is what a write accepts
    let own = tx
        .env()
        .records("res.users")
        .unwrap()
        .browse([f.me])
        .unwrap();
    for (code, _) in &expected {
        own.write_values(&mut tx, &map(&[("lang", s(code))]))
            .await
            .unwrap();
    }
    for zone in [&zones[0], &zones[first_etc]] {
        own.write_values(&mut tx, &map(&[("tz", s(zone))]))
            .await
            .unwrap();
    }
    drop(tx);
    f.finish().await;
}

#[tokio::test]
async fn spec_prefs_004_a_user_opens_and_saves_only_their_own_record() {
    let f = fixture().await;
    let mut tx = begin(&f.orm, f.me).await;
    let own = tx
        .env()
        .records("res.users")
        .unwrap()
        .browse([f.me])
        .unwrap();
    // the fields of the dialog read and save on the own record
    own.write_values(
        &mut tx,
        &map(&[
            ("name", s("Me Myself")),
            ("email", s("me@example.com")),
            ("lang", s("id_ID")),
            ("tz", s("Asia/Jakarta")),
        ]),
    )
    .await
    .unwrap();
    for (field, expected) in [
        ("name", "Me Myself"),
        ("email", "me@example.com"),
        ("lang", "id_ID"),
        ("tz", "Asia/Jakarta"),
    ] {
        assert_eq!(
            own.value(&mut tx, field).await.unwrap(),
            s(expected),
            "{field}"
        );
    }
    // the record rule leaves only the own row visible
    let visible = tx
        .env()
        .records("res.users")
        .unwrap()
        .search_read(&mut tx, &Domain::TRUE, &["login"], &SearchOptions::new())
        .await
        .unwrap();
    assert_eq!(visible.len(), 1, "{visible:?}");
    assert_eq!(visible[0]["login"], s("me"));
    // another user's row is out of reach, and so is a field the dialog does not offer
    let foreign = tx
        .env()
        .records("res.users")
        .unwrap()
        .browse([f.other])
        .unwrap();
    let err = foreign
        .write_values(&mut tx, &map(&[("name", s("Hijacked"))]))
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Access, "{err}");
    let err = own
        .write_values(&mut tx, &map(&[("login", s("me2"))]))
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Access, "{err}");
    drop(tx);
    let login: String = f
        .client
        .query_one("SELECT login FROM res_users WHERE id = $1", &[&f.other])
        .await
        .unwrap()
        .get(0);
    assert_eq!(login, "other");
    f.finish().await;
}

#[tokio::test]
async fn spec_prefs_005_the_language_must_be_an_installed_one() {
    let f = fixture().await;
    let mut tx = begin(&f.orm, f.me).await;
    let own = tx
        .env()
        .records("res.users")
        .unwrap()
        .browse([f.me])
        .unwrap();
    own.write_values(
        &mut tx,
        &map(&[("name", s("Before")), ("lang", s("de_DE"))]),
    )
    .await
    .unwrap();
    // a code nobody installed, and one that exists but is not active: nothing is written
    for code in ["zz_ZZ", "fr_FR"] {
        let err = own
            .write_values(&mut tx, &map(&[("name", s("After")), ("lang", s(code))]))
            .await
            .unwrap_err();
        assert_error(
            &err,
            ErrorKind::Validation,
            &format!("Wrong value for res.users.lang: {}", Value::str(code)),
        );
        assert_eq!(own.value(&mut tx, "name").await.unwrap(), s("Before"));
        assert_eq!(own.value(&mut tx, "lang").await.unwrap(), s("de_DE"));
    }
    // creating a user checks it the same way
    let err = tx
        .env()
        .sudo(true)
        .records("res.users")
        .unwrap()
        .create_values(
            &mut tx,
            &[map(&[
                ("name", s("n")),
                ("login", s("n")),
                ("lang", s("zz_ZZ")),
            ])],
        )
        .await
        .unwrap_err();
    assert_error(
        &err,
        ErrorKind::Validation,
        "Wrong value for res.users.lang: 'zz_ZZ'",
    );
    // a `default_lang` / `default_tz` from the context is checked the same way
    for (key, value, field) in [
        ("default_lang", "zz_ZZ", "lang"),
        ("default_tz", "Mars/Olympus_Mons", "tz"),
    ] {
        let login = format!("d-{field}");
        let err = tx
            .env()
            .sudo(true)
            .records("res.users")
            .unwrap()
            .with_context([(key, s(value))])
            .create_values(&mut tx, &[map(&[("name", s("d")), ("login", s(&login))])])
            .await
            .unwrap_err();
        assert_error(
            &err,
            ErrorKind::Validation,
            &format!("Wrong value for res.users.{field}: {}", Value::str(value)),
        );
    }
    // clearing the language is not a wrong value
    own.write_values(&mut tx, &map(&[("lang", s(""))]))
        .await
        .unwrap();
    drop(tx);
    f.finish().await;
}

#[tokio::test]
async fn spec_prefs_006_the_time_zone_must_be_a_known_one() {
    let f = fixture().await;
    let mut tx = begin(&f.orm, f.me).await;
    let own = tx
        .env()
        .records("res.users")
        .unwrap()
        .browse([f.me])
        .unwrap();
    // a zone from the list the dialog offers, `Etc/*` included
    for zone in ["Asia/Jakarta", "Etc/UTC"] {
        own.write_values(&mut tx, &map(&[("tz", s(zone))]))
            .await
            .unwrap();
        assert_eq!(own.value(&mut tx, "tz").await.unwrap(), s(zone));
    }
    for zone in ["Mars/Olympus_Mons", "asia/jakarta", " UTC"] {
        let err = own
            .write_values(&mut tx, &map(&[("name", s("After")), ("tz", s(zone))]))
            .await
            .unwrap_err();
        assert_error(
            &err,
            ErrorKind::Validation,
            &format!("Wrong value for res.users.tz: {}", Value::str(zone)),
        );
    }
    assert_eq!(own.value(&mut tx, "tz").await.unwrap(), s("Etc/UTC"));
    assert_eq!(own.value(&mut tx, "name").await.unwrap(), s("me"));
    own.write_values(&mut tx, &map(&[("tz", s(""))]))
        .await
        .unwrap();
    drop(tx);
    f.finish().await;
}

#[tokio::test]
async fn spec_prefs_007_update_preferences_closes_the_dialog_for_the_own_record_only() {
    let f = fixture().await;
    let mut tx = begin(&f.orm, f.me).await;
    let own = tx
        .env()
        .records("res.users")
        .unwrap()
        .browse([f.me])
        .unwrap();
    // it writes nothing: the dialog's form was saved by the client before the button's method runs
    let answer = own
        .call::<_, Value>(&mut tx, "preference_save", ())
        .await
        .unwrap();
    assert_eq!(
        answer,
        Value::Json(Arc::new(json!({"type": "ir.actions.act_window_close"})))
    );
    let message = "Preferences can only be saved for the current user's own record.";
    let foreign = tx
        .env()
        .records("res.users")
        .unwrap()
        .browse([f.other])
        .unwrap();
    let err = foreign
        .call::<_, Value>(&mut tx, "preference_save", ())
        .await
        .unwrap_err();
    assert_error(&err, ErrorKind::Validation, message);
    let both = tx
        .env()
        .records("res.users")
        .unwrap()
        .browse([f.me, f.other])
        .unwrap();
    let err = both
        .call::<_, Value>(&mut tx, "preference_save", ())
        .await
        .unwrap_err();
    assert_error(&err, ErrorKind::Validation, message);
    drop(tx);
    f.finish().await;
}

/// A member of `base.group_erp_manager` who is not in `base.group_system` has the internal self
/// row and no user-management row (the shipped `ir.access` data), so on their own record the
/// dialog's four fields work and every other field is refused, nothing committed. A member of
/// `base.group_system` still writes them.
#[tokio::test]
async fn spec_prefs_004_an_access_rights_manager_without_settings_writes_only_the_dialog_fields() {
    let f = fixture().await;
    let mut tx = begin(&f.orm, f.manager).await;
    let own = tx
        .env()
        .records("res.users")
        .unwrap()
        .browse([f.manager])
        .unwrap();
    let foreign_partner: i64 = f
        .client
        .query_one("SELECT COALESCE(MAX(id), 1)::int8 FROM res_partner", &[])
        .await
        .unwrap()
        .get(0);
    own.write_values(
        &mut tx,
        &map(&[
            ("name", s("Boss")),
            ("email", s("boss@example.com")),
            ("lang", s("de_DE")),
            ("tz", s("Asia/Jakarta")),
        ]),
    )
    .await
    .unwrap();
    for (field, value) in [
        ("password", s("taken-over")),
        ("login", s("changed")),
        ("partner_id", Value::Int(foreign_partner)),
        (
            "group_ids",
            Value::list([Value::list([
                Value::Int(4),
                Value::Int(f.system_group),
                Value::Int(0),
            ])]),
        ),
        ("active", Value::Bool(true)),
    ] {
        let err = own
            .write_values(&mut tx, &map(&[("name", s("Not Saved")), (field, value)]))
            .await
            .unwrap_err();
        assert_eq!(err.kind(), ErrorKind::Access, "{field}: {err}");
        assert_eq!(own.value(&mut tx, "name").await.unwrap(), s("Boss"));
    }
    tx.commit().await.unwrap();
    drop(tx);
    let row = f
        .client
        .query_one(
            "SELECT name, login FROM res_users WHERE id = $1",
            &[&f.manager],
        )
        .await
        .unwrap();
    assert_eq!(row.get::<_, String>(0), "Boss");
    assert_eq!(row.get::<_, String>(1), "manager");
    let in_system: i64 = f
        .client
        .query_one(
            "SELECT count(*)::int8 FROM res_groups_users_rel WHERE uid = $1 AND gid = $2",
            &[&f.manager, &f.system_group],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(in_system, 0);

    // a Settings administrator is not held to the mask
    let mut tx = begin(&f.orm, f.admin).await;
    let target = tx
        .env()
        .records("res.users")
        .unwrap()
        .browse([f.me])
        .unwrap();
    target
        .write_values(&mut tx, &map(&[("login", s("me2"))]))
        .await
        .unwrap();
    tx.commit().await.unwrap();
    drop(tx);
    let login: String = f
        .client
        .query_one("SELECT login FROM res_users WHERE id = $1", &[&f.me])
        .await
        .unwrap()
        .get(0);
    assert_eq!(login, "me2");
    f.finish().await;
}
