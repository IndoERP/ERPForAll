//! URL scheme allow-list for link attributes (plan decision D-1).
//!
//! The reference environment blocks a short deny-list of schemes (`javascript:`, `vbscript:`) and
//! lets everything else through. ERPForAll inverts that: only the schemes below pass, so a scheme
//! nobody thought of is refused rather than kept (a divergence recorded in the spec's section 7).

/// Schemes kept besides relative references: `http`, `https`, `mailto`, `tel`, `cid`, plus `data:`
/// restricted to the image types in [`ALLOWED_DATA_IMAGE_TYPES`].
const ALLOWED_SCHEMES: &[&str] = &["cid", "http", "https", "mailto", "tel"];

/// Media types allowed after `data:`; every other `data:` URL (HTML, SVG, script) is refused.
const ALLOWED_DATA_IMAGE_TYPES: &[&str] = &["image/gif", "image/jpeg", "image/png", "image/webp"];

/// Whether `url` may stay in a link attribute (`href`, `src`, `action` and the others in
/// [`crate::tables::LINK_ATTRS`]).
///
/// The scheme is read after removing every ASCII whitespace and control character (browsers drop
/// tab, newline and carriage return inside a URL and ignore leading control characters, so
/// `java\0script:` and `jav\nascript:` must be seen as `javascript:`) and is compared ignoring
/// ASCII case. A value with no scheme (a path, `//host`, `#fragment`, `?query`, `@`, empty) is a
/// relative reference and passes. A value whose scheme is not in the allow-list is refused.
#[must_use]
pub(crate) fn is_allowed_url(url: &str) -> bool {
    // One forward pass over the filtered characters, stopping at the first ':' '/' '?' or '#'.
    // Only the scheme is kept, in a small fixed buffer: no copy of the URL is made and a long
    // value costs one pass, however many times a caller asks.
    let mut chars = url
        .chars()
        .filter(|c| !(c.is_ascii_whitespace() || c.is_ascii_control()));
    let mut scheme = [0u8; SCHEME_BUF];
    let mut len = 0;
    let mut fits = true;
    loop {
        match chars.next() {
            // No scheme at all, or a ':' after '/', '?' or '#' that belongs to a relative
            // reference (`/a:b`, `?x=a:b`, `#a:b`).
            None | Some('/' | '?' | '#') => return true,
            Some(':') => break,
            Some(c) => {
                if fits && c.is_ascii() && len < SCHEME_BUF {
                    scheme[len] = c.to_ascii_lowercase() as u8;
                    len += 1;
                } else {
                    // Longer than any allowed scheme, or not ASCII: refused unless a separator
                    // shows it was not a scheme (handled above), so keep scanning.
                    fits = false;
                }
            }
        }
    }
    if !fits {
        return false;
    }
    let scheme = &scheme[..len];
    if scheme == b"data" {
        return is_allowed_data_url(chars);
    }
    ALLOWED_SCHEMES.iter().any(|s| s.as_bytes() == scheme)
}

/// Longest allowed scheme (`https`) rounded up; a longer one cannot match.
const SCHEME_BUF: usize = 8;

/// Longest allowed media type (`image/jpeg`, `image/webp`) rounded up.
const MEDIA_BUF: usize = 16;

/// `rest` yields what follows `data:`: the media type must be an allowed image type, optionally
/// followed by parameters (`;base64`, `;charset=...`) and then the comma and the data.
fn is_allowed_data_url(mut rest: impl Iterator<Item = char>) -> bool {
    let mut media = [0u8; MEDIA_BUF];
    let mut len = 0;
    let mut fits = true;
    let mut comma = false;
    while let Some(c) = rest.next() {
        match c {
            ',' => {
                comma = true;
                break;
            }
            ';' => {
                comma = rest.any(|c| c == ',');
                break;
            }
            _ if fits && c.is_ascii() && len < MEDIA_BUF => {
                media[len] = c.to_ascii_lowercase() as u8;
                len += 1;
            }
            _ => fits = false,
        }
    }
    comma
        && fits
        && ALLOWED_DATA_IMAGE_TYPES
            .iter()
            .any(|t| t.as_bytes() == &media[..len])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allowed_urls_pass() {
        for url in [
            "",
            "@",
            "#target",
            "./link",
            "../up",
            "relative/path",
            "/abs/path",
            "//example.com/x",
            "?q=1",
            "/a:b",
            "?x=a:b",
            "#a:b",
            "http://example.com",
            "HTTPS://example.com",
            "https://example.com",
            "mailto:a@example.com",
            "tel:+62123",
            "cid:image001",
            "data:image/png;base64,AAAA",
            "data:image/gif;base64,AAAA",
            "data:image/jpeg;base64,AAAA",
            "data:image/webp;base64,AAAA",
            "DATA:IMAGE/PNG;base64,AAAA",
            " https://example.com ",
            "ht\ttp://example.com",
        ] {
            assert!(is_allowed_url(url), "{url:?} should pass");
        }
    }

    #[test]
    fn scheme_detection_stops_at_the_first_separator() {
        for url in [
            "a/b:c",
            "a?b:c",
            "a#b:c",
            "aaaaaaaaaaaaaaaaaaaa/x:y",
            "ja\tva/script:x",
            "data/image:x",
            "\u{0}/a:b",
        ] {
            assert!(is_allowed_url(url), "{url:?} should pass");
        }
        for url in [
            "aaaaaaaaaaaaaaaaaaaa:x",
            "javascript\u{0}:x",
            "j\u{ff41}vascript:x",
            "\u{ff4a}avascript:x",
            "httpss:x",
            "htt:x",
            "data:image/png;x",
            "data:image/pngg;base64,AA",
            "data:image/png/x,AA",
            "data:text/html;image/png,AA",
            "data:image/png;base64;",
        ] {
            assert!(!is_allowed_url(url), "{url:?} should be refused");
        }
        assert!(is_allowed_url("data:image/png;base64;,x"));
        assert!(is_allowed_url("DA\nTA:image/png;\tbase64,x"));
        assert!(!is_allowed_url(&format!("{}:x", "a".repeat(100_000))));
        assert!(is_allowed_url(&"a".repeat(100_000)));
    }

    #[test]
    fn refused_urls_are_blocked() {
        for url in [
            "javascript:alert(1)",
            "JaVaScRiPt:alert(1)",
            "java\0script:alert(1)",
            "jav\nascript:alert(1)",
            "jav\tascript:alert(1)",
            "jav\rascript:alert(1)",
            "\u{1}javascript:alert(1)",
            " javascript:alert(1)",
            "vbscript:msgbox(1)",
            "data:text/html;base64,AAAA",
            "data:text/html,<script>",
            "data:image/svg+xml;base64,AAAA",
            "data:image/png",
            "data:,x",
            "data:",
            "file:///etc/passwd",
            "ftp://example.com",
            "blob:https://example.com/x",
            "x-unknown:thing",
            "javascript:",
            ":",
        ] {
            assert!(!is_allowed_url(url), "{url:?} should be refused");
        }
    }
}
