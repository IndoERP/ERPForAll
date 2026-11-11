//! DDL of typed exclusion constraints, their period checks and `WITHOUT OVERLAPS` keys
//! (design §5; sub-slice).
//!
//! The definitions are canonical: the same declaration always produces the same text, which schema
//! sync stores in the constraint's comment and compares on the next sync (a changed declaration is
//! dropped and re-created).

use erp_fields::{FieldType, RangeBounds};
use erp_registry::{ExclusionDecl, ModelSpec, PgExtension, TemporalKeyDecl};

use crate::{literal, quote};

/// The range constructor of a period over two fields of `ty` (`daterange` / `tsrange`).
fn range_constructor(ty: &FieldType) -> Option<&'static str> {
    match ty {
        FieldType::Date => Some("daterange"),
        FieldType::Datetime => Some("tsrange"),
        _ => None,
    }
}

/// `<constructor>("from", "to", '<bounds>')`: the period of an exclusion as a range expression.
///
/// # Errors
///
/// The period's field types are checked when the registry is built; a field that is missing or
/// not a date/datetime here is reported with its name (a registry bug, never input).
pub fn period_expression(model: &ModelSpec, decl: &ExclusionDecl) -> Result<String, String> {
    let (from, to) = decl.period;
    let field = model
        .field(from)
        .ok_or_else(|| format!("period field '{from}' does not exist"))?;
    let constructor = range_constructor(&field.ty)
        .ok_or_else(|| format!("period field '{from}' is not a date or datetime field"))?;
    Ok(format!(
        "{constructor}({}, {}, {})",
        quote(from),
        quote(to),
        literal(decl.bounds.as_str())
    ))
}

/// [`period_expression`] as the compiled period the conflict diagnostic accepts.
///
/// # Errors
///
/// See [`period_expression`].
pub fn compiled_period(
    model: &ModelSpec,
    decl: &ExclusionDecl,
) -> Result<crate::conflict::CompiledPeriod, String> {
    period_expression(model, decl).map(crate::conflict::CompiledPeriod::new)
}

/// The range column of a `WITHOUT OVERLAPS` key as the compiled period the conflict diagnostic
/// accepts.
pub fn compiled_range(decl: &TemporalKeyDecl) -> crate::conflict::CompiledPeriod {
    crate::conflict::CompiledPeriod::new(quote(decl.range))
}

/// The canonical definition of a typed exclusion: `EXCLUDE USING gist ("k" WITH =, …,
/// <period> WITH &&) [WHERE (<filter>)]`, `filter` being the compiled constant filter.
///
/// # Errors
///
/// See [`period_expression`].
pub fn exclusion_definition(
    model: &ModelSpec,
    decl: &ExclusionDecl,
    filter: Option<&str>,
) -> Result<String, String> {
    let mut parts: Vec<String> = decl
        .equal
        .iter()
        .map(|f| format!("{} WITH =", quote(f)))
        .collect();
    parts.push(format!("{} WITH &&", period_expression(model, decl)?));
    let mut sql = format!("EXCLUDE USING gist ({})", parts.join(", "));
    if let Some(filter) = filter {
        sql.push_str(&format!(" WHERE ({filter})"));
    }
    Ok(sql)
}

/// The canonical definition of a `WITHOUT OVERLAPS` key: `UNIQUE ("k", …, "range" WITHOUT
/// OVERLAPS)`.
pub fn temporal_key_definition(decl: &TemporalKeyDecl) -> String {
    let mut parts: Vec<String> = decl.fields.iter().map(|f| quote(f)).collect();
    parts.push(format!("{} WITHOUT OVERLAPS", quote(decl.range)));
    format!("UNIQUE ({})", parts.join(", "))
}

/// The name of the `CHECK` that refuses empty or inverted periods of the exclusion `full_name`
/// (`<full name>_period_check`, shortened to 63 bytes).
pub fn period_check_name(full_name: &str) -> String {
    erp_registry::period_check_name(full_name)
}

/// The canonical definition of a period check: `CHECK ("from" IS NULL OR "to" IS NULL OR "from" <
/// "to")` (`<=` for `[]`, where a period of one instant holds that instant). A NULL end is
/// unbounded, so it never makes a period empty.
pub fn period_check_definition(decl: &ExclusionDecl) -> String {
    let (from, to) = (quote(decl.period.0), quote(decl.period.1));
    let op = match decl.bounds {
        RangeBounds::HalfOpen => "<",
        RangeBounds::Closed => "<=",
    };
    format!("CHECK ({from} IS NULL OR {to} IS NULL OR {from} {op} {to})")
}

/// Add a `CHECK` constraint as `ADD CONSTRAINT … NOT VALID`, then `VALIDATE CONSTRAINT`, then the
/// comment holding the canonical definition.
///
/// Both statements run in the schema-sync transaction, so the `ACCESS EXCLUSIVE` lock of the `ADD`
/// is held until commit and the validation scan runs under it (the weaker lock of `VALIDATE` only
/// helps when the statements are committed separately). What keeps this safe is the large-table
/// gate: schema sync refuses to build these objects on a table over the size limit (
/// adds the online path).
pub fn add_check(table: &str, name: &str, definition: &str) -> Vec<String> {
    let (t, n) = (quote(table), quote(name));
    vec![
        format!("ALTER TABLE {t} ADD CONSTRAINT {n} {definition} NOT VALID"),
        format!("ALTER TABLE {t} VALIDATE CONSTRAINT {n}"),
        format!(
            "COMMENT ON CONSTRAINT {n} ON {t} IS {}",
            literal(definition)
        ),
    ]
}

/// `CREATE EXTENSION IF NOT EXISTS <name>` (trusted extensions: the owner role may run it).
pub fn create_extension(extension: PgExtension) -> String {
    format!("CREATE EXTENSION IF NOT EXISTS {}", quote(extension.name()))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "tests")]

    use erp_registry::{AddonDescriptor, FieldDecl, ModelDecl, RegistryBuilder};

    use super::*;

    static ADDON: AddonDescriptor = AddonDescriptor {
        name: "test_exclusion_ddl",
        depends: &["base"],
        exact_only: false,
    };

    static FIELDS: [FieldDecl; 3] = [
        FieldDecl::integer("resource"),
        FieldDecl::datetime("start"),
        FieldDecl::datetime("stop"),
    ];

    #[test]
    fn definitions_are_canonical() {
        let decl: &'static ModelDecl = Box::leak(Box::new(
            ModelDecl::new("test_exclusion_ddl", "ddl.booking").fields(&FIELDS),
        ));
        let registry = RegistryBuilder::new([&ADDON])
            .linked(false)
            .with_decls([decl])
            .build()
            .unwrap();
        let m = registry.model("ddl.booking").unwrap();
        let excl = ExclusionDecl::new(("start", "stop"), RangeBounds::HalfOpen, "taken")
            .equal(&["resource"]);
        assert_eq!(
            exclusion_definition(m, &excl, Some("\"resource\" <> 0")).unwrap(),
            "EXCLUDE USING gist (\"resource\" WITH =, tsrange(\"start\", \"stop\", '[)') WITH &&) \
             WHERE (\"resource\" <> 0)"
        );
        assert_eq!(
            period_check_definition(&excl),
            "CHECK (\"start\" IS NULL OR \"stop\" IS NULL OR \"start\" < \"stop\")"
        );
        let closed = ExclusionDecl::new(("start", "stop"), RangeBounds::Closed, "taken");
        assert_eq!(
            exclusion_definition(m, &closed, None).unwrap(),
            "EXCLUDE USING gist (tsrange(\"start\", \"stop\", '[]') WITH &&)"
        );
        assert!(period_check_definition(&closed).ends_with("\"start\" <= \"stop\")"));
        let key = TemporalKeyDecl::new("validity", "overlap").fields(&["product_id"]);
        assert_eq!(
            temporal_key_definition(&key),
            "UNIQUE (\"product_id\", \"validity\" WITHOUT OVERLAPS)"
        );
        assert_eq!(
            period_check_name("scale_booking__no_double_booking"),
            "scale_booking__no_double_booking_period_check"
        );
        assert_eq!(period_check_name(&"x".repeat(63)).len(), 63);
        assert_eq!(
            create_extension(PgExtension::BtreeGist),
            "CREATE EXTENSION IF NOT EXISTS \"btree_gist\""
        );
    }
}
