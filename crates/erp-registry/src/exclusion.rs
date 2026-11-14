//! Typed exclusion constraints, `WITHOUT OVERLAPS` keys and PostgreSQL extensions a model needs
//! (design §5, §9; sub-slice).
//!
//! Declarations name fields, never SQL, so untrusted `models/*.toml` can use them: the ORM compiles
//! them into DDL. They are table objects ([`crate::TableObjectKind::Exclusion`],
//! [`crate::TableObjectKind::TemporalKey`]), composed by full name with the others (later
//! declaration wins), and checked against the model's fields when the registry is built
//! ([`check_model`]). The constant `filter` of an exclusion is a domain the ORM compiles when it is
//! built (the registry cannot depend on `erp-domain`).
//!
//! ```
//! use erp_registry::{ExclusionDecl, TemporalKeyDecl};
//! use erp_fields::RangeBounds;
//!
//! // no two bookings of a resource overlap, cancelled ones excepted
//! const BOOKING: ExclusionDecl = ExclusionDecl::new(
//!     ("start", "stop"),
//!     RangeBounds::HalfOpen,
//!     "This resource is already booked for an overlapping period.",
//! )
//! .equal(&["resource_id"])
//! .filter("[('state', '!=', 'cancel')]");
//! // one rate per product for any date
//! const RATE: TemporalKeyDecl =
//!     TemporalKeyDecl::new("validity", "Rates of a product may not overlap.").fields(&["product_id"]);
//! assert_eq!(BOOKING.period, ("start", "stop"));
//! assert_eq!(RATE.fields, ["product_id"]);
//! ```

use erp_fields::{Error, FieldType, RangeBounds, Result};
use rustc_hash::FxHashMap;

use crate::decl::TableObjectKind;
use crate::ident::shorten_identifier;
use crate::spec::{FieldId, FieldSpec, TableObject};

/// PostgreSQL limits an index (and so an exclusion constraint) to 32 columns (`INDEX_MAX_KEYS`).
pub const MAX_CONSTRAINT_COLUMNS: usize = 32;

/// A typed exclusion constraint over a period of two fields (`exclude(...)`, design §5.1): no two
/// rows whose [`ExclusionDecl::equal`] fields are equal may have overlapping periods. Rows the
/// [`ExclusionDecl::filter`] does not match are not checked. A NULL period end is unbounded (an
/// open-ended period overlaps everything after its start).
///
/// Compiled to `EXCLUDE USING gist (<equal> WITH =, <range>(<from>, <to>, '<bounds>') WITH &&)
/// WHERE (<filter>)` plus a `CHECK` that refuses empty or inverted periods (an empty range overlaps
/// nothing and would escape the rule).
#[derive(Debug, Clone, Copy)]
#[non_exhaustive]
pub struct ExclusionDecl {
    /// Fields compared with `=` (stored scalar, selection, boolean, date, datetime or many2one
    /// fields); empty when the rule holds for the whole table.
    pub equal: &'static [&'static str],
    /// The period's `(from, to)` fields: two stored `date` or two stored `datetime` fields.
    pub period: (&'static str, &'static str),
    /// Which ends of the period are included (`[)`: adjacent periods do not overlap).
    pub bounds: RangeBounds,
    /// A constant domain in list syntax over the model's own stored fields
    /// (`[('state', '!=', 'cancel')]`), compiled to the constraint's `WHERE`.
    pub filter: Option<&'static str>,
    /// The message users see when the rule is violated.
    pub message: &'static str,
}

impl ExclusionDecl {
    /// An exclusion over the period `(from, to)` with `bounds`, comparing no other field.
    pub const fn new(
        period: (&'static str, &'static str),
        bounds: RangeBounds,
        message: &'static str,
    ) -> Self {
        Self {
            equal: &[],
            period,
            bounds,
            filter: None,
            message,
        }
    }

    /// Set the fields compared with `=`.
    pub const fn equal(mut self, fields: &'static [&'static str]) -> Self {
        self.equal = fields;
        self
    }

    /// Set the constant domain filter.
    pub const fn filter(mut self, domain: &'static str) -> Self {
        self.filter = Some(domain);
        self
    }
}

/// A `UNIQUE (<fields>, <range> WITHOUT OVERLAPS)` key on a range field (`key(...,
/// without_overlaps = "<range field>")`, design §5.2): no two rows with equal [`fields`] have
/// overlapping ranges. PostgreSQL enforces it as a GiST exclusion constraint; it never arbitrates an
/// upsert.
///
/// [`fields`]: TemporalKeyDecl::fields
#[derive(Debug, Clone, Copy)]
#[non_exhaustive]
pub struct TemporalKeyDecl {
    /// Fields compared with `=` (as [`ExclusionDecl::equal`]).
    pub fields: &'static [&'static str],
    /// The stored `date_range` or `datetime_range` field that may not overlap.
    pub range: &'static str,
    /// The message users see when the key is violated.
    pub message: &'static str,
}

impl TemporalKeyDecl {
    /// A key on `range` alone (no two rows of the table overlap).
    pub const fn new(range: &'static str, message: &'static str) -> Self {
        Self {
            fields: &[],
            range,
            message,
        }
    }

    /// Set the fields compared with `=`.
    pub const fn fields(mut self, fields: &'static [&'static str]) -> Self {
        self.fields = fields;
        self
    }
}

/// A PostgreSQL extension a native model may state it needs (`extensions = [...]`, design §5.3),
/// for raw `Constraint` definitions that use it. Schema sync installs it in the install
/// transaction; it never parses raw SQL to guess.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[non_exhaustive]
pub enum PgExtension {
    /// `btree_gist`: `=` on scalar columns inside GiST indexes (exclusion constraints).
    BtreeGist,
    /// `pg_trgm`: trigram indexes.
    PgTrgm,
    /// `unaccent`: accent folding.
    Unaccent,
}

impl PgExtension {
    /// The extension's name in `CREATE EXTENSION`.
    pub const fn name(self) -> &'static str {
        match self {
            Self::BtreeGist => "btree_gist",
            Self::PgTrgm => "pg_trgm",
            Self::Unaccent => "unaccent",
        }
    }

    /// The extension named `name`; `None` for any other (only these three may be declared).
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "btree_gist" => Some(Self::BtreeGist),
            "pg_trgm" => Some(Self::PgTrgm),
            "unaccent" => Some(Self::Unaccent),
            _ => None,
        }
    }
}

/// The name of the `CHECK` that refuses empty or inverted periods of the exclusion `full_name`
/// (`<full name>_period_check`, shortened to 63 bytes).
pub fn period_check_name(full_name: &str) -> String {
    shorten_identifier(&format!("{full_name}_period_check"))
}

/// How messages name a table object of these kinds: `exclusion '<name>'` or `key '<name>'`.
pub fn describe(object: &TableObject) -> Option<String> {
    match object.kind {
        TableObjectKind::Exclusion(_) => Some(format!("exclusion '{}'", object.name)),
        TableObjectKind::TemporalKey(_) => Some(format!("key '{}'", object.name)),
        TableObjectKind::Constraint { .. }
        | TableObjectKind::Index { .. }
        | TableObjectKind::UniqueIndex { .. }
        | TableObjectKind::Key(_)
        | TableObjectKind::TypedIndex(_)
        | TableObjectKind::SignalTrigger(_)
        | TableObjectKind::FollowDelete(_) => None,
    }
}

/// Check the exclusions and temporal keys of model `model` against its fields (registry build).
///
/// # Errors
///
/// `Programming` naming the object, the model and what is wrong: a field that does not exist, is
/// not stored or has a type the rule cannot use, a field listed twice, period fields of different
/// types, an empty message, too many columns.
pub(crate) fn check_model(
    model: &str,
    objects: &[TableObject],
    fields: &[FieldSpec],
    by_name: &FxHashMap<&'static str, FieldId>,
) -> Result<()> {
    check_period_check_names(model, objects)?;
    for object in objects {
        let Some(what) = describe(object) else {
            continue;
        };
        let fail = |reason: String| Error::programming(format!("{what} on '{model}': {reason}"));
        let field = |name: &str| -> Result<&FieldSpec> {
            let id = by_name
                .get(name)
                .ok_or_else(|| fail(format!("field '{name}' does not exist")))?;
            let spec = &fields[id.index()];
            if !spec.has_column() {
                return Err(fail(format!(
                    "field '{name}' is not stored in the model's table"
                )));
            }
            Ok(spec)
        };
        let (equal, message) = match object.kind {
            TableObjectKind::Exclusion(decl) => (decl.equal, decl.message),
            TableObjectKind::TemporalKey(decl) => (decl.fields, decl.message),
            TableObjectKind::Constraint { .. }
            | TableObjectKind::Index { .. }
            | TableObjectKind::UniqueIndex { .. }
            | TableObjectKind::Key(_)
            | TableObjectKind::TypedIndex(_)
            | TableObjectKind::SignalTrigger(_)
            | TableObjectKind::FollowDelete(_) => continue,
        };
        if message.trim().is_empty() {
            return Err(fail("the message is empty".to_owned()));
        }
        let mut seen: Vec<&str> = Vec::with_capacity(equal.len() + 2);
        let mut note = |name: &'static str| -> Result<()> {
            if seen.contains(&name) {
                return Err(fail(format!("field '{name}' is listed twice")));
            }
            seen.push(name);
            Ok(())
        };
        for name in equal {
            let spec = field(name)?;
            if !comparable(&spec.ty) {
                return Err(fail(format!(
                    "field '{name}' is a {} field; `=` takes stored char, text, selection, \
                     boolean, integer, float, monetary, date, datetime and many2one fields",
                    spec.ty.type_name()
                )));
            }
            note(name)?;
        }
        match object.kind {
            TableObjectKind::Exclusion(decl) => {
                let (from, to) = decl.period;
                let (a, b) = (field(from)?, field(to)?);
                note(from)?;
                note(to)?;
                let same = matches!(
                    (&a.ty, &b.ty),
                    (FieldType::Date, FieldType::Date) | (FieldType::Datetime, FieldType::Datetime)
                );
                if !same {
                    return Err(fail(format!(
                        "the period fields '{from}' ({}) and '{to}' ({}) must both be date or both \
                         be datetime fields",
                        a.ty.type_name(),
                        b.ty.type_name()
                    )));
                }
                if let Some(filter) = decl.filter
                    && filter.trim().is_empty()
                {
                    return Err(fail(
                        "the filter is empty (leave it out instead)".to_owned(),
                    ));
                }
            }
            TableObjectKind::TemporalKey(decl) => {
                if decl.fields.is_empty() {
                    return Err(fail(
                        "a WITHOUT OVERLAPS key needs at least one field besides the range \
                         (PostgreSQL requires two columns)"
                            .to_owned(),
                    ));
                }
                let range = field(decl.range)?;
                if range.ty.range_bounds().is_none() {
                    return Err(fail(format!(
                        "'{}' must be a date_range or datetime_range field (it is a {} field)",
                        decl.range,
                        range.ty.type_name()
                    )));
                }
                note(decl.range)?;
            }
            TableObjectKind::Constraint { .. }
            | TableObjectKind::Index { .. }
            | TableObjectKind::UniqueIndex { .. }
            | TableObjectKind::Key(_)
            | TableObjectKind::TypedIndex(_)
            | TableObjectKind::SignalTrigger(_)
            | TableObjectKind::FollowDelete(_) => {}
        }
        // the equal columns plus the range (expression) column
        let columns = match object.kind {
            TableObjectKind::Exclusion(decl) => decl.equal.len() + 1,
            TableObjectKind::TemporalKey(decl) => decl.fields.len() + 1,
            TableObjectKind::Constraint { .. }
            | TableObjectKind::Index { .. }
            | TableObjectKind::UniqueIndex { .. }
            | TableObjectKind::Key(_)
            | TableObjectKind::TypedIndex(_)
            | TableObjectKind::SignalTrigger(_)
            | TableObjectKind::FollowDelete(_) => 0,
        };
        if columns > MAX_CONSTRAINT_COLUMNS {
            return Err(fail(format!(
                "{columns} columns, more than PostgreSQL's limit of {MAX_CONSTRAINT_COLUMNS}"
            )));
        }
    }
    Ok(())
}

/// Refuse an object whose database name is the period check of an exclusion (after shortening to
/// 63 bytes): the constraints would replace each other in the catalog (shortened names carry a
/// hash of the whole name, so two period checks do not collide).
fn check_period_check_names(model: &str, objects: &[TableObject]) -> Result<()> {
    let mut checks: Vec<(String, &TableObject)> = Vec::new();
    for object in objects {
        if matches!(object.kind, TableObjectKind::Exclusion(_)) {
            checks.push((period_check_name(&object.full_name), object));
        }
    }
    for (check, owner) in &checks {
        let owner_what = describe(owner).unwrap_or_else(|| format!("'{}'", owner.name));
        for object in objects {
            if object.full_name == *check {
                let what =
                    describe(object).unwrap_or_else(|| format!("SQL object '{}'", object.name));
                return Err(Error::programming(format!(
                    "{what} on '{model}': its database name '{check}' is the name of the period \
                     check of {owner_what}; rename one of them"
                )));
            }
        }
    }
    Ok(())
}

/// Types a GiST `=` can compare (with `btree_gist`).
fn comparable(ty: &FieldType) -> bool {
    matches!(
        ty,
        FieldType::Boolean
            | FieldType::BigInt
            | FieldType::Integer
            | FieldType::Float { .. }
            | FieldType::Monetary { .. }
            | FieldType::Char { .. }
            | FieldType::Text
            | FieldType::Selection { .. }
            | FieldType::Date
            | FieldType::Datetime
            | FieldType::Many2one { .. }
    )
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "tests")]

    use erp_fields::RangeBounds;

    use super::*;
    use crate::{AddonDescriptor, FieldDecl, ModelDecl, RegistryBuilder, TableObjectDecl};

    static ADDON: AddonDescriptor = AddonDescriptor {
        name: "test_exclusion_checks",
        depends: &["base"],
        exact_only: false,
    };

    static FIELDS: [FieldDecl; 7] = [
        FieldDecl::integer("room"),
        FieldDecl::date("arrival"),
        FieldDecl::date("departure"),
        FieldDecl::datetime("start"),
        FieldDecl::json("payload"),
        FieldDecl::date_range("validity", RangeBounds::HalfOpen),
        FieldDecl::integer("total").compute("_compute_total"),
    ];

    /// The registry error for a model declaring `kind` as its only table object.
    fn refused(kind: TableObjectKind) -> String {
        let objects: &'static [TableObjectDecl] = Box::leak(Box::new([TableObjectDecl {
            name: "_rule",
            kind,
        }]));
        let decl: &'static ModelDecl = Box::leak(Box::new(
            ModelDecl::new("test_exclusion_checks", "excl.stay")
                .fields(&FIELDS)
                .table_objects(objects),
        ));
        let err = RegistryBuilder::new([&ADDON])
            .linked(false)
            .with_decls([decl])
            .build()
            .unwrap_err();
        assert_eq!(err.kind(), erp_fields::ErrorKind::Programming);
        err.message().to_owned()
    }

    fn exclude(from: &'static str, to: &'static str) -> ExclusionDecl {
        ExclusionDecl::new((from, to), RangeBounds::HalfOpen, "taken")
    }

    #[test]
    fn exclusions_name_existing_stored_fields_of_usable_types() {
        let cases: [(TableObjectKind, &str); 9] = [
            (
                TableObjectKind::Exclusion(exclude("arrival", "nope")),
                "exclusion '_rule' on 'excl.stay': field 'nope' does not exist",
            ),
            (
                TableObjectKind::Exclusion(exclude("arrival", "departure").equal(&["total"])),
                "exclusion '_rule' on 'excl.stay': field 'total' is not stored in the model's \
                 table",
            ),
            (
                TableObjectKind::Exclusion(exclude("arrival", "departure").equal(&["payload"])),
                "exclusion '_rule' on 'excl.stay': field 'payload' is a json field; `=` takes \
                 stored char, text, selection, boolean, integer, float, monetary, date, datetime \
                 and many2one fields",
            ),
            (
                TableObjectKind::Exclusion(exclude("arrival", "start")),
                "exclusion '_rule' on 'excl.stay': the period fields 'arrival' (date) and 'start' \
                 (datetime) must both be date or both be datetime fields",
            ),
            (
                TableObjectKind::Exclusion(exclude("room", "room")),
                "exclusion '_rule' on 'excl.stay': field 'room' is listed twice",
            ),
            (
                TableObjectKind::Exclusion(exclude("arrival", "departure").equal(&["arrival"])),
                "exclusion '_rule' on 'excl.stay': field 'arrival' is listed twice",
            ),
            (
                TableObjectKind::Exclusion(ExclusionDecl::new(
                    ("arrival", "departure"),
                    RangeBounds::Closed,
                    "  ",
                )),
                "exclusion '_rule' on 'excl.stay': the message is empty",
            ),
            (
                TableObjectKind::TemporalKey(
                    TemporalKeyDecl::new("arrival", "taken").fields(&["room"]),
                ),
                "key '_rule' on 'excl.stay': 'arrival' must be a date_range or datetime_range \
                 field (it is a date field)",
            ),
            (
                TableObjectKind::TemporalKey(TemporalKeyDecl::new("validity", "taken")),
                "key '_rule' on 'excl.stay': a WITHOUT OVERLAPS key needs at least one field \
                 besides the range (PostgreSQL requires two columns)",
            ),
        ];
        for (kind, message) in cases {
            assert_eq!(refused(kind), message);
        }
        // an empty filter is a mistake, not "no filter"
        assert_eq!(
            refused(TableObjectKind::Exclusion(
                exclude("arrival", "departure").filter(" ")
            )),
            "exclusion '_rule' on 'excl.stay': the filter is empty (leave it out instead)"
        );
    }

    /// Review D5/R8: no object may take the database name of an exclusion's period check.
    #[test]
    fn period_check_names_are_not_taken_by_other_objects() {
        let build = |objects: Vec<TableObjectDecl>| {
            let objects: &'static [TableObjectDecl] = Box::leak(objects.into_boxed_slice());
            let decl: &'static ModelDecl = Box::leak(Box::new(
                ModelDecl::new("test_exclusion_checks", "excl.stay")
                    .fields(&FIELDS)
                    .table_objects(objects),
            ));
            RegistryBuilder::new([&ADDON])
                .linked(false)
                .with_decls([decl])
                .build()
                .unwrap_err()
        };
        let rule = TableObjectDecl {
            name: "_rule",
            kind: TableObjectKind::Exclusion(exclude("arrival", "departure")),
        };
        let taker = TableObjectDecl {
            name: "_rule_period_check",
            kind: TableObjectKind::Constraint {
                definition: "CHECK (true)",
                message: "no",
            },
        };
        let err = build(vec![rule, taker]);
        assert_eq!(err.kind(), erp_fields::ErrorKind::Programming);
        assert_eq!(
            err.message(),
            "SQL object '_rule_period_check' on 'excl.stay': its database name \
             'excl_stay_rule_period_check' is the name of the period check of exclusion '_rule'; \
             rename one of them"
        );
    }

    #[test]
    fn extensions_parse_only_the_allowed_names() {
        for ext in [
            PgExtension::BtreeGist,
            PgExtension::PgTrgm,
            PgExtension::Unaccent,
        ] {
            assert_eq!(PgExtension::parse(ext.name()), Some(ext));
        }
        assert_eq!(PgExtension::parse("plpython3u"), None);
    }
}
