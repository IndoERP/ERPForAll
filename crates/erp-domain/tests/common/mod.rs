#![allow(dead_code)]

use std::any::Any;
use std::collections::HashMap;
use std::sync::{Arc, LazyLock};

use erp_domain::{
    CustomDomain, Domain, DomainError, ExternalValue, FieldInfo, FieldType, ModelContext, Opaque,
    OpaqueKind, Operator, Schema, TimeZone, Value,
};
use time::{Date, Month, PrimitiveDateTime, Time, UtcOffset};

struct FieldDef {
    field_type: FieldType,
    comodel: Option<&'static str>,
    not_null: bool,
}

/// Fixture schema: model -> field -> definition.
pub struct TestSchema {
    models: HashMap<&'static str, HashMap<&'static str, FieldDef>>,
}

impl Schema for TestSchema {
    fn field(&self, model: &str, field: &str) -> Option<FieldInfo<'_>> {
        let (name, def) = self.models.get(model)?.get_key_value(field)?;
        let mut info = FieldInfo::new(name, def.field_type).with_not_null(def.not_null);
        if let Some(comodel) = def.comodel {
            info = info.with_comodel(comodel);
        }
        Some(info)
    }

    fn has_model(&self, model: &str) -> bool {
        self.models.contains_key(model)
    }
}

use FieldType as T;

type Fields = &'static [(&'static str, FieldType, Option<&'static str>, bool)];

const MIXED: Fields = &[
    ("boolean", T::Boolean, None, false),
    ("integer", T::Integer, None, false),
    ("float", T::Float, None, false),
    ("numeric", T::Float, None, false),
    ("number", T::Float, None, false),
    ("monetary", T::Monetary, None, false),
    ("char", T::Char, None, false),
    ("text", T::Text, None, false),
    ("html", T::Html, None, false),
    ("date", T::Date, None, false),
    ("moment", T::Datetime, None, false),
    ("selection", T::Selection, None, false),
    ("currency_id", T::Many2one, Some("res.currency"), false),
    (
        "many2one_id",
        T::Many2one,
        Some("test_orm.mixed_relations"),
        false,
    ),
    (
        "one2many_ids",
        T::One2many,
        Some("test_orm.mixed_relations"),
        false,
    ),
    (
        "many2many_ids",
        T::Many2many,
        Some("test_orm.mixed_relations"),
        false,
    ),
];

const NAMED: Fields = &[("name", T::Char, None, false)];

const MESSAGE: Fields = &[
    ("name", T::Char, None, false),
    ("body", T::Text, None, false),
    (
        "discussion",
        T::Many2one,
        Some("test_orm.discussion"),
        false,
    ),
    ("author", T::Many2one, Some("res.users"), false),
    ("important", T::Boolean, None, false),
    ("active", T::Boolean, None, false),
    ("has_important_sibling", T::Boolean, None, false),
    ("color", T::Integer, None, false),
    ("priority", T::Integer, None, false),
];

const DISCUSSION: Fields = &[
    ("name", T::Char, None, true),
    ("moderator", T::Many2one, Some("res.users"), false),
    ("categories", T::Many2many, Some("test_orm.category"), false),
    ("messages", T::One2many, Some("test_orm.message"), false),
];

const USERS: Fields = &[
    ("name", T::Char, None, false),
    ("login", T::Char, None, true),
    ("active", T::Boolean, None, false),
];

const CATEGORY: Fields = &[
    ("name", T::Char, None, true),
    ("parent", T::Many2one, Some("test_orm.category"), false),
];

const PARTNER: Fields = &[
    ("name", T::Char, None, true),
    ("email", T::Char, None, false),
    ("active", T::Boolean, None, false),
    ("website", T::Char, None, false),
    (
        "category_id",
        T::Many2many,
        Some("test_orm.partner.category"),
        false,
    ),
    ("business_id", T::Many2one, Some("test_orm.business"), false),
    ("parent_id", T::Many2one, Some("test_orm.partner"), false),
    ("child_ids", T::One2many, Some("test_orm.partner"), false),
    ("country_id", T::Many2one, Some("test_orm.country"), false),
];

const BUSINESS: Fields = &[
    ("name", T::Char, None, true),
    ("active", T::Boolean, None, false),
    ("partner_id", T::Many2one, Some("test_orm.partner"), true),
];

const COUNTRY: Fields = &[
    ("name", T::Char, None, true),
    ("code", T::Char, None, false),
    ("phone_code", T::Integer, None, false),
];

const SELECTION: Fields = &[("state", T::Selection, None, false)];

pub static SCHEMA: LazyLock<TestSchema> = LazyLock::new(|| {
    let models: [(&'static str, Fields); 12] = [
        ("test_orm.mixed", MIXED),
        ("test_orm.mixed_relations", NAMED),
        ("res.currency", NAMED),
        ("test_orm.message", MESSAGE),
        ("test_orm.discussion", DISCUSSION),
        ("res.users", USERS),
        ("test_orm.category", CATEGORY),
        ("test_orm.partner", PARTNER),
        ("test_orm.partner.category", NAMED),
        ("test_orm.business", BUSINESS),
        ("test_orm.country", COUNTRY),
        ("test_orm.selection", SELECTION),
    ];
    let mut map = HashMap::new();
    for (model, fields) in models {
        let mut defs = HashMap::new();
        // Every model has `id` and `display_name`.
        defs.insert(
            "id",
            FieldDef {
                field_type: T::Id,
                comodel: None,
                not_null: true,
            },
        );
        defs.insert(
            "display_name",
            FieldDef {
                field_type: T::Char,
                comodel: None,
                not_null: false,
            },
        );
        for (name, field_type, comodel, not_null) in fields {
            defs.insert(
                *name,
                FieldDef {
                    field_type: *field_type,
                    comodel: *comodel,
                    not_null: *not_null,
                },
            );
        }
        map.insert(model, defs);
    }
    TestSchema { models: map }
});

/// The optimization context for a fixture model (UTC, no new records).
pub fn model(name: &'static str) -> ModelContext<'static> {
    ModelContext::new(&*SCHEMA, name)
}

/// BASIC-optimize, failing the test on error.
pub fn opt(model_name: &'static str, domain: &Domain) -> Domain {
    domain
        .optimize(&model(model_name))
        .unwrap_or_else(|e| panic!("optimize({domain}) failed: {e}"))
}

/// BASIC-optimize, returning the error.
pub fn opt_err(model_name: &'static str, domain: &Domain) -> DomainError {
    match domain.optimize(&model(model_name)) {
        Ok(d) => panic!("optimize({domain}) should fail, got {d}"),
        Err(e) => e,
    }
}

/// A checked condition with a string operator.
pub fn c(field: &str, op: &str, value: impl Into<Value>) -> Domain {
    Domain::condition_str(field, op, value).unwrap()
}

/// A condition built without the checked constructor's rewrites (expected optimized forms).
pub fn cond(field: &str, op: Operator, value: impl Into<Value>) -> Domain {
    Domain::condition(field, op, value).unwrap()
}

pub fn set<V: Into<Value>>(values: impl IntoIterator<Item = V>) -> Value {
    Value::set(values)
}

/// A list value.
pub fn list<V: Into<Value>>(values: impl IntoIterator<Item = V>) -> Value {
    Value::list(values)
}

/// Parse a JSON list-syntax domain.
pub fn json(value: serde_json::Value) -> Domain {
    Domain::from_json(&value).unwrap()
}

/// A JSON value as a domain list value.
pub fn jv(value: serde_json::Value) -> Value {
    Value::from_json(&value).unwrap()
}

pub fn date(y: i32, m: u8, d: u8) -> Date {
    Date::from_calendar_date(y, Month::try_from(m).unwrap(), d).unwrap()
}

pub fn datetime(y: i32, m: u8, d: u8, hh: u8, mm: u8, ss: u8) -> PrimitiveDateTime {
    PrimitiveDateTime::new(date(y, m, d), Time::from_hms(hh, mm, ss).unwrap())
}

/// A stand-in for an `erp-sql` query or SQL value.
#[derive(Debug)]
pub struct FakeExternal(pub OpaqueKind);

impl ExternalValue for FakeExternal {
    fn kind(&self) -> OpaqueKind {
        self.0
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

pub fn query() -> Value {
    Value::Opaque(Opaque::new(Arc::new(FakeExternal(OpaqueKind::Query))))
}

pub fn sql() -> Value {
    Value::Opaque(Opaque::new(Arc::new(FakeExternal(OpaqueKind::Sql))))
}

/// A custom node.
#[derive(Debug)]
pub struct FakeCustom;

impl CustomDomain for FakeCustom {
    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// A fixed UTC offset.
pub struct Fixed(pub UtcOffset, pub &'static str);

impl TimeZone for Fixed {
    fn id(&self) -> &str {
        self.1
    }

    fn local_to_utc(&self, local: PrimitiveDateTime) -> Option<PrimitiveDateTime> {
        let utc = local.assume_offset(self.0).to_offset(UtcOffset::UTC);
        Some(PrimitiveDateTime::new(utc.date(), utc.time()))
    }

    fn utc_to_local(&self, utc: PrimitiveDateTime) -> Option<PrimitiveDateTime> {
        let local = utc.assume_utc().to_offset(self.0);
        Some(PrimitiveDateTime::new(local.date(), local.time()))
    }
}

/// Europe/Brussels, simplified to the months the tests use: UTC+2 from April to September,
/// UTC+1 otherwise.
pub struct Brussels;

impl Brussels {
    fn offset(month: Month) -> UtcOffset {
        let hours = if (4..=9).contains(&u8::from(month)) {
            2
        } else {
            1
        };
        UtcOffset::from_hms(hours, 0, 0).unwrap()
    }
}

impl TimeZone for Brussels {
    fn id(&self) -> &str {
        "Europe/Brussels"
    }

    fn local_to_utc(&self, local: PrimitiveDateTime) -> Option<PrimitiveDateTime> {
        Fixed(Self::offset(local.month()), "Europe/Brussels").local_to_utc(local)
    }

    fn utc_to_local(&self, utc: PrimitiveDateTime) -> Option<PrimitiveDateTime> {
        Fixed(Self::offset(utc.month()), "Europe/Brussels").utc_to_local(utc)
    }
}
