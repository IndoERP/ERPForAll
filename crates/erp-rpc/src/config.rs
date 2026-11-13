//! Operator settings of the RPC layer, each within a documented floor and hard bound (standards
//! §4; for the `limits.*` values, and 9 for the
//! `rpc.*` ones). A value outside its bounds fails startup.

use std::time::Duration;

use erp_fields::{Error, Result};

const MIB: u64 = 1024 * 1024;

/// One bounded setting: (name, default, floor, hard bound).
struct Bound {
    name: &'static str,
    default: u64,
    floor: u64,
    max: u64,
}

const RESULT_BYTES: Bound = Bound {
    name: "limits.rpc_result_bytes",
    default: 16 * MIB,
    floor: MIB,
    max: 1024 * MIB,
};
const BODY_BYTES: Bound = Bound {
    name: "limits.rpc_body_bytes",
    default: 2 * MIB,
    floor: 64 * 1024,
    max: 128 * MIB,
};
const JSON_DEPTH: Bound = Bound {
    name: "limits.json_depth",
    default: 64,
    floor: 8,
    max: 1024,
};
const DEADLINE_SECS: Bound = Bound {
    name: "limits.rpc_deadline_secs",
    default: 120,
    floor: 1,
    max: 3600,
};
const TTL_SECS: Bound = Bound {
    name: "rpc.idempotency_ttl_secs",
    default: 86_400,
    floor: 3_600,
    max: 2_592_000,
};
const RESPONSE_BYTES: Bound = Bound {
    name: "rpc.idempotency_response_bytes",
    default: MIB,
    floor: 1024,
    max: 64 * MIB,
};
const REPLAY_SECS: Bound = Bound {
    name: "rpc.idempotency_replay_secs",
    default: 3_600,
    floor: 60,
    // and at most the TTL (checked in `build`)
    max: 2_592_000,
};

impl Bound {
    fn check(&self, value: u64) -> Result<u64> {
        if (self.floor..=self.max).contains(&value) {
            Ok(value)
        } else {
            Err(Error::programming(format!(
                "{} = {value} is outside its bounds {} – {}",
                self.name, self.floor, self.max
            )))
        }
    }
}

/// The validated settings ([`RpcConfig::builder`]).
///
/// ```
/// use erp_rpc::RpcConfig;
/// let config = RpcConfig::builder().idempotency_replay_secs(600).build()?;
/// assert_eq!(config.idempotency_replay_secs(), 600);
/// assert!(!config.error_debug());
/// let err = RpcConfig::builder().json_depth(4).build().unwrap_err();
/// assert_eq!(err.message(), "limits.json_depth = 4 is outside its bounds 8 – 1024");
/// # Ok::<(), erp_fields::Error>(())
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RpcConfig {
    error_debug: bool,
    read_audit_policy: crate::ReadAuditPolicy,
    result_bytes: u64,
    body_bytes: u64,
    json_depth: usize,
    deadline_secs: u64,
    idempotency_ttl_secs: u64,
    idempotency_replay_secs: u64,
    idempotency_response_bytes: u64,
}

impl Default for RpcConfig {
    fn default() -> Self {
        Self {
            error_debug: false,
            read_audit_policy: crate::ReadAuditPolicy::default(),
            result_bytes: RESULT_BYTES.default,
            body_bytes: BODY_BYTES.default,
            json_depth: 64,
            deadline_secs: DEADLINE_SECS.default,
            idempotency_ttl_secs: TTL_SECS.default,
            idempotency_replay_secs: REPLAY_SECS.default,
            idempotency_response_bytes: RESPONSE_BYTES.default,
        }
    }
}

impl RpcConfig {
    /// Start from the defaults.
    pub fn builder() -> RpcConfigBuilder {
        RpcConfigBuilder(Self::default())
    }

    /// Operator widening of successful-read auditing.
    pub fn read_audit_policy(&self) -> &crate::ReadAuditPolicy {
        &self.read_audit_policy
    }

    /// `rpc.error_debug`: whether `debug` may be shown to holders of `rpc.debug_errors`
    /// (default off; the startup summary reports it when on).
    pub fn error_debug(&self) -> bool {
        self.error_debug
    }

    /// `limits.rpc_result_bytes` (default 16 MiB, 1 MiB – 1 GiB).
    pub fn result_bytes(&self) -> u64 {
        self.result_bytes
    }

    /// `limits.rpc_body_bytes` (default 2 MiB, 64 KiB – 128 MiB).
    pub fn body_bytes(&self) -> u64 {
        self.body_bytes
    }

    /// `limits.json_depth` (default 64, 8 – 1024).
    pub fn json_depth(&self) -> usize {
        self.json_depth
    }

    /// `limits.rpc_deadline_secs` (default 120 s, 1 – 3600 s): a call's deadline when the
    /// transport gives none.
    pub fn deadline(&self) -> Duration {
        Duration::from_secs(self.deadline_secs)
    }

    /// `rpc.idempotency_ttl_secs`: how long a key deduplicates (default 24 h, 1 h – 30 days).
    pub fn idempotency_ttl_secs(&self) -> u64 {
        self.idempotency_ttl_secs
    }

    /// `rpc.idempotency_replay_secs`: how long the stored response is replayed (default 1 h,
    /// 60 s – the TTL).
    pub fn idempotency_replay_secs(&self) -> u64 {
        self.idempotency_replay_secs
    }

    /// `rpc.idempotency_response_bytes`: the largest response stored for replay (default 1 MiB,
    /// 1 KiB – 64 MiB). A larger one is not stored: the key still deduplicates and a replay
    /// answers 409 `erp.idempotency_result_expired`.
    pub fn idempotency_response_bytes(&self) -> u64 {
        self.idempotency_response_bytes
    }

    /// The startup summary line for the debug switch, when it is on.
    pub fn startup_summary(&self) -> Option<&'static str> {
        self.error_debug
            .then_some("rpc error debug: ON (stack details visible to holders of rpc.debug_errors)")
    }
}

/// Section `rpc` of the configuration digest: every RPC setting is shared.
/// Two replicas with different settings would answer one request differently depending on where
/// the load balancer sent it, so a replica whose settings differ stays out of readiness. The
/// `limits.*` settings are section `limits` ([`RpcConfig::limits`]): describe both.
impl erp_orm::node::SharedSettings for RpcConfig {
    fn section(&self) -> erp_orm::node::SectionName {
        erp_orm::node::SectionName::new("rpc")
    }

    fn describe(
        &self,
        out: &mut erp_orm::node::SettingsWriter<'_>,
    ) -> std::result::Result<(), erp_orm::node::NodeError> {
        let Self {
            error_debug,
            read_audit_policy,
            // section `limits` (`RpcLimits`)
            result_bytes: _,
            body_bytes: _,
            json_depth: _,
            deadline_secs: _,
            idempotency_ttl_secs,
            idempotency_replay_secs,
            idempotency_response_bytes,
        } = self;
        read_audit_policy.describe(out)?;
        out.shared_bool("rpc.error_debug", *error_debug)?;
        out.shared_u64(TTL_SECS.name, *idempotency_ttl_secs)?;
        out.shared_u64(REPLAY_SECS.name, *idempotency_replay_secs)?;
        out.shared_u64(RESPONSE_BYTES.name, *idempotency_response_bytes)
    }
}

/// The `limits.*` settings of an [`RpcConfig`]: section `limits` of the
/// configuration digest, every value shared, so a mismatch report names `limits` when a limit
/// differs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RpcLimits {
    result_bytes: u64,
    body_bytes: u64,
    json_depth: usize,
    deadline_secs: u64,
}

impl RpcConfig {
    /// The `limits.*` settings, for the configuration digest.
    pub fn limits(&self) -> RpcLimits {
        RpcLimits {
            result_bytes: self.result_bytes,
            body_bytes: self.body_bytes,
            json_depth: self.json_depth,
            deadline_secs: self.deadline_secs,
        }
    }
}

impl erp_orm::node::SharedSettings for RpcLimits {
    fn section(&self) -> erp_orm::node::SectionName {
        erp_orm::node::SectionName::new("limits")
    }

    fn describe(
        &self,
        out: &mut erp_orm::node::SettingsWriter<'_>,
    ) -> std::result::Result<(), erp_orm::node::NodeError> {
        let Self {
            result_bytes,
            body_bytes,
            json_depth,
            deadline_secs,
        } = self;
        let depth = u64::try_from(*json_depth).map_err(|_| {
            erp_orm::node::NodeError::invalid_setting(
                JSON_DEPTH.name,
                format!("{} = {json_depth} does not fit in 64 bits", JSON_DEPTH.name),
            )
        })?;
        out.shared_u64(RESULT_BYTES.name, *result_bytes)?;
        out.shared_u64(BODY_BYTES.name, *body_bytes)?;
        out.shared_u64(JSON_DEPTH.name, depth)?;
        out.shared_u64(DEADLINE_SECS.name, *deadline_secs)
    }
}

/// Builds an [`RpcConfig`].
#[derive(Debug, Clone)]
#[must_use = "call `.build()`"]
pub struct RpcConfigBuilder(RpcConfig);

impl RpcConfigBuilder {
    /// Operator widening; this policy never disables model, principal or elevation auditing.
    pub fn read_audit_policy(mut self, policy: crate::ReadAuditPolicy) -> Self {
        self.0.read_audit_policy = policy;
        self
    }

    /// `rpc.error_debug`.
    pub fn error_debug(mut self, on: bool) -> Self {
        self.0.error_debug = on;
        self
    }

    /// `limits.rpc_result_bytes`.
    pub fn result_bytes(mut self, bytes: u64) -> Self {
        self.0.result_bytes = bytes;
        self
    }

    /// `limits.rpc_body_bytes`.
    pub fn body_bytes(mut self, bytes: u64) -> Self {
        self.0.body_bytes = bytes;
        self
    }

    /// `limits.json_depth`.
    pub fn json_depth(mut self, depth: usize) -> Self {
        self.0.json_depth = depth;
        self
    }

    /// `limits.rpc_deadline_secs`.
    pub fn deadline_secs(mut self, secs: u64) -> Self {
        self.0.deadline_secs = secs;
        self
    }

    /// `rpc.idempotency_ttl_secs`.
    pub fn idempotency_ttl_secs(mut self, secs: u64) -> Self {
        self.0.idempotency_ttl_secs = secs;
        self
    }

    /// `rpc.idempotency_replay_secs`.
    pub fn idempotency_replay_secs(mut self, secs: u64) -> Self {
        self.0.idempotency_replay_secs = secs;
        self
    }

    /// `rpc.idempotency_response_bytes`.
    pub fn idempotency_response_bytes(mut self, bytes: u64) -> Self {
        self.0.idempotency_response_bytes = bytes;
        self
    }

    /// Validate.
    ///
    /// # Errors
    ///
    /// `Programming` naming the setting and its bounds for a value out of range, and for a
    /// replay window longer than the TTL.
    pub fn build(self) -> Result<RpcConfig> {
        let c = self.0;
        RESULT_BYTES.check(c.result_bytes)?;
        BODY_BYTES.check(c.body_bytes)?;
        // a depth beyond u64 is out of bounds as well
        JSON_DEPTH.check(u64::try_from(c.json_depth).unwrap_or(u64::MAX))?;
        DEADLINE_SECS.check(c.deadline_secs)?;
        TTL_SECS.check(c.idempotency_ttl_secs)?;
        REPLAY_SECS.check(c.idempotency_replay_secs)?;
        RESPONSE_BYTES.check(c.idempotency_response_bytes)?;
        if c.idempotency_replay_secs > c.idempotency_ttl_secs {
            return Err(Error::programming(format!(
                "rpc.idempotency_replay_secs = {} is longer than rpc.idempotency_ttl_secs = {}",
                c.idempotency_replay_secs, c.idempotency_ttl_secs
            )));
        }
        Ok(c)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "tests")]

    use super::*;
    use erp_orm::node::{ConfigDigest, NodeOptions, Scope, SectionDigest};
    use erp_orm::signal::ListenerOptions;

    #[test]
    fn every_setting_declares_its_scope_and_bounds_are_refused() {
        let rpc = SectionDigest::of(&RpcConfig::default()).unwrap();
        assert_eq!(rpc.name().as_str(), "rpc");
        let names: Vec<(&str, Scope)> = rpc
            .settings()
            .iter()
            .map(|s| (s.name(), s.scope()))
            .collect();
        assert_eq!(
            names,
            [
                "rpc.audit_read_policy.groups_digest",
                "rpc.audit_read_policy.mode",
                "rpc.error_debug",
                "rpc.idempotency_replay_secs",
                "rpc.idempotency_response_bytes",
                "rpc.idempotency_ttl_secs",
            ]
            .map(|n| (n, Scope::Shared))
        );
        // the `limits.*` settings are their own section, so a mismatch names `limits`
        let limits = SectionDigest::of(&RpcConfig::default().limits()).unwrap();
        assert_eq!(limits.name().as_str(), "limits");
        let names: Vec<(&str, Scope)> = limits
            .settings()
            .iter()
            .map(|s| (s.name(), s.scope()))
            .collect();
        assert_eq!(
            names,
            [
                "limits.json_depth",
                "limits.rpc_body_bytes",
                "limits.rpc_deadline_secs",
                "limits.rpc_result_bytes",
            ]
            .map(|n| (n, Scope::Shared))
        );
        let node = SectionDigest::of(&NodeOptions::default()).unwrap();
        let names: Vec<(&str, Scope)> = node
            .settings()
            .iter()
            .map(|s| (s.name(), s.scope()))
            .collect();
        assert_eq!(
            names,
            [
                "node.heartbeat_interval_secs",
                "node.lease_intervals",
                "node.retention_hours",
            ]
            .map(|n| (n, Scope::Shared))
        );
        let signal = SectionDigest::of(&ListenerOptions::default()).unwrap();
        assert!(signal.settings().iter().all(|s| s.scope() == Scope::Node));
        assert_eq!(signal.settings().len(), 4);
        // node settings change no digest
        let slower = ListenerOptions::default().poll_interval(Duration::from_secs(90));
        assert_eq!(
            SectionDigest::of(&slower).unwrap().digest(),
            signal.digest()
        );
        // a changed RPC limit changes the configuration digest
        let config = |rpc: &RpcConfig| {
            ConfigDigest::from_sections(vec![
                SectionDigest::of(rpc).unwrap(),
                SectionDigest::of(&rpc.limits()).unwrap(),
                SectionDigest::of(&ListenerOptions::default()).unwrap(),
            ])
            .unwrap()
            .digest()
        };
        let changed = RpcConfig::builder().json_depth(65).build().unwrap();
        assert_ne!(config(&changed), config(&RpcConfig::default()));
        assert_eq!(config(&RpcConfig::default()), config(&RpcConfig::default()));
        // out of bounds: refused, never clamped
        let refused = [
            (
                NodeOptions::builder().heartbeat_interval_secs(4).build(),
                "node.heartbeat_interval_secs = 4 is outside its bounds 5 – 60",
            ),
            (
                NodeOptions::builder().lease_intervals(2).build(),
                "node.lease_intervals = 2 is outside its bounds 3 – 10",
            ),
            (
                NodeOptions::builder().retention_hours(721).build(),
                "node.retention_hours = 721 is outside its bounds 1 – 720",
            ),
        ];
        for (built, message) in refused {
            assert_eq!(built.unwrap_err().to_string(), message);
        }
    }

    #[test]
    fn defaults_and_bounds() {
        let d = RpcConfig::builder().build().unwrap();
        assert_eq!(d, RpcConfig::default());
        assert_eq!(
            (
                d.result_bytes(),
                d.body_bytes(),
                d.json_depth(),
                d.deadline()
            ),
            (16 * MIB, 2 * MIB, 64, Duration::from_secs(120))
        );
        assert_eq!(
            (d.idempotency_ttl_secs(), d.idempotency_replay_secs()),
            (86_400, 3_600)
        );
        assert_eq!(d.startup_summary(), None);
        assert_eq!(d.idempotency_response_bytes(), MIB);
        assert_eq!(
            RpcConfig::builder()
                .idempotency_response_bytes(1023)
                .build()
                .unwrap_err()
                .message(),
            "rpc.idempotency_response_bytes = 1023 is outside its bounds 1024 – 67108864"
        );
        let cases: [(RpcConfigBuilder, &str); 6] = [
            (
                RpcConfig::builder().result_bytes(MIB - 1),
                "limits.rpc_result_bytes = 1048575 is outside its bounds 1048576 – 1073741824",
            ),
            (
                RpcConfig::builder().body_bytes(0),
                "limits.rpc_body_bytes = 0 is outside its bounds 65536 – 134217728",
            ),
            (
                RpcConfig::builder().deadline_secs(3601),
                "limits.rpc_deadline_secs = 3601 is outside its bounds 1 – 3600",
            ),
            (
                RpcConfig::builder().idempotency_ttl_secs(3_599),
                "rpc.idempotency_ttl_secs = 3599 is outside its bounds 3600 – 2592000",
            ),
            (
                RpcConfig::builder().idempotency_replay_secs(59),
                "rpc.idempotency_replay_secs = 59 is outside its bounds 60 – 2592000",
            ),
            (
                RpcConfig::builder()
                    .idempotency_ttl_secs(3_600)
                    .idempotency_replay_secs(3_601),
                "rpc.idempotency_replay_secs = 3601 is longer than rpc.idempotency_ttl_secs = \
                 3600",
            ),
        ];
        for (builder, message) in cases {
            assert_eq!(builder.build().unwrap_err().message(), message);
        }
        let on = RpcConfig::builder().error_debug(true).build().unwrap();
        assert_eq!(
            on.startup_summary(),
            Some("rpc error debug: ON (stack details visible to holders of rpc.debug_errors)")
        );
    }
}
