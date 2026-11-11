#![allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]
use erp_server::proxy::{ProxyError, ProxyGate};

#[test]
fn forwarding_requires_both_trusted_peer_and_private_token() {
    let peer = "127.0.0.1".parse().unwrap();
    let stranger = "192.0.2.1".parse().unwrap();
    let token = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    let gate = ProxyGate::authenticated(vec![peer], token).unwrap();
    assert_eq!(gate.authorize(peer, &[token], true), Ok(()));
    assert_eq!(gate.authorize(peer, &[token], false), Ok(()));
    for headers in [vec![], vec!["wrong"], vec![token, token]] {
        assert_eq!(gate.authorize(peer, &headers, true), Err(ProxyError));
    }
    assert_eq!(gate.authorize(stranger, &[token], true), Err(ProxyError));
    assert!(!format!("{gate:?}").contains(token));
    assert!(ProxyGate::authenticated(vec![], token).is_err());
    assert!(ProxyGate::authenticated(vec![peer], "").is_err());
    let direct = ProxyGate::direct();
    assert_eq!(direct.authorize(peer, &[], false), Ok(()));
    assert_eq!(direct.authorize(peer, &[], true), Err(ProxyError));
    assert_eq!(direct.authorize(peer, &[token], false), Err(ProxyError));
}
