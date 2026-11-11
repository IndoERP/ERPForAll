//! Operator identity command admission; no database connection is needed for parser tests.
#![allow(clippy::expect_used, reason = "parser test assertions")]
use clap::Parser;
use erp_cli::Cli;

#[test]
fn explicit_enrollment_requires_operator_and_note() {
    assert!(
        Cli::try_parse_from([
            "erp",
            "module",
            "identity-enroll",
            "--config",
            "server.toml",
            "--operator",
            "release-operator",
            "--note",
            "approved legacy enrollment",
            "base"
        ])
        .is_ok()
    );
    let error = Cli::try_parse_from([
        "erp",
        "module",
        "identity-enroll",
        "--config",
        "server.toml",
        "base",
    ])
    .expect_err("enrollment without operator and note must fail");
    assert_eq!(
        error.kind(),
        clap::error::ErrorKind::MissingRequiredArgument
    );
    // Pin the complete missing-arguments paragraph; generated usage/help text is separate.
    let diagnostic = error.to_string();
    assert!(
        diagnostic.starts_with(
            "error: the following required arguments were not provided:\n  --operator <OPERATOR>\n  --note <NOTE>\n\n"
        ),
        "{diagnostic}"
    );
}

#[test]
fn forward_activation_requires_both_release_roots() {
    assert!(
        Cli::try_parse_from([
            "erp",
            "module",
            "identity-activate",
            "--config",
            "server.toml",
            "--release",
            "/releases/new",
            "--previous",
            "/releases/old",
            "--upgrade-receipt",
            "/state/upgrade.json"
        ])
        .is_ok()
    );
    let error = Cli::try_parse_from([
        "erp",
        "module",
        "identity-activate",
        "--config",
        "server.toml",
        "--release",
        "/releases/new",
    ])
    .expect_err("activation without previous release and upgrade receipt must fail");
    assert_eq!(
        error.kind(),
        clap::error::ErrorKind::MissingRequiredArgument
    );
    let diagnostic = error.to_string();
    assert!(
        diagnostic.starts_with(
            "error: the following required arguments were not provided:\n  --previous <PREVIOUS>\n  --upgrade-receipt <UPGRADE_RECEIPT>\n\n"
        ),
        "{diagnostic}"
    );
}

#[test]
fn operator_upgrade_accepts_explicit_previous_release() {
    assert!(
        Cli::try_parse_from([
            "erp",
            "module",
            "upgrade",
            "-d",
            "erp_test",
            "--identity-previous",
            "/releases/old",
            "base"
        ])
        .is_ok()
    );
    assert!(
        Cli::try_parse_from([
            "erp",
            "module",
            "install",
            "-d",
            "erp_test",
            "--identity-previous",
            "/releases/old",
            "base"
        ])
        .is_ok()
    );
    // The lifecycle dispatcher rejects this flag on install; parsing preserves it for a precise error.
}

#[test]
fn legacy_bootstrap_enrolls_the_installed_set_without_positional_names() {
    let args = [
        "erp",
        "module",
        "identity-enroll",
        "--config",
        "server.toml",
        "--operator",
        "operator",
        "--note",
        "reviewed old release",
        "--legacy-release",
        "/releases/old",
        "--installed",
    ];
    assert!(Cli::try_parse_from(args).is_ok());
    let mut mixed = args.to_vec();
    mixed.push("base");
    let error =
        Cli::try_parse_from(mixed).expect_err("bootstrap cannot select only some installed addons");
    assert_eq!(error.kind(), clap::error::ErrorKind::ArgumentConflict);
    // Pin the complete stable diagnostic paragraph; generated usage/help text is separate.
    let diagnostic = error.to_string();
    assert!(diagnostic.starts_with("error: the argument '--legacy-release <LEGACY_RELEASE>' cannot be used with '[MODULES]...'\n\n"), "{diagnostic}");
}

#[test]
fn legacy_bootstrap_requires_installed_and_legacy_release_together() {
    let base = [
        "erp",
        "module",
        "identity-enroll",
        "--config",
        "server.toml",
        "--operator",
        "operator",
        "--note",
        "reviewed old release",
    ];
    for (flags, expected) in [
        (
            vec!["--installed"],
            "error: the following required arguments were not provided:\n  --legacy-release <LEGACY_RELEASE>\n\n",
        ),
        (
            vec!["--legacy-release", "/releases/old"],
            "error: the following required arguments were not provided:\n  --installed\n  <MODULES>...\n\n",
        ),
    ] {
        let error = Cli::try_parse_from(base.into_iter().chain(flags.iter().copied()))
            .expect_err("both legacy bootstrap flags are required");
        assert_eq!(
            error.kind(),
            clap::error::ErrorKind::MissingRequiredArgument
        );
        // The complete required-arguments paragraph is stable; generated usage/help is separate.
        let diagnostic = error.to_string();
        assert!(diagnostic.starts_with(expected), "{diagnostic}");
    }
}
