//! Removal report and strict mode: and at
//! the public API. Error kind, exact message and report contents are asserted.

// Helpers outside `#[test]` functions use expect (clippy.toml only exempts test fns).
#![allow(clippy::expect_used)]

use erp_html::{
    DefaultPolicy, HtmlSanitizeError, LimitError, LimitKind, Limits, RemovalKind, RemovalReason,
    RemovalReport, SanitizeMode, SanitizePolicy, sanitize,
};

fn clean_with(input: &str, policy: DefaultPolicy, limits: &Limits) -> RemovalReport {
    sanitize(
        input,
        &SanitizePolicy::Default(policy),
        limits,
        SanitizeMode::Clean,
    )
    .expect("sanitises")
    .into_parts()
    .1
}

fn report(input: &str) -> RemovalReport {
    clean_with(input, DefaultPolicy::new(), &Limits::ceiling())
}

fn strict(
    input: &str,
    policy: DefaultPolicy,
    limits: &Limits,
) -> Result<erp_html::SanitizeResult, HtmlSanitizeError> {
    sanitize(
        input,
        &SanitizePolicy::Default(policy),
        limits,
        SanitizeMode::Strict,
    )
}

/// `(kind, name, reason, count)` of every entry, for compact assertions.
fn shape(report: &RemovalReport) -> Vec<(RemovalKind, &str, RemovalReason, u64)> {
    report
        .entries()
        .iter()
        .map(|e| (e.kind(), e.name(), e.reason(), e.count()))
        .collect()
}

#[test]
fn report_names_each_kind_of_removal() {
    let r = report(
        "<script>alert(1)</script><foo>b</foo><p onclick=\"x()\" foo=\"1\">t</p>\
         <a href=\"javascript:alert(1)\">l</a>",
    );
    assert_eq!(
        shape(&r),
        vec![
            (
                RemovalKind::Tag,
                "script",
                RemovalReason::ForbiddenElement,
                1
            ),
            (RemovalKind::Tag, "foo", RemovalReason::UnwrappedElement, 1),
            (
                RemovalKind::Attribute,
                "onclick",
                RemovalReason::EventHandler,
                1
            ),
            (
                RemovalKind::Attribute,
                "foo",
                RemovalReason::ForbiddenAttribute,
                1
            ),
            (RemovalKind::Url, "href", RemovalReason::DisallowedUrl, 1),
        ]
    );
    assert_eq!(r.total_count(), 5);
}

#[test]
fn report_names_policy_stripped_attributes() {
    let policy = DefaultPolicy::new().with_strip_classes().with_strip_style();
    let r = clean_with(
        "<p class=\"a\" style=\"color:red\">x</p>",
        policy,
        &Limits::ceiling(),
    );
    assert_eq!(
        shape(&r),
        vec![
            (
                RemovalKind::Attribute,
                "class",
                RemovalReason::StrippedClass,
                1
            ),
            (
                RemovalKind::Attribute,
                "style",
                RemovalReason::StrippedStyle,
                1
            ),
        ]
    );
}

#[test]
fn report_names_css_declarations_by_property() {
    let style = "color:red; position:fixed; background:url(javascript:alert(1)); junk";
    let plain = report(&format!("<p style=\"{style}\">x</p>"));
    assert_eq!(
        shape(&plain),
        vec![(
            RemovalKind::Css,
            "background",
            RemovalReason::DangerousCss,
            1
        )]
    );
    let whitelisted = clean_with(
        &format!("<p style=\"{style}\">x</p>"),
        DefaultPolicy::new().with_sanitize_style(),
        &Limits::ceiling(),
    );
    assert_eq!(
        shape(&whitelisted),
        vec![
            (
                RemovalKind::Css,
                "position",
                RemovalReason::CssNotWhitelisted,
                1
            ),
            (
                RemovalKind::Css,
                "background",
                RemovalReason::DangerousCss,
                1
            ),
            (
                RemovalKind::Css,
                "(invalid)",
                RemovalReason::MalformedCss,
                1
            ),
        ]
    );
}

#[test]
fn report_counts_conditional_comments_only() {
    let r = report("<p>a<!--[if IE]>x<![endif]-->b<!-- kept --></p>");
    assert_eq!(
        shape(&r),
        vec![(
            RemovalKind::Comment,
            "conditional",
            RemovalReason::ConditionalComment,
            1
        )]
    );
}

#[test]
fn repeated_removals_aggregate_and_samples_stop_at_the_bound() {
    for repeats in [1usize, 7, 8, 9, 500] {
        let input = "<script>x</script>".repeat(repeats);
        let r = report(&input);
        assert_eq!(r.entries().len(), 1, "{repeats}");
        let entry = &r.entries()[0];
        assert_eq!(entry.count(), repeats as u64, "{repeats}");
        let kept = repeats.min(8);
        assert_eq!(entry.sample_locations().len(), kept, "{repeats}");
        // Each script is one top-level node, so the first samples are 1, 2, 3, ... in order.
        let want: Vec<u64> = (1..=kept as u64).collect();
        assert_eq!(entry.sample_locations(), want.as_slice(), "{repeats}");
    }
}

#[test]
fn sample_bound_is_lowerable_and_validated() {
    let limits = Limits::ceiling().with_report_samples(2).expect("limit");
    let input = "<script>x</script>".repeat(5);
    let r = clean_with(&input, DefaultPolicy::new(), &limits);
    assert_eq!(r.entries()[0].count(), 5);
    assert_eq!(r.entries()[0].sample_locations(), &[1, 2]);

    assert_eq!(Limits::ceiling().report_samples(), 8);
    assert_eq!(Limits::ceiling().report_bytes(), 1_000_000);
    assert_eq!(
        Limits::ceiling().with_report_samples(0),
        Err(LimitError::Zero {
            limit: LimitKind::ReportSamples
        })
    );
    assert_eq!(
        Limits::ceiling().with_report_samples(9),
        Err(LimitError::AboveCeiling {
            limit: LimitKind::ReportSamples,
            requested: 9,
            ceiling: 8
        })
    );
    assert_eq!(
        Limits::ceiling().with_report_bytes(1_000_001),
        Err(LimitError::AboveCeiling {
            limit: LimitKind::ReportBytes,
            requested: 1_000_001,
            ceiling: 1_000_000
        })
    );
    assert_eq!(
        Limits::ceiling().with_report_bytes(0),
        Err(LimitError::Zero {
            limit: LimitKind::ReportBytes
        })
    );
    for (result, message) in [
        (
            Limits::ceiling().with_report_samples(0),
            "limit `report_samples` must be at least 1",
        ),
        (
            Limits::ceiling().with_report_samples(9),
            "limit `report_samples` is 9, above the ceiling of 8",
        ),
        (
            Limits::ceiling().with_report_bytes(0),
            "limit `report_bytes` must be at least 1",
        ),
        (
            Limits::ceiling().with_report_bytes(1_000_001),
            "limit `report_bytes` is 1000001, above the ceiling of 1000000",
        ),
    ] {
        assert_eq!(result.expect_err("invalid limit").to_string(), message);
    }
    assert_eq!(LimitKind::ReportBytes.name(), "report_bytes");
    assert_eq!(LimitKind::ReportSamples.name(), "report_samples");
}

#[test]
fn report_holds_no_removed_values() {
    let r = report(
        "<script>SECRETBODY</script><a href=\"javascript:SECRETURL\" foo=\"SECRETATTR\">t</a>\
         <p style=\"background:url(javascript:SECRETCSS)\">x</p>",
    );
    let dump = format!("{r:?}");
    assert!(!dump.contains("SECRET"), "{dump}");
    let err = strict(
        "<script>SECRETBODY</script>",
        DefaultPolicy::new(),
        &Limits::ceiling(),
    )
    .expect_err("strict rejects");
    assert!(!format!("{err} {err:?}").contains("SECRET"));
}

#[test]
fn report_is_deterministic_in_encounter_order() {
    let input = "<foo>a</foo><script>1</script><foo>b</foo>";
    let first = report(input);
    assert_eq!(first, report(input));
    assert_eq!(first.total_count(), 3);
    // Entries are in order of first encounter.
    assert_eq!(first.entries()[0].name(), "foo");
    assert_eq!(first.entries()[1].name(), "script");
}

#[test]
fn clean_input_has_an_empty_report() {
    let r = report("<p>hello <b>world</b></p>");
    assert!(r.is_empty());
    assert_eq!(r.total_count(), 0);
    assert_eq!(r.byte_size(), 0);
}

#[test]
fn only_an_oversize_aggregate_hits_the_report_limit() {
    // A single entry costs 64 + name bytes + 8 per sample. One script with one sample is 78.
    let one = report("<script>x</script>");
    assert_eq!(one.byte_size(), 64 + 6 + 8);
    let at = Limits::ceiling()
        .with_report_bytes(one.byte_size())
        .and_then(|l| l.with_report_samples(1))
        .expect("limit");
    // Many repeats of the same feature stay inside the limit that fits the first sample ...
    let many = "<script>x</script>".repeat(2000);
    let r = clean_with(&many, DefaultPolicy::new(), &at);
    assert_eq!(r.entries()[0].count(), 2000);
    // ... while a second distinct entry does not fit, and is refused rather than truncated.
    let err = sanitize(
        "<script>x</script><foo>b</foo>",
        &SanitizePolicy::Default(DefaultPolicy::new()),
        &at,
        SanitizeMode::Clean,
    )
    .expect_err("report too large");
    assert_eq!(err, HtmlSanitizeError::ReportTooLarge { limit: 78 });
    assert_eq!(
        err.to_string(),
        "removal report exceeds the limit of 78 bytes"
    );
    // One byte less refuses even the single entry.
    let below = Limits::ceiling()
        .with_report_bytes(one.byte_size() - 1)
        .expect("limit");
    assert_eq!(
        sanitize(
            "<script>x</script>",
            &SanitizePolicy::Default(DefaultPolicy::new()),
            &below,
            SanitizeMode::Clean,
        ),
        Err(HtmlSanitizeError::ReportTooLarge { limit: 77 })
    );
}

#[test]
fn many_distinct_removals_exceed_the_default_report_limit() {
    let mut input = String::new();
    for n in 0..20_000 {
        input.push_str(&format!(
            "<x-{n:05}-tag-padding-padding>a</x-{n:05}-tag-padding-padding>"
        ));
    }
    let big = Limits::ceiling();
    let err = sanitize(
        &input,
        &SanitizePolicy::Default(DefaultPolicy::new()),
        &big,
        SanitizeMode::Clean,
    )
    .expect_err("report too large");
    assert_eq!(err, HtmlSanitizeError::ReportTooLarge { limit: 1_000_000 });
    assert_eq!(
        err.to_string(),
        "removal report exceeds the limit of 1000000 bytes"
    );
}

#[test]
fn strict_rejects_every_policy_removal_with_the_same_report() {
    let cases = [
        "<script>x</script>",
        "<foo>b</foo>",
        "<p onclick=\"x()\">t</p>",
        "<p foo=\"1\">t</p>",
        "<a href=\"javascript:alert(1)\">l</a>",
        "<p style=\"background:url(javascript:alert(1))\">x</p>",
        "<p>a<!--[if IE]>x<![endif]-->b</p>",
        "<svg><html onload=\"x()\" foo=\"1\">t</html></svg>",
    ];
    for input in cases {
        let default_report = report(input);
        assert!(!default_report.is_empty(), "{input}");
        let err =
            strict(input, DefaultPolicy::new(), &Limits::ceiling()).expect_err("strict rejects");
        match &err {
            HtmlSanitizeError::DisallowedContent { report } => {
                assert_eq!(report, &default_report, "{input}");
            }
            other => panic!("wrong error for {input}: {other:?}"),
        }
        assert_eq!(
            err.to_string(),
            format!(
                "strict mode: the policy removes {} item(s) in {} group(s)",
                default_report.total_count(),
                default_report.entries().len()
            )
        );
    }
}

#[test]
fn strict_rejects_policy_strictness_removals() {
    let policy = DefaultPolicy::new().with_strip_classes();
    let err = strict("<p class=\"a\">x</p>", policy, &Limits::ceiling()).expect_err("rejects");
    assert_eq!(
        err.to_string(),
        "strict mode: the policy removes 1 item(s) in 1 group(s)"
    );
    assert!(matches!(err, HtmlSanitizeError::DisallowedContent { .. }));
    let policy = DefaultPolicy::new().with_sanitize_style();
    let err = strict(
        "<p style=\"position:fixed\">x</p>",
        policy,
        &Limits::ceiling(),
    )
    .expect_err("rejects");
    assert_eq!(
        err.to_string(),
        "strict mode: the policy removes 1 item(s) in 1 group(s)"
    );
    assert!(matches!(err, HtmlSanitizeError::DisallowedContent { .. }));
}

#[test]
fn strict_accepts_harmless_canonicalisation_and_repair() {
    for input in [
        "<p>hello</p>",
        "<p>unclosed <b>bold",
        "text led<br>line",
        "<p>a<!-- kept comment -->b</p>",
        "<!-- comment only -->",
        "<p style=\"\">empty style</p>",
        "<p style=\" ; \">whitespace style</p>",
        "<p style=\"color:red/*c*/\">css comment</p>",
        "<p>a &amp; b &nbsp; c</p>",
        "<DIV>upper</DIV>",
    ] {
        let strict_result = strict(input, DefaultPolicy::new(), &Limits::ceiling())
            .unwrap_or_else(|e| panic!("{input}: {e}"));
        let clean_result = sanitize(
            input,
            &SanitizePolicy::Default(DefaultPolicy::new()),
            &Limits::ceiling(),
            SanitizeMode::Clean,
        )
        .expect("sanitises");
        assert!(strict_result.report().is_empty(), "{input}");
        assert_eq!(strict_result, clean_result, "{input}");
    }
}

#[test]
fn strict_keeps_budget_failures_typed() {
    let limits = Limits::ceiling().with_input_bytes(4).expect("limit");
    assert_eq!(
        strict("<script>x</script>", DefaultPolicy::new(), &limits).map(|_| ()),
        Err(HtmlSanitizeError::InputTooLarge { len: 18, limit: 4 })
    );
    assert_eq!(
        strict("<script>x</script>", DefaultPolicy::new(), &limits)
            .expect_err("input limit")
            .to_string(),
        "input of 18 bytes exceeds the limit of 4 bytes"
    );
    let tiny = Limits::ceiling().with_report_bytes(10).expect("limit");
    assert_eq!(
        strict("<script>x</script>", DefaultPolicy::new(), &tiny).map(|_| ()),
        Err(HtmlSanitizeError::ReportTooLarge { limit: 10 })
    );
    assert_eq!(
        strict("<script>x</script>", DefaultPolicy::new(), &tiny)
            .expect_err("report limit")
            .to_string(),
        "removal report exceeds the limit of 10 bytes"
    );
}

#[test]
fn hostile_names_are_not_copied_into_the_report() {
    let long = "a".repeat(999_990);
    let input = format!("<p javascript:alert(1)=\"x\" {long}=\"y\">t</p>");
    let r = report(&input);
    assert_eq!(
        shape(&r),
        vec![(
            RemovalKind::Attribute,
            "(invalid)",
            RemovalReason::ForbiddenAttribute,
            2
        )]
    );
}

#[test]
fn overridden_css_declarations_are_reported() {
    let r = clean_with(
        "<p style=\"color:red;color:blue\">x</p>",
        DefaultPolicy::new().with_sanitize_style(),
        &Limits::ceiling(),
    );
    assert_eq!(
        shape(&r),
        vec![(RemovalKind::Css, "color", RemovalReason::DuplicateCss, 1)]
    );
    let err = strict(
        "<p style=\"color:red;color:blue\">x</p>",
        DefaultPolicy::new().with_sanitize_style(),
        &Limits::ceiling(),
    )
    .expect_err("duplicate removed");
    assert_eq!(
        err.to_string(),
        "strict mode: the policy removes 1 item(s) in 1 group(s)"
    );
    assert_eq!(err, HtmlSanitizeError::DisallowedContent { report: r });
}

#[test]
fn foreign_content_html_element_is_reported() {
    let input = "<svg><html onload=\"x()\" foo=\"1\" title=\"SECRET\">t</html></svg>";
    let r = report(input);
    assert_eq!(
        r.entries()
            .iter()
            .map(|e| (e.kind(), e.name(), e.count()))
            .collect::<Vec<_>>(),
        vec![
            (RemovalKind::Tag, "html", 1),
            (RemovalKind::Attribute, "onload", 1),
            (RemovalKind::Attribute, "foo", 1),
            (RemovalKind::Attribute, "title", 1),
        ]
    );
    assert_eq!(r.entries()[1].reason(), RemovalReason::EventHandler);
    assert_eq!(r.entries()[2].reason(), RemovalReason::ForbiddenAttribute);
    assert_eq!(r.entries()[3].reason(), RemovalReason::RemovedWithElement);
    for entry in r.entries() {
        assert_eq!(entry.sample_locations(), &[2]);
    }
    let err = strict(input, DefaultPolicy::new(), &Limits::ceiling()).expect_err("rejects");
    assert_eq!(
        err.to_string(),
        "strict mode: the policy removes 4 item(s) in 4 group(s)"
    );
    assert_eq!(err, HtmlSanitizeError::DisallowedContent { report: r });
    // The HTML wrappers the parser adds around a fragment stay unreported.
    assert!(report("<p>x</p>").is_empty());
    assert!(report("<html><body><p>x</p></body></html>").is_empty());
}

#[test]
fn unstable_comment_is_reported() {
    let r = report("<p>a<!-- b <!-- c -->d</p>");
    assert_eq!(
        shape(&r),
        vec![(
            RemovalKind::Comment,
            "unstable",
            RemovalReason::UnstableComment,
            1
        )]
    );
}

/// outcome is a typed budget error or a complete strict result)
#[test]
fn lowered_budgets_never_yield_a_partial_strict_outcome() {
    let input = "<script>1</script><foo>b</foo><p onclick=\"x()\" foo=\"1\" \
                 style=\"position:fixed\">t<!--[if IE]>x<![endif]--></p>"
        .repeat(20);
    let policy = DefaultPolicy::new().with_sanitize_style();
    let full = strict(&input, policy, &Limits::ceiling()).expect_err("strict rejects");
    for memory in [true, false] {
        let (mut refused, mut complete) = (0, 0);
        let mut value = 64_u64;
        while value < 50_000_000 {
            let limits = if memory {
                Limits::ceiling().with_working_bytes(value)
            } else {
                Limits::ceiling().with_work_units(value)
            }
            .expect("limits");
            let err = strict(&input, policy, &limits).expect_err("strict or budget rejection");
            match &err {
                HtmlSanitizeError::MemoryLimit { limit } if memory => {
                    assert_eq!(*limit, value);
                    assert_eq!(
                        err.to_string(),
                        format!("sanitising exceeds the working memory limit of {value} bytes")
                    );
                    refused += 1;
                }
                HtmlSanitizeError::WorkLimit { limit } if !memory => {
                    assert_eq!(*limit, value);
                    assert_eq!(
                        err.to_string(),
                        format!("sanitising exceeds the work limit of {value} units")
                    );
                    refused += 1;
                }
                HtmlSanitizeError::DisallowedContent { .. } => {
                    assert_eq!(err, full, "memory={memory}, budget={value}");
                    complete += 1;
                }
                other => panic!("memory={memory}, budget={value}: {other:?}"),
            }
            value = value * 3 / 2;
        }
        assert!(
            complete > 0 && refused > 0,
            "memory={memory}, refused={refused}, complete={complete}"
        );
    }
}

#[test]
fn unwrapped_elements_report_every_discarded_attribute() {
    let r = report("<foo onclick=\"SECRET\" title=\"SECRET\"><b>kept</b></foo>");
    assert_eq!(
        r.entries().iter().map(|e| e.name()).collect::<Vec<_>>(),
        vec!["foo", "onclick", "title"]
    );
    assert_eq!(r.entries()[2].reason(), RemovalReason::RemovedWithElement);
    assert!(!format!("{r:?}").contains("SECRET"));
    for entry in r.entries() {
        assert_eq!(entry.sample_locations(), &[1]);
    }
}

#[test]
fn html_root_attributes_are_reported() {
    let input =
        "<html onload=\"x()\" foo=\"1\" title=\"SECRET\"><p onclick=\"y()\">text</p></html>";
    let r = report(input);
    assert_eq!(
        shape(&r),
        vec![
            (
                RemovalKind::Attribute,
                "onload",
                RemovalReason::EventHandler,
                1
            ),
            (
                RemovalKind::Attribute,
                "foo",
                RemovalReason::ForbiddenAttribute,
                1
            ),
            (
                RemovalKind::Attribute,
                "title",
                RemovalReason::RemovedWithElement,
                1
            ),
            (
                RemovalKind::Attribute,
                "onclick",
                RemovalReason::EventHandler,
                1
            ),
        ]
    );
    assert_eq!(r.entries()[0].sample_locations(), &[1]);
    assert_eq!(r.entries()[3].sample_locations(), &[2]);
    let err = strict(input, DefaultPolicy::new(), &Limits::ceiling()).expect_err("strict rejects");
    assert_eq!(
        err.to_string(),
        "strict mode: the policy removes 4 item(s) in 4 group(s)"
    );
    assert_eq!(err, HtmlSanitizeError::DisallowedContent { report: r });
    let clean = sanitize(
        input,
        &SanitizePolicy::Default(DefaultPolicy::new()),
        &Limits::ceiling(),
        SanitizeMode::Clean,
    )
    .expect("clean");
    assert_eq!(clean.output().as_str(), "<p>text</p>");
}
