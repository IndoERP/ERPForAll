//! Public-API tests of the policy, limit and error types (step 2).

// Helpers outside `#[test]` functions use expect and panic (clippy.toml only exempts test fns).
#![allow(clippy::expect_used, clippy::panic)]

use std::error::Error;

use erp_html::{
    DefaultPolicy, FieldPolicy, HtmlSanitizeError, LimitError, LimitKind, Limits, OutputMethod,
    ParseFailure, SanitizeMode, SanitizePolicy,
};

const CEILING: [(LimitKind, &str, u64); 6] = [
    (LimitKind::InputBytes, "input_bytes", 10_000_000),
    (LimitKind::Depth, "depth", 256),
    (LimitKind::Nodes, "nodes", 100_000),
    (LimitKind::OutputBytes, "output_bytes", 32_000_000),
    (LimitKind::WorkingBytes, "working_bytes", 128_000_000),
    (LimitKind::WorkUnits, "work_units", 50_000_000),
];

/// The decoder error for `bytes`, taken from a runtime value so the compiler does not lint it.
fn utf8_error(bytes: &[u8]) -> std::str::Utf8Error {
    std::str::from_utf8(bytes).expect_err("the bytes are not valid UTF-8")
}

fn get(limits: &Limits, kind: LimitKind) -> u64 {
    match kind {
        LimitKind::InputBytes => limits.input_bytes(),
        LimitKind::Depth => limits.depth(),
        LimitKind::Nodes => limits.nodes(),
        LimitKind::OutputBytes => limits.output_bytes(),
        LimitKind::WorkingBytes => limits.working_bytes(),
        LimitKind::WorkUnits => limits.work_units(),
        other => panic!("unlisted limit kind {other:?}"),
    }
}

fn set(limits: Limits, kind: LimitKind, value: u64) -> Result<Limits, LimitError> {
    match kind {
        LimitKind::InputBytes => limits.with_input_bytes(value),
        LimitKind::Depth => limits.with_depth(value),
        LimitKind::Nodes => limits.with_nodes(value),
        LimitKind::OutputBytes => limits.with_output_bytes(value),
        LimitKind::WorkingBytes => limits.with_working_bytes(value),
        LimitKind::WorkUnits => limits.with_work_units(value),
        other => panic!("unlisted limit kind {other:?}"),
    }
}

#[test]
fn ceiling_values_are_the_adr_0042_d3_numbers() {
    let ceiling = Limits::ceiling();
    for (kind, _, value) in CEILING {
        assert_eq!(get(&ceiling, kind), value, "{kind:?}");
    }
}

/// `Limits::recommended` lowers only the input cap; the other limits stay at the ceiling, and the
/// result can be rebuilt with the validated setters (so it is a valid `Limits`).
#[test]
fn recommended_limits_differ_from_the_ceiling_only_in_the_input_cap() {
    let recommended = Limits::recommended();
    let ceiling = Limits::ceiling();
    assert!(recommended.input_bytes() < ceiling.input_bytes());
    for (kind, name, _) in CEILING {
        if kind != LimitKind::InputBytes {
            assert_eq!(get(&recommended, kind), get(&ceiling, kind), "{name}");
        }
    }
    let rebuilt = ceiling
        .with_input_bytes(recommended.input_bytes())
        .expect("the recommended cap is a valid input limit");
    assert_eq!(rebuilt, recommended);
}

#[test]
fn every_limit_accepts_one_and_the_ceiling_and_refuses_zero_and_above() {
    for (kind, name, ceiling) in CEILING {
        let one = set(Limits::ceiling(), kind, 1).expect("1 is accepted");
        assert_eq!(get(&one, kind), 1, "{name} lowered to 1");
        let same = set(Limits::ceiling(), kind, ceiling).expect("the ceiling is accepted");
        assert_eq!(get(&same, kind), ceiling, "{name} at the ceiling");
        assert_eq!(same, Limits::ceiling());

        let zero = set(Limits::ceiling(), kind, 0).expect_err("0 is refused");
        assert_eq!(zero, LimitError::Zero { limit: kind });
        assert_eq!(
            zero.to_string(),
            format!("limit `{name}` must be at least 1")
        );

        let above = set(Limits::ceiling(), kind, ceiling + 1).expect_err("ceiling + 1 is refused");
        assert_eq!(
            above,
            LimitError::AboveCeiling {
                limit: kind,
                requested: ceiling + 1,
                ceiling
            }
        );
        assert_eq!(
            above.to_string(),
            format!(
                "limit `{name}` is {}, above the ceiling of {ceiling}",
                ceiling + 1
            )
        );

        let max = set(Limits::ceiling(), kind, u64::MAX).expect_err("u64::MAX is refused");
        assert_eq!(
            max,
            LimitError::AboveCeiling {
                limit: kind,
                requested: u64::MAX,
                ceiling
            }
        );
    }
}

#[test]
fn lowering_one_limit_leaves_the_others_at_the_ceiling() {
    let limits = Limits::ceiling()
        .with_depth(8)
        .expect("8 is within the ceiling");
    assert_eq!(limits.depth(), 8);
    assert_eq!(limits.input_bytes(), 10_000_000);
    assert_eq!(limits.nodes(), 100_000);
    assert_eq!(limits.output_bytes(), 32_000_000);
    assert_eq!(limits.working_bytes(), 128_000_000);
    assert_eq!(limits.work_units(), 50_000_000);
}

#[test]
fn limit_error_has_no_source() {
    let error = LimitError::Zero {
        limit: LimitKind::Nodes,
    };
    assert!(error.source().is_none());
}

#[test]
fn default_policy_starts_with_no_extra_strictness() {
    let policy = DefaultPolicy::new();
    assert!(!policy.strip_classes());
    assert!(!policy.sanitize_style());
    assert!(!policy.strip_style());
    assert_eq!(policy.output_method(), OutputMethod::Html);
}

#[test]
fn default_policy_builders_only_add_strictness() {
    let policy = DefaultPolicy::new().with_strip_classes();
    assert!(policy.strip_classes());
    assert!(!policy.sanitize_style() && !policy.strip_style());

    let policy = DefaultPolicy::new().with_sanitize_style();
    assert!(policy.sanitize_style());
    assert!(!policy.strip_classes() && !policy.strip_style());

    let policy = DefaultPolicy::new().with_strip_style();
    assert!(policy.strip_style());
    assert!(!policy.strip_classes() && !policy.sanitize_style());

    let policy = DefaultPolicy::new().with_xml_output();
    assert_eq!(policy.output_method(), OutputMethod::Xml);

    // applying a builder twice changes nothing, and nothing can switch a setting back off
    assert_eq!(
        DefaultPolicy::new()
            .with_strip_classes()
            .with_strip_classes(),
        DefaultPolicy::new().with_strip_classes()
    );
    let all = DefaultPolicy::new()
        .with_strip_classes()
        .with_sanitize_style()
        .with_strip_style()
        .with_xml_output();
    assert!(all.strip_classes() && all.sanitize_style() && all.strip_style());
    assert_eq!(all.output_method(), OutputMethod::Xml);
}

#[test]
fn field_policy_wraps_the_closed_policy_choices() {
    let policy = SanitizePolicy::Default(DefaultPolicy::new());
    let field = FieldPolicy::Sanitize(policy);
    assert_eq!(
        field,
        FieldPolicy::Sanitize(SanitizePolicy::Default(DefaultPolicy::new()))
    );
    assert_ne!(field, FieldPolicy::UnsanitisedText);
    assert!(matches!(
        field,
        FieldPolicy::Sanitize(SanitizePolicy::Default(_))
    ));
}

#[test]
fn sanitize_mode_clean_exists() {
    assert_eq!(SanitizeMode::Clean, SanitizeMode::Clean);
}

#[test]
fn error_messages_are_fixed_and_never_contain_input() {
    let utf8 = utf8_error(&[b'a', b'b', 0xff, b'c', b'd']);
    let cases: Vec<(HtmlSanitizeError, &str)> = vec![
        (
            HtmlSanitizeError::InvalidUtf8 { source: utf8 },
            "input is not valid UTF-8 (valid up to byte 2)",
        ),
        (
            HtmlSanitizeError::NulByte { offset: 7 },
            "input contains a NUL byte at offset 7",
        ),
        (
            HtmlSanitizeError::InputTooLarge {
                len: 10_000_001,
                limit: 10_000_000,
            },
            "input of 10000001 bytes exceeds the limit of 10000000 bytes",
        ),
        (
            HtmlSanitizeError::DepthExceeded { limit: 256 },
            "element nesting exceeds the depth limit of 256",
        ),
        (
            HtmlSanitizeError::NodeLimit { limit: 100_000 },
            "document exceeds the node limit of 100000",
        ),
        (
            HtmlSanitizeError::WorkLimit { limit: 50_000_000 },
            "sanitising exceeds the work limit of 50000000 units",
        ),
        (
            HtmlSanitizeError::MemoryLimit { limit: 128_000_000 },
            "sanitising exceeds the working memory limit of 128000000 bytes",
        ),
        (
            HtmlSanitizeError::OutputTooLarge { limit: 32_000_000 },
            "output exceeds the limit of 32000000 bytes (output is capped by min(output_bytes, input_bytes))",
        ),
        (
            HtmlSanitizeError::ParseFailed(ParseFailure::ReparseUnstable),
            "HTML parsing failed: the sanitised output changed when parsed again",
        ),
        (
            HtmlSanitizeError::Internal {
                reason: "tree builder stopped",
            },
            "internal sanitiser error: tree builder stopped",
        ),
    ];
    for (error, message) in cases {
        assert_eq!(error.to_string(), message, "{error:?}");
    }
}

#[test]
fn errors_keep_their_source() {
    let utf8 = utf8_error(&[0xff]);
    let error = HtmlSanitizeError::InvalidUtf8 { source: utf8 };
    assert_eq!(
        error.source().expect("utf-8 source").to_string(),
        utf8.to_string()
    );

    let error = HtmlSanitizeError::ParseFailed(ParseFailure::ReparseUnstable);
    assert_eq!(
        error.source().expect("parse-failure source").to_string(),
        "the sanitised output changed when parsed again"
    );

    for error in [
        HtmlSanitizeError::NulByte { offset: 0 },
        HtmlSanitizeError::InputTooLarge { len: 2, limit: 1 },
        HtmlSanitizeError::DepthExceeded { limit: 1 },
        HtmlSanitizeError::NodeLimit { limit: 1 },
        HtmlSanitizeError::WorkLimit { limit: 1 },
        HtmlSanitizeError::MemoryLimit { limit: 1 },
        HtmlSanitizeError::OutputTooLarge { limit: 1 },
    ] {
        assert!(error.source().is_none(), "{error:?}");
    }
}
