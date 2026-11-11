//! branches of the dispatched login selection.
//!
//! The selection domain of every cell is stored in `test_login_selection::INJECTED_DOMAIN` and
//! reached through the `inject:` lookup, so the complete path runs: framework admission,
//! optimization, SQL compile, execution and the login verdict. A cell passes when the user the
//! domain should match logs in and every other user is denied.
//! All logins use
//! synthetic passwords; every attempt has its own submitted text and client address so the throttle
//! never cools a later cell down.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

mod auth_support;

use auth_support::*;
use erp_domain::{Domain, Operator, Value};
use erp_fields::ErrorKind;
use erp_security::auth::password::{HashParams, limits};
use erp_security::auth::{AuthConfig, HashPoolConfig, LoginError, LoginOrderTerm};
use erp_test_models::test_login_selection::{INJECTED_DOMAIN, INJECTED_ORDER};
use erp_testkit::Harness;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// One test at a time may use the process-wide injection slots.
static SLOTS: std::sync::LazyLock<tokio::sync::Mutex<()>> =
    std::sync::LazyLock::new(|| tokio::sync::Mutex::new(()));

/// Holds the injection-slot lock; clears both slots when dropped, so no test inherits another's
/// domain or order.
struct SlotGuard(
    #[allow(dead_code, reason = "held for its Drop")] tokio::sync::MutexGuard<'static, ()>,
);

impl Drop for SlotGuard {
    fn drop(&mut self) {
        if let Ok(mut domain) = INJECTED_DOMAIN.lock() {
            *domain = None;
        }
        if let Ok(mut order) = INJECTED_ORDER.lock() {
            *order = None;
        }
    }
}

async fn slots() -> SlotGuard {
    let guard = SLOTS.lock().await;
    // a test that panicked may have left its slots set
    *INJECTED_DOMAIN.lock().unwrap_or_else(|e| e.into_inner()) = None;
    *INJECTED_ORDER.lock().unwrap_or_else(|e| e.into_inner()) = None;
    SlotGuard(guard)
}

async fn selection_world() -> World {
    selection_world_with(HashPoolConfig::DEFAULT).await
}

async fn selection_world_with(config: HashPoolConfig) -> World {
    let h = Harness::builder([
        &erp_addon_base::ADDON,
        &erp_data::model_data::ADDON,
        &erp_test_models::test_login_selection::ADDON,
    ])
    .with_decls([erp_data::ir_model_data_decl()])
    .require_core_tables(erp_security::auth::core_tables())
    .build()
    .await
    .expect("harness");
    let admin = prepare(&h).await;
    let pool = own_pool(config);
    let auth = erp_security::auth::test_support::login_selection_auth(
        h.orm().clone(),
        AuthConfig::DEFAULT,
        pool,
    )
    .await
    .unwrap();
    World {
        h,
        auth,
        pool,
        admin,
    }
}

async fn plain_user(h: &Harness, login: &str, alias: &str, priority: i32) -> i64 {
    let mut tx = h.begin().await.unwrap();
    let id = create(
        &mut tx,
        "res.users",
        &[
            ("name", s(login)),
            ("login", s(login)),
            ("password", s("pw")),
            ("alias", s(alias)),
            ("priority", erp_fields::Value::from(priority)),
        ],
    )
    .await;
    tx.commit().await.unwrap();
    id
}

/// A test row: the three nullable columns the matrix reads.
#[derive(Clone, Copy)]
struct Row {
    login: &'static str,
    flag: Option<bool>,
    prio: Option<i64>,
    alias: Option<&'static str>,
}

const ONE: Row = Row {
    login: "mx_one",
    flag: Some(true),
    prio: Some(3),
    alias: Some("x"),
};
const TWO: Row = Row {
    login: "mx_two",
    flag: Some(false),
    prio: Some(0),
    alias: Some(""),
};
const NUL: Row = Row {
    login: "mx_nul",
    flag: None,
    prio: None,
    alias: None,
};
const ROWS: [Row; 3] = [ONE, TWO, NUL];

/// Create the three rows (and set their nullable columns by SQL: NULL cannot go through the model).
async fn create_rows(w: &World) -> [i64; 3] {
    let mut ids = [0i64; 3];
    for (slot, row) in ROWS.iter().enumerate() {
        let id = plain_user(&w.h, row.login, "tmp", 1).await;
        let mut tx = w.h.begin().await.unwrap();
        let prio = row.prio.map(|p| i32::try_from(p).unwrap());
        tx.execute(
            "UPDATE res_users SET selection_flag = $2, priority = $3, alias = $4 WHERE id = $1",
            &[&id, &row.flag, &prio, &row.alias],
        )
        .await
        .unwrap();
        tx.commit().await.unwrap();
        ids[slot] = id;
    }
    ids
}

fn address(n: u32) -> String {
    format!("10.{}.{}.{}", 1 + n / 62_500, (n / 250) % 250, 1 + n % 250)
}

/// Where a login attempt ended, for the cell tables.
#[derive(Debug, PartialEq, Eq)]
enum Outcome {
    Matched(i64),
    Denied,
    Refused(ErrorKind, String),
}

struct Runner<'a> {
    w: &'a World,
    attempt: u32,
}

impl Runner<'_> {
    async fn run(&mut self, domain: Domain) -> Outcome {
        self.attempt += 1;
        *INJECTED_DOMAIN.lock().unwrap() = Some(domain);
        let n = self.attempt;
        let result = login_from(&self.w.auth, &format!("inject:{n}"), "pw", &address(n)).await;
        match result {
            Ok(logged) => Outcome::Matched(logged.auth().uid().get()),
            Err(LoginError::Denied) => Outcome::Denied,
            Err(LoginError::Failed(e)) => Outcome::Refused(e.kind(), e.message().to_owned()),
            Err(other) => panic!("unexpected login error {other:?}"),
        }
    }

    /// The set of rows `domain` matches, one login per row (`domain AND login = row`).
    async fn matching(&mut self, domain: &Domain, ids: &[i64; 3]) -> Result<[bool; 3], Outcome> {
        let mut matched = [false; 3];
        for (slot, row) in ROWS.iter().enumerate() {
            let one = Domain::condition("login", Operator::Eq, row.login).unwrap();
            match self.run(Domain::and([domain.clone(), one])).await {
                Outcome::Matched(uid) => {
                    assert_eq!(uid, ids[slot], "the row the login selected");
                    matched[slot] = true;
                }
                Outcome::Denied => {}
                refused => return Err(refused),
            }
        }
        Ok(matched)
    }
}

fn truthy(value: &Value) -> bool {
    match value {
        Value::Bool(b) => *b,
        Value::Int(n) => *n != 0,
        Value::Str(text) => matches!(
            text.to_lowercase().as_str(),
            "y" | "yes" | "1" | "true" | "t" | "on"
        ),
        other => panic!("model has no truthiness for {other:?}"),
    }
}

/// Members of an `=`/`!=`/`in`/`not in` operand, and whether the operator is the negative one.
fn members(op: Operator, operand: &Value) -> (Vec<Value>, bool) {
    let negative = matches!(op, Operator::Ne | Operator::NotIn);
    let list = match (op, operand) {
        (Operator::Eq | Operator::Ne, one) => vec![one.clone()],
        (_, Value::List(items)) => items.clone(),
        (_, other) => panic!("membership operand {other:?}"),
    };
    (list, negative)
}

/// Boolean column: the set of truthiness values a condition accepts; NULL counts as false.
fn bool_model(op: Operator, operand: &Value, cell: Option<bool>) -> bool {
    let (list, negative) = members(op, operand);
    let seen: Vec<bool> = list.iter().map(truthy).collect();
    let accepts = |b: bool| {
        if negative {
            !seen.contains(&b)
        } else {
            seen.contains(&b)
        }
    };
    accepts(cell.unwrap_or(false))
}

/// A nullable column with a falsy value (`0` or `''`): `False` and the falsy value both mean
/// "unset", so a membership with either also accepts NULL, and `not in` with either excludes it.
fn nullable_model<T: PartialEq + Clone>(
    op: Operator,
    operand: &Value,
    falsy: T,
    convert: impl Fn(&Value) -> T,
    cell: Option<T>,
) -> bool {
    let (list, negative) = members(op, operand);
    let has_false = list.iter().any(|v| matches!(v, Value::Bool(false)));
    let mut params: Vec<T> = list
        .iter()
        .filter(|v| !matches!(v, Value::Bool(false)))
        .map(&convert)
        .collect();
    let nullish = has_false || params.contains(&falsy);
    if has_false && !params.contains(&falsy) {
        params.push(falsy);
    }
    match (cell, negative) {
        (None, false) => nullish,
        (None, true) => !nullish,
        (Some(v), false) => params.contains(&v),
        (Some(v), true) => !params.contains(&v),
    }
}

fn int_of(value: &Value) -> i64 {
    match value {
        Value::Int(n) => *n,
        Value::Bool(b) => i64::from(*b),
        other => panic!("model has no integer for {other:?}"),
    }
}

fn str_of(value: &Value) -> String {
    match value {
        Value::Str(text) => text.clone(),
        other => panic!("model has no text for {other:?}"),
    }
}

/// Integer inequality: the operand (`False` is 0) is compared to the cell; a NULL cell passes when
/// the falsy value 0 satisfies the comparison.
fn int_order_model(op: Operator, operand: &Value, cell: Option<i64>) -> bool {
    let bound = int_of(operand);
    let compare = |c: i64| match op {
        Operator::Lt => c < bound,
        Operator::Le => c <= bound,
        Operator::Gt => c > bound,
        Operator::Ge => c >= bound,
        other => panic!("not an inequality: {other:?}"),
    };
    compare(cell.unwrap_or(0))
}

type Expect = Box<dyn Fn(&Row) -> bool>;
type Cell = (String, Domain, Expect);

fn flag_cell(op: Operator, operand: Value) -> Cell {
    let label = format!("selection_flag {op:?} {operand:?}");
    let domain = Domain::condition("selection_flag", op, operand.clone()).unwrap();
    (
        label,
        domain,
        Box::new(move |row| bool_model(op, &operand, row.flag)),
    )
}

fn prio_cell(op: Operator, operand: Value) -> Cell {
    let label = format!("priority {op:?} {operand:?}");
    let domain = Domain::condition("priority", op, operand.clone()).unwrap();
    let expect: Expect = match op {
        Operator::Lt | Operator::Le | Operator::Gt | Operator::Ge => {
            Box::new(move |row| int_order_model(op, &operand, row.prio))
        }
        _ => Box::new(move |row| nullable_model(op, &operand, 0i64, int_of, row.prio)),
    };
    (label, domain, expect)
}

fn alias_cell(op: Operator, operand: Value) -> Cell {
    let label = format!("alias {op:?} {operand:?}");
    let domain = Domain::condition("alias", op, operand.clone()).unwrap();
    (
        label,
        domain,
        Box::new(move |row| {
            nullable_model(
                op,
                &operand,
                String::new(),
                str_of,
                row.alias.map(str::to_owned),
            )
        }),
    )
}

fn list(items: impl IntoIterator<Item = Value>) -> Value {
    Value::List(items.into_iter().collect())
}

fn no_values() -> Value {
    Value::List(Vec::new())
}

const F: Value = Value::Bool(false);
const T: Value = Value::Bool(true);

fn int(n: i64) -> Value {
    Value::Int(n)
}

fn text(t: &str) -> Value {
    Value::Str(t.to_owned())
}

async fn check_cells(w: &World, ids: &[i64; 3], cells: Vec<Cell>) {
    let mut runner = Runner { w, attempt: 0 };
    for (label, domain, expect) in cells {
        let expected: [bool; 3] = [expect(&ONE), expect(&TWO), expect(&NUL)];
        match runner.matching(&domain, ids).await {
            Ok(got) => assert_eq!(got, expected, "{label}: rows [one, two, null]"),
            Err(refused) => panic!("{label}: expected a verdict per row, got {refused:?}"),
        }
    }
}

#[tokio::test]
async fn r7_boolean_matrix() {
    let _slots = slots().await;
    let w = selection_world().await;
    let ids = create_rows(&w).await;
    *INJECTED_ORDER.lock().unwrap() = Some(vec![LoginOrderTerm::asc("id")]);
    let mut cells = Vec::new();
    for op in [Operator::Eq, Operator::Ne] {
        // numbers: 1 is true, 0 false; text is parsed (`'true'`/`'YES'` true, `'x'`/`''`/`'off'` false)
        for operand in [
            F,
            T,
            int(1),
            int(0),
            text("x"),
            text(""),
            text("true"),
            text("YES"),
            text("off"),
        ] {
            cells.push(flag_cell(op, operand));
        }
    }
    for op in [Operator::In, Operator::NotIn] {
        for operand in [
            no_values(),
            list([F]),
            list([T]),
            list([T, F]),
            list([int(1)]),
            list([text("x")]),
            list([text("true")]),
            list([int(0), text("x")]),
            list([int(0), text("on")]),
        ] {
            cells.push(flag_cell(op, operand));
        }
    }
    check_cells(&w, &ids, cells).await;
}

/// the falsy 0, inequality against NULL)
#[tokio::test]
async fn r7_integer_matrix() {
    let _slots = slots().await;
    let w = selection_world().await;
    let ids = create_rows(&w).await;
    *INJECTED_ORDER.lock().unwrap() = Some(vec![LoginOrderTerm::asc("id")]);
    let mut cells = Vec::new();
    for op in [Operator::Eq, Operator::Ne] {
        for operand in [F, int(0), int(3), int(5)] {
            cells.push(prio_cell(op, operand));
        }
    }
    for op in [Operator::In, Operator::NotIn] {
        for operand in [
            no_values(),
            list([F]),
            list([int(3)]),
            list([int(3), F]),
            list([int(0)]),
            list([int(5)]),
            list([int(3), int(5)]),
        ] {
            cells.push(prio_cell(op, operand));
        }
    }
    for op in [Operator::Lt, Operator::Le, Operator::Gt, Operator::Ge] {
        for operand in [int(0), int(3), F] {
            cells.push(prio_cell(op, operand));
        }
    }
    check_cells(&w, &ids, cells).await;
}

#[tokio::test]
async fn r7_char_matrix() {
    let _slots = slots().await;
    let w = selection_world().await;
    let ids = create_rows(&w).await;
    *INJECTED_ORDER.lock().unwrap() = Some(vec![LoginOrderTerm::asc("id")]);
    let mut cells = Vec::new();
    for op in [Operator::Eq, Operator::Ne] {
        for operand in [F, text(""), text("x"), text("y")] {
            cells.push(alias_cell(op, operand));
        }
    }
    for op in [Operator::In, Operator::NotIn] {
        for operand in [
            no_values(),
            list([F]),
            list([text("")]),
            list([text("x")]),
            list([text("x"), F]),
            list([text("y")]),
        ] {
            cells.push(alias_cell(op, operand));
        }
    }
    check_cells(&w, &ids, cells).await;
}

/// and/or over several columns)
#[tokio::test]
async fn r7_optimizer_and_composition_matrix() {
    let _slots = slots().await;
    let w = selection_world().await;
    let ids = create_rows(&w).await;
    *INJECTED_ORDER.lock().unwrap() = Some(vec![LoginOrderTerm::asc("id")]);
    let flag_true = || flag_cell(Operator::Eq, T);
    let prio_gt0 = || prio_cell(Operator::Gt, int(0));
    let alias_x = || alias_cell(Operator::Eq, text("x"));
    let mut cells: Vec<Cell> = Vec::new();
    // constants: an empty `in` is FALSE, an empty `not in` is TRUE (the optimizer rewrites them)
    cells.push(("constant FALSE".into(), Domain::FALSE, Box::new(|_| false)));
    cells.push(("constant TRUE".into(), Domain::TRUE, Box::new(|_| true)));
    // negation is the exact complement for equality and membership
    for (label, domain, expect) in [
        flag_true(),
        flag_cell(Operator::In, list([F])),
        prio_cell(Operator::Eq, int(3)),
        prio_cell(Operator::In, list([F])),
        alias_cell(Operator::Eq, F),
    ] {
        cells.push((
            format!("not ({label})"),
            !domain,
            Box::new(move |row| !expect(row)),
        ));
    }
    // and / or over different columns, constants dropped or absorbing
    let compose = |label: &str, or: bool, parts: Vec<Cell>, constant: Option<bool>| -> Cell {
        let mut domains: Vec<Domain> = parts.iter().map(|p| p.1.clone()).collect();
        match constant {
            Some(true) => domains.push(Domain::TRUE),
            Some(false) => domains.push(Domain::FALSE),
            None => {}
        }
        let domain = if or {
            Domain::or(domains)
        } else {
            Domain::and(domains)
        };
        let expects: Vec<Expect> = parts.into_iter().map(|p| p.2).collect();
        let expect: Expect = Box::new(move |row| {
            let mut values = expects.iter().map(|e| e(row));
            let base = if or {
                values.any(|v| v)
            } else {
                values.all(|v| v)
            };
            match (or, constant) {
                (true, Some(true)) => true,
                (true, Some(false)) => base,
                (false, Some(false)) => false,
                (false, Some(true)) => base,
                (_, None) => base,
            }
        });
        (label.to_owned(), domain, expect)
    };
    cells.push(compose(
        "flag or prio",
        true,
        vec![flag_true(), prio_gt0()],
        None,
    ));
    cells.push(compose(
        "flag and prio",
        false,
        vec![flag_true(), prio_gt0()],
        None,
    ));
    cells.push(compose(
        "flag or prio or alias",
        true,
        vec![flag_true(), prio_gt0(), alias_x()],
        None,
    ));
    cells.push(compose(
        "flag or FALSE",
        true,
        vec![flag_true()],
        Some(false),
    ));
    cells.push(compose("flag or TRUE", true, vec![flag_true()], Some(true)));
    cells.push(compose(
        "flag and FALSE",
        false,
        vec![flag_true()],
        Some(false),
    ));
    cells.push(compose(
        "flag and TRUE",
        false,
        vec![flag_true()],
        Some(true),
    ));
    check_cells(&w, &ids, cells).await;
}

/// refused with the redacted compiler message)
#[tokio::test]
async fn r7_operand_type_mismatch_matrix() {
    let _slots = slots().await;
    let w = selection_world().await;
    create_rows(&w).await;
    *INJECTED_ORDER.lock().unwrap() = Some(vec![LoginOrderTerm::asc("id")]);
    let mut runner = Runner { w: &w, attempt: 0 };
    let refused = Outcome::Refused(
        ErrorKind::Validation,
        "invalid scalar operand in login selection domain".to_owned(),
    );
    for (op, operand) in [
        (Operator::Eq, text("x")),
        (Operator::Ne, text("x")),
        (Operator::Gt, text("x")),
        (Operator::In, list([text("x")])),
        (Operator::NotIn, list([int(1), text("x")])),
    ] {
        let one = Domain::condition("login", Operator::Eq, ONE.login).unwrap();
        let domain = Domain::and([
            Domain::condition("priority", op, operand.clone()).unwrap(),
            one,
        ]);
        assert_eq!(
            runner.run(domain).await,
            refused,
            "priority {op:?} {operand:?}"
        );
    }
    // the first attempt of the rows never created a session or a failure for a refused domain
    assert_eq!(count(&w.h, "erp_session").await, 0);
    assert_eq!(count(&w.h, "erp_login_throttle").await, 0);
}

/// several terms)
#[tokio::test]
async fn r7_order_matrix() {
    let _slots = slots().await;
    let w = selection_world().await;
    let ids = create_rows(&w).await;
    // a second row with the same priority as `mx_one`, created later: it never wins a tie
    let twin = plain_user(&w.h, "mx_twin", "x", 3).await;
    let mut tx = w.h.begin().await.unwrap();
    tx.execute(
        "UPDATE res_users SET selection_flag = true WHERE id = $1",
        &[&twin],
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    let [one, two, nul] = ids;
    let everyone = |logins: &[&str]| {
        Domain::condition(
            "login",
            Operator::In,
            Value::List(logins.iter().map(|l| text(l)).collect()),
        )
        .unwrap()
    };
    let cells: Vec<(&str, Vec<LoginOrderTerm>, Domain, i64)> = vec![
        (
            "priority ASC puts NULL last",
            vec![LoginOrderTerm::asc("priority")],
            everyone(&["mx_one", "mx_two", "mx_nul"]),
            two,
        ),
        (
            "priority DESC puts NULL first",
            vec![LoginOrderTerm::desc("priority")],
            everyone(&["mx_one", "mx_two", "mx_nul"]),
            nul,
        ),
        (
            "priority DESC without NULL",
            vec![LoginOrderTerm::desc("priority")],
            everyone(&["mx_one", "mx_two"]),
            one,
        ),
        (
            "id ASC is appended: a tie goes to the lower id",
            vec![LoginOrderTerm::desc("priority")],
            everyone(&["mx_one", "mx_twin"]),
            one,
        ),
        (
            "an explicit id DESC is kept",
            vec![LoginOrderTerm::desc("priority"), LoginOrderTerm::desc("id")],
            everyone(&["mx_one", "mx_twin"]),
            twin,
        ),
        (
            "two terms: the second breaks the first's tie",
            vec![
                LoginOrderTerm::asc("priority"),
                LoginOrderTerm::desc("alias"),
            ],
            everyone(&["mx_one", "mx_twin"]),
            one,
        ),
        (
            "Boolean ASC: false before true before NULL",
            vec![LoginOrderTerm::asc("selection_flag")],
            everyone(&["mx_one", "mx_two", "mx_nul"]),
            two,
        ),
        (
            "Char ASC: empty before text before NULL",
            vec![LoginOrderTerm::asc("alias")],
            everyone(&["mx_one", "mx_two", "mx_nul"]),
            two,
        ),
    ];
    let mut runner = Runner { w: &w, attempt: 0 };
    for (label, terms, domain, winner) in cells {
        *INJECTED_ORDER.lock().unwrap() = Some(terms);
        assert_eq!(
            runner.run(domain).await,
            Outcome::Matched(winner),
            "{label}"
        );
    }
}

// ---- LD2: the failure-accounting write fails -------------------------------------------------

async fn counted_rows(w: &World) -> i64 {
    count(&w.h, "erp_login_throttle WHERE kind IN ('address','login')").await
}

const TRIGGER_ERROR: &str = "ld2 failure counter unavailable";

async fn refuse_counter_writes(h: &Harness) {
    let mut tx = h.begin().await.unwrap();
    tx.batch(
        "CREATE FUNCTION ld2_refuse_counter() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN \
         RAISE EXCEPTION 'ld2 failure counter unavailable' USING ERRCODE='P0001'; END $$; \
         CREATE TRIGGER ld2_refuse_counter BEFORE INSERT OR UPDATE ON erp_login_throttle \
         FOR EACH ROW EXECUTE FUNCTION ld2_refuse_counter()",
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
}

async fn allow_counter_writes(h: &Harness) {
    let mut tx = h.begin().await.unwrap();
    tx.batch("DROP TRIGGER ld2_refuse_counter ON erp_login_throttle")
        .await
        .unwrap();
    tx.commit().await.unwrap();
}

fn assert_failed_by_the_store(result: Result<erp_security::auth::Login, LoginError>, what: &str) {
    match result {
        Err(LoginError::Failed(e)) => assert_eq!(
            (e.kind(), e.message(), e.sqlstate()),
            (ErrorKind::Database, TRIGGER_ERROR, Some("P0001")),
            "{what}"
        ),
        other => panic!("{what}: expected LoginError::Failed, got {other:?}"),
    }
}

/// with the store back it is `Denied` and counted, LD2)
#[tokio::test]
async fn ld2_unknown_login_failure_write_error_is_failed() {
    let _slots = slots().await;
    let w = selection_world().await;
    refuse_counter_writes(&w.h).await;
    let before = w.auth.stats().failure_write_errors;
    assert_failed_by_the_store(
        login_from(&w.auth, "ld2_nobody", "wrong", "192.0.2.50").await,
        "unknown login",
    );
    assert_eq!(w.auth.stats().failure_write_errors, before + 1);
    assert_eq!(
        count(&w.h, "erp_login_throttle").await,
        0,
        "no partial rows"
    );
    allow_counter_writes(&w.h).await;
    assert!(matches!(
        login_from(&w.auth, "ld2_nobody", "wrong", "192.0.2.50").await,
        Err(LoginError::Denied)
    ));
    assert_eq!(
        count(&w.h, "erp_login_throttle WHERE kind IN ('address','login')").await,
        2,
        "the address and the login key were counted once the store answers"
    );
}

/// `Failed`; with the store back it is `Denied` and counted, LD2: the base check answers such a hash
/// with the access-denied verdict, so this is the AccessDenied arm of the accounting)
#[tokio::test]
async fn ld2_memory_over_reservation_failure_write_error_is_failed() {
    let _slots = slots().await;
    let w = selection_world().await;
    let uid = plain_user(&w.h, "ld2_heavy", "ld2_heavy_alias", 1).await;
    // a verifiable hash that needs twice the memory a permit reserves
    let reserved = w.pool.config().memory_kib();
    assert_eq!(reserved, limits::ARGON2_MEMORY_KIB_MIN);
    let heavy = erp_security::auth::password::hash(
        b"pw",
        &HashParams::argon2id(2 * reserved, limits::ARGON2_TIME_MIN).unwrap(),
    )
    .unwrap();
    let mut tx = w.h.begin().await.unwrap();
    tx.execute(
        "UPDATE res_users SET password = $2 WHERE id = $1",
        &[&uid, &heavy],
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();

    refuse_counter_writes(&w.h).await;
    let (errors, over) = (
        w.auth.stats().failure_write_errors,
        w.pool.stats().over_reservation,
    );
    assert_failed_by_the_store(
        login_from(&w.auth, "ld2_heavy", "pw", "192.0.2.60").await,
        "over reservation",
    );
    assert_eq!(w.auth.stats().failure_write_errors, errors + 1);
    assert_eq!(w.pool.stats().over_reservation, over + 1);
    assert_eq!(
        count(&w.h, "erp_login_throttle").await,
        0,
        "no partial rows"
    );

    allow_counter_writes(&w.h).await;
    assert!(matches!(
        login_from(&w.auth, "ld2_heavy", "pw", "192.0.2.60").await,
        Err(LoginError::Denied)
    ));
    assert_eq!(w.pool.stats().over_reservation, over + 2);
    assert_eq!(
        count(&w.h, "erp_login_throttle WHERE kind IN ('address','login')").await,
        2,
        "the refusal is counted as a failure once the store answers"
    );
    assert_eq!(count(&w.h, "erp_session").await, 0);
}

/// failing accounting write is counted as a write error and a verdict failure, LD2 detached
/// verdict)
#[tokio::test]
async fn ld2_detached_verdict_failure_write_error_is_counted() {
    let _slots = slots().await;
    let w = selection_world().await;
    plain_user(&w.h, "ld2_detached", "ld2_detached_alias", 1).await;
    refuse_counter_writes(&w.h).await;
    let (errors, verdicts) = (
        w.auth.stats().failure_write_errors,
        w.auth.stats().verdict_failures,
    );
    let release = hold_permits(w.pool, w.pool.config().permits()).await;
    let auth = Arc::new(w.auth);
    let task_auth = Arc::clone(&auth);
    let caller =
        tokio::spawn(
            async move { login_from(&task_auth, "ld2_detached", "wrong", "192.0.2.70").await },
        );
    // the verdict waits for a hashing permit; its caller goes away first
    until_waiting(w.pool, 1).await;
    caller.abort();
    assert!(caller.await.unwrap_err().is_cancelled());
    drop(release);

    let started = Instant::now();
    loop {
        let stats = auth.stats();
        if stats.failure_write_errors == errors + 1 && stats.verdict_failures == verdicts + 1 {
            break;
        }
        assert!(
            started.elapsed() < Duration::from_secs(30),
            "the detached verdict never finished: {stats:?}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(
        count(&w.h, "erp_login_throttle").await,
        0,
        "no partial rows"
    );
    assert_eq!(count(&w.h, "erp_session").await, 0);
}

/// pool's memory over-reservation refusal or with another error: the refusal is counted, so a
/// failing write is `Failed` and a working store counts it once; any other error writes nothing)
#[tokio::test]
async fn ld2_memory_refusal_arm_and_other_errors_of_the_failure_accounting() {
    let _slots = slots().await;
    let h = Harness::builder([
        &erp_addon_base::ADDON,
        &erp_data::model_data::ADDON,
        &erp_test_models::test_auth::ADDON,
    ])
    .with_decls([erp_data::ir_model_data_decl()])
    .require_core_tables(erp_security::auth::core_tables())
    .build()
    .await
    .expect("harness");
    let w = world_with(h, AuthConfig::DEFAULT, HashPoolConfig::DEFAULT).await;
    for login in ["poolmem_u", "broken_u"] {
        user_with_password(&w.h, login, "pw").await;
    }
    let broken = |result: Result<erp_security::auth::Login, LoginError>| match result {
        Err(LoginError::Failed(e)) => assert_eq!(
            (e.kind(), e.message()),
            (
                ErrorKind::Validation,
                "test_auth cannot check this credential"
            )
        ),
        other => panic!("expected the override's own error, got {other:?}"),
    };

    refuse_counter_writes(&w.h).await;
    let (errors, over) = (
        w.auth.stats().failure_write_errors,
        w.pool.stats().over_reservation,
    );
    assert_failed_by_the_store(
        login_from(&w.auth, "poolmem_u", "pw", "192.0.2.81").await,
        "MemoryOverReservation",
    );
    assert_eq!(w.auth.stats().failure_write_errors, errors + 1);
    assert_eq!(w.pool.stats().over_reservation, over + 1);
    // any other error is returned as it is, nothing written
    broken(login_from(&w.auth, "broken_u", "pw", "192.0.2.83").await);
    assert_eq!(
        w.auth.stats().failure_write_errors,
        errors + 1,
        "only the first wrote"
    );
    assert_eq!(counted_rows(&w).await, 0);

    allow_counter_writes(&w.h).await;
    assert!(matches!(
        login_from(&w.auth, "poolmem_u", "pw", "192.0.2.81").await,
        Err(LoginError::Denied)
    ));
    assert_eq!(
        counted_rows(&w).await,
        2,
        "the refusal counts the address and the login"
    );
    broken(login_from(&w.auth, "broken_u", "pw", "192.0.2.83").await);
    assert_eq!(counted_rows(&w).await, 2, "another error counts nothing");
}

/// denied without any accounting write, known or unknown, even while the store refuses writes)
#[tokio::test]
async fn ld2_overloaded_pool_denies_without_a_failure_write() {
    let _slots = slots().await;
    let w = selection_world_with(HashPoolConfig::new(1, 1, limits::ARGON2_MEMORY_KIB_MIN).unwrap())
        .await;
    plain_user(&w.h, "ld2_busy", "ld2_busy_alias", 1).await;
    refuse_counter_writes(&w.h).await;
    let errors = w.auth.stats().failure_write_errors;
    let release = hold_permits(w.pool, 1).await;
    for (login, addr) in [("ld2_busy", "192.0.2.91"), ("ld2_nobody", "192.0.2.92")] {
        assert!(
            matches!(
                login_from(&w.auth, login, "pw", addr).await,
                Err(LoginError::Denied)
            ),
            "{login}"
        );
    }
    drop(release);
    assert_eq!(w.pool.stats().overloaded, 2);
    assert_eq!(
        w.auth.stats().failure_write_errors,
        errors,
        "no write was tried"
    );
    allow_counter_writes(&w.h).await;
    assert_eq!(count(&w.h, "erp_login_throttle").await, 0);
}
