// SPDX-License-Identifier: MIT OR Apache-2.0
//! What the transport knows about the request a transaction serves ([`RequestInfo`]).
//!
//! Set only by server code that opens a request's transaction ([`crate::BeginOptions::request`],
//! [`crate::CredentialOptions::request`]); never read from the context, the body or a header the
//! server does not trust. Jobs, the command line, tests and internal transactions have none.

use std::net::IpAddr;

/// The request a transaction serves, as the HTTP layer resolved it.
///
/// ```
/// use erp_orm::RequestInfo;
/// let request = RequestInfo::new("192.0.2.7".parse()?);
/// assert_eq!(request.client_address().to_string(), "192.0.2.7");
/// # Ok::<(), std::net::AddrParseError>(())
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct RequestInfo {
    client_address: IpAddr,
}

impl RequestInfo {
    /// A request from `client_address`: the peer, or the forwarded client when the peer is a
    /// trusted proxy (the same address the server's login and API key throttles use).
    pub fn new(client_address: IpAddr) -> Self {
        Self { client_address }
    }

    /// The client's address. An IPv4 client reached over IPv6 is the mapped address
    /// (`::ffff:a.b.c.d`) as the socket reported it; compare with
    /// [`IpAddr::to_canonical`] when that matters.
    pub fn client_address(&self) -> IpAddr {
        self.client_address
    }
}
