//! Declared rule operators in the policy compiler (Amendment 1).
//!
//! `erp-orm` holds the declarations ([`erp_orm::RuleOperatorDecl`], linked in
//! [`erp_orm::RULE_OPERATORS`]); this module checks them against the registry when a policy is
//! built ([`Operators::new`]), recognizes their spelling in rule text (`rule::Builder`), carries a
//! recognized condition through the compiled rule as a [`RuleOperatorLeaf`] and finds the models it
//! ranges over ([`Operators::targets`]). The expansion into one branch per target model happens in
//! `snapshot::Combine::resolve`, under the memo key of the combined predicate (model, operation,
//! groups, shape): the branches depend on the groups and the shape, never on a principal value.

use std::any::Any;
use std::fmt;

use erp_domain::{CustomDomain, Operation, Operator};
use erp_fields::{Error, FieldType, Result};
use erp_orm::RuleOperatorDecl;
use erp_registry::{ModelKind, ModelSpec, Registry};

/// Longest operator name (bytes).
pub const MAX_RULE_OPERATOR_NAME_LEN: usize = 64;

/// A condition of a rule that uses a declared operator, as it travels in the compiled rule's
/// domain until the combined predicate is built: `(field, 'name', 'operation')`.
pub(crate) struct RuleOperatorLeaf {
    decl: &'static RuleOperatorDecl,
    op: Operation,
}

impl RuleOperatorLeaf {
    pub(crate) fn new(decl: &'static RuleOperatorDecl, op: Operation) -> Self {
        Self { decl, op }
    }

    pub(crate) fn decl(&self) -> &'static RuleOperatorDecl {
        self.decl
    }

    pub(crate) fn op(&self) -> Operation {
        self.op
    }

    /// The leaf held by a custom domain node, if it is one.
    pub(crate) fn of(node: &dyn CustomDomain) -> Option<&Self> {
        node.as_any().downcast_ref::<Self>()
    }
}

/// The condition as rule text, for messages.
pub(crate) struct LeafText<'a>(pub(crate) &'a RuleOperatorDecl, pub(crate) Operation);

impl fmt::Display for LeafText<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "('{}', '{}', '{}')",
            self.0.field(),
            self.0.name(),
            self.1
        )
    }
}

impl fmt::Debug for RuleOperatorLeaf {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", LeafText(self.decl, self.op))
    }
}

impl CustomDomain for RuleOperatorLeaf {
    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// The validated declared operators of one policy build.
#[derive(Debug, Clone, Default)]
pub struct Operators {
    decls: Vec<&'static RuleOperatorDecl>,
}

fn valid_name(name: &str) -> bool {
    let mut chars = name.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_lowercase())
        && name.len() <= MAX_RULE_OPERATOR_NAME_LEN
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}

impl Operators {
    /// No declared operator: rule text may use core operators only.
    pub fn none() -> Self {
        Self::default()
    }

    /// Check `decls` against `registry`; declarations of addons the registry does not have
    /// installed are skipped.
    ///
    /// # Errors
    ///
    /// `Programming`, naming the declaration (`declared_in`), for a malformed name, a name equal
    /// to a core operator, a name declared twice, a model or inherited model the registry does
    /// not have, a field that is not a stored `many2one_reference` of the model, a reference
    /// field whose model column is missing, or no operation (or one listed twice).
    pub fn new(
        registry: &Registry,
        decls: impl IntoIterator<Item = &'static RuleOperatorDecl>,
    ) -> Result<Self> {
        let mut out: Vec<&'static RuleOperatorDecl> = Vec::new();
        let installed: Vec<&str> = registry.addons().iter().map(|a| a.name).collect();
        for decl in decls {
            // a declaration of an addon that is not installed in this database is not part of
            // its policy (as `#[access_domain]` overrides, `chain.rs`); one of an installed
            // addon that does not hold up is refused below
            if !installed.contains(&decl.addon()) {
                continue;
            }
            let bad = |why: String| {
                Error::programming(format!(
                    "rule operator '{}' (declared in {}): {why}",
                    decl.name(),
                    decl.declared_in()
                ))
            };
            if !valid_name(decl.name()) {
                return Err(bad(format!(
                    "the name must start with a lowercase ASCII letter and hold lowercase ASCII \
                     letters, digits and '_' only, at most {MAX_RULE_OPERATOR_NAME_LEN} bytes"
                )));
            }
            if Operator::parse(decl.name()).is_some() {
                return Err(bad("the name is a core domain operator".to_owned()));
            }
            if let Some(first) = out.iter().find(|d| d.name() == decl.name()) {
                return Err(bad(format!(
                    "the name is declared twice (first in {})",
                    first.declared_in()
                )));
            }
            let Some(model) = registry.model(decl.model()) else {
                return Err(bad(format!("the model '{}' does not exist", decl.model())));
            };
            if registry.model(decl.inherits()).is_none() {
                return Err(bad(format!(
                    "the inherited model '{}' does not exist",
                    decl.inherits()
                )));
            }
            let Some(field) = model.field(decl.field()) else {
                return Err(bad(format!(
                    "{} has no field '{}'",
                    model.name,
                    decl.field()
                )));
            };
            let FieldType::Many2oneReference { model_field } = &field.ty else {
                return Err(bad(format!(
                    "{}.{} is a {} field, not a many2one_reference",
                    model.name,
                    field.name,
                    field.ty.type_name()
                )));
            };
            if !field.has_column() || model.field(model_field).is_none_or(|f| !f.has_column()) {
                return Err(bad(format!(
                    "{}.{} and its model column '{model_field}' must be stored columns",
                    model.name, field.name
                )));
            }
            if decl.operations().is_empty() {
                return Err(bad("it lists no operation".to_owned()));
            }
            for (i, op) in decl.operations().iter().enumerate() {
                if decl.operations()[..i].contains(op) {
                    return Err(bad(format!("the operation '{op}' is listed twice")));
                }
            }
            out.push(decl);
        }
        Ok(Self { decls: out })
    }

    /// The declaration spelled `name`, if any.
    pub(crate) fn by_name(&self, name: &str) -> Option<&'static RuleOperatorDecl> {
        self.decls.iter().copied().find(|d| d.name() == name)
    }

    /// The models a declared operator ranges over: concrete (not abstract, not transient)
    /// models that inherit the declaration's `inherits` model, except the declaring model, in
    /// registry order. Whether a principal may read them is decided per policy.
    pub(crate) fn targets<'r>(
        registry: &'r Registry,
        decl: &RuleOperatorDecl,
    ) -> impl Iterator<Item = &'r ModelSpec> + use<'r> {
        let own = decl.model();
        let inherits = decl.inherits();
        registry.models().iter().filter(move |m| {
            m.kind == ModelKind::Regular
                && m.name != own
                && m.has_generic_table()
                && m.inherits(inherits)
        })
    }
}

#[cfg(test)]
mod tests;
