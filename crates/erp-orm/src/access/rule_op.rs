//! Declared access-rule operators (Amendment 1): a condition spelling an addon may use in
//! the domain text of an `ir.access` row, expanded by the policy compiler while it builds the
//! combined predicate.
//!
//! A [`RuleOperatorDecl`] is plain data in the [`RULE_OPERATORS`] slice, declared with
//! [`rule_operator!`](crate::rule_operator) (with the declaring `addon`): no addon code runs while a policy is built, which
//! keeps the registration style of [`capability!`](crate::capability). The operator is **not** an
//! `erp-domain` [`Operator`](erp_domain::Operator): the domain parsers refuse its spelling in RPC,
//! search and list-syntax domains with the ordinary invalid-operator error, and only the policy
//! compiler (`erp-security`) recognizes it, in rule text only. Declaration errors (a duplicate or
//! malformed name, a name that is a core operator, a field that is not a `many2one_reference`, an
//! unknown model) refuse to build the policy; see `erp_security::snapshot::Compiler::compile`.
//!
//! The first declaration is `thread_access` of the `mail` addon: on `mail.message.res_id`, it
//! stands for "the document `(model, res_id)` is readable by the principal", for the documents of
//! every model that inherits `mail.thread`.

use erp_domain::Operation;

/// One declared rule operator. Declare it with [`rule_operator!`](crate::rule_operator); the
/// hidden constructor is public only so the macro can use it, and the policy compiler checks every
/// member of [`RULE_OPERATORS`] again when it builds.
#[derive(Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct RuleOperatorDecl {
    name: &'static str,
    addon: &'static str,
    model: &'static str,
    field: &'static str,
    inherits: &'static str,
    operations: &'static [Operation],
    declared_in: &'static str,
}

impl RuleOperatorDecl {
    /// Used by [`rule_operator!`](crate::rule_operator) only.
    #[doc(hidden)]
    pub const fn __declare(
        name: &'static str,
        addon: &'static str,
        model: &'static str,
        field: &'static str,
        inherits: &'static str,
        operations: &'static [Operation],
        declared_in: &'static str,
    ) -> Self {
        Self {
            name,
            addon,
            model,
            field,
            inherits,
            operations,
            declared_in,
        }
    }

    /// The spelling in rule text, e.g. `thread_access`.
    pub fn name(&self) -> &'static str {
        self.name
    }

    /// The addon that declares the operator. The policy compiler ignores the declaration when
    /// that addon is not installed in the database, as it does for `#[access_domain]` overrides;
    /// for an installed addon a declaration that does not hold up refuses to build.
    pub fn addon(&self) -> &'static str {
        self.addon
    }

    /// The only model whose rules may use the operator (the model that holds the reference
    /// column), e.g. `mail.message`.
    pub fn model(&self) -> &'static str {
        self.model
    }

    /// The `many2one_reference` field the condition names, e.g. `res_id`; the field's own
    /// `model_field` column names the target model of each row.
    pub fn field(&self) -> &'static str {
        self.field
    }

    /// The model every target must inherit, e.g. `mail.thread`: the operator ranges over the
    /// registry's concrete models that inherit it.
    pub fn inherits(&self) -> &'static str {
        self.inherits
    }

    /// The operations a rule may name as the condition's value, e.g. `read`.
    pub fn operations(&self) -> &'static [Operation] {
        self.operations
    }

    /// Module path of the declaration.
    pub fn declared_in(&self) -> &'static str {
        self.declared_in
    }
}

/// Every rule operator linked into the binary.
#[linkme::distributed_slice]
pub static RULE_OPERATORS: [RuleOperatorDecl];

/// Declare a rule operator: a `static` of type [`RuleOperatorDecl`] registered in
/// [`RULE_OPERATORS`]. The operator is valid in the domain text of `ir.access` rows on `model`,
/// as `(<field>, '<name>', '<operation>')`, with the operations listed; at policy build it
/// expands to one branch per concrete model that inherits `inherits`.
///
/// ```
/// erp_orm::rule_operator!(pub DOC_ACCESS = "doc_access" {
///     addon: "my_addon",
///     model: "my.note",
///     field: "res_id",
///     inherits: "my.document",
///     operations: [Read],
/// });
///
/// fn main() {
///     assert_eq!(DOC_ACCESS.name(), "doc_access");
///     assert!(erp_orm::RULE_OPERATORS.iter().any(|op| op.name() == "doc_access"));
/// }
/// ```
#[macro_export]
macro_rules! rule_operator {
    (
        $(#[$meta:meta])* $vis:vis $ident:ident = $name:literal {
            addon: $addon:literal,
            model: $model:literal,
            field: $field:literal,
            inherits: $inherits:literal,
            operations: [$($operation:ident),+ $(,)?] $(,)?
        }
    ) => {
        $(#[$meta])*
        #[$crate::linkme::distributed_slice($crate::RULE_OPERATORS)]
        #[linkme(crate = $crate::linkme)]
        $vis static $ident: $crate::RuleOperatorDecl = $crate::RuleOperatorDecl::__declare(
            $name,
            $addon,
            $model,
            $field,
            $inherits,
            &[$($crate::Operation::$operation),+],
            ::core::module_path!(),
        );
    };
}
