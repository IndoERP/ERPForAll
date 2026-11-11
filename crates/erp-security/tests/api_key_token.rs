//! Native token primitives only; database issuance and authentication belong to later children.

use erp_fields::ErrorKind;
use erp_security::auth::apikey::{ApiKeyHasher, GeneratedApiKey, NativeApiKey};
use erp_security::auth::secret::DatabaseSecret;

// Public dummy input, generated
const TOKEN: &str = "erpk_AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8";
const SECRET: &str = "0123456789abcdef0123456789abcdef";

#[test]
fn canonical_token_and_independent_digest_vector() {
    let token = NativeApiKey::parse(TOKEN).unwrap();
    assert_eq!(token.display_prefix(), "AAECAwQF");
    let hasher = ApiKeyHasher::derive(&DatabaseSecret::from_stored(SECRET).unwrap()).unwrap();
    let digest = hasher.digest(&token);
    assert_eq!(digest.kid(), 1);
    let hex: String = digest.bytes().iter().map(|b| format!("{b:02x}")).collect();
    assert_eq!(
        hex,
        "bfff270cf3ab15e280e8ce9ab66a3b175fdeafceae87fe981a646436a7eed9ab"
    );
    assert_eq!(
        hasher.digest_for_kid(&token, 1).unwrap().bytes(),
        digest.bytes()
    );
}

#[test]
fn malformed_native_keys_fail_with_one_redacted_error() {
    let invalid = [
        String::new(),
        TOKEN[..47].to_owned(),
        format!("{TOKEN}A"),
        TOKEN.replacen("erpk_", "ERPK_", 1),
        format!("{TOKEN}="),
        format!(" {TOKEN}"),
        format!("{TOKEN}\r\n"),
        format!("erpk_{}", "é".repeat(21) + "A"),
        TOKEN.replace('8', "9"), // same decoded bytes if nonzero trailing bits were accepted
        format!("erpk_{}+", "A".repeat(42)),
        format!("erpk_{}节", "A".repeat(40)),
        "x".repeat(1024 * 1024),
        "0123456789abcdef0123456789abcdef01234567".to_owned(), // legacy is a separate path
    ];
    for value in invalid {
        let error = NativeApiKey::parse(&value).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::AccessDenied);
        assert_eq!(error.to_string(), "Invalid apikey");
    }
}

#[test]
fn generated_keys_are_canonical_distinct_and_redacted() {
    let a = GeneratedApiKey::generate().unwrap();
    let b = GeneratedApiKey::generate().unwrap();
    assert_eq!(a.expose().len(), 48);
    assert_ne!(a.expose(), b.expose());
    let parsed = NativeApiKey::parse(a.expose()).unwrap();
    assert_eq!(parsed.display_prefix(), &a.expose()[5..13]);
    assert_eq!(a.token().display_prefix(), parsed.display_prefix());
    let hasher = ApiKeyHasher::derive(&DatabaseSecret::from_stored(SECRET).unwrap()).unwrap();
    let digest = hasher.digest(&a.token());
    assert_eq!(hasher.digest(&parsed).bytes(), digest.bytes());
    assert_eq!(format!("{a:?}"), "GeneratedApiKey(<redacted>)");
    assert_eq!(format!("{parsed:?}"), "NativeApiKey(<redacted>)");
    assert_eq!(format!("{hasher:?}"), "ApiKeyHasher(<redacted>)");
    assert_eq!(format!("{digest:?}"), "ApiKeyDigest(<redacted>)");
}

#[test]
fn digest_binds_secret_and_full_token_and_rejects_unknown_kids() {
    let token = NativeApiKey::parse(TOKEN).unwrap();
    let secret = DatabaseSecret::from_stored(SECRET).unwrap();
    let a = ApiKeyHasher::derive(&secret).unwrap();
    let b = ApiKeyHasher::derive(&DatabaseSecret::from_stored(&"z".repeat(32)).unwrap()).unwrap();
    assert_ne!(a.digest(&token).bytes(), b.digest(&token).bytes());
    let changed = TOKEN.replacen('A', "B", 1);
    assert_ne!(
        a.digest(&token).bytes(),
        a.digest(&NativeApiKey::parse(&changed).unwrap()).bytes()
    );
    for kid in [i16::MIN, -1, 0, 2, i16::MAX] {
        let error = a.digest_for_kid(&token, kid).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Programming);
        assert_eq!(
            error.to_string(),
            format!("API key digest kid {kid} is not available")
        );
    }
}
