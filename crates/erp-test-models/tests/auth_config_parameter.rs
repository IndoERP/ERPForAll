//! `ir.config_parameter` (BUG-083;
//! §14.6): the defaults `init()` creates, the typed API, the default-key guards, the
//! `database.secret` rule and the settings cache generation.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

mod auth_support;

use auth_support::*;
use erp_orm::{AccessDomainChains, Operation};
use erp_sdk::{Domain, Error, ErrorKind, Records, Transaction, Value};
use erp_security::auth::password::limits;
use erp_security::auth::settings::keys;
use erp_testkit::{DomainPolicy, Harness, Otherwise};

struct NoDebugHost;
impl erp_rpc::RpcHost for NoDebugHost {
    fn language_active(&self, _code: &str) -> Result<bool, Error> {
        Ok(false)
    }
    fn has_capability(
        &self,
        _principal: &erp_orm::Principal,
        _capability: &erp_orm::Capability,
    ) -> Result<bool, Error> {
        Ok(false)
    }
}

/// `base` with the `#[access_domain]` overrides applied by a policy that allows everything else
/// (`deny_read`: users may not read `ir.config_parameter`).
async fn harness(deny_read: bool) -> Harness {
    let registry = builder().registry_only().unwrap();
    let chains = AccessDomainChains::build(&registry, &[]).unwrap();
    let mut policy = DomainPolicy::builder(Otherwise::Allow);
    if deny_read {
        policy = policy.deny("ir.config_parameter", Operation::Read);
    }
    let policy = policy.overrides(registry, chains).build().unwrap();
    let h = builder().policy(policy).build().await.unwrap();
    prepare(&h).await;
    h
}

fn params(tx: &Transaction) -> Records {
    tx.env().records("ir.config_parameter").unwrap()
}

async fn get_str(tx: &mut Transaction, key: &str, default: Option<&str>) -> Result<String, Error> {
    params(tx)
        .call(tx, "get_str", (key.to_owned(), default.map(str::to_owned)))
        .await
}

async fn set_str(tx: &mut Transaction, key: &str, value: Option<&str>) -> Result<String, Error> {
    params(tx)
        .call(tx, "set_str", (key.to_owned(), value.map(str::to_owned)))
        .await
}

async fn get_int(tx: &mut Transaction, key: &str, default: Option<i64>) -> Result<i64, Error> {
    params(tx)
        .call(tx, "get_int", (key.to_owned(), default))
        .await
}

async fn set_int(tx: &mut Transaction, key: &str, value: Option<i64>) -> Result<i64, Error> {
    params(tx)
        .call(tx, "set_int", (key.to_owned(), value))
        .await
}

async fn get_float(tx: &mut Transaction, key: &str, default: Option<f64>) -> Result<f64, Error> {
    params(tx)
        .call(tx, "get_float", (key.to_owned(), default))
        .await
}

async fn set_float(tx: &mut Transaction, key: &str, value: Option<f64>) -> Result<f64, Error> {
    params(tx)
        .call(tx, "set_float", (key.to_owned(), value))
        .await
}

async fn get_bool(tx: &mut Transaction, key: &str, default: Option<bool>) -> Result<bool, Error> {
    params(tx)
        .call(tx, "get_bool", (key.to_owned(), default))
        .await
}

async fn set_bool(tx: &mut Transaction, key: &str, value: Option<bool>) -> Result<bool, Error> {
    params(tx)
        .call(tx, "set_bool", (key.to_owned(), value))
        .await
}

async fn row_of(tx: &mut Transaction, key: &str) -> Records {
    params(tx)
        .sudo(true)
        .search(
            tx,
            &Domain::condition("key", erp_sdk::Operator::Eq, key).unwrap(),
        )
        .await
        .unwrap()
}

/// from the canonical origin)
#[tokio::test]
async fn init_creates_the_defaults_once() {
    let h = harness(false).await;
    let mut tx = h.begin().await.unwrap();
    let secret = get_str(&mut tx, "database.secret", None).await.unwrap();
    assert_eq!(secret.len(), 64);
    assert!(secret.bytes().all(|b| b.is_ascii_hexdigit()));
    let uuid = get_str(&mut tx, "database.uuid", None).await.unwrap();
    assert_eq!((uuid.len(), uuid.as_bytes()[14]), (36, b'4'));
    let created = get_str(&mut tx, "database.create_date", None)
        .await
        .unwrap();
    assert_eq!(created.len(), 19, "{created}");
    assert_eq!(
        get_int(&mut tx, "base.login_cooldown_after", None)
            .await
            .unwrap(),
        10
    );
    assert_eq!(
        get_int(&mut tx, "base.login_cooldown_duration", None)
            .await
            .unwrap(),
        60
    );
    assert_eq!(
        get_str(&mut tx, "web.base.url", Some("absent"))
            .await
            .unwrap(),
        "absent"
    );
    // a second init keeps what exists
    erp_orm::model_hooks::call_init(&mut tx, "ir.config_parameter")
        .await
        .unwrap();
    assert_eq!(
        get_str(&mut tx, "database.secret", None).await.unwrap(),
        secret
    );
    tx.rollback().await.unwrap();
}

#[tokio::test]
async fn default_keys_cannot_be_renamed_or_deleted() {
    let h = harness(false).await;
    let mut tx = h.begin().await.unwrap();
    let secret = row_of(&mut tx, "database.secret").await;
    let e = secret.unlink(&mut tx).await.unwrap_err();
    assert_eq!(
        (e.kind(), e.message()),
        (
            ErrorKind::Validation,
            "You cannot delete the database.secret record."
        )
    );
    tx.rollback().await.unwrap();
    let mut tx = h.begin().await.unwrap();
    let secret = row_of(&mut tx, "database.secret").await;
    let e = secret
        .write_values(&mut tx, &map(&[("key", s("database.other"))]))
        .await
        .unwrap_err();
    assert_eq!(
        (e.kind(), e.message()),
        (
            ErrorKind::Validation,
            "You cannot rename config parameters with keys database.secret"
        )
    );
    tx.rollback().await.unwrap();
}

#[tokio::test]
async fn typed_str() {
    let h = harness(false).await;
    let mut tx = h.begin().await.unwrap();
    assert_eq!(
        get_str(&mut tx, "test.str", Some("fallback"))
            .await
            .unwrap(),
        "fallback"
    );
    assert_eq!(
        set_str(&mut tx, "test.str", Some("hello")).await.unwrap(),
        ""
    );
    assert_eq!(
        get_str(&mut tx, "test.str", Some("fallback"))
            .await
            .unwrap(),
        "hello"
    );
    assert_eq!(set_str(&mut tx, "test.str", None).await.unwrap(), "hello");
    assert_eq!(
        get_str(&mut tx, "test.str", Some("fallback"))
            .await
            .unwrap(),
        "fallback"
    );
    tx.rollback().await.unwrap();
}

#[tokio::test]
async fn typed_int() {
    let h = harness(false).await;
    let mut tx = h.begin().await.unwrap();
    assert_eq!(set_int(&mut tx, "test.int", Some(10)).await.unwrap(), 0);
    assert_eq!(get_int(&mut tx, "test.int", Some(7)).await.unwrap(), 10);
    assert_eq!(set_int(&mut tx, "test.int", Some(0)).await.unwrap(), 10);
    assert_eq!(get_int(&mut tx, "test.int", Some(7)).await.unwrap(), 0);
    set_int(&mut tx, "test.int", None).await.unwrap();
    assert_eq!(get_int(&mut tx, "test.int", Some(7)).await.unwrap(), 7);
    tx.rollback().await.unwrap();
}

#[tokio::test]
async fn typed_float() {
    let h = harness(false).await;
    let mut tx = h.begin().await.unwrap();
    set_float(&mut tx, "test.float", Some(2.75)).await.unwrap();
    assert_eq!(
        get_float(&mut tx, "test.float", Some(1.5)).await.unwrap(),
        2.75
    );
    assert_eq!(
        set_float(&mut tx, "test.float", Some(0.0)).await.unwrap(),
        2.75
    );
    assert_eq!(
        get_float(&mut tx, "test.float", Some(1.5)).await.unwrap(),
        0.0
    );
    assert_eq!(
        get_str(&mut tx, "test.float", None).await.unwrap(),
        "0.0",
        "stored as Python's str(float)"
    );
    set_float(&mut tx, "test.float", None).await.unwrap();
    assert_eq!(
        get_float(&mut tx, "test.float", Some(1.5)).await.unwrap(),
        1.5
    );
    tx.rollback().await.unwrap();
}

#[tokio::test]
async fn typed_bool() {
    let h = harness(false).await;
    let mut tx = h.begin().await.unwrap();
    set_bool(&mut tx, "test.bool", Some(true)).await.unwrap();
    assert!(get_bool(&mut tx, "test.bool", Some(false)).await.unwrap());
    assert_eq!(get_str(&mut tx, "test.bool", None).await.unwrap(), "True");
    assert!(set_bool(&mut tx, "test.bool", Some(false)).await.unwrap());
    assert!(!get_bool(&mut tx, "test.bool", Some(true)).await.unwrap());
    set_bool(&mut tx, "test.bool", None).await.unwrap();
    assert!(get_bool(&mut tx, "test.bool", Some(true)).await.unwrap());
    tx.rollback().await.unwrap();
}

/// rows of `base` come with)
#[tokio::test]
async fn getters_check_read_access() {
    let h = harness(true).await;
    let bob = user_with_password(&h, "bob", "pw").await;
    let mut tx = h.begin_as(bob).await.unwrap();
    let e = get_str(&mut tx, "web.base.url", None).await.unwrap_err();
    assert_eq!(e.kind(), ErrorKind::Access);
    tx.rollback().await.unwrap();
}

/// SEC-R7-1: no setter may inspect a previous value before model write authorization.
/// Both ordinary dispatch and the exposed RPC binder must refuse even a no-op.
#[tokio::test]
async fn setters_authorize_before_reading_previous_values() {
    let registry = builder().registry_only().unwrap();
    let chains = AccessDomainChains::build(&registry, &[]).unwrap();
    let policy = DomainPolicy::builder(Otherwise::Allow)
        .deny("ir.config_parameter", Operation::Read)
        .deny("ir.config_parameter", Operation::Write)
        .deny("ir.config_parameter", Operation::Create)
        .overrides(registry, chains)
        .build()
        .unwrap();
    let h = builder().policy(policy).build().await.unwrap();
    prepare(&h).await;
    let bob = user_with_password(&h, "bob", "pw").await;
    set_param(&h, "review.private", "sentinel-private-text").await;
    set_param(&h, "review.numeric", "7").await;
    set_param(&h, "review.empty", "").await;
    let expected = "You are not allowed to write 'System Parameter' (ir.config_parameter) records.";
    for rpc in [false, true] {
        for (method, key, value) in [
            ("set_int", "review.private", Value::Int(0)),
            ("set_float", "review.private", Value::Float(0.0)),
            ("set_bool", "review.private", Value::Bool(false)),
            (
                "set_str",
                "review.private",
                Value::str("sentinel-private-text"),
            ),
            ("set_int", "review.numeric", Value::Int(7)),
            ("set_str", "review.private", Value::str("changed")),
            ("set_str", "review.missing", Value::str("new")),
            ("set_str", "review.empty", Value::False),
        ] {
            let mut tx = h.begin_as(bob).await.unwrap();
            let records = params(&tx);
            let error = if rpc {
                let Some(erp_orm::rpc::Exposure::Exposed(exposed)) =
                    h.orm().exposure().get(records.model_id(), method)
                else {
                    panic!("setter must be exposed");
                };
                let args = erp_orm::rpc::RpcArgs::new(
                    [
                        ("key".to_owned(), Value::str(key)),
                        ("value".to_owned(), value.clone()),
                    ]
                    .into_iter()
                    .collect(),
                );
                match exposed.call(&mut tx, records, args).await.unwrap_err() {
                    erp_orm::rpc::BindError::Call(error) => error,
                    other => panic!("unexpected binding error: {other:?}"),
                }
            } else {
                match method {
                    "set_str" => set_str(
                        &mut tx,
                        key,
                        match &value {
                            Value::Str(s) => Some(s.as_ref()),
                            Value::False => None,
                            _ => unreachable!(),
                        },
                    )
                    .await
                    .map(|_| ())
                    .unwrap_err(),
                    "set_int" => set_int(
                        &mut tx,
                        key,
                        Some(if key == "review.numeric" { 7 } else { 0 }),
                    )
                    .await
                    .map(|_| ())
                    .unwrap_err(),
                    "set_float" => set_float(&mut tx, key, Some(0.0))
                        .await
                        .map(|_| ())
                        .unwrap_err(),
                    "set_bool" => set_bool(&mut tx, key, Some(false))
                        .await
                        .map(|_| ())
                        .unwrap_err(),
                    _ => unreachable!(),
                }
            };
            assert_eq!(
                (error.kind(), error.message()),
                (ErrorKind::Access, expected)
            );
            tx.rollback().await.unwrap();
        }
    }
    let mut tx = h.begin().await.unwrap();
    assert_eq!(
        get_str(&mut tx, "review.private", None).await.unwrap(),
        "sentinel-private-text"
    );
    assert_eq!(get_str(&mut tx, "review.numeric", None).await.unwrap(), "7");
    assert_eq!(
        get_str(&mut tx, "review.missing", Some("absent"))
            .await
            .unwrap(),
        "absent"
    );
    tx.rollback().await.unwrap();
    let mut tx = h.begin_as(bob).await.unwrap();
    let caller = erp_rpc::Caller::Principal(tx.env().principal().clone());
    tx.rollback().await.unwrap();
    let service = erp_rpc::RpcService::new(h.orm().clone(), NoDebugHost, Default::default());
    for method in ["set_int", "set_float", "set_bool"] {
        let value = if method == "set_bool" { "false" } else { "0" };
        let body = format!(r#"{{"key":"review.private","value":{value}}}"#);
        let response = service
            .call(erp_rpc::RpcCall::json2(
                &format!("/json/2/ir.config_parameter/{method}"),
                body.into_bytes(),
                caller.clone(),
            ))
            .await;
        assert_eq!(response.status(), 403);
        let decoded: serde_json::Value = serde_json::from_slice(response.body()).unwrap();
        assert_eq!(decoded["name"], "erp.exceptions.AccessError");
        assert_eq!(decoded["message"], expected);
        assert!(!String::from_utf8_lossy(response.body()).contains("sentinel-private-text"));
    }
}

/// Model permission alone is insufficient: row authorization precedes parsing and no-op
/// return of previous values too. This policy allows the model but excludes the seeded row.
#[tokio::test]
async fn setters_authorize_record_before_parsing_or_returning() {
    let registry = builder().registry_only().unwrap();
    let chains = AccessDomainChains::build(&registry, &[]).unwrap();
    let policy = DomainPolicy::builder(Otherwise::Allow)
        .rule(
            "ir.config_parameter",
            Operation::Write,
            Domain::condition("key", erp_sdk::Operator::Eq, "review.allowed").unwrap(),
        )
        .overrides(registry, chains)
        .build()
        .unwrap();
    let h = builder().policy(policy).build().await.unwrap();
    prepare(&h).await;
    let bob = user_with_password(&h, "bob", "pw").await;
    set_param(&h, "review.private", "sentinel-private-text").await;
    let mut tx = h.begin().await.unwrap();
    let id = row_of(&mut tx, "review.private").await.ids()[0];
    tx.rollback().await.unwrap();
    let expected = format!(
        "You are not allowed to modify 'System Parameter' (ir.config_parameter) records.\n\nRecords: ir.config_parameter({id},), User: {bob}"
    );
    for no_op in [false, true] {
        let mut tx = h.begin_as(bob).await.unwrap();
        let error = if no_op {
            set_str(&mut tx, "review.private", Some("sentinel-private-text"))
                .await
                .map(|_| ())
                .unwrap_err()
        } else {
            set_int(&mut tx, "review.private", Some(0))
                .await
                .map(|_| ())
                .unwrap_err()
        };
        assert_eq!(
            (error.kind(), error.message()),
            (ErrorKind::Access, expected.as_str())
        );
        tx.rollback().await.unwrap();
    }
}

/// API outside superuser mode, whatever the policy allows; the JSON-2 run is's)
#[tokio::test]
async fn database_secret_is_hidden_outside_superuser_mode() {
    let h = harness(false).await;
    let bob = user_with_password(&h, "bob", "pw").await;
    let mut tx = h.begin_as(bob).await.unwrap();
    let visible = params(&tx).search(&mut tx, &Domain::TRUE).await.unwrap();
    assert!(!visible.is_empty());
    let mut keys = Vec::new();
    for rec in visible.iter() {
        keys.push(rec.value(&mut tx, "key").await.unwrap());
    }
    assert!(!keys.contains(&Value::str("database.secret")), "{keys:?}");
    for result in [
        get_str(&mut tx, "database.secret", None).await,
        set_str(&mut tx, "database.secret", Some("mine")).await,
    ] {
        let e = result.unwrap_err();
        assert_eq!(e.kind(), ErrorKind::Access);
        assert!(e.message().contains(""), "{e}");
    }
    let all = params(&tx)
        .sudo(true)
        .search(&mut tx, &Domain::TRUE)
        .await
        .unwrap();
    assert_eq!(all.len(), visible.len() + 1, "superuser mode sees it");
    tx.rollback().await.unwrap();
}

/// the default)
#[tokio::test]
async fn invalid_values_are_errors() {
    let h = harness(false).await;
    let mut tx = h.begin().await.unwrap();
    set_str(&mut tx, "bad_int", Some("invalid integer"))
        .await
        .unwrap();
    let e = get_int(&mut tx, "bad_int", Some(100)).await.unwrap_err();
    assert_eq!(
        (e.kind(), e.message()),
        (
            ErrorKind::Validation,
            "ir.config_parameter bad_int has a stored value that is not of type int"
        )
    );
    tx.rollback().await.unwrap();
}

/// BUG-083: a typed setter over a value that does not parse is an error and writes nothing;
/// `set_str` repairs it.
#[tokio::test]
async fn typed_setters_refuse_an_unparsable_previous_value() {
    let h = harness(false).await;
    let mut tx = h.begin().await.unwrap();
    set_str(&mut tx, "bad_int", Some("x")).await.unwrap();
    let e = set_int(&mut tx, "bad_int", Some(5)).await.unwrap_err();
    assert_eq!(e.kind(), ErrorKind::Validation);
    assert_eq!(get_str(&mut tx, "bad_int", None).await.unwrap(), "x");
    assert_eq!(set_str(&mut tx, "bad_int", Some("5")).await.unwrap(), "x");
    assert_eq!(set_int(&mut tx, "bad_int", Some(6)).await.unwrap(), 5);
    tx.rollback().await.unwrap();
}

#[tokio::test]
async fn keys_are_unique() {
    let h = harness(false).await;
    let mut tx = h.begin().await.unwrap();
    set_str(&mut tx, "test.key", Some("a")).await.unwrap();
    let e = params(&tx)
        .create_values(
            &mut tx,
            &[map(&[("key", s("test.key")), ("value", s("b"))])],
        )
        .await
        .unwrap_err();
    assert!(e.message().contains("Key must be unique."), "{e}");
    tx.rollback().await.unwrap();
}

/// Every write through the model moves the `config_parameter` generation at commit, which tells
/// the authentication cache to reload (design note §14.5).
#[tokio::test]
async fn writes_move_the_settings_generation() {
    let h = harness(false).await;
    let before = h.orm().ormcache_generation("config_parameter");
    let mut tx = h.begin().await.unwrap();
    set_str(&mut tx, "test.gen", Some("1")).await.unwrap();
    tx.commit().await.unwrap();
    let after_create = h.orm().ormcache_generation("config_parameter");
    assert!(after_create > before);
    let mut tx = h.begin().await.unwrap();
    set_str(&mut tx, "test.gen", Some("1")).await.unwrap();
    tx.commit().await.unwrap();
    assert_eq!(
        h.orm().ormcache_generation("config_parameter"),
        after_create,
        "an unchanged value writes nothing"
    );
    let mut tx = h.begin().await.unwrap();
    set_str(&mut tx, "test.gen", Some("2")).await.unwrap();
    tx.commit().await.unwrap();
    assert!(h.orm().ormcache_generation("config_parameter") > after_create);
}

/// Round 1 decision 5 (security M3): every setting of authentication is checked when written,
/// with the parser it is read with; a bad value is refused naming the key and the value (never
/// the secret's), and nothing is written.
#[tokio::test]
async fn bad_auth_settings_are_refused_when_written() {
    let h = harness(false).await;
    let whole = "is not a whole number in";
    let cases: [(&str, Option<&str>, String); 13] = [
        (
            keys::SCHEME,
            Some("md5"),
            "ir.config_parameter password.hashing.scheme = 'md5' is not a scheme: expected \
             'argon2id' or 'pbkdf2_sha512'"
                .to_owned(),
        ),
        (
            keys::ROUNDS,
            Some("1000"),
            format!(
                "ir.config_parameter password.hashing.rounds = '1000' is outside {}..={}",
                limits::PBKDF2_ROUNDS_MIN,
                limits::PBKDF2_ROUNDS_MAX
            ),
        ),
        (
            keys::ROUNDS,
            Some("many"),
            format!("ir.config_parameter password.hashing.rounds = 'many' {whole} 0..=4294967295"),
        ),
        (
            keys::ARGON2_MEMORY,
            Some("1024"),
            format!(
                "ir.config_parameter password.hashing.argon2_memory_kib = '1024' is outside {}..={}",
                limits::ARGON2_MEMORY_KIB_MIN,
                limits::ARGON2_MEMORY_KIB_MAX
            ),
        ),
        (
            keys::ARGON2_TIME,
            Some("1"),
            format!(
                "ir.config_parameter password.hashing.argon2_time = '1' is outside {}..={}",
                limits::ARGON2_TIME_MIN,
                limits::ARGON2_TIME_MAX
            ),
        ),
        (
            keys::COOLDOWN_AFTER,
            Some("0"),
            format!("ir.config_parameter base.login_cooldown_after = '0' {whole} 1..=1000000"),
        ),
        (
            keys::COOLDOWN_AFTER,
            Some("ten"),
            format!("ir.config_parameter base.login_cooldown_after = 'ten' {whole} 1..=1000000"),
        ),
        (
            keys::COOLDOWN_DURATION,
            Some("0"),
            format!("ir.config_parameter base.login_cooldown_duration = '0' {whole} 1..=604800"),
        ),
        (
            keys::COOLDOWN_AFTER_LOGIN,
            Some("-1"),
            format!(
                "ir.config_parameter base.login_cooldown_after_login = '-1' {whole} 0..=4000000"
            ),
        ),
        (
            keys::MAX_INACTIVITY,
            Some("59"),
            format!(
                "ir.config_parameter sessions.max_inactivity_seconds = '59' {whole} 60..=31536000"
            ),
        ),
        (
            keys::MAX_LIFETIME,
            Some("1 day"),
            format!(
                "ir.config_parameter sessions.max_lifetime_seconds = '1 day' {whole} \
                 3600..=31536000"
            ),
        ),
        (
            keys::SECRET,
            Some("too-short"),
            "ir.config_parameter database.secret has 9 bytes; expected 32 to 512".to_owned(),
        ),
        (
            keys::SECRET,
            None,
            "ir.config_parameter database.secret cannot be empty: every login and session check \
             derives its keys from it"
                .to_owned(),
        ),
    ];
    for (key, value, message) in cases {
        let mut tx = h.begin().await.unwrap();
        let before = stored(&mut tx, key).await;
        let e = sudo_set(&mut tx, key, value).await.unwrap_err();
        assert_eq!(
            (e.kind(), e.message()),
            (ErrorKind::Validation, message.as_str()),
            "{key} = {value:?}"
        );
        if key == keys::SECRET {
            assert!(
                !e.message().contains("too-short"),
                "the secret is never shown"
            );
        }
        assert_eq!(stored(&mut tx, key).await, before, "{key}: nothing written");
        tx.rollback().await.unwrap();
    }
}

/// `set_str` in superuser mode (the only mode that may write `database.secret`).
async fn sudo_set(tx: &mut Transaction, key: &str, value: Option<&str>) -> Result<String, Error> {
    params(tx)
        .sudo(true)
        .call(tx, "set_str", (key.to_owned(), value.map(str::to_owned)))
        .await
}

/// The stored value of `key` (`None`: no row), read with SQL after the pending writes.
async fn stored(tx: &mut Transaction, key: &str) -> Option<Option<String>> {
    tx.flush().await.unwrap();
    let rows = tx
        .query(
            "SELECT value FROM ir_config_parameter WHERE key = $1",
            &[&key],
        )
        .await
        .unwrap();
    rows.first().map(|r| r.get(0))
}

/// Round 1 decision 5: a good value of every setting is accepted by `set_str`, `create` and
/// `write`; an empty value (undefined) is accepted for every setting but the secret.
#[tokio::test]
async fn good_auth_settings_are_accepted() {
    let h = harness(false).await;
    let mut tx = h.begin().await.unwrap();
    let good = [
        (keys::SCHEME, "pbkdf2_sha512"),
        (keys::ROUNDS, "600000"),
        // the most a permit of the hashing pool reserves (security review round 2 M1)
        (keys::ARGON2_MEMORY, "19456"),
        (keys::ARGON2_TIME, "3"),
        (keys::COOLDOWN_AFTER, "1"),
        (keys::COOLDOWN_DURATION, "30"),
        (keys::COOLDOWN_AFTER_LOGIN, "0"),
        (keys::MAX_INACTIVITY, "3600"),
        (keys::MAX_LIFETIME, "86400"),
        (keys::SECRET, "0123456789abcdef0123456789abcdef"),
    ];
    for (key, value) in good {
        sudo_set(&mut tx, key, Some(value)).await.unwrap();
        assert_eq!(stored(&mut tx, key).await, Some(Some(value.to_owned())));
    }
    for (key, _) in good.iter().filter(|(k, _)| *k != keys::SECRET) {
        sudo_set(&mut tx, key, None).await.unwrap();
    }
    tx.rollback().await.unwrap();
}

/// Round 1 decision 5: `create` and `write` are checked too, a renamed key included.
#[tokio::test]
async fn create_and_write_check_auth_settings() {
    let h = harness(false).await;
    let mut tx = h.begin().await.unwrap();
    let message = "ir.config_parameter sessions.max_lifetime_seconds = 'soon' is not a whole \
                   number in 3600..=31536000";
    let e = params(&tx)
        .create_values(
            &mut tx,
            &[map(&[("key", s(keys::MAX_LIFETIME)), ("value", s("soon"))])],
        )
        .await
        .unwrap_err();
    assert_eq!((e.kind(), e.message()), (ErrorKind::Validation, message));
    set_str(&mut tx, "test.lifetime", Some("soon"))
        .await
        .unwrap();
    let rec = row_of(&mut tx, "test.lifetime").await;
    let e = rec
        .write_values(&mut tx, &map(&[("key", s(keys::MAX_LIFETIME))]))
        .await
        .unwrap_err();
    assert_eq!((e.kind(), e.message()), (ErrorKind::Validation, message));
    rec.write_values(&mut tx, &map(&[("value", s("7200"))]))
        .await
        .unwrap();
    rec.write_values(&mut tx, &map(&[("key", s(keys::MAX_LIFETIME))]))
        .await
        .unwrap();
    assert_eq!(
        stored(&mut tx, keys::MAX_LIFETIME).await,
        Some(Some("7200".to_owned()))
    );
    tx.rollback().await.unwrap();
}

/// Round 1 decision 5: a bad row written around the model (SQL, an old revision) still fails
/// loudly when the settings are read, naming the key.
#[tokio::test]
async fn a_bad_stored_setting_still_fails_at_read() {
    let w = world().await;
    erp_security::auth::check_stored(w.h.orm()).await.unwrap();
    let mut tx = w.h.begin().await.unwrap();
    tx.execute(
        "UPDATE ir_config_parameter SET value = '0' WHERE key = $1",
        &[&keys::COOLDOWN_AFTER],
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    // at startup (review of R1B, silent 9): the same error, before any login
    let e = erp_security::auth::check_stored(w.h.orm())
        .await
        .unwrap_err();
    assert_eq!(
        (e.kind(), e.message()),
        (
            ErrorKind::Validation,
            "ir.config_parameter base.login_cooldown_after = '0' is not a whole number in \
             1..=1000000"
        )
    );
    // the service caches what `Auth::new` read (rust review round 3 RD-7): a write around the
    // model is seen once the generation moves, as after any write through the model
    make_settings_stale(&w.h).await;
    let e = w.auth.settings().await.unwrap_err();
    assert_eq!(
        (e.kind(), e.message()),
        (
            ErrorKind::Validation,
            "ir.config_parameter base.login_cooldown_after = '0' is not a whole number in \
             1..=1000000"
        )
    );
}

/// Integration of review round 1 (silent 9): a bad stored setting stops `Auth::new` itself, with
/// the same error naming the key, so the server does not start with a configuration that refuses
/// every login.
#[tokio::test]
async fn a_bad_stored_setting_stops_auth_new() {
    let w = world().await;
    let mut tx = w.h.begin().await.unwrap();
    tx.execute(
        "UPDATE ir_config_parameter SET value = 'abc' WHERE key = $1",
        &[&keys::COOLDOWN_AFTER],
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    let Err(e) = erp_security::auth::Auth::new(
        w.h.orm().clone(),
        erp_security::auth::AuthConfig::DEFAULT,
        w.pool,
    )
    .await
    else {
        panic!("Auth::new started over a bad stored setting");
    };
    assert_eq!(
        (e.kind(), e.message()),
        (
            ErrorKind::Validation,
            "ir.config_parameter base.login_cooldown_after = 'abc' is not a whole number in \
             1..=1000000"
        )
    );
}

/// Integration of review round 1 (types 2, silent 8): an ORM built without requiring the
/// authentication tables is refused by `Auth::new`, naming the table, its owner and the fix.
#[tokio::test]
async fn auth_new_refuses_an_orm_without_the_core_tables() {
    let w = world().await;
    let orm = erp_orm::Orm::builder(std::sync::Arc::clone(w.h.registry()), pool_of(&w.h, 4))
        .policy(erp_orm::StaticPolicy::AllowAll)
        .trust_caller_resolutions()
        .build()
        .unwrap();
    let Err(e) =
        erp_security::auth::Auth::new(orm, erp_security::auth::AuthConfig::DEFAULT, w.pool).await
    else {
        panic!("Auth::new accepted an ORM that does not require the authentication tables");
    };
    assert_eq!(
        (e.kind(), e.message()),
        (
            ErrorKind::Programming,
            "the ORM was built without requiring the core table 'erp_session' of 'erp-security': \
             build it with OrmBuilder::require_core_tables(erp_security::auth::core_tables())"
        )
    );
}

/// The refusal of a hashing memory of 65536 KiB over a reservation of `reserved` KiB.
fn over_reservation(reserved: u32) -> String {
    format!(
        "ir.config_parameter password.hashing.argon2_memory_kib = 65536 KiB is more than the \
         {reserved} KiB a hashing permit reserves (admission.hash_memory_kib): raise \
         admission.hash_memory_kib on every node first, or lower \
         password.hashing.argon2_memory_kib"
    )
}

/// Security review round 2 M1 (the write): a hashing memory the database's hashing pool cannot
/// run is refused by `set_str` and `create`, naming both settings; a pool that reserves it
/// accepts it.
#[tokio::test]
async fn a_hashing_memory_over_the_pool_reservation_is_refused_when_written() {
    let w = world().await;
    let reserved = w.pool.config().memory_kib();
    let mut tx = w.h.begin().await.unwrap();
    let e = set_str(&mut tx, keys::ARGON2_MEMORY, Some("65536"))
        .await
        .unwrap_err();
    assert_eq!(
        (e.kind(), e.message()),
        (ErrorKind::Validation, over_reservation(reserved).as_str())
    );
    tx.rollback().await.unwrap();
    let mut tx = w.h.begin().await.unwrap();
    let rec = params(&tx)
        .create_values(
            &mut tx,
            &[map(&[("key", s("test.memory")), ("value", s("65536"))])],
        )
        .await
        .unwrap();
    // a rename onto the key is checked too
    let e = rec
        .write_values(&mut tx, &map(&[("key", s(keys::ARGON2_MEMORY))]))
        .await
        .unwrap_err();
    assert_eq!(
        (e.kind(), e.message()),
        (ErrorKind::Validation, over_reservation(reserved).as_str())
    );
    tx.rollback().await.unwrap();
    let big = world_with(
        builder().build().await.unwrap(),
        erp_security::auth::AuthConfig::DEFAULT,
        erp_security::auth::HashPoolConfig::new(2, 5, 65_536).unwrap(),
    )
    .await;
    set_param(&big.h, keys::ARGON2_MEMORY, "65536").await;
}

/// Security review round 2 M1 (startup): hashing parameters stored around the model that need
/// more memory than a permit of the pool reserves stop `Auth::new`, naming both settings.
#[tokio::test]
async fn hashing_parameters_over_the_pool_reservation_stop_auth_new() {
    let w = world().await;
    // written around the model (`init` creates no row for this key)
    let mut tx = w.h.begin().await.unwrap();
    tx.execute(
        "INSERT INTO ir_config_parameter (key, value) VALUES ($1, '65536')",
        &[&keys::ARGON2_MEMORY],
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    let Err(e) = erp_security::auth::Auth::new(
        w.h.orm().clone(),
        erp_security::auth::AuthConfig::DEFAULT,
        w.pool,
    )
    .await
    else {
        panic!("Auth::new started over hashing parameters its pool cannot run");
    };
    assert_eq!(
        (e.kind(), e.message()),
        (
            ErrorKind::Validation,
            over_reservation(w.pool.config().memory_kib()).as_str()
        )
    );
}
