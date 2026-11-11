#![allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]
#[path = "auth_support/mod.rs"]
mod support;
use support::*;
#[tokio::test]
async fn browser_tokens_reload_with_authentication_secret_generation() {
    let world = world().await;
    let first = world.auth.browser_tokens().await.unwrap();
    let old_cookie = first.issue().unwrap();
    let prefs = first.verify(&old_cookie).unwrap();
    let old_csrf = first.csrf(&prefs, 1_000).unwrap();
    set_param(
        &world.h,
        "database.secret",
        "replacement-test-only-secret-32-bytes",
    )
    .await;
    let next = world.auth.browser_tokens().await.unwrap();
    assert!(next.verify(&old_cookie).is_err());
    let new_cookie = next.issue().unwrap();
    let new_prefs = next.verify(&new_cookie).unwrap();
    assert!(next.verify_csrf(&new_prefs, &old_csrf, 1_001).is_err());
    assert!(first.verify(&new_cookie).is_err());
}
