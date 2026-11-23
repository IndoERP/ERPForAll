// SPDX-License-Identifier: MIT OR Apache-2.0
//! Network ranges for rules over a request's client address ([`IpNetwork`]).
//!
//! ```
//! use erp_sdk::net::IpNetwork;
//! let office: IpNetwork = "10.20.0.0/16".parse()?;
//! assert!(office.contains("10.20.3.4".parse()?));
//! assert!(!office.contains("10.21.0.1".parse()?));
//! // an IPv4 client seen through an IPv6 socket is the mapped address
//! assert!(office.contains("::ffff:10.20.3.4".parse()?));
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

use std::fmt;
use std::net::IpAddr;
use std::str::FromStr;

use erp_fields::{Error, Result};

/// An IPv4 or IPv6 network in CIDR notation (`10.0.0.0/8`, `2001:db8::/32`); a bare address is
/// the network of that one address (`/32` or `/128`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct IpNetwork {
    /// The network address: no host bits set.
    address: IpAddr,
    prefix: u8,
}

impl IpNetwork {
    /// The network `address/prefix`.
    ///
    /// # Errors
    ///
    /// `Validation` when `prefix` is longer than the address (32 bits for IPv4, 128 for IPv6) or
    /// `address` has bits set after the prefix (`10.0.0.1/8`: write `10.0.0.0/8`), so a typo does
    /// not silently widen or move the range.
    pub fn new(address: IpAddr, prefix: u8) -> Result<Self> {
        let address = address.to_canonical();
        let bits = match address {
            IpAddr::V4(_) => 32,
            IpAddr::V6(_) => 128,
        };
        if prefix > bits {
            return Err(Error::validation(format!(
                "The network '{address}/{prefix}' has a prefix longer than {bits} bits."
            )));
        }
        let network = Self { address, prefix };
        if network.masked(address) != Some(address) {
            return Err(Error::validation(format!(
                "The network '{address}/{prefix}' has bits set after its prefix; write it as \
                 '{}/{prefix}'.",
                network.masked(address).unwrap_or(address)
            )));
        }
        Ok(network)
    }

    /// The network address.
    pub fn address(&self) -> IpAddr {
        self.address
    }

    /// The prefix length in bits.
    pub fn prefix(&self) -> u8 {
        self.prefix
    }

    /// Whether `address` is in this network. An IPv4-mapped IPv6 address (`::ffff:a.b.c.d`)
    /// counts as its IPv4 address; otherwise an IPv4 address is never in an IPv6 network and the
    /// other way round.
    pub fn contains(&self, address: IpAddr) -> bool {
        self.masked(address.to_canonical()) == Some(self.address)
    }

    /// `address` with the bits after the prefix cleared, or `None` for the other family.
    fn masked(&self, address: IpAddr) -> Option<IpAddr> {
        match (self.address, address) {
            (IpAddr::V4(_), IpAddr::V4(address)) => {
                let mask = u32::MAX
                    .checked_shl(32 - u32::from(self.prefix))
                    .unwrap_or(0);
                Some(IpAddr::V4((u32::from(address) & mask).into()))
            }
            (IpAddr::V6(_), IpAddr::V6(address)) => {
                let mask = u128::MAX
                    .checked_shl(128 - u32::from(self.prefix))
                    .unwrap_or(0);
                Some(IpAddr::V6((u128::from(address) & mask).into()))
            }
            _ => None,
        }
    }
}

impl FromStr for IpNetwork {
    type Err = Error;

    fn from_str(text: &str) -> Result<Self> {
        let invalid = || {
            Error::validation(format!(
                "'{text}' is not a network: write an address with a prefix length, such as \
                 10.0.0.0/8 or 2001:db8::/32."
            ))
        };
        let (address, prefix) = match text.trim().split_once('/') {
            Some((address, prefix)) => {
                // digits only: no sign, no spaces, no leading '+'
                if prefix.is_empty() || !prefix.bytes().all(|b| b.is_ascii_digit()) {
                    return Err(invalid());
                }
                (address, Some(prefix.parse::<u8>().map_err(|_| invalid())?))
            }
            None => (text.trim(), None),
        };
        let address: IpAddr = address.parse().map_err(|_| invalid())?;
        let prefix = prefix.unwrap_or(match address.to_canonical() {
            IpAddr::V4(_) => 32,
            IpAddr::V6(_) => 128,
        });
        Self::new(address, prefix)
    }
}

impl fmt::Display for IpNetwork {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.address, self.prefix)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "tests")]
mod tests {
    use super::*;

    fn net(text: &str) -> IpNetwork {
        text.parse().unwrap()
    }

    fn ip(text: &str) -> IpAddr {
        text.parse().unwrap()
    }

    #[test]
    fn ipv4_networks_contain_their_addresses() {
        let office = net("10.20.0.0/16");
        assert!(office.contains(ip("10.20.0.0")));
        assert!(office.contains(ip("10.20.255.255")));
        assert!(!office.contains(ip("10.21.0.0")));
        assert!(!office.contains(ip("10.19.255.255")));
        assert_eq!(office.to_string(), "10.20.0.0/16");
        let everything = net("0.0.0.0/0");
        assert!(everything.contains(ip("203.0.113.9")));
        assert!(!everything.contains(ip("2001:db8::1")));
        let one = net("192.0.2.7");
        assert_eq!(one.prefix(), 32);
        assert!(one.contains(ip("192.0.2.7")));
        assert!(!one.contains(ip("192.0.2.8")));
    }

    #[test]
    fn ipv6_networks_contain_their_addresses() {
        let range = net("2001:db8::/32");
        assert!(range.contains(ip("2001:db8:ffff::1")));
        assert!(!range.contains(ip("2001:db9::1")));
        assert!(!range.contains(ip("10.0.0.1")));
        assert_eq!(net("::/0").prefix(), 0);
        assert!(net("::/0").contains(ip("fe80::1")));
        assert_eq!(net("2001:db8::1").prefix(), 128);
        assert_eq!(range.to_string(), "2001:db8::/32");
    }

    #[test]
    fn mapped_ipv4_addresses_count_as_ipv4() {
        assert!(net("10.0.0.0/8").contains(ip("::ffff:10.1.2.3")));
        assert!(!net("10.0.0.0/8").contains(ip("::ffff:11.1.2.3")));
        // a mapped network is the IPv4 network it maps
        let mapped = net("::ffff:10.0.0.0/8");
        assert_eq!(mapped.to_string(), "10.0.0.0/8");
        assert!(mapped.contains(ip("10.9.9.9")));
    }

    #[test]
    fn malformed_networks_are_refused_by_name() {
        for text in [
            "",
            "10.0.0.0/",
            "10.0.0.0/+8",
            "10.0.0.0/ 8",
            "10.0.0/8",
            "office",
            "10.0.0.0/8/8",
            "10.0.0.0/256",
        ] {
            let error = text.parse::<IpNetwork>().unwrap_err();
            assert_eq!(error.kind(), erp_fields::ErrorKind::Validation, "{text}");
            assert!(
                error.message().contains("is not a network"),
                "{text}: {error}"
            );
        }
        let error = net_error("10.0.0.0/33");
        assert!(error.contains("longer than 32 bits"), "{error}");
        let error = net_error("2001:db8::/129");
        assert!(error.contains("longer than 128 bits"), "{error}");
        let error = net_error("10.0.0.1/8");
        assert!(error.contains("write it as '10.0.0.0/8'"), "{error}");
        let error = net_error("2001:db8::1/32");
        assert!(error.contains("write it as '2001:db8::/32'"), "{error}");
    }

    fn net_error(text: &str) -> String {
        text.parse::<IpNetwork>().unwrap_err().message().to_owned()
    }
}
