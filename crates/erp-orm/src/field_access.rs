//! Field-level access (and §3.3): which fields of
//! a model a principal may read and write.
//!
//! The policy answers once per model with a [`FieldMask`] ([`crate::AccessPolicy::field_access`]);
//! the engine ([`crate::Transaction::check_field`]) tests single fields against it on every ORM
//! path (read, write, create, x2many commands, domains, order, group by, `fields_get`). A model's
//! `#[field_access]` overrides ([`crate::field_chain`]) narrow the policy's mask afterwards.

use std::fmt;
use std::sync::{Arc, OnceLock};

use erp_fields::{Error, Result};
use erp_registry::{FieldId, ModelSpec};

use crate::bitset::BitSet;

/// Creating
/// a record checks [`FieldOp::Write`] on the given fields.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FieldOp {
    /// Reading the field's value (also searching, ordering and grouping by it).
    Read,
    /// Writing the field, or giving it a value at creation.
    Write,
}

impl FieldOp {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Write => "write",
        }
    }
}

impl fmt::Display for FieldOp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
struct Bits {
    readable: BitSet,
    writable: BitSet,
    never: BitSet,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Repr {
    /// Every field, both operations (what no group restriction means).
    All,
    /// Explicit bitsets indexed by the field's position in its model; a position past the sets
    /// is denied (fail closed).
    Bits(Bits),
}

/// [`FieldMask::all`] allows everything and needs no field count; [`FieldMask::full`] and
/// [`FieldMask::none`] start an explicit mask that [`FieldMask::deny`], [`FieldMask::grant`] and
/// [`FieldMask::forbid`] refine. A position outside an explicit mask is denied.
///
/// ```
/// use erp_orm::{FieldMask, FieldOp};
/// use erp_registry::ModelSpec;
///
/// /// Everything readable, but the first field is read-only.
/// fn mask_for(model: &ModelSpec) -> erp_fields::Result<FieldMask> {
///     let mut mask = FieldMask::full(model.fields.len());
///     if let Some(first) = model.fields.first() {
///         mask.deny(first.id, FieldOp::Write)?;
///     }
///     Ok(mask)
/// }
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldMask {
    repr: Repr,
}

impl FieldMask {
    /// Every field readable and writable.
    pub fn all() -> Self {
        Self { repr: Repr::All }
    }

    /// The shared [`FieldMask::all`] (what permissive policies hand out without allocating).
    pub fn shared_all() -> Arc<Self> {
        static ALL: OnceLock<Arc<FieldMask>> = OnceLock::new();
        Arc::clone(ALL.get_or_init(|| Arc::new(Self::all())))
    }

    /// An explicit mask over `field_count` fields with every field readable and writable.
    pub fn full(field_count: usize) -> Self {
        let mut bits = Bits::default();
        for i in 0..field_count {
            bits.readable.set(i);
            bits.writable.set(i);
        }
        Self {
            repr: Repr::Bits(bits),
        }
    }

    /// An explicit mask with no field readable or writable.
    pub fn none() -> Self {
        Self {
            repr: Repr::Bits(Bits::default()),
        }
    }

    /// Whether this is [`FieldMask::all`] (the engine skips field walks for it).
    pub fn is_all(&self) -> bool {
        matches!(self.repr, Repr::All)
    }

    /// Whether `field` may be read.
    pub fn readable(&self, field: FieldId) -> bool {
        match &self.repr {
            Repr::All => true,
            Repr::Bits(b) => b.readable.get(field.index()),
        }
    }

    /// Whether `field` may be written.
    pub fn writable(&self, field: FieldId) -> bool {
        match &self.repr {
            Repr::All => true,
            Repr::Bits(b) => b.writable.get(field.index()),
        }
    }

    pub fn never(&self, field: FieldId) -> bool {
        match &self.repr {
            Repr::All => false,
            Repr::Bits(b) => b.never.get(field.index()),
        }
    }

    /// Whether `op` is allowed on `field`.
    pub fn allows(&self, field: FieldId, op: FieldOp) -> bool {
        match op {
            FieldOp::Read => self.readable(field),
            FieldOp::Write => self.writable(field),
        }
    }

    fn bits_mut(&mut self) -> Result<&mut Bits> {
        match &mut self.repr {
            Repr::Bits(b) => Ok(b),
            Repr::All => Err(Error::programming(
                "FieldMask::all() has no field count and cannot be refined: start from \
                 FieldMask::full(field_count) or FieldMask::none()",
            )),
        }
    }

    /// Allow `op` on `field`.
    ///
    /// # Errors
    ///
    /// `Programming` on [`FieldMask::all`], and for a field marked [`FieldMask::forbid`]den.
    pub fn grant(&mut self, field: FieldId, op: FieldOp) -> Result<&mut Self> {
        let bits = self.bits_mut()?;
        if bits.never.get(field.index()) {
            return Err(Error::programming(format!(
                "field #{} is forbidden for everyone (groups '.') and cannot be granted",
                field.index()
            )));
        }
        match op {
            FieldOp::Read => bits.readable.set(field.index()),
            FieldOp::Write => bits.writable.set(field.index()),
        }
        Ok(self)
    }

    /// Deny `op` on `field`.
    ///
    /// # Errors
    ///
    /// `Programming` on [`FieldMask::all`].
    pub fn deny(&mut self, field: FieldId, op: FieldOp) -> Result<&mut Self> {
        let bits = self.bits_mut()?;
        match op {
            FieldOp::Read => bits.readable.clear(field.index()),
            FieldOp::Write => bits.writable.clear(field.index()),
        }
        Ok(self)
    }

    /// # Errors
    ///
    /// `Programming` on [`FieldMask::all`].
    pub fn forbid(&mut self, field: FieldId) -> Result<&mut Self> {
        let bits = self.bits_mut()?;
        bits.readable.clear(field.index());
        bits.writable.clear(field.index());
        bits.never.set(field.index());
        Ok(self)
    }

    /// The first field of `model` that this mask allows (for an operation) although `base` does
    /// not: what the narrowing check of `#[field_access]` overrides looks for.
    pub(crate) fn first_widened(&self, base: &FieldMask, model: &ModelSpec) -> Option<FieldId> {
        if self.is_all() && base.is_all() {
            return None;
        }
        model.fields.iter().map(|f| f.id).find(|f| {
            (self.readable(*f) && !base.readable(*f)) || (self.writable(*f) && !base.writable(*f))
        })
    }

    /// This mask with every permission `base` lacks removed (an intersection).
    pub(crate) fn clamped_to(&self, base: &FieldMask, model: &ModelSpec) -> Self {
        let mut bits = Bits::default();
        for f in &model.fields {
            let i = f.id.index();
            if self.readable(f.id) && base.readable(f.id) {
                bits.readable.set(i);
            }
            if self.writable(f.id) && base.writable(f.id) {
                bits.writable.set(i);
            }
            if self.never(f.id) || base.never(f.id) {
                bits.never.set(i);
            }
        }
        Self {
            repr: Repr::Bits(bits),
        }
    }

    /// The mask with `readable` taken from `read` and `writable` from `write` (the two runs of
    /// the override chain, [`crate::field_chain`]); `never` comes from `self`.
    pub(crate) fn combined(&self, read: &FieldMask, write: &FieldMask, model: &ModelSpec) -> Self {
        let mut bits = Bits::default();
        for f in &model.fields {
            let i = f.id.index();
            if read.readable(f.id) {
                bits.readable.set(i);
            }
            if write.writable(f.id) {
                bits.writable.set(i);
            }
            if self.never(f.id) {
                bits.never.set(i);
            }
        }
        Self {
            repr: Repr::Bits(bits),
        }
    }
}

/// A policy decides who sees it
/// ([`crate::AccessPolicy::field_denial_detail`]); the group names come from the policy side.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum FieldDenialDetail {
    /// `groups='.'`: forbidden for everyone.
    AlwaysForbidden,
    /// The field has no groups; an access rule of its own denied it (an override).
    CustomRules,
    /// The display names of the groups that would allow it, sorted by group id.
    Groups(Vec<String>),
}

impl FieldDenialDetail {
    /// The text after `Groups:` in the error message.
    pub fn text(&self) -> String {
        match self {
            Self::AlwaysForbidden => "always forbidden".to_owned(),
            Self::CustomRules => "custom field access rules".to_owned(),
            Self::Groups(names) => {
                let quoted: Vec<String> = names.iter().map(|n| py_repr(n)).collect();
                format!("allowed for groups {}", quoted.join(", "))
            }
        }
    }
}

/// A string as Python's `repr` writes it (the error text quotes group names this way).
fn py_repr(s: &str) -> String {
    let quote = if s.contains('\'') && !s.contains('"') {
        '"'
    } else {
        '\''
    };
    let mut out = String::with_capacity(s.len() + 2);
    out.push(quote);
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c == quote => {
                out.push('\\');
                out.push(c);
            }
            c => out.push(c),
        }
    }
    out.push(quote);
    out
}

/// The `Access` error of a denied field operation.
pub(crate) fn field_denied(
    model: &ModelSpec,
    field: &str,
    op: FieldOp,
    detail: Option<(i64, &FieldDenialDetail)>,
) -> Error {
    let mut message = format!(
        "You do not have enough rights to access the field \"{field}\" on {} ({}). Please \
         contact your system administrator.\n\nOperation: {op}",
        model.description, model.name
    );
    if let Some((uid, detail)) = detail {
        message.push_str(&format!("\nUser: {uid}\nGroups: {}", detail.text()));
    }
    Error::access(message)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn python_repr_of_group_names() {
        assert_eq!(py_repr("Role / Portal"), "'Role / Portal'");
        assert_eq!(py_repr("Bob's"), "\"Bob's\"");
        assert_eq!(py_repr("a\\b"), "'a\\\\b'");
        assert_eq!(py_repr("it's \"x\""), "'it\\'s \"x\"'");
        assert_eq!(
            FieldDenialDetail::Groups(vec!["Role / Portal".into(), "Test Group".into()]).text(),
            "allowed for groups 'Role / Portal', 'Test Group'"
        );
        assert_eq!(
            FieldDenialDetail::AlwaysForbidden.text(),
            "always forbidden"
        );
        assert_eq!(
            FieldDenialDetail::CustomRules.text(),
            "custom field access rules"
        );
    }
}
