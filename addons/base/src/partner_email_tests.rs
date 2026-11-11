//! Private parser contracts. Model and RPC acceptance lives in base_partner_labels tests.
#![allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]

use std::sync::Arc;

use erp_sdk::ErrorKind;

use super::{format_email, parse_contact};

#[test]
fn parses_named_angle_quoted_and_trailing_mailboxes() {
    for (source, expected_name, expected_email) in [
        (
            "Ada Lovelace <ADA@EXAMPLE.ORG>",
            "Ada Lovelace",
            "ada@example.org",
        ),
        (
            "  \"Grace Hopper\" <Grace@NAVY.EXAMPLE>  ",
            "Grace Hopper",
            "grace@navy.example",
        ),
        (
            "Linus linus+Work@Example.Org",
            "Linus",
            "linus+work@example.org",
        ),
        (
            " Pat O'Hara  <!@historicalsociety.example>",
            "Pat O'Hara",
            "!@historicalsociety.example",
        ),
    ] {
        let (name, email) = parse_contact(source).unwrap();
        assert_eq!(name, expected_name, "input {source:?}");
        assert_eq!(email.as_deref(), Some(expected_email), "input {source:?}");
    }
}

#[test]
fn lone_mailbox_is_the_normalized_name_too() {
    let (name, email) = parse_contact("RYU+Giga-Sushi@Aizubange.Fukushima.JP").unwrap();
    assert_eq!(name, "ryu+giga-sushi@aizubange.fukushima.jp");
    assert_eq!(email.as_deref(), Some(name.as_str()));
}

#[test]
fn no_mailbox_keeps_contact_text_without_inventing_an_address() {
    for source in ["Just a contact", "Ada example.org", "", "notanemail"] {
        let (name, email) = parse_contact(source).unwrap();
        assert_eq!(name, source);
        assert_eq!(email, None);
    }
}

#[test]
fn name_create_uses_the_first_valid_address_and_its_name() {
    let (name, email) =
        parse_contact("Balázs <FIRST@EXAMPLE.ORG>, Second Person <second@example.org>").unwrap();
    assert_eq!(name, "Balázs");
    assert_eq!(email.as_deref(), Some("first@example.org"));
}

#[test]
fn quoted_display_name_mailbox_text_is_not_an_address_candidate() {
    let (name, email) = parse_contact("\"a@b.co\" <c@d.co>").unwrap();
    assert_eq!(name, "a@b.co");
    assert_eq!(email.as_deref(), Some("c@d.co"));
    assert_eq!(
        format_email(Some("Recipient"), Some("\"a@b.co\" <c@d.co>"))
            .unwrap()
            .as_deref(),
        Some("\"Recipient\" <c@d.co>")
    );
}

#[test]
fn unquoted_display_name_mailbox_text_does_not_override_angle_recipient() {
    let (name, email) = parse_contact("a@b.co <c@d.co>").unwrap();
    assert_eq!(name, "a@b.co");
    assert_eq!(email.as_deref(), Some("c@d.co"));
}

#[test]
fn angle_precedence_still_charges_the_complete_unquoted_input() {
    let display = vec!["a@b.co"; 256].join(" ");
    let source = format!("{display} <c@d.co>");
    let error = parse_contact(&source).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Validation);
    assert_eq!(
        error.message(),
        "Contact email input exceeds the 256-address limit"
    );
}

#[test]
fn named_email_formats_normalized_embedded_addresses() {
    for (email, expected) in [
        (
            "Sender <FIRST@EXAMPLE.ORG>",
            "\"Balázs\" <first@example.org>",
        ),
        (
            "\"Someone Else\" <first@example.org>",
            "\"Balázs\" <first@example.org>",
        ),
        (
            "FIRST@EXAMPLE.ORG, second@example.org",
            "\"Balázs\" <first@example.org,second@example.org>",
        ),
        (
            "notanemail, Second <SECOND@EXAMPLE.ORG>",
            "\"Balázs\" <second@example.org>",
        ),
    ] {
        let actual = format_email(Some("Balázs"), Some(email)).unwrap();
        assert_eq!(actual.as_deref(), Some(expected), "email {email:?}");
    }
}

#[test]
fn invalid_only_email_keeps_the_historical_raw_fallback_format() {
    assert_eq!(
        format_email(Some("Balázs"), Some("notanemail")).unwrap(),
        Some(Arc::from("\"Balázs\" <@notanemail>"))
    );
    assert_eq!(
        format_email(Some("Balázs"), Some(" ")).unwrap(),
        Some(Arc::from("\"Balázs\" <@ >"))
    );
}

#[test]
fn absent_or_empty_email_has_no_formatted_value() {
    for email in [None, Some("")] {
        assert_eq!(format_email(Some("A Name"), email).unwrap(), None);
        assert_eq!(format_email(None, email).unwrap(), None);
    }
}

#[test]
fn nameless_email_is_bare_and_never_uses_false_as_the_name() {
    for name in [None, Some("")] {
        assert_eq!(
            format_email(name, Some("Ship <SHIP@ACME.EXAMPLE>")).unwrap(),
            Some(Arc::from("ship@acme.example"))
        );
        assert_eq!(
            format_email(name, Some("FIRST@EXAMPLE.ORG,second@example.org")).unwrap(),
            Some(Arc::from("first@example.org,second@example.org"))
        );
    }
}

#[test]
fn formatted_name_escapes_quotes_and_backslashes_and_keeps_unicode() {
    let result = format_email(Some("Zoë \"A\" \\ Lab"), Some("zoe@example.org")).unwrap();
    assert_eq!(
        result.as_deref(),
        Some("\"Zoë \\\"A\\\" \\\\ Lab\" <zoe@example.org>")
    );
}

#[test]
fn mailbox_input_accepts_crlf_between_separate_addresses() {
    let result = format_email(
        Some("Team"),
        Some("first@example.org,\r\n Second <SECOND@EXAMPLE.ORG>"),
    )
    .unwrap();
    assert_eq!(
        result.as_deref(),
        Some("\"Team\" <first@example.org,second@example.org>")
    );
}

#[test]
fn parsing_input_bytes_are_bounded_before_processing() {
    let exact = "x".repeat(65_536);
    let (name, email) = parse_contact(&exact).unwrap();
    assert_eq!(name, exact);
    assert_eq!(email, None);

    let over = "x".repeat(65_537);
    let error = parse_contact(&over).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Validation);
    assert_eq!(
        error.message(),
        "Contact email input exceeds the 65536-byte limit"
    );
    let error = format_email(Some("Name"), Some(&over)).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Validation);
    assert_eq!(
        error.message(),
        "Contact email input exceeds the 65536-byte limit"
    );
}

#[test]
fn candidate_count_is_bounded_without_truncating_accepted_addresses() {
    let exact = vec!["a@b.co"; 256].join(",");
    let expected = format!("\"Name\" <{exact}>");
    assert_eq!(
        format_email(Some("Name"), Some(&exact)).unwrap().as_deref(),
        Some(expected.as_str())
    );
    let over = vec!["a@b.co"; 257].join(",");
    let error = format_email(Some("Name"), Some(&over)).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Validation);
    assert_eq!(
        error.message(),
        "Contact email input exceeds the 256-address limit"
    );
    let error = parse_contact(&over).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Validation);
    assert_eq!(
        error.message(),
        "Contact email input exceeds the 256-address limit"
    );
}

#[test]
fn formatted_output_limit_counts_escaping_and_framing_bytes() {
    // Framing a six-byte mailbox adds eleven bytes around the display name.
    let name = "x".repeat(1_048_576 - 11);
    let output = format_email(Some(&name), Some("a@b.co")).unwrap().unwrap();
    assert_eq!(output.len(), 1_048_576);

    let name = "x".repeat(1_048_576 - 10);
    let error = format_email(Some(&name), Some("a@b.co")).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Validation);
    assert_eq!(
        error.message(),
        "Contact label output exceeds the 1048576-byte limit"
    );

    let escaped_name = "\"".repeat(524_283);
    let error = format_email(Some(&escaped_name), Some("a@b.co")).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Validation);
    assert_eq!(
        error.message(),
        "Contact label output exceeds the 1048576-byte limit"
    );
}
