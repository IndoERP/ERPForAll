//! Bounded binary feature declarations for digest-bound rollout.

use std::collections::BTreeMap;

use super::super::{NodeError, SectionName, SettingsWriter, SharedSettings};

/// A feature identifier: 1–63 ASCII bytes matching `[a-z][a-z0-9_.-]*`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[non_exhaustive]
pub struct FeatureKey(String);

impl FeatureKey {
    /// Validate a feature identifier without normalizing it.
    ///
    /// # Errors
    ///
    /// [`NodeError::InvalidSetting`] when the identifier violates its grammar or length bound.
    pub fn new(key: &str) -> Result<Self, NodeError> {
        let bytes = key.as_bytes();
        let valid = !bytes.is_empty()
            && bytes.len() <= 63
            && bytes[0].is_ascii_lowercase()
            && bytes.iter().all(|byte| {
                byte.is_ascii_lowercase()
                    || byte.is_ascii_digit()
                    || matches!(byte, b'_' | b'.' | b'-')
            });
        if key.len() > 64 {
            return Err(NodeError::invalid_setting(
                "node_features.key",
                format!(
                    "invalid feature key ({} bytes): 1 to 63 ASCII bytes matching [a-z][a-z0-9_.-]*",
                    key.len()
                ),
            ));
        }
        if !valid {
            return Err(NodeError::invalid_setting(
                "node_features.key",
                format!(
                    "invalid feature key {key:?}: 1 to 63 ASCII bytes matching [a-z][a-z0-9_.-]*"
                ),
            ));
        }
        Ok(Self(key.to_owned()))
    }

    /// The validated identifier.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A positive PostgreSQL `int4` capability version.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[non_exhaustive]
pub struct FeatureVersion(i32);

impl FeatureVersion {
    /// Validate a binary capability version.
    ///
    /// # Errors
    ///
    /// [`NodeError::InvalidSetting`] for zero or a negative value.
    pub fn new(version: i32) -> Result<Self, NodeError> {
        if version <= 0 {
            return Err(NodeError::invalid_setting(
                "node_features.version",
                format!("invalid feature version {version}: expected a positive int4"),
            ));
        }
        Ok(Self(version))
    }

    /// The positive version.
    pub fn get(self) -> i32 {
        self.0
    }
}

/// At most eight distinct binary capability declarations, ordered by key.
///
/// Trusted binaries declare their own support; arbitrary operator settings must never manufacture
/// support. These declarations identify a binary's capabilities but grant no serving authority.
/// The default is an empty declaration set.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct FeatureSet(BTreeMap<FeatureKey, FeatureVersion>);

impl FeatureSet {
    /// Collect bounded declarations, rejecting every duplicate key even when versions agree.
    ///
    /// ```
    /// use erp_orm::node::{FeatureKey, FeatureSet, FeatureVersion};
    /// let features = FeatureSet::new([
    ///     (FeatureKey::new("auth.login_principal")?, FeatureVersion::new(1)?),
    /// ])?;
    /// assert_eq!(features.iter().count(), 1);
    /// # Ok::<(), erp_orm::node::NodeError>(())
    /// ```
    ///
    /// # Errors
    ///
    /// [`NodeError::InvalidSetting`] for more than eight declarations;
    /// [`NodeError::DuplicateSetting`] for a repeated feature key.
    pub fn new(
        declarations: impl IntoIterator<Item = (FeatureKey, FeatureVersion)>,
    ) -> Result<Self, NodeError> {
        let mut features = BTreeMap::new();
        for (key, version) in declarations {
            if features.contains_key(&key) {
                return Err(NodeError::DuplicateSetting {
                    section: "node_features".to_owned(),
                    name: Some(format!("node_features.{}", key.as_str())),
                });
            }
            if features.len() == 8 {
                return Err(NodeError::invalid_setting(
                    "node_features",
                    "a node identity declares at most 8 features",
                ));
            }
            features.insert(key, version);
        }
        Ok(Self(features))
    }

    /// Declarations in canonical key order.
    pub fn iter(&self) -> impl ExactSizeIterator<Item = (&FeatureKey, &FeatureVersion)> {
        self.0.iter()
    }

    /// Encode the already bounded declaration map once for an identity's membership writes.
    pub(crate) fn capabilities_json(&self) -> serde_json::Value {
        let object = self
            .iter()
            .map(|(key, version)| {
                (
                    key.as_str().to_owned(),
                    serde_json::Value::from(version.get()),
                )
            })
            .collect();
        serde_json::Value::Object(object)
    }

    /// The declared version of a feature, if present.
    pub fn get(&self, key: &FeatureKey) -> Option<FeatureVersion> {
        self.0.get(key).copied()
    }
}

impl SharedSettings for FeatureSet {
    fn section(&self) -> SectionName {
        SectionName::new("node_features")
    }

    fn describe(&self, out: &mut SettingsWriter<'_>) -> Result<(), NodeError> {
        for (key, version) in self.iter() {
            // FeatureVersion is positive int4 by construction, so its unsigned magnitude is exact.
            out.shared_u64(
                &format!("node_features.{}", key.as_str()),
                u64::from(version.get().unsigned_abs()),
            )?;
        }
        Ok(())
    }
}
