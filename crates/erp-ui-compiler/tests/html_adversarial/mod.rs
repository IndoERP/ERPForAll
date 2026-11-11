//! Deterministic adversarial HTML corpus for the bounded-parser envelope. Every
//! entry is a pure function of its size parameter: no randomness, no clock. The file's sha256 is
//! written into the envelope log by `scripts/html-budget-envelope.sh`.

/// One corpus entry: a stable name and the input chunks fed to the parser.
pub struct Entry {
    pub name: String,
    pub chunks: Vec<String>,
}

fn one(name: &str, html: String) -> Entry {
    Entry {
        name: name.to_owned(),
        chunks: vec![html],
    }
}

/// Split `s` into chunks of at most `n` bytes on char boundaries.
fn chunked(name: &str, s: &str, n: usize) -> Entry {
    let mut chunks = Vec::new();
    let mut start = 0;
    while start < s.len() {
        let mut end = (start + n).min(s.len());
        while !s.is_char_boundary(end) {
            end += 1;
        }
        chunks.push(s[start..end].to_owned());
        start = end;
    }
    Entry {
        name: name.to_owned(),
        chunks,
    }
}

pub fn empty() -> Vec<Entry> {
    vec![one("empty", String::new())]
}

pub fn attribute_floods() -> Vec<Entry> {
    let mut v = Vec::new();
    for n in [1_000usize, 20_000] {
        let attrs: Vec<String> = (0..n).map(|i| format!("a{i}=1")).collect();
        v.push(one(
            &format!("attr_flood_distinct_{n}"),
            format!("<p {}>", attrs.join(" ")),
        ));
        v.push(one(
            &format!("attr_flood_duplicate_{n}"),
            format!("<p {}>", "a=1 ".repeat(n)),
        ));
    }
    v.push(chunked(
        "attr_value_unclosed_256k",
        &format!("<p a=\"{}", "x".repeat(256 * 1024)),
        1024,
    ));
    v
}

pub fn nesting() -> Vec<Entry> {
    let mut v = Vec::new();
    for d in [255usize, 256, 10_000] {
        v.push(one(&format!("nest_div_{d}"), "<div>".repeat(d)));
    }
    v
}

pub fn char_ref_bombs() -> Vec<Entry> {
    vec![
        one("charref_named_50k", "&amp;".repeat(50_000)),
        one("charref_numeric_50k", "&#x1F600;".repeat(50_000)),
        one("charref_unterminated_named_50k", "&notit".repeat(50_000)),
        one("charref_long_digits", format!("&#{}", "9".repeat(100_000))),
    ]
}

pub fn unclosed() -> Vec<Entry> {
    vec![
        one(
            "unclosed_comment_256k",
            format!("<!--{}", "x".repeat(256 * 1024)),
        ),
        one(
            "unclosed_doctype_256k",
            format!("<!DOCTYPE {}", "x".repeat(256 * 1024)),
        ),
        one(
            "unclosed_script_256k",
            format!("<script>{}", "<!--<script>".repeat(20_000)),
        ),
        one("unclosed_formatting_b_10k", "<b>".repeat(10_000)),
        one(
            "unclosed_tag_name_256k",
            format!("<{}", "a".repeat(256 * 1024)),
        ),
        one("table_text_20k", "<table>x<tr>y<td>z".repeat(20_000)),
        one("template_chain_5k", "<template>".repeat(5_000)),
    ]
}

pub fn multibyte() -> Vec<Entry> {
    vec![one(
        "multibyte_text_100k",
        format!("<p>{}</p>", "é漢😀".repeat(100_000)),
    )]
}

pub fn all() -> Vec<(&'static str, Vec<Entry>)> {
    vec![
        ("empty", empty()),
        ("attribute_floods", attribute_floods()),
        ("nesting", nesting()),
        ("char_ref_bombs", char_ref_bombs()),
        ("unclosed", unclosed()),
        ("multibyte", multibyte()),
    ]
}
