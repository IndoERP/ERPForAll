// SPDX-License-Identifier: MIT OR Apache-2.0
//! Base's declared technical parameters: the keys that were fixed lists in
//! `ir_config_parameter.rs` (authentication and sessions, `attachments.*`, partition upkeep) and
//! base's other `base.*` parameters. Their read parsers and write-time checks (with their
//! messages) stay where they were; these declarations add the kind, default, floor and hard bound,
//! the default rows and the prefix rule.
use erp_sdk::auth::{
    COOLDOWN_AFTER_FALLBACK, COOLDOWN_DURATION_FALLBACK, MAX_INACTIVITY_DEFAULT,
    MAX_LIFETIME_DEFAULT, hash_limits, keys,
};
use erp_sdk::settings::ParameterDecl;
use erp_sdk::{Error, Result};

pub(crate) const EMAIL_SIZE_KEY: &str = "base.default_max_email_size";
/// Whether users may create programmatic API keys.
pub(crate) const API_KEYS_ENABLED_KEY: &str = "base.enable_programmatic_api_keys";
/// Programmatic API keys per user.
pub(crate) const API_KEYS_LIMIT_KEY: &str = "base.programmatic_api_keys_limit";

fn attachment_label(key: &str) -> Result<&'static str> {
    Ok(match key {
        "attachments.usage_delta_read_limit" => "Attachment usage deltas read per quota check",
        "attachments.usage_compaction_batch" => "Attachment usage compaction batch",
        "attachments.usage_compaction_interval_secs" => {
            "Attachment usage compaction interval (seconds)"
        }
        "attachments.upload_idle_timeout_secs" => "Upload idle timeout (seconds)",
        "attachments.upload_receive_timeout_secs" => "Upload receive timeout (seconds)",
        "attachments.gc_interval_secs" => "Attachment blob collection interval (seconds)",
        "attachments.cleanup_interval_secs" => "Attachment staging cleanup interval (seconds)",
        "attachments.reconcile_interval_secs" => "Attachment store reconcile interval (seconds)",
        "attachments.maintenance_batch" => "Attachment maintenance batch",
        "attachments.composer_retention_secs" => "Composer draft attachment retention (seconds)",
        other => {
            return Err(Error::programming(format!(
                "The attachment parameter {other} has no label in system_parameters.rs."
            )));
        }
    })
}

fn int(key: &str, label: &str, default: u32, floor: u32, bound: u32) -> Result<ParameterDecl> {
    ParameterDecl::integer(
        key,
        label,
        i64::from(default),
        (i64::from(floor), i64::from(bound)),
    )
}

/// Base's parameters. The bounds equal the ones the existing parsers enforce.
pub(crate) fn declarations() -> Result<Vec<ParameterDecl>> {
    use hash_limits as h;
    let partition = |v: u64| {
        i64::try_from(v).map_err(|_| Error::programming("partition upkeep bound overflows i64"))
    };
    let mut out = vec![
        ParameterDecl::selection(
            keys::SCHEME,
            "Password hashing scheme",
            "argon2id",
            &[("argon2id", "Argon2id"), ("pbkdf2_sha512", "PBKDF2-SHA512")],
        )?,
        int(
            keys::ROUNDS,
            "PBKDF2 rounds",
            h::PBKDF2_ROUNDS_MIN,
            h::PBKDF2_ROUNDS_MIN,
            h::PBKDF2_ROUNDS_MAX,
        )?,
        int(
            keys::ARGON2_MEMORY,
            "Argon2id memory (KiB)",
            h::ARGON2_MEMORY_KIB_MIN,
            h::ARGON2_MEMORY_KIB_MIN,
            h::ARGON2_MEMORY_KIB_MAX,
        )?,
        int(
            keys::ARGON2_TIME,
            "Argon2id passes",
            h::ARGON2_TIME_MIN,
            h::ARGON2_TIME_MIN,
            h::ARGON2_TIME_MAX,
        )?,
        int(
            keys::COOLDOWN_AFTER,
            "Failed logins per address before a cooldown",
            COOLDOWN_AFTER_FALLBACK,
            1,
            1_000_000,
        )?,
        int(
            keys::COOLDOWN_DURATION,
            "Login cooldown (seconds)",
            COOLDOWN_DURATION_FALLBACK,
            1,
            604_800,
        )?,
        // 0 turns the per-login rule off (the per-address rule stays); the absent value is four
        // times the per-address threshold.
        int(
            keys::COOLDOWN_AFTER_LOGIN,
            "Failed logins per login before a cooldown (0: per-login rule off)",
            COOLDOWN_AFTER_FALLBACK.saturating_mul(4),
            0,
            4_000_000,
        )?,
        int(
            keys::MAX_INACTIVITY,
            "Session idle limit (seconds)",
            MAX_INACTIVITY_DEFAULT,
            60,
            31_536_000,
        )?,
        int(
            keys::MAX_LIFETIME,
            "Session lifetime (seconds)",
            MAX_LIFETIME_DEFAULT,
            3_600,
            31_536_000,
        )?,
        int(
            keys::TOUCH_INTERVAL,
            "Session touch interval (seconds)",
            60,
            1,
            3_600,
        )?,
        int(
            keys::ROTATION_INTERVAL,
            "Session secret rotation interval (seconds)",
            10_800,
            60,
            86_400,
        )?,
        int(
            keys::ROTATION_GRACE,
            "Session secret rotation grace (seconds)",
            120,
            5,
            600,
        )?,
        ParameterDecl::integer(
            crate::partition_upkeep_settings::KEY,
            "Partition upkeep interval (seconds)",
            partition(crate::partition_upkeep_settings::DEFAULT_SECS)?,
            (
                partition(crate::partition_upkeep_settings::FLOOR_SECS)?,
                partition(crate::partition_upkeep_settings::HARD_MAX_SECS)?,
            ),
        )?,
        ParameterDecl::float(
            EMAIL_SIZE_KEY,
            "Default maximum email size (MB)",
            20.0,
            (1.0, 1_024.0),
        )?,
        ParameterDecl::boolean(
            API_KEYS_ENABLED_KEY,
            "Users may create programmatic API keys",
            false,
        )?,
        ParameterDecl::integer(
            API_KEYS_LIMIT_KEY,
            "Programmatic API keys per user",
            10,
            (1, i64::from(u32::MAX)),
        )?,
    ];
    for (key, default, floor, bound) in crate::attachments::settings::PARAMETERS {
        out.push(ParameterDecl::integer(
            key,
            attachment_label(key)?,
            default,
            (floor, bound),
        )?);
    }
    Ok(out)
}
erp_sdk::system_parameters!(declarations);

/// The parameter registry, refusing a binary whose linker dropped base's own registration
/// : base would then publish with its keys undeclared.
/// # Errors
/// The registry's refusals; `Programming` when base is installed but its declarations are absent.
pub(crate) fn published(env: &erp_sdk::Env) -> Result<erp_sdk::settings::ParameterRegistry> {
    let registry = erp_sdk::params::registry(env)?;
    let base_installed = env.registry().addons().iter().any(|a| a.name == "base");
    if base_installed
        && registry
            .get(crate::partition_upkeep_settings::KEY)
            .is_none()
    {
        return Err(Error::programming(
            "The system parameters of base are not linked into this binary (owner crate \
             erp-addon-base); their registration was dropped by the linker.",
        ));
    }
    Ok(registry)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base_declarations_match_the_existing_parsers() {
        let declared = declarations().unwrap();
        assert_eq!(
            declared.len(),
            16 + crate::attachments::settings::PARAMETERS.len()
        );
        for decl in &declared {
            let key = decl.key();
            let default = decl.default_text();
            let existing = |text: &str| {
                erp_sdk::auth::validate_setting(key, Some(text))
                    .and_then(|()| crate::attachments::settings::validate(key, Some(text)))
                    .and_then(|()| crate::partition_upkeep_settings::validate(key, Some(text)))
            };
            assert!(existing(default).is_ok(), "{key} default {default}");
            assert!(decl.check_text(Some(default)).is_ok(), "{key}");
            if let erp_sdk::settings::SettingKind::Integer { min, max } = decl.kind() {
                for ok in [*min, *max] {
                    assert!(existing(&ok.to_string()).is_ok(), "{key} {ok}");
                    assert!(decl.check_text(Some(&ok.to_string())).is_ok(), "{key} {ok}");
                }
                for refused in [min - 1, max + 1] {
                    assert!(decl.check_text(Some(&refused.to_string())).is_err());
                    // the base keys with an existing parser refuse the same values
                    let has_parser = erp_sdk::auth::keys::ALL.contains(&key)
                        || crate::attachments::settings::contains(key)
                        || key == crate::partition_upkeep_settings::KEY;
                    if has_parser {
                        assert!(existing(&refused.to_string()).is_err(), "{key} {refused}");
                    }
                }
            }
        }
    }
}
