//! The `PolicySourceWrite` registration point (DN §3.4, seam 4): the
//! models whose ORM writes change the access policy (policy tables) or a principal's membership.
//! A write to one flushes pending computes first, marks the transaction dirty, and the next
//! engine call flushes the pending writes of every registered model before it rebuilds the
//! policy privately or re-resolves the bound principals (Revision 3 M2). `addons/base` registers
//! its models; addons may register more.

use erp_fields::{Error, Result};
use erp_registry::{ModelSpec, Registry};

/// What a write to a registered model changes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum SourceKind {
    /// A policy table (`ir.access`, `res.groups` and its relations, `ir.capability`,
    /// `ir.model.fields` groups): the transaction rebuilds its policy privately.
    Policy,
    /// A membership (`res.users` groups, companies, active): the bound principals are
    /// re-resolved. The tables a membership is read from also carry a statement trigger that
    /// bumps the `membership` signal generation (`signal_triggers = [("_membership_bump",
    /// "membership")]` on the model), so a read replica that has not replayed the change is
    /// detected by the routing lag guard; a registered addon model declares it too.
    Membership,
}

/// A registered model ([`POLICY_SOURCE_WRITES`]).
///
/// ```
/// use erp_orm::{PolicySourceWrite, SourceKind};
/// static USERS: PolicySourceWrite =
///     PolicySourceWrite::new("res.users", SourceKind::Membership)
///         .fields(&["group_ids", "company_ids", "company_id", "active"]);
/// assert!(USERS.watches_field("company_id"));
/// assert!(!USERS.watches_field("signature"));
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PolicySourceWrite {
    model: &'static str,
    kind: SourceKind,
    fields: Option<&'static [&'static str]>,
}

impl PolicySourceWrite {
    /// Every create, write and unlink of `model` changes `kind`.
    pub const fn new(model: &'static str, kind: SourceKind) -> Self {
        Self {
            model,
            kind,
            fields: None,
        }
    }

    /// Only writes of these fields count (creates and unlinks always do).
    #[must_use]
    pub const fn fields(mut self, fields: &'static [&'static str]) -> Self {
        self.fields = Some(fields);
        self
    }

    /// The model.
    pub fn model(&self) -> &'static str {
        self.model
    }

    /// What it changes.
    pub fn kind(&self) -> SourceKind {
        self.kind
    }

    /// Whether a write of `field` counts.
    pub fn watches_field(&self, field: &str) -> bool {
        self.fields.is_none_or(|fields| fields.contains(&field))
    }

    /// Check the registration against a registry: the model and its listed fields must exist
    /// when the model is installed (an unknown field would silently never mark the transaction
    /// dirty).
    ///
    /// # Errors
    ///
    /// `Programming` naming the field that the model does not have.
    pub fn validate<'r>(&self, registry: &'r Registry) -> Result<Option<&'r ModelSpec>> {
        let Some(spec) = registry.model(self.model) else {
            // the model's addon is not installed in this database: nothing to watch
            return Ok(None);
        };
        if let Some(fields) = self.fields
            && let Some(field) = fields.iter().find(|f| spec.field(f).is_none())
        {
            return Err(Error::programming(format!(
                "the policy-source registration of '{}' watches the field '{field}', which the \
                 model does not have",
                self.model
            )));
        }
        Ok(Some(spec))
    }
}

/// Policy-source and membership models linked into the binary.
#[linkme::distributed_slice]
pub static POLICY_SOURCE_WRITES: [PolicySourceWrite];

/// Register a model whose writes change the access policy or a membership: a `static` of type
/// [`PolicySourceWrite`] in [`POLICY_SOURCE_WRITES`]. Only tables the policy is compiled from or
/// the principals are resolved from need one; data that access predicates read in SQL does not
/// (the record-check memo follows the fields each predicate reads).
///
/// [`crate::OrmBuilder::build`] checks every registration of an installed model: a watched field
/// the model does not have is a `Programming` error.
///
/// ```
/// use erp_orm::{PolicySourceWrite, SourceKind};
///
/// erp_orm::policy_source!(
///     /// Approver roles put users in groups: a membership the resolver reads.
///     pub APPROVERS = PolicySourceWrite::new("my_addon.approver", SourceKind::Membership)
///         .fields(&["user_id", "active"])
/// );
///
/// fn main() {
///     assert!(erp_orm::POLICY_SOURCE_WRITES.iter().any(|s| s.model() == "my_addon.approver"));
///     assert!(APPROVERS.watches_field("active"));
/// }
/// ```
#[macro_export]
macro_rules! policy_source {
    ($(#[$meta:meta])* $vis:vis $ident:ident = $registration:expr) => {
        $(#[$meta])*
        #[$crate::linkme::distributed_slice($crate::POLICY_SOURCE_WRITES)]
        #[linkme(crate = $crate::linkme)]
        $vis static $ident: $crate::PolicySourceWrite = $registration;
    };
}
