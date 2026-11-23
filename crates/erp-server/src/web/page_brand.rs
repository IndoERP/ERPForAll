//! What the installed brand addon gives the server-rendered pages: the product name (`static/brand/brand.json`)
//! for `<title>`, `<meta name="application-name">` and the error page's logo text, and whether the login pages
//! link its stylesheet (`static/brand/login.css`). Both come from the published asset snapshot, validated at
//! publication; anonymous pages never read the database for them.

use crate::assets::PublishedAssets;
use crate::routes;

/// The product name when no installed addon names one.
pub(super) const DEFAULT_PRODUCT_NAME: &str = "ERPForAll";

/// The brand of one rendered page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct PageBrand {
    name: String,
    login_stylesheet: bool,
}

impl Default for PageBrand {
    fn default() -> Self {
        Self {
            name: DEFAULT_PRODUCT_NAME.to_owned(),
            login_stylesheet: false,
        }
    }
}

impl PageBrand {
    /// The brand of published snapshot `assets`; the built-in brand without one.
    pub(super) fn of(assets: Option<&PublishedAssets>) -> Self {
        let Some(assets) = assets else {
            return Self::default();
        };
        Self {
            name: assets
                .product_name()
                .unwrap_or(DEFAULT_PRODUCT_NAME)
                .to_owned(),
            login_stylesheet: assets.has_brand_stylesheet(),
        }
    }

    /// The product name escaped for HTML text and quoted attribute values.
    pub(super) fn name_html(&self) -> String {
        escape_html(&self.name)
    }

    /// `<title>` (with `prefix · ` before the name when given) and the `application-name` meta the client reads
    /// for its logo text.
    pub(super) fn head(&self, prefix: Option<&str>) -> String {
        let name = self.name_html();
        let title = match prefix {
            Some(prefix) => format!("{} · {name}", escape_html(prefix)),
            None => name.clone(),
        };
        format!("<title>{title}</title><meta name=\"application-name\" content=\"{name}\">")
    }

    /// The brand addon's login stylesheet link, for after the client stylesheet; empty without one. An external
    /// same-origin stylesheet, so the page policy (`style-src-elem 'self'`) needs no inline allowance.
    pub(super) fn login_stylesheet_link(&self) -> String {
        if self.login_stylesheet {
            format!(
                "<link rel=\"stylesheet\" href=\"{}\">",
                routes::BRAND_LOGIN_CSS_PATH
            )
        } else {
            String::new()
        }
    }
}

/// `text` escaped for HTML text and quoted attribute values.
pub(super) fn escape_html(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&#39;"),
            _ => escaped.push(c),
        }
    }
    escaped
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_markup_and_quotes() {
        assert_eq!(
            escape_html(r#"<b>"A&B's"</b>"#),
            "&lt;b&gt;&quot;A&amp;B&#39;s&quot;&lt;/b&gt;"
        );
        assert_eq!(escape_html("Plain"), "Plain");
    }

    #[test]
    fn the_built_in_brand_names_the_product_and_links_no_stylesheet() {
        let brand = PageBrand::of(None);
        assert_eq!(brand.name, DEFAULT_PRODUCT_NAME);
        assert_eq!(
            brand.head(Some("Login successful")),
            "<title>Login successful · ERPForAll</title><meta name=\"application-name\" content=\"ERPForAll\">"
        );
        assert_eq!(brand.login_stylesheet_link(), "");
    }

    #[test]
    fn a_named_brand_is_escaped_in_title_and_meta() {
        let brand = PageBrand {
            name: "A&B <Suite>".to_owned(),
            login_stylesheet: true,
        };
        assert_eq!(
            brand.head(None),
            "<title>A&amp;B &lt;Suite&gt;</title><meta name=\"application-name\" content=\"A&amp;B &lt;Suite&gt;\">"
        );
        assert_eq!(
            brand.login_stylesheet_link(),
            "<link rel=\"stylesheet\" href=\"/erp/brand/login.css\">"
        );
    }
}
