//! Floor comparison of the two served paths (
//! `perf-first.md` §5b): a session check and a password login through [`Auth`], next to the
//! same logical work in hand-written SQL on one `tokio-postgres` connection (the floor).
//!
//! - Session check: [`Auth::begin_session`] (the opening flight with the check, the token
//!   verified against the user's fields) and the rollback of the request's transaction. Floor:
//!   `BEGIN` pipelined with one `SELECT` of the session row and its user by digest, then
//!   `ROLLBACK` (two round trips, like ours; the floor reads the same principal columns but
//!   verifies no token and reads no signal generation). `floor_unprepared` sends the same
//!   statement unnamed, parsed and planned per call, as the ORM's opening flight does.
//! - Login: [`Auth::authenticate`] (phase A: the throttle rows, the trusted pair, the user and the
//!   installed languages in one READ COMMITTED READ ONLY flight; the Argon2id verify on the
//!   hashing pool with no connection held; phase B: the login's failures cleared, the pair
//!   trusted, a `res_users_log` row and an `erp_session` row in one READ COMMITTED flight). Floor:
//!   the same two flights in hand-written SQL (`BEGIN`, the user, throttle and languages read,
//!   `COMMIT`; the same verify on the same pool; `BEGIN`, the inserts, `COMMIT`): two round trips,
//!   like ours. What the floor leaves out is listed next to the
//!   ratio (the settings and keys cache check, the session token, the admission and the verdict
//!   task, the trusted-pair upsert and the session cap's count). The user's sessions are deleted
//!   before each iteration of both (outside the measurement), so the cap of 100 sessions per user
//!   never ends sessions inside the measured login.
//! - `argon2id_verify`: the verify alone, which dominates both logins.
//!
//! Needs `DATABASE_URL` (a test cluster, like the tests). Results.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::print_stdout,
    reason = "benchmark setup"
)]

use std::collections::BTreeMap;
use std::hint::black_box;

use criterion::{BatchSize, Criterion, criterion_group, criterion_main};
use erp_fields::Value;
use erp_orm::model_hooks::call_init;
use erp_security::auth::{
    Auth, AuthConfig, ClientAddr, CredentialOptions, HashPool, HashPoolConfig, LoginRequest,
    SessionSecret, StoredHash,
};
use erp_testkit::Harness;
use erp_testkit::deadpool_postgres::Object;
use erp_testkit::tokio_postgres::types::{ToSql, Type};

const LOGIN: &str = "bench";
const PASSWORD: &str = "a bench password";

/// The session row, its user's fields the token covers, and the user's principal (groups and
/// companies), as the check reads them.
const FLOOR_CHECK_SQL: &str = "\
SELECT u.id, u.active, u.company_id,
       ARRAY(SELECT r.gid FROM res_groups_users_rel r WHERE r.uid = u.id ORDER BY 1),
       ARRAY(SELECT c.cid FROM res_company_users_rel c WHERE c.user_id = u.id ORDER BY 1),
       s.id, s.state, s.token, s.token_kid, s.expires_at > now(), u.login, u.password
  FROM erp_session s JOIN res_users u ON u.id = s.user_id
 WHERE s.sid_digest = $1 OR s.prev_digest = $1";

/// The user, the failures of its keys and the installed languages (`res_lang` exists in the
/// bench's database only once adds the model; ours then reads it the same way).
const FLOOR_LOGIN_SQL: &str = "\
SELECT u.id, u.password, u.lang, u.tz,
       (SELECT max(failures) FROM erp_login_throttle WHERE key_hash = ANY($2)),
       NULL::text[]
  FROM res_users u WHERE u.login = $1 AND u.active";

const FLOOR_SUCCESS_SQL: &str = "\
WITH cleared AS (DELETE FROM erp_login_throttle WHERE key_hash = ANY($2)),
     logged AS (INSERT INTO res_users_log (create_uid, create_date, write_uid, write_date)
                VALUES ($1, now() AT TIME ZONE 'UTC', $1, now() AT TIME ZONE 'UTC'))
INSERT INTO erp_session (id, user_id, sid_digest, token, token_kid, state, expires_at, context)
VALUES (uuidv7(), $1, sha256(random()::text::bytea), sha256(random()::text::bytea), 1, 'active',
        now() + interval '7 days', jsonb_build_object('lang', $3::text, 'tz', $4::text, 'uid', $1))";

struct World {
    rt: tokio::runtime::Runtime,
    _h: Harness,
    auth: Auth,
    pool: &'static HashPool,
    floor: Object,
    secret: SessionSecret,
    digest: Vec<u8>,
}

fn world() -> World {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let pool: &'static HashPool = Box::leak(Box::new(HashPool::new(HashPoolConfig::DEFAULT)));
    let (h, auth, floor, secret, digest) = rt.block_on(async {
        let h = Harness::builder([&erp_addon_base::ADDON, &erp_data::model_data::ADDON])
            .with_decls([erp_data::ir_model_data_decl()])
            .require_core_tables(erp_security::auth::core_tables())
            .build()
            .await
            .expect("harness (DATABASE_URL)");
        let mut tx = h.begin().await.unwrap();
        call_init(&mut tx, "ir.config_parameter").await.unwrap();
        call_init(&mut tx, "res.users").await.unwrap();
        // the database holds none of base's groups: create the user as the base installation
        erp_testkit::base_install::set_base_installing(&mut tx, true).unwrap();
        let vals: BTreeMap<String, Value> =
            [("name", LOGIN), ("login", LOGIN), ("password", PASSWORD)]
                .into_iter()
                .map(|(k, v)| (k.to_owned(), Value::str(v)))
                .collect();
        tx.env()
            .records("res.users")
            .unwrap()
            .create_values(&mut tx, &[vals])
            .await
            .unwrap();
        tx.commit().await.unwrap();
        let auth = Auth::new(h.orm().clone(), AuthConfig::DEFAULT, pool)
            .await
            .unwrap();
        let login = auth
            .authenticate(LoginRequest::new(
                LOGIN,
                PASSWORD,
                ClientAddr::InProcess("bench"),
            ))
            .await
            .unwrap();
        let secret = SessionSecret::from_cookie(&login.session().secret().to_cookie()).unwrap();
        let floor = h.db().client().await.unwrap();
        let digest: Vec<u8> = floor
            .query_one(
                "SELECT sid_digest FROM erp_session WHERE id = $1::text::uuid",
                &[&login.session().id().to_text()],
            )
            .await
            .unwrap()
            .get(0);
        (h, auth, floor, secret, digest)
    });
    World {
        rt,
        _h: h,
        auth,
        pool,
        floor,
        secret,
        digest,
    }
}

async fn orm_check(w: &World) {
    let begun = w
        .auth
        .begin_session(&w.secret, CredentialOptions::new())
        .await
        .unwrap();
    let mut tx = begun.tx;
    tx.rollback().await.unwrap();
    black_box(begun.session);
}

async fn floor_check(w: &World, stmt: &erp_testkit::tokio_postgres::Statement) {
    let begin = w
        .floor
        .batch_execute("BEGIN ISOLATION LEVEL REPEATABLE READ");
    let params: [&(dyn ToSql + Sync); 1] = [&w.digest];
    let row = w.floor.query_one(stmt, &params);
    let (began, row) = tokio::join!(begin, row);
    began.unwrap();
    black_box(row.unwrap());
    w.floor.batch_execute("ROLLBACK").await.unwrap();
}

/// The floor check with an unnamed statement (parsed and planned per call, as the ORM's
/// opening flight sends its statements).
async fn floor_check_unprepared(w: &World) {
    let begin = w
        .floor
        .batch_execute("BEGIN ISOLATION LEVEL REPEATABLE READ");
    let params: [(&(dyn ToSql + Sync), Type); 1] = [(&w.digest, Type::BYTEA)];
    let row = w.floor.query_typed(FLOOR_CHECK_SQL, &params);
    let (began, row) = tokio::join!(begin, row);
    began.unwrap();
    black_box(row.unwrap());
    w.floor.batch_execute("ROLLBACK").await.unwrap();
}

async fn floor_login(
    w: &World,
    read: &erp_testkit::tokio_postgres::Statement,
    write: &erp_testkit::tokio_postgres::Statement,
) {
    let keys: Vec<Vec<u8>> = vec![vec![1; 32], vec![2; 32]];
    // phase A: one flight, no connection state kept
    let begin = w
        .floor
        .batch_execute("BEGIN ISOLATION LEVEL READ COMMITTED READ ONLY");
    let read_params: [&(dyn ToSql + Sync); 2] = [&LOGIN, &keys];
    let row = w.floor.query_one(read, &read_params);
    let commit = w.floor.batch_execute("COMMIT");
    let (began, row, committed) = tokio::join!(begin, row, commit);
    began.unwrap();
    committed.unwrap();
    let row = row.unwrap();
    let uid: i64 = row.get(0);
    let stored: String = row.get(1);
    let lang: Option<String> = row.get(2);
    let tz: Option<String> = row.get(3);
    let hash = StoredHash::parse(&stored).unwrap();
    let ok = w
        .pool
        .run(hash.memory_kib(), move || hash.verify(PASSWORD.as_bytes()))
        .await
        .unwrap()
        .unwrap();
    assert!(ok);
    // phase B: one flight
    let params: [&(dyn ToSql + Sync); 4] = [&uid, &keys, &lang, &tz];
    let begin = w
        .floor
        .batch_execute("BEGIN ISOLATION LEVEL READ COMMITTED");
    let insert = w.floor.execute(write, &params);
    let commit = w.floor.batch_execute("COMMIT");
    let (began, inserted, committed) = tokio::join!(begin, insert, commit);
    began.unwrap();
    inserted.unwrap();
    committed.unwrap();
}

/// Delete the bench user's sessions (outside the measurement: the session cap stays out of it).
fn clear_sessions(w: &World) {
    w.rt.block_on(async {
        w.floor
            .execute(
                "DELETE FROM erp_session WHERE user_id = \
                 (SELECT id FROM res_users WHERE login = $1)",
                &[&LOGIN],
            )
            .await
            .unwrap();
    });
}

fn bench(c: &mut Criterion) {
    let w = world();
    let (check_stmt, read_stmt, write_stmt) = w.rt.block_on(async {
        (
            w.floor.prepare(FLOOR_CHECK_SQL).await.unwrap(),
            w.floor.prepare(FLOOR_LOGIN_SQL).await.unwrap(),
            w.floor.prepare(FLOOR_SUCCESS_SQL).await.unwrap(),
        )
    });
    let stored: String = w.rt.block_on(async {
        w.floor
            .query_one("SELECT password FROM res_users WHERE login = $1", &[&LOGIN])
            .await
            .unwrap()
            .get(0)
    });

    let mut group = c.benchmark_group("auth_session_check");
    group.bench_function("orm", |b| b.iter(|| w.rt.block_on(orm_check(&w))));
    group.bench_function("floor", |b| {
        b.iter(|| w.rt.block_on(floor_check(&w, &check_stmt)));
    });
    group.bench_function("floor_unprepared", |b| {
        b.iter(|| w.rt.block_on(floor_check_unprepared(&w)));
    });
    group.finish();

    let mut group = c.benchmark_group("auth_login");
    group.sample_size(20);
    group.bench_function("orm", |b| {
        b.iter_batched(
            || clear_sessions(&w),
            |()| {
                w.rt.block_on(async {
                    black_box(
                        w.auth
                            .authenticate(LoginRequest::new(
                                LOGIN,
                                PASSWORD,
                                ClientAddr::InProcess("bench"),
                            ))
                            .await
                            .unwrap(),
                    );
                });
            },
            BatchSize::PerIteration,
        );
    });
    group.bench_function("floor", |b| {
        b.iter_batched(
            || clear_sessions(&w),
            |()| w.rt.block_on(floor_login(&w, &read_stmt, &write_stmt)),
            BatchSize::PerIteration,
        );
    });
    group.bench_function("argon2id_verify", |b| {
        let hash = StoredHash::parse(&stored).unwrap();
        b.iter(|| black_box(hash.verify(PASSWORD.as_bytes()).unwrap()));
    });
    group.finish();
}

criterion_group!(benches, bench);
criterion_main!(benches);
