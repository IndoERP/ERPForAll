// SPDX-License-Identifier: MIT OR Apache-2.0
use super::{ConfigError, read_secret};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    net::IpAddr,
    path::{Path, PathBuf},
    sync::Arc,
};
use zeroize::Zeroizing;

/// TLS is required outside credential-free loopback test relays.
#[derive(Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SmtpTls {
    Starttls,
    Implicit,
    Loopback,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RawProfile {
    host: String,
    port: u16,
    tls: SmtpTls,
    companies: BTreeSet<i64>,
    username: Option<String>,
    password_file: Option<PathBuf>,
}

/// A company must be authorized for a profile before selecting it in Settings.
pub struct SmtpProfile {
    pub host: String,
    pub port: u16,
    pub tls: SmtpTls,
    companies: BTreeSet<i64>,
    credentials: Option<(String, Zeroizing<String>)>,
}

impl SmtpProfile {
    pub fn credentials(&self) -> Option<(&str, &str)> {
        self.credentials
            .as_ref()
            .map(|(name, password)| (name.as_str(), password.as_str()))
    }
}

/// Node configuration owns the allowlist. Database settings never select a secret-file path.
#[derive(Clone, Default)]
pub struct SmtpProfiles(Arc<BTreeMap<String, Arc<SmtpProfile>>>);

impl SmtpProfiles {
    pub(super) fn load(
        raw: BTreeMap<String, RawProfile>,
        resolve: impl Fn(&Path) -> PathBuf,
    ) -> Result<Self, ConfigError> {
        let mut profiles = BTreeMap::new();
        for (alias, raw) in raw {
            if alias.is_empty()
                || alias.len() > 63
                || !alias
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_'))
                || raw.host.is_empty()
                || raw.host.len() > 253
                || !raw
                    .host
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'-' | b':'))
                || raw.port == 0
                || raw.companies.is_empty()
                || raw.companies.iter().any(|c| *c < 1)
                || (raw.tls == SmtpTls::Loopback
                    && (!raw.host.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback())
                        || raw.username.is_some()))
            {
                return Err(ConfigError::Invalid);
            }
            let credentials = match (raw.username, raw.password_file) {
                (None, None) => None,
                (Some(name), Some(path))
                    if !name.is_empty() && !name.chars().any(char::is_control) =>
                {
                    Some((name, read_secret(&resolve(&path))?))
                }
                _ => return Err(ConfigError::Invalid),
            };
            profiles.insert(
                alias,
                Arc::new(SmtpProfile {
                    host: raw.host,
                    port: raw.port,
                    tls: raw.tls,
                    companies: raw.companies,
                    credentials,
                }),
            );
        }
        Ok(Self(Arc::new(profiles)))
    }

    pub fn for_company(&self, name: &str, company: i64) -> Option<Arc<SmtpProfile>> {
        self.0
            .get(name)
            .filter(|profile| profile.companies.contains(&company))
            .cloned()
    }

    pub(super) fn fingerprint(&self) -> Option<[u8; 32]> {
        if self.0.is_empty() {
            return None;
        }
        let mut digest = Sha256::new();
        for (name, profile) in self.0.iter() {
            let (user, password) = profile.credentials().unwrap_or_default();
            for field in [
                name.as_bytes(),
                profile.host.as_bytes(),
                user.as_bytes(),
                password.as_bytes(),
            ] {
                digest.update((field.len() as u64).to_be_bytes());
                digest.update(field);
            }
            digest.update(profile.port.to_be_bytes());
            digest.update([profile.tls as u8]);
            digest.update((profile.companies.len() as u64).to_be_bytes());
            for company in &profile.companies {
                digest.update(company.to_be_bytes());
            }
        }
        Some(digest.finalize().into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn plaintext_is_only_available_on_literal_loopback_and_profiles_are_company_scoped()
    -> Result<(), ConfigError> {
        let load = |host: &str| {
            let profile = RawProfile {
                host: host.into(),
                port: 2525,
                tls: SmtpTls::Loopback,
                companies: BTreeSet::from([1]),
                username: None,
                password_file: None,
            };
            SmtpProfiles::load(BTreeMap::from([("test".into(), profile)]), Path::to_owned)
        };
        assert!(load("mail.example.test").is_err());
        let profiles = load("127.0.0.1")?;
        assert!(profiles.for_company("test", 1).is_some());
        assert!(profiles.for_company("test", 2).is_none());
        Ok(())
    }
}
