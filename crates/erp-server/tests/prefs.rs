#![allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]
use erp_server::prefs::{PrefsError, PrefsSigner};

#[test]
fn signed_prefs_bind_csrf_and_refuse_tamper_or_expiry() {
    let signer = PrefsSigner::new("test-only-database-key-material-32bytes").unwrap();
    let cookie = signer.issue().unwrap();
    let prefs = signer.verify(&cookie).unwrap();
    let token = signer.csrf(&prefs, 1_000).unwrap();
    assert_eq!(signer.verify_csrf(&prefs, &token, 1_001), Ok(()));
    assert_eq!(
        signer.verify_csrf(&prefs, &token, 173_801),
        Err(PrefsError::Csrf)
    );
    let other = signer.verify(&signer.issue().unwrap()).unwrap();
    assert_eq!(
        signer.verify_csrf(&other, &token, 1_001),
        Err(PrefsError::Csrf)
    );
    let wrong_key = PrefsSigner::new("another-test-only-database-key-32bytes").unwrap();
    assert_eq!(wrong_key.verify(&cookie).unwrap_err(), PrefsError::Cookie);
    let mut bad = cookie.clone();
    bad.replace_range(0..1, if &cookie[0..1] == "A" { "B" } else { "A" });
    assert_eq!(signer.verify(&bad).unwrap_err(), PrefsError::Cookie);
    for malformed in ["", "x.y.z", "a.b.c.d"] {
        assert_eq!(signer.verify(malformed).unwrap_err(), PrefsError::Cookie);
    }
    for malformed in ["", "0o1234", "x", "0o0001"] {
        let error = signer.verify_csrf(&prefs, malformed, 1_001).unwrap_err();
        assert_eq!(error, PrefsError::Csrf);
        assert_eq!(error.to_string(), "Session expired (invalid CSRF token)");
    }
    assert!(!format!("{signer:?}").contains("test-only"));
}

#[test]
fn response_nonce_is_fixed_header_safe_random_material() {
    let nonce = erp_server::prefs::ResponseNonce::generate().unwrap();
    assert_eq!(nonce.as_str().len(), 32);
    assert!(
        nonce
            .as_str()
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    );
    // A nonce is explicitly supplied to rendering; Debug does not disclose it in diagnostic logs.
    assert_eq!(format!("{nonce:?}"), "ResponseNonce(<redacted>)");
}
