//! The registry as the domain optimizer sees it, for compile-time validation of rules (BASIC
//! optimization checks fields, operators and values, and refuses principal parameters where a
//! rewrite needs a literal).
//!
//! It mirrors `erp-orm`'s own (crate-private) schema, except that no column is declared
//! `NOT NULL`: the compiler only validates with it, and fewer rewrites are the safe direction.

use erp_domain::{FieldInfo, FieldType as DomainFieldType, Schema};
use erp_fields::FieldType;
use erp_registry::Registry;

/// The registry seen by the domain optimizer.
pub(crate) struct RegistrySchema<'a>(pub(crate) &'a Registry);

fn domain_type(ty: &FieldType) -> (DomainFieldType, Option<&str>) {
    // exhaustive on purpose: a new field type must decide how rules compare with it
    let t = match ty {
        FieldType::Id => DomainFieldType::Id,
        FieldType::Boolean => DomainFieldType::Boolean,
        // the row version is an integer to rules
        FieldType::BigInt | FieldType::Integer | FieldType::RowVersion => DomainFieldType::Integer,
        FieldType::Float { .. } => DomainFieldType::Float,
        FieldType::Monetary { .. } => DomainFieldType::Monetary,
        FieldType::Char { .. } => DomainFieldType::Char,
        FieldType::Text => DomainFieldType::Text,
        FieldType::Html => DomainFieldType::Html,
        FieldType::Selection { .. } => DomainFieldType::Selection,
        FieldType::Date => DomainFieldType::Date,
        FieldType::Datetime => DomainFieldType::Datetime,
        FieldType::Uuid => DomainFieldType::Uuid,
        FieldType::Xid8 => DomainFieldType::Xid8,
        FieldType::TimestampTz => DomainFieldType::TimestampTz,
        FieldType::DateRange { .. } => DomainFieldType::DateRange,
        FieldType::DatetimeRange { .. } => DomainFieldType::DatetimeRange,
        FieldType::Json => DomainFieldType::Json,
        FieldType::Many2one { comodel, .. } => {
            return (DomainFieldType::Many2one, Some(comodel.as_str()));
        }
        FieldType::One2many { comodel, .. } => {
            return (DomainFieldType::One2many, Some(comodel.as_str()));
        }
        FieldType::Many2many { comodel, .. } => {
            return (DomainFieldType::Many2many, Some(comodel.as_str()));
        }
        FieldType::Reference { .. } => DomainFieldType::Reference,
        FieldType::Many2oneReference { .. } => DomainFieldType::Many2oneReference,
    };
    (t, None)
}

impl Schema for RegistrySchema<'_> {
    fn field(&self, model: &str, field: &str) -> Option<FieldInfo<'_>> {
        let spec = self.0.model(model)?.field(field)?;
        let (ty, comodel) = domain_type(&spec.ty);
        let info = FieldInfo::new(spec.name, ty);
        Some(match comodel {
            Some(comodel) => info.with_comodel(comodel),
            None => info,
        })
    }

    fn has_model(&self, model: &str) -> bool {
        self.0.contains(model)
    }

    fn version(&self) -> u64 {
        self.0.generation()
    }
}

#[cfg(test)]
mod native_fields_tests {
    use super::*;
    #[test]
    fn native_fields_security_schema_preserves_native_nonrelational_kinds() {
        for (field, domain) in [
            (FieldType::Uuid, DomainFieldType::Uuid),
            (FieldType::Xid8, DomainFieldType::Xid8),
            (FieldType::TimestampTz, DomainFieldType::TimestampTz),
        ] {
            assert_eq!(domain_type(&field), (domain, None));
            assert_eq!(crate::analyze::comodel_of(&field), None);
            assert!(!domain.has_falsy_value());
        }
    }
}
