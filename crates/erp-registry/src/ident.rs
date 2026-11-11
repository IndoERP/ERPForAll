//! PostgreSQL identifier rules shared by the registry and the SQL builder.

/// PostgreSQL's identifier length limit in bytes.
pub const MAX_IDENTIFIER_LEN: usize = 63;

/// CRC-32 (IEEE, as Python's `zlib.crc32`).
pub fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &byte in data {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
        }
    }
    !crc
}

pub fn shorten_identifier(name: &str) -> String {
    if name.len() <= MAX_IDENTIFIER_LEN {
        return name.to_owned();
    }
    let mut cut = 54;
    while !name.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}_{:08x}", &name[..cut], crc32(name.as_bytes()))
}

/// The conventional name of a field index, `<table>__<field>_index`, shortened.
pub fn index_name(table: &str, field: &str) -> String {
    shorten_identifier(&format!("{table}__{field}_index"))
}

/// The name of a relation table's `(column2, column1)` index (PostgreSQL's own convention).
pub fn relation_index_name(table: &str, column2: &str, column1: &str) -> String {
    shorten_identifier(&format!("{table}_{column2}_{column1}_idx"))
}

/// PostgreSQL's auto-generated object name truncation (`makeObjectName(name1, name2, label)`):
/// `<name1>_<name2>_<label>` or `<name1>_<label>` truncated at 63 bytes.
pub fn pg_make_object_name(name1: &str, name2: Option<&str>, label: &str) -> String {
    let suffix = match name2 {
        Some(n2) => format!("_{n2}_{label}"),
        None => format!("_{label}"),
    };
    if name1.len() + suffix.len() <= MAX_IDENTIFIER_LEN {
        return format!("{name1}{suffix}");
    }
    let max_n1 = MAX_IDENTIFIER_LEN.saturating_sub(suffix.len());
    let mut cut = max_n1;
    while cut > 0 && !name1.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}{suffix}", &name1[..cut])
}

pub fn is_valid_pg_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$')
}

/// Quote an identifier: `name` → `"name"` (embedded quotes doubled).
///
/// Identifiers come from the registry, which rejects NUL bytes; PostgreSQL would reject them too.
pub fn quote(ident: &str) -> String {
    debug_assert!(!ident.contains('\0'), "NUL byte in identifier {ident:?}");
    format!("\"{}\"", ident.replace('"', "\"\""))
}

/// Quote an identifier into an `Arc<str>`.
pub fn quote_arc(ident: &str) -> std::sync::Arc<str> {
    std::sync::Arc::from(quote(ident))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc32_matches_zlib() {
        assert_eq!(crc32(b""), 0);
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
    }

    #[test]
    fn spec_crud_062_identifier_shortening() {
        let long = "x".repeat(70);
        let short = shorten_identifier(&long);
        assert_eq!(short.len(), 63);
        assert_eq!(&short[..54], &long[..54]);
        assert_eq!(&short[54..55], "_");
        assert_eq!(&short[55..], format!("{:08x}", crc32(long.as_bytes())));
        assert_eq!(shorten_identifier("short_name"), "short_name");
        assert_eq!(shorten_identifier(&"y".repeat(63)), "y".repeat(63));
    }

    #[test]
    fn pg_names() {
        assert!(is_valid_pg_name("x_ok"));
        assert!(is_valid_pg_name("Test_Orm$1"));
        assert!(!is_valid_pg_name("bad-name"));
        assert!(!is_valid_pg_name("1abc"));
        assert!(!is_valid_pg_name(""));
    }

    #[test]
    fn pg_make_object_name_truncation() {
        assert_eq!(pg_make_object_name("stock", None, "pkey"), "stock_pkey");
        assert_eq!(
            pg_make_object_name("stock", Some("id"), "seq"),
            "stock_id_seq"
        );
        let long_table = "t".repeat(60);
        let pkey = pg_make_object_name(&long_table, None, "pkey");
        assert_eq!(pkey.len(), 63);
        assert_eq!(pkey, format!("{}_pkey", "t".repeat(58)));

        let seq = pg_make_object_name(&long_table, Some("id"), "seq");
        assert_eq!(seq.len(), 63);
        assert_eq!(seq, format!("{}_id_seq", "t".repeat(56)));
    }

    #[test]
    fn quoting() {
        assert_eq!(quote("res_partner"), "\"res_partner\"");
        assert_eq!(quote("a\"b"), "\"a\"\"b\"");
        assert_eq!(&*quote_arc("res_partner"), "\"res_partner\"");
    }
}
