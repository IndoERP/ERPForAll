//! Startup admission for's scalar limits and byte-budget relationships.
//!
//! Admission validates configuration; a getter alone is not runtime enforcement of a future
//! websocket, export, cancellation or cache facility. See the slice's limits inventory.
use erp_orm::node::{NodeError, SectionName, SettingsWriter, SharedSettings};
use serde::Deserialize;
use thiserror::Error;

const KIB: u64 = 1024;
const MIB: u64 = 1024 * KIB;
const GIB: u64 = 1024 * MIB;

/// Native server execution permits. The serving semaphore and startup accounting use this value.
/// It is the current native implementation's bound, not's future configurable default.
pub const RPC_CONCURRENCY: usize = 16;
/// Workers in the native CLI's current-thread runtime. Update its runtime composition and this
/// accounting input together if the native server later moves to a multi-thread runtime.
pub const NATIVE_WORKER_THREADS: u64 = 1;

// 's startup policy coefficients. These are budget gates/estimates, not measurements
// of this build's actual object layouts, heap retention or process RSS.
const IDLE_CONNECTION_ALLOWANCE: u64 = 16 * KIB;
const FIXED_REQUEST_ALLOWANCE: u64 = 32 * KIB;
const RATE_ENTRY_ALLOWANCE: u64 = 64;
const TOUCH_ENTRY_ALLOWANCE: u64 = 48;

/// Accounting inputs from the actual native serving implementation and immutable hashing pool.
#[derive(Clone, Copy, Debug)]
pub(crate) struct NativeAccounting {
    worker_threads: u64,
    rpc_concurrency: u64,
    hash: erp_security::auth::pool::HashPoolConfig,
}
impl NativeAccounting {
    pub(crate) fn current() -> Self {
        Self {
            worker_threads: NATIVE_WORKER_THREADS,
            // The small shared constant is representable on every supported target.
            rpc_concurrency: RPC_CONCURRENCY as u64,
            hash: erp_security::auth::HashPool::global().config(),
        }
    }
    #[cfg(test)]
    fn default_for_tests() -> Self {
        Self {
            worker_threads: NATIVE_WORKER_THREADS,
            rpc_concurrency: RPC_CONCURRENCY as u64,
            hash: erp_security::auth::pool::HashPoolConfig::DEFAULT,
        }
    }
    fn describe(self, out: &mut SettingsWriter<'_>) -> Result<(), NodeError> {
        let Self {
            worker_threads,
            rpc_concurrency,
            hash,
        } = self;
        out.shared_u64("startup_limits.native.worker_threads", worker_threads)?;
        out.shared_u64("startup_limits.native.rpc_concurrency", rpc_concurrency)?;
        out.shared_u64("startup_limits.hashing.permits", u64::from(hash.permits()))?;
        out.shared_u64(
            "startup_limits.hashing.memory_kib",
            u64::from(hash.memory_kib()),
        )?;
        out.shared_duration("startup_limits.hashing.queue_wait", hash.queue_wait())
    }
}

/// A refused startup limit. Messages name only fixed setting identifiers, never input secrets.
#[derive(Clone, Copy, Debug, Error, PartialEq, Eq)]
#[non_exhaustive]
pub enum StartupLimitError {
    /// A scalar is outside its ADR floor or hard bound.
    #[error("server startup limit {setting} is outside its bounds")]
    OutsideBounds {
        /// Fixed setting identifier from the ADR table.
        setting: &'static str,
    },
    /// Computing a byte-budget relationship overflowed.
    #[error("server startup byte-budget arithmetic overflowed")]
    ArithmeticOverflow,
    /// The inflight budget cannot admit a maximum RPC plus the websocket share.
    #[error(
        "server startup limits require admission.inflight_bytes >= 2 * limits.rpc_body_bytes + limits.rpc_result_bytes + admission.ws_inflight_bytes"
    )]
    InflightMinimum,
    /// The websocket share cannot hold a maximum websocket message.
    #[error("server startup limits require admission.ws_inflight_bytes >= ws.max_message_bytes")]
    WebsocketMinimum,
    /// The inflight budget exceeds the process's memory cap.
    #[error("server startup limits require admission.inflight_bytes <= process.memory_soft_bytes")]
    ProcessMemory,
    /// Configured policy reservations and hashing together exceed the process soft cap.
    #[error(
        "server startup configured memory sum (http.max_connections, admission.inflight_bytes, admission.rpc_concurrency, assets.memory_bytes, buffers.thread_cache_bytes, rpc.spec_cache_bytes, limits.rate_table_entries, session.touch_queue_entries, admission.hash_concurrency, admission.hash_memory_kib) exceeds process.memory_soft_bytes"
    )]
    ConfiguredMemory,
    /// The actual configured hashing pool cannot consume more than a quarter of the soft cap.
    #[error(
        "server startup limits require admission.hash_concurrency * admission.hash_memory_kib * 1024 <= process.memory_soft_bytes / 4"
    )]
    HashingMemory,
    /// The configured memory cap exceeds physical host RAM.
    #[error("server startup limits require process.memory_soft_bytes <= host RAM")]
    HostMemory,
    /// Physical host memory could not be discovered; startup never assumes a capacity.
    #[error("server startup limits could not determine host RAM")]
    HostMemoryUnavailable,
}

macro_rules! limits {
    ($( $field:ident => ($name:literal, $default:expr, $floor:expr, $ceiling:expr) ),+ $(,)?) => {
        #[derive(Clone, Copy, Debug, Deserialize)]
        #[serde(default, deny_unknown_fields)]
        pub(crate) struct RawStartupLimits {
            $( #[serde(rename = $name)] $field: u64, )+
        }
        impl Default for RawStartupLimits {
            fn default() -> Self {
                Self { $( $field: $default, )+ }
            }
        }
        /// Validated startup settings. Constructed only through [`crate::config::ServerConfig`].
        ///
        /// All values have passed their scalar and cross-setting checks. Runtime consumers must
        /// explicitly use the appropriate getter; this object does not enable absent facilities.
        #[derive(Clone, Copy, Debug)]
        #[non_exhaustive]
        pub struct StartupLimits {
            values: RawStartupLimits,
            native: NativeAccounting,
            reservation_bytes: u64,
        }
        impl StartupLimits {
            $( #[doc = concat!("Validated `", $name, "` startup setting.")]
            pub fn $field(&self) -> u64 { self.values.$field } )+
            /// Checked configured reservation sum used for startup admission, including hashing.
            /// This is the ADR's policy model, not measured actual heap usage or process RSS.
            pub fn configured_reservation_bytes(&self) -> u64 { self.reservation_bytes }
            /// Reservation of the actual immutable global hashing pool at config admission.
            pub fn hash_reserved_bytes(&self) -> u64 { self.native.hash.reserved_bytes() }
        }
        impl RawStartupLimits {
            pub(crate) fn validate(self, host_ram: u64, native: NativeAccounting) -> Result<StartupLimits, StartupLimitError> {
                $( if !($floor..=$ceiling).contains(&self.$field) {
                    return Err(StartupLimitError::OutsideBounds { setting: $name });
                } )+
                if host_ram == 0 {
                    return Err(StartupLimitError::HostMemoryUnavailable);
                }
                if self.memory_soft_bytes > host_ram {
                    return Err(StartupLimitError::HostMemory);
                }
                if self.inflight_bytes > self.memory_soft_bytes {
                    return Err(StartupLimitError::ProcessMemory);
                }
                if self.ws_inflight_bytes < self.ws_max_message_bytes {
                    return Err(StartupLimitError::WebsocketMinimum);
                }
                let minimum = minimum_inflight(self.rpc_body_bytes, self.rpc_result_bytes, self.ws_inflight_bytes)?;
                if self.inflight_bytes < minimum {
                    return Err(StartupLimitError::InflightMinimum);
                }
                if native.hash.reserved_bytes() > self.memory_soft_bytes / 4 {
                    return Err(StartupLimitError::HashingMemory);
                }
                let reservation_bytes = self.configured_reservation(native)?;
                if reservation_bytes > self.memory_soft_bytes {
                    return Err(StartupLimitError::ConfiguredMemory);
                }
                Ok(StartupLimits { values: self, native, reservation_bytes })
            }
        }
        impl RawStartupLimits {
            fn configured_reservation(&self, native: NativeAccounting) -> Result<u64, StartupLimitError> {
                let parts = [
                    self.max_connections.checked_mul(IDLE_CONNECTION_ALLOWANCE),
                    Some(self.inflight_bytes),
                    native.rpc_concurrency.checked_mul(FIXED_REQUEST_ALLOWANCE),
                    Some(self.assets_memory_bytes),
                    native.worker_threads.checked_mul(self.thread_cache_bytes),
                    Some(self.spec_cache_bytes),
                    self.rate_table_entries.checked_mul(RATE_ENTRY_ALLOWANCE),
                    self.touch_queue_entries.checked_mul(TOUCH_ENTRY_ALLOWANCE),
                    Some(native.hash.reserved_bytes()),
                ];
                parts.into_iter().try_fold(0u64, |total, part| {
                    total.checked_add(part.ok_or(StartupLimitError::ArithmeticOverflow)?)
                        .ok_or(StartupLimitError::ArithmeticOverflow)
                })
            }
        }
        impl SharedSettings for StartupLimits {
            fn section(&self) -> SectionName { SectionName::new("startup_limits") }
            fn describe(&self, out: &mut SettingsWriter<'_>) -> Result<(), NodeError> {
                // Explicit destructuring makes additions require an identity decision.
                let Self { values, native, reservation_bytes: _ } = self;
                let RawStartupLimits { $( $field, )+ } = values;
                $( out.shared_u64(concat!("startup_limits.", $name), *$field)?; )+
                native.describe(out)
            }
        }
    };
}

limits! {
    max_connections => ("http.max_connections", 8192, 64, 1_048_576),
    keepalive_idle_secs => ("http.keepalive_idle_secs", 60, 1, 3600),
    header_read_secs => ("http.header_read_secs", 10, 1, 120),
    body_read_secs => ("http.body_read_secs", 30, 1, 600),
    body_min_bytes_per_sec => ("http.body_min_bytes_per_sec", 8 * KIB, KIB, MIB),
    response_min_bytes_per_sec => ("http.response_min_bytes_per_sec", 16 * KIB, KIB, 16 * MIB),
    cancel_concurrency => ("db.cancel_concurrency", 4, 1, 64),
    header_bytes => ("limits.header_bytes", 16 * KIB, 8 * KIB, MIB),
    header_count => ("limits.header_count", 100, 16, 1000),
    inflight_bytes => ("admission.inflight_bytes", 512 * MIB, 1, u64::MAX),
    thread_cache_bytes => ("buffers.thread_cache_bytes", 4 * MIB, 0, 256 * MIB),
    assets_memory_bytes => ("assets.memory_bytes", 256 * MIB, 16 * MIB, 16 * GIB),
    spec_cache_bytes => ("rpc.spec_cache_bytes", 16 * MIB, MIB, GIB),
    rate_table_entries => ("limits.rate_table_entries", 65_536, 1024, 16_777_216),
    touch_queue_entries => ("session.touch_queue_entries", 65_536, 1024, 16_777_216),
    ws_read_buffer_bytes => ("ws.read_buffer_bytes", 4 * KIB, KIB, 128 * KIB),
    ws_inflight_bytes => ("admission.ws_inflight_bytes", 64 * MIB, 1, u64::MAX),
    ws_message_read_secs => ("ws.message_read_secs", 30, 1, 600),
    ws_min_bytes_per_sec => ("ws.min_bytes_per_sec", 8 * KIB, KIB, MIB),
    // settings needed to validate D14's dynamic byte bounds.
    rpc_body_bytes => ("limits.rpc_body_bytes", 2 * MIB, 64 * KIB, 128 * MIB),
    rpc_result_bytes => ("limits.rpc_result_bytes", 16 * MIB, MIB, GIB),
    ws_max_message_bytes => ("ws.max_message_bytes", MIB, 4 * KIB, 16 * MIB),
    memory_soft_bytes => ("process.memory_soft_bytes", 4 * GIB, 256 * MIB, u64::MAX),
}

fn minimum_inflight(body: u64, result: u64, websocket: u64) -> Result<u64, StartupLimitError> {
    body.checked_mul(2)
        .and_then(|value| value.checked_add(result))
        .and_then(|value| value.checked_add(websocket))
        .ok_or(StartupLimitError::ArithmeticOverflow)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]
mod tests {
    use super::*;

    #[test]
    fn byte_relationship_arithmetic_is_checked() {
        assert_eq!(
            minimum_inflight(u64::MAX, 1, 1),
            Err(StartupLimitError::ArithmeticOverflow)
        );
        assert_eq!(
            minimum_inflight(1, u64::MAX, 1),
            Err(StartupLimitError::ArithmeticOverflow)
        );
        assert_eq!(
            minimum_inflight(1, 1, u64::MAX),
            Err(StartupLimitError::ArithmeticOverflow)
        );
        assert_eq!(
            StartupLimitError::ArithmeticOverflow.to_string(),
            "server startup byte-budget arithmetic overflowed"
        );
    }

    #[test]
    fn configured_reservation_products_and_sum_are_checked() {
        let native = NativeAccounting::default_for_tests();
        for limits in [
            RawStartupLimits {
                max_connections: u64::MAX,
                ..RawStartupLimits::default()
            },
            RawStartupLimits {
                rate_table_entries: u64::MAX,
                ..RawStartupLimits::default()
            },
            RawStartupLimits {
                touch_queue_entries: u64::MAX,
                ..RawStartupLimits::default()
            },
            RawStartupLimits {
                inflight_bytes: u64::MAX,
                ..RawStartupLimits::default()
            },
        ] {
            assert_eq!(
                limits.configured_reservation(native),
                Err(StartupLimitError::ArithmeticOverflow)
            );
        }
        let limits = RawStartupLimits::default();
        let excessive_workers = NativeAccounting {
            worker_threads: u64::MAX,
            ..native
        };
        assert_eq!(
            limits.configured_reservation(excessive_workers),
            Err(StartupLimitError::ArithmeticOverflow)
        );
        let excessive_requests = NativeAccounting {
            rpc_concurrency: u64::MAX,
            ..native
        };
        assert_eq!(
            limits.configured_reservation(excessive_requests),
            Err(StartupLimitError::ArithmeticOverflow)
        );
        assert_eq!(
            StartupLimitError::ArithmeticOverflow.to_string(),
            "server startup byte-budget arithmetic overflowed"
        );
    }

    #[test]
    fn native_workers_and_execution_permits_participate_in_reservation() {
        let limits = RawStartupLimits::default();
        let native = NativeAccounting::default_for_tests();
        let original = limits.configured_reservation(native).unwrap();
        let two_workers = NativeAccounting {
            worker_threads: 2,
            ..native
        };
        assert_eq!(
            limits
                .configured_reservation(two_workers)
                .unwrap()
                .checked_sub(original)
                .unwrap(),
            limits.thread_cache_bytes
        );
        let more_requests = NativeAccounting {
            rpc_concurrency: native.rpc_concurrency.checked_add(1).unwrap(),
            ..native
        };
        assert_eq!(
            limits
                .configured_reservation(more_requests)
                .unwrap()
                .checked_sub(original)
                .unwrap(),
            32 * 1024
        );
    }

    #[test]
    fn configured_hashing_reservation_is_counted_and_capped() {
        let defaults = NativeAccounting::default_for_tests();
        let more_hashing = NativeAccounting {
            hash: erp_security::auth::pool::HashPoolConfig::new(4, 5, 19_456).unwrap(),
            ..defaults
        };
        let base = RawStartupLimits::default()
            .validate(u64::MAX, defaults)
            .unwrap();
        let more = RawStartupLimits::default()
            .validate(u64::MAX, more_hashing)
            .unwrap();
        assert_eq!(
            more.configured_reservation_bytes()
                .checked_sub(base.configured_reservation_bytes())
                .unwrap(),
            more_hashing
                .hash
                .reserved_bytes()
                .checked_sub(defaults.hash.reserved_bytes())
                .unwrap()
        );
        let over_quarter = NativeAccounting {
            hash: erp_security::auth::pool::HashPoolConfig::new(64, 5, 19_456).unwrap(),
            ..defaults
        };
        let error = RawStartupLimits::default()
            .validate(u64::MAX, over_quarter)
            .unwrap_err();
        assert_eq!(error, StartupLimitError::HashingMemory);
        assert_eq!(
            error.to_string(),
            "server startup limits require admission.hash_concurrency * admission.hash_memory_kib * 1024 <= process.memory_soft_bytes / 4"
        );
    }

    #[test]
    fn host_memory_is_required_and_bounds_the_soft_cap() {
        assert_eq!(
            RawStartupLimits::default()
                .validate(0, NativeAccounting::default_for_tests())
                .unwrap_err(),
            StartupLimitError::HostMemoryUnavailable
        );
        assert_eq!(
            StartupLimitError::HostMemoryUnavailable.to_string(),
            "server startup limits could not determine host RAM"
        );
        assert_eq!(
            RawStartupLimits::default()
                .validate(GIB, NativeAccounting::default_for_tests())
                .unwrap_err(),
            StartupLimitError::HostMemory
        );
        assert_eq!(
            StartupLimitError::HostMemory.to_string(),
            "server startup limits require process.memory_soft_bytes <= host RAM"
        );
    }
}
