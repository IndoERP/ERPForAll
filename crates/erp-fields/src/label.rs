//! Default field labels.

pub fn default_label(name: &str) -> String {
    let base = name
        .strip_suffix("_ids")
        .or_else(|| name.strip_suffix("_id"))
        .unwrap_or(name);
    let mut out = String::with_capacity(base.len());
    let mut prev_cased = false;
    for c in base.chars() {
        let c = if c == '_' { ' ' } else { c };
        if c.is_alphabetic() {
            if prev_cased {
                out.extend(c.to_lowercase());
            } else {
                out.extend(c.to_uppercase());
            }
            prev_cased = true;
        } else {
            out.push(c);
            prev_cased = false;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spec_field_001_default_labels() {
        assert_eq!(default_label("customer_ref"), "Customer Ref");
        assert_eq!(default_label("partner_id"), "Partner");
        assert_eq!(default_label("tag_ids"), "Tag");
        assert_eq!(default_label("iban2code"), "Iban2Code");
        assert_eq!(default_label("name"), "Name");
        assert_eq!(default_label("display_name"), "Display Name");
    }
}
