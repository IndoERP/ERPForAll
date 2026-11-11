//! Explicit direct/proxy request boundary. A trusted peer alone never authenticates forwarding.
use std::{fmt, net::IpAddr};
use subtle::ConstantTimeEq;
use thiserror::Error;
use zeroize::Zeroizing;

/// Forwarding refusal; never includes supplied header or key material.
#[derive(Debug, Error, PartialEq, Eq)]
#[error("untrusted proxy request")]
pub struct ProxyError;
/// Transport authority mode, fixed at server startup.
pub struct ProxyGate(Mode);
enum Mode {
    Direct,
    Authenticated {
        peers: Vec<IpAddr>,
        token: Zeroizing<Vec<u8>>,
    },
}
impl fmt::Debug for ProxyGate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ProxyGate(<redacted>)")
    }
}
impl ProxyGate {
    /// Explicit development transport; forwarded authority is never accepted.
    #[must_use]
    pub fn direct() -> Self {
        Self(Mode::Direct)
    }
    /// Construct the trusted-peer boundary with a dedicated 32-byte lowercase hex token.
    /// # Errors
    /// Empty/duplicate peers or invalid secret material.
    pub fn authenticated(peers: Vec<IpAddr>, token: &str) -> Result<Self, ProxyError> {
        if peers.is_empty()
            || peers.len() > 32
            || token.len() != 64
            || !token
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err(ProxyError);
        }
        let unique: std::collections::BTreeSet<_> = peers.iter().collect();
        if unique.len() != peers.len() {
            return Err(ProxyError);
        }
        Ok(Self(Mode::Authenticated {
            peers,
            token: Zeroizing::new(token.as_bytes().to_vec()),
        }))
    }
    /// Classify the transport boundary within ServerConfig's `server` identity section.
    pub(crate) fn describe_settings(
        &self,
        out: &mut erp_orm::node::SettingsWriter<'_>,
    ) -> Result<(), erp_orm::node::NodeError> {
        out.node("server.proxy.trusted_peers")?;
        match &self.0 {
            Mode::Direct => out.shared_bool("server.proxy.authenticated", false),
            Mode::Authenticated { peers: _, token } => {
                out.shared_bool("server.proxy.authenticated", true)?;
                out.shared_secret("server.proxy.token", token)
            }
        }
    }
    /// Authenticate before any forwarded header affects origin, identity or cookie behavior.
    /// # Errors
    /// Unknown peer, missing/wrong/duplicate token, or forwarding in direct mode.
    pub fn authorize(
        &self,
        peer: IpAddr,
        tokens: &[&str],
        has_forwarded: bool,
    ) -> Result<(), ProxyError> {
        match &self.0 {
            Mode::Direct if !has_forwarded && tokens.is_empty() => Ok(()),
            Mode::Authenticated { peers, token } if peers.contains(&peer) && tokens.len() == 1 => {
                // Only public token length can short-circuit; equal-length comparison is constant time.
                if token.as_slice().ct_eq(tokens[0].as_bytes()).into() {
                    Ok(())
                } else {
                    Err(ProxyError)
                }
            }
            _ => Err(ProxyError),
        }
    }
}
