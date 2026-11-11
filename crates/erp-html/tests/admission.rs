//! Admission tests of the bounded entry points (step 4): input length,
//! NUL bytes and UTF-8 are checked before any byte reaches the parser. Error kind and the full
//! message are asserted.

// Helpers outside `#[test]` functions use expect (clippy.toml only exempts test fns).
#![allow(clippy::expect_used)]

use erp_html::{
    DefaultPolicy, HtmlSanitizeError, Limits, SanitizeMode, SanitizePolicy, sanitize,
    sanitize_bytes,
};

fn policy() -> SanitizePolicy {
    SanitizePolicy::Default(DefaultPolicy::new())
}

fn run(input: &str, limits: &Limits) -> Result<String, HtmlSanitizeError> {
    sanitize(input, &policy(), limits, SanitizeMode::Clean)
        .map(|r| r.into_output().as_str().to_owned())
}

fn run_bytes(input: &[u8], limits: &Limits) -> Result<String, HtmlSanitizeError> {
    sanitize_bytes(input, &policy(), limits, SanitizeMode::Clean)
        .map(|r| r.into_output().as_str().to_owned())
}

fn limit_64() -> Limits {
    Limits::ceiling()
        .with_input_bytes(64)
        .expect("64 is below the ceiling")
}

fn too_large(len: u64, limit: u64) -> HtmlSanitizeError {
    HtmlSanitizeError::InputTooLarge { len, limit }
}

/// A string of exactly `bytes` bytes made of `unit` repeated, padded with ASCII.
fn sized(unit: &str, bytes: usize) -> String {
    let whole = bytes / unit.len();
    let pad = bytes - whole * unit.len();
    format!("{}{}", unit.repeat(whole), "a".repeat(pad))
}

/// `<p>` + `bytes - 7` bytes of `unit` text + `</p>`: exactly `bytes` long and already in the form the
/// sanitiser writes, so the output is as long as the input. (Text-led input of exactly the limit
/// gets a `<p>` wrapper, the output is longer than the limit and is refused: the output must be
/// acceptable input,; see `lead_r1_tests` in the crate.)
fn wrapped(unit: &str, bytes: usize) -> String {
    format!("<p>{}</p>", sized(unit, bytes - 7))
}

#[test]
fn input_at_limit_minus_one_limit_and_plus_one_ascii() {
    let limits = limit_64();
    for len in [63usize, 64] {
        let input = wrapped("a", len);
        assert_eq!(input.len(), len);
        assert_eq!(run(&input, &limits).as_deref(), Ok(input.as_str()), "{len}");
    }
    let err = run(&"a".repeat(65), &limits).expect_err("65 bytes is over the limit of 64");
    assert_eq!(err, too_large(65, 64));
    assert_eq!(
        err.to_string(),
        "input of 65 bytes exceeds the limit of 64 bytes"
    );
}

/// Output is capped by `input_bytes`: text-led input gets the 7-byte `<p></p>` wrapper, so with a
/// limit of 64 it has 57 bytes of room; one byte more is `OutputTooLarge` with the input limit.
#[test]
fn text_led_input_output_boundary_is_input_bytes_minus_wrapper() {
    let limits = limit_64();
    let ok = "a".repeat(57);
    let out = run(&ok, &limits).expect("57 bytes of text wrap to exactly 64");
    assert_eq!(out.len(), 64);
    assert_eq!(out, format!("<p>{ok}</p>"));
    let err = run(&"a".repeat(58), &limits).expect_err("58 bytes wrap to 65");
    assert_eq!(err, HtmlSanitizeError::OutputTooLarge { limit: 64 });
    assert_eq!(
        err.to_string(),
        "output exceeds the limit of 64 bytes (output is capped by min(output_bytes, input_bytes))"
    );
}

#[test]
fn multibyte_input_is_counted_in_bytes_not_chars() {
    let limits = limit_64();
    for unit in ["\u{e9}", "\u{20ac}", "\u{1f600}"] {
        for len in [63usize, 64] {
            let input = wrapped(unit, len);
            assert_eq!(input.len(), len);
            assert!(input.chars().count() < len, "{unit:?} is multibyte");
            assert_eq!(run(&input, &limits).as_deref(), Ok(input.as_str()));
        }
        let input = sized(unit, 65);
        assert_eq!(input.len(), 65);
        let err = run(&input, &limits).expect_err("65 bytes is over the limit of 64");
        assert_eq!(err, too_large(65, 64), "{unit:?}");
        assert_eq!(
            err.to_string(),
            "input of 65 bytes exceeds the limit of 64 bytes"
        );
    }
}

#[test]
fn bytes_entry_point_has_the_same_boundary() {
    let limits = limit_64();
    for len in [63usize, 64] {
        let input = wrapped("b", len);
        assert_eq!(
            run_bytes(input.as_bytes(), &limits).as_deref(),
            Ok(input.as_str())
        );
    }
    let err = run_bytes("b".repeat(65).as_bytes(), &limits).expect_err("over the limit");
    assert_eq!(err, too_large(65, 64));
}

#[test]
fn input_over_the_default_ceiling_is_refused_before_anything_is_parsed() {
    // Ten million and one bytes of markup that would trip the depth or node limit if it were
    // parsed, and that holds a NUL: only the length check can have produced the error.
    let mut input = "<div>".repeat(2_000_001);
    input.truncate(10_000_001);
    assert_eq!(input.len(), 10_000_001);
    input.replace_range(5..6, "\0");
    let err = run(&input, &Limits::ceiling()).expect_err("over the ceiling");
    assert_eq!(err, too_large(10_000_001, 10_000_000));
    assert_eq!(
        err.to_string(),
        "input of 10000001 bytes exceeds the limit of 10000000 bytes"
    );
    let err = run_bytes(input.as_bytes(), &Limits::ceiling()).expect_err("over the ceiling");
    assert_eq!(err, too_large(10_000_001, 10_000_000));
}

#[test]
fn empty_input_is_ok_and_empty() {
    assert_eq!(run("", &Limits::ceiling()).as_deref(), Ok(""));
    assert_eq!(run_bytes(b"", &Limits::ceiling()).as_deref(), Ok(""));
}

#[test]
fn nul_byte_is_refused_with_its_offset() {
    for (input, offset) in [
        ("\0abc", 0u64),
        ("ab\0cd", 2),
        ("abc\0", 3),
        ("\u{e9}\0", 2),
        ("ab\0cd\0", 2),
    ] {
        let err = run(input, &Limits::ceiling()).expect_err("NUL");
        assert_eq!(err, HtmlSanitizeError::NulByte { offset }, "{input:?}");
        assert_eq!(
            err.to_string(),
            format!("input contains a NUL byte at offset {offset}")
        );
        let err = run_bytes(input.as_bytes(), &Limits::ceiling()).expect_err("NUL");
        assert_eq!(err, HtmlSanitizeError::NulByte { offset });
    }
}

#[test]
fn nul_in_input_just_over_the_limit_reports_the_length_first() {
    let mut input = "a".repeat(65);
    input.replace_range(3..4, "\0");
    let err = run(&input, &limit_64()).expect_err("over the limit");
    assert_eq!(err, too_large(65, 64));
}

#[test]
fn invalid_utf8_is_refused_with_the_valid_prefix_length() {
    for (bytes, valid_up_to) in [
        (&b"ab\xffcd"[..], 2usize),
        (&b"\xff"[..], 0),
        (&b"ab\xc3"[..], 2),
        (&b"ok\xe2\x82"[..], 2),
    ] {
        let source = std::str::from_utf8(bytes).expect_err("not UTF-8");
        assert_eq!(source.valid_up_to(), valid_up_to);
        let err = run_bytes(bytes, &Limits::ceiling()).expect_err("invalid UTF-8");
        assert_eq!(err, HtmlSanitizeError::InvalidUtf8 { source });
        assert_eq!(
            err.to_string(),
            format!("input is not valid UTF-8 (valid up to byte {valid_up_to})")
        );
    }
}

#[test]
fn byte_checks_run_in_order_length_then_utf8_then_nul() {
    let err = run_bytes(b"\0\xff", &Limits::ceiling()).expect_err("invalid UTF-8 comes first");
    assert!(
        matches!(err, HtmlSanitizeError::InvalidUtf8 { .. }),
        "{err:?}"
    );
    let mut bytes = vec![b'a'; 65];
    bytes[10] = 0xff;
    let err = run_bytes(&bytes, &limit_64()).expect_err("length comes first");
    assert_eq!(err, too_large(65, 64));
}

/// The recommended input cap is the formula (ENGINE.D amendment) applied to the committed
/// envelope log; `tests/scripts/test_html_limits_log.py` recomputes it. At the cap, markup in the
/// form the sanitiser writes is accepted; one byte more is refused before the parser runs.
#[test]
fn recommended_input_cap_is_accepted_and_one_byte_more_is_refused() {
    let limits = Limits::recommended();
    let cap = limits.input_bytes();
    assert!(
        cap < Limits::ceiling().input_bytes(),
        "the envelope lowers the input cap"
    );
    let at_cap = wrapped("a", usize::try_from(cap).expect("the cap fits in usize"));
    assert_eq!(at_cap.len() as u64, cap);
    assert_eq!(run(&at_cap, &limits).as_deref(), Ok(at_cap.as_str()));
    let over = "a".repeat(usize::try_from(cap + 1).expect("the cap fits in usize"));
    let err = run(&over, &limits).expect_err("one byte over the recommended cap");
    assert_eq!(err, too_large(cap + 1, cap));
    assert_eq!(
        err.to_string(),
        format!(
            "input of {} bytes exceeds the limit of {cap} bytes",
            cap + 1
        )
    );
    let err = run_bytes(over.as_bytes(), &limits).expect_err("one byte over, bytes entry");
    assert_eq!(err, too_large(cap + 1, cap));
}
