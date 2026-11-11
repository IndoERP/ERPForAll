//! Allocation budget of the password login and of the session check (
//! §14.11; standards §5b): every path pinned at its measured value plus 3 % (the margin of the
//! other pins, e.g. `alloc_rpc_kinds.rs`), a ratchet: they must not grow. The paths: a warm
//! login (verify, no rehash), the same login from a client address, a denial without a stored
//! hash (the dummy verify, the flood path), a login that rehashes a legacy hash, a session check,
//! and a refused session check.
//!
//! The bytes of the hashing paths are dominated by Argon2id's memory block, allocated once per
//! hash on the hashing thread; its size comes from the parameters (`HashParams::DEFAULT`), not a
//! literal, and the pins add the measured rest.
//!
//! One binary, run on the main thread by `erp_alloc_budget::harness` (lesson: every
//! thread counted, the hashing pool's blocking thread included).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::print_stdout,
    reason = "tests"
)]

mod auth_support;

use auth_support::*;
use erp_security::auth::{CredentialOptions, HashParams, LoginError, SessionSecret};
use erp_testkit::alloc::{self, AllocStats, Budget};

#[global_allocator]
static ALLOC: alloc::Alloc = alloc::Alloc;

alloc::harness!(
    #[ignore = "DEFERRED_PERFORMANCE: owner milestone-end policy; semantic coverage runs in ordinary CI"]
    login_and_session_check_allocate_within_their_pins,
    #[ignore = "rehash allocation attribution probe; run explicitly"]
    profile_rehash,
    login_and_session_semantics,
);

/// Argon2id's memory block at the default parameters, bytes.
const ARGON2_BLOCK: u64 = (HashParams::DEFAULT.memory_kib() as u64) << 10;

/// A pin: `blocks` and `rest` bytes measured (besides `argon2` Argon2id blocks), plus 3 %.
const fn pin(blocks: u64, rest: u64, argon2: u64) -> Budget {
    Budget::new(
        blocks + (blocks * 3).div_ceil(100),
        argon2 * ARGON2_BLOCK + rest + (rest * 3).div_ceil(100),
    )
}

// Against the
// figures of the round-1 integration (2e6c7b1e, the previous pins' measured values: login 125 /
// 40,212, denial 117 / 46,926, rehash 195 / 54,240, check 90 / 30,340, refused 72 / 28,623):
// rounds 2 to 4 lowered the logins (one block and 751 bytes), the denial (11 blocks and 14,281
// bytes) and the rehash (10 blocks and 2,869 bytes), and added 24 bytes to both session checks
// (not attributed to one change: rounds 2 to 4 all touched the path and were not measured one by
// one). The in-process login is measured before the first login with a client address, as before.
// The other order is not measured for the record: one run in that order at 03ea7010 lost its raw
// output to a sync, so its figure is not recorded (review round 4 C4-2).

/// One warm login (verify, no rehash): 124 blocks, the Argon2id block and 39,461 bytes (84 blocks
/// and 26,966 bytes before review round 1: two one-flight transactions, the admission permit, the
/// verdict task and its watch add the rest).
const LOGIN: Budget = pin(124, 39_461, 1);
/// One warm login from a client address (`ClientAddr::Known`: the address and pair keys, the
/// trusted-pair upsert and trim, the address text on the session row; rust review round 3
/// RD-1): 133 blocks, the Argon2id block and 41,581 bytes. The login as an HTTP route runs it
/// (request, proxy mapping, cookie) is's pin (design §14.13).
const LOGIN_FROM: Budget = pin(133, 41_581, 1);
/// One denial of an unknown login (the dummy verify and the failure count): 106 blocks, the
/// Argon2id block and 32,645 bytes.
const DENIED: Budget = pin(106, 32_645, 1);
/// One login that rehashes a legacy (PBKDF2) hash: its verify, the new hash and the end of the
/// user's other sessions (none here): 185 blocks, the Argon2id block (the new hash; PBKDF2
/// verifies in a few hundred bytes) and 51,371 bytes.
const REHASH: Budget = pin(185, 51_371, 1);
/// One warm session check (the opening flight with the check, and the rollback): 90 blocks,
/// 30,364 bytes.
const CHECK: Budget = pin(90, 30_364, 0);
/// One refused session check (an unknown secret: no row, nothing deleted): 72 blocks, 28,647
/// bytes.
const REFUSED: Budget = pin(72, 28_647, 0);

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

/// Print every path's figures first, then check each against its pin: a run that fails a pin
/// still shows all the figures (the re-measure reads them).
#[derive(Default)]
struct Report(Vec<(&'static str, AllocStats, Budget)>);

impl Report {
    fn add(&mut self, name: &'static str, stats: AllocStats, budget: Budget) {
        println!(
            "{name}: {} blocks, {} bytes ({} besides the Argon2id blocks)",
            stats.total_blocks,
            stats.total_bytes,
            stats.total_bytes % ARGON2_BLOCK
        );
        self.0.push((name, stats, budget));
    }

    fn check(self) {
        for (name, stats, budget) in &self.0 {
            println!("checking {name}");
            stats.assert_within(budget);
        }
    }
}

fn login_and_session_check_allocate_within_their_pins() {
    run_scenario(false);
}

fn profile_rehash() {
    run_scenario(true);
}

/// Both modes execute the same setup and earlier counting windows. The opt-in probe changes
/// only the rehash profiler, then returns before the unrelated check/refusal windows.
fn run_scenario(profile: bool) {
    let rt = runtime();
    let w = rt.block_on(world());
    rt.block_on(async {
        user_with_password(&w.h, "bob", "pw").await;
        set_param(&w.h, "password.hashing.scheme", "pbkdf2_sha512").await;
        user_with_password(&w.h, "legacy", "pw").await;
        set_param(&w.h, "password.hashing.scheme", "argon2id").await;
    });
    // warm: the settings cache, the dummy hash, the pooled connections, a first check
    let first = rt.block_on(login(&w.auth, "bob", "pw")).unwrap();
    let secret = cookie(&first);
    let unknown = SessionSecret::generate().unwrap();
    rt.block_on(async {
        assert!(matches!(
            login(&w.auth, "nobody", "pw").await,
            Err(LoginError::Denied)
        ));
        check_user(&w.auth, &secret).await.unwrap();
        w.auth
            .begin_session(&unknown, CredentialOptions::new())
            .await
            .unwrap_err();
    });
    let mut report = Report::default();
    let (stats, made) =
        alloc::measure(|| rt.block_on(login(&w.auth, "bob", "pw"))).expect("measure");
    made.unwrap();
    report.add("login", stats, LOGIN);
    // warm the path with a client address (its pair row exists from here on)
    rt.block_on(login_from(&w.auth, "bob", "pw", "192.0.2.1"))
        .unwrap();
    let (stats, made) =
        alloc::measure(|| rt.block_on(login_from(&w.auth, "bob", "pw", "192.0.2.1")))
            .expect("measure");
    made.unwrap();
    report.add("login from an address", stats, LOGIN_FROM);
    let (stats, refused) =
        alloc::measure(|| rt.block_on(login(&w.auth, "nobody", "pw"))).expect("measure");
    assert!(matches!(refused, Err(LoginError::Denied)));
    report.add("denied (dummy)", stats, DENIED);
    if profile {
        let out = std::env::var("ERP_PROFILE_OUT").expect("ERP_PROFILE_OUT is required");
        let profiler = dhat::Profiler::builder().file_name(&out).build();
        let rehashed = rt.block_on(login(&w.auth, "legacy", "pw"));
        let stats = dhat::HeapStats::get();
        drop(profiler);
        assert!(rehashed.unwrap().auth().rehash());
        println!("rehash attribution profile {out}: {stats:?}");
        return;
    }
    let (stats, rehashed) =
        alloc::measure(|| rt.block_on(login(&w.auth, "legacy", "pw"))).expect("measure");
    assert!(rehashed.unwrap().auth().rehash());
    report.add("rehash", stats, REHASH);
    let (stats, user) =
        alloc::measure(|| rt.block_on(check_user(&w.auth, &secret))).expect("measure");
    user.unwrap();
    report.add("session check", stats, CHECK);
    let (stats, refused) =
        alloc::measure(|| rt.block_on(w.auth.begin_session(&unknown, CredentialOptions::new())))
            .expect("measure");
    assert!(refused.is_err());
    report.add("refused session check", stats, REFUSED);
    report.check();
}

fn login_and_session_semantics() {
    let rt = runtime();
    let w = rt.block_on(world());
    rt.block_on(async {
        user_with_password(&w.h, "bob", "pw").await;
        set_param(&w.h, "password.hashing.scheme", "pbkdf2_sha512").await;
        user_with_password(&w.h, "legacy", "pw").await;
        set_param(&w.h, "password.hashing.scheme", "argon2id").await;
        let first = login(&w.auth, "bob", "pw").await.unwrap();
        let secret = cookie(&first);
        login_from(&w.auth, "bob", "pw", "192.0.2.1").await.unwrap();
        assert!(matches!(
            login(&w.auth, "nobody", "pw").await,
            Err(LoginError::Denied)
        ));
        assert!(
            login(&w.auth, "legacy", "pw")
                .await
                .unwrap()
                .auth()
                .rehash()
        );
        check_user(&w.auth, &secret).await.unwrap();
        let unknown = SessionSecret::generate().unwrap();
        assert!(
            w.auth
                .begin_session(&unknown, CredentialOptions::new())
                .await
                .is_err()
        );
    });
}
