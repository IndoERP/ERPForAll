//! Values for `create`/`write`.
//!
//! - [`Vals<M>`]: typed values for model `M`, built with field handles of `M` or of the models it
//!   inherits (`records.vals().set(Partner::NAME, "x")`); a handle of another model does not
//!   compile.
//! - [`RawVals`]: field name → cell, what the method chain carries (overrides of `create`/`write`
//!   receive them) and what the dynamic surface builds through the one converter
//!   ([`crate::Records::vals_from_values`]).
//!
//! Whatever builds them, `create` and `write` validate every cell against its field
//! ([`erp_fields::convert::validate_cell`]).
//!
//! One2many and many2many values are lists of [`Command`]s ([`RawVals::set_commands`],
//! [`Vals::commands`]); a plain list of ids ([`Vals::set`] with a `Vec<RecId>`) means `SET`.

use std::borrow::Cow;
use std::marker::PhantomData;

use erp_fields::{Cell, FieldValue, IntoFieldValue};

use crate::commands::Command;
use crate::records::{Field, Inherits, Model};

/// Untyped field values for one record, in insertion order (a later set of the same field replaces
/// it). Scalar and many2one values are cells; x2many values are command lists.
///
/// ```
/// use erp_orm::RawVals;
/// use erp_fields::Cell;
///
/// let vals = RawVals::new().set_cell("name", Cell::Str("Book".into()));
/// assert_eq!(vals.get("name"), Some(&Cell::Str("Book".into())));
/// ```
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RawVals {
    items: Vec<(Cow<'static, str>, Cell)>,
    commands: Vec<(Cow<'static, str>, Vec<Command>)>,
}

impl RawVals {
    /// No values.
    pub fn new() -> Self {
        Self::default()
    }

    /// Set a cell by field name (checked by `create`/`write`).
    #[must_use]
    pub fn set_cell(mut self, name: impl Into<Cow<'static, str>>, cell: Cell) -> Self {
        self.insert(name.into(), cell);
        self
    }

    /// Insert or replace.
    pub fn insert(&mut self, name: Cow<'static, str>, cell: Cell) {
        match self.items.iter_mut().find(|(n, _)| *n == name) {
            Some(slot) => slot.1 = cell,
            None => self.items.push((name, cell)),
        }
    }

    /// The cell of a field.
    pub fn get(&self, name: &str) -> Option<&Cell> {
        self.items.iter().find(|(n, _)| n == name).map(|(_, c)| c)
    }

    /// Remove a field.
    pub fn remove(&mut self, name: &str) -> Option<Cell> {
        let pos = self.items.iter().position(|(n, _)| n == name)?;
        Some(self.items.remove(pos).1)
    }

    /// True when the field is set.
    pub fn contains(&self, name: &str) -> bool {
        self.get(name).is_some()
    }

    /// `(name, cell)` pairs (x2many command lists excluded, see [`RawVals::iter_commands`]).
    pub fn iter(&self) -> impl Iterator<Item = (&str, &Cell)> {
        self.items.iter().map(|(n, c)| (n.as_ref(), c))
    }

    /// Every field name given, plain values first, then x2many command lists.
    pub(crate) fn names(&self) -> impl Iterator<Item = &str> {
        self.items
            .iter()
            .map(|(n, _)| n.as_ref())
            .chain(self.commands.iter().map(|(n, _)| n.as_ref()))
    }

    /// Number of fields, x2many command lists included.
    pub fn len(&self) -> usize {
        self.items.len() + self.commands.len()
    }

    /// No fields.
    pub fn is_empty(&self) -> bool {
        self.items.is_empty() && self.commands.is_empty()
    }

    /// Set the commands of an x2many field (an empty list changes nothing).
    #[must_use]
    pub fn set_commands(
        mut self,
        name: impl Into<Cow<'static, str>>,
        commands: impl IntoIterator<Item = Command>,
    ) -> Self {
        self.insert_commands(name.into(), commands.into_iter().collect());
        self
    }

    /// Insert or replace the commands of an x2many field.
    pub fn insert_commands(&mut self, name: Cow<'static, str>, commands: Vec<Command>) {
        match self.commands.iter_mut().find(|(n, _)| *n == name) {
            Some(slot) => slot.1 = commands,
            None => self.commands.push((name, commands)),
        }
    }

    /// The commands of an x2many field.
    pub fn commands(&self, name: &str) -> Option<&[Command]> {
        self.commands
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, c)| c.as_slice())
    }

    /// Remove the commands of an x2many field.
    pub fn remove_commands(&mut self, name: &str) -> Option<Vec<Command>> {
        let pos = self.commands.iter().position(|(n, _)| n == name)?;
        Some(self.commands.remove(pos).1)
    }

    /// `(name, commands)` pairs of the x2many fields.
    pub fn iter_commands(&self) -> impl Iterator<Item = (&str, &[Command])> {
        self.commands
            .iter()
            .map(|(n, c)| (n.as_ref(), c.as_slice()))
    }
}

/// Typed field values for one record of model `M`.
pub struct Vals<M> {
    raw: RawVals,
    _m: PhantomData<fn() -> M>,
}

impl<M> Clone for Vals<M> {
    fn clone(&self) -> Self {
        Self {
            raw: self.raw.clone(),
            _m: PhantomData,
        }
    }
}

impl<M> Default for Vals<M> {
    fn default() -> Self {
        Self {
            raw: RawVals::default(),
            _m: PhantomData,
        }
    }
}

impl<M> std::fmt::Debug for Vals<M> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.raw.fmt(f)
    }
}

impl<M: Model> Vals<M> {
    /// No values.
    pub fn new() -> Self {
        Self::default()
    }

    /// Set a field of `M` (or of a model `M` inherits).
    #[must_use]
    pub fn set<P: Model, T: FieldValue, V: IntoFieldValue<T>>(
        mut self,
        field: Field<P, T>,
        value: V,
    ) -> Self
    where
        M: Inherits<P>,
    {
        self.raw.insert(
            Cow::Borrowed(field.name()),
            value.into_field_value().into_cell(),
        );
        self
    }

    /// Commands for an x2many field of `M`: [`Command`]s for a `Vec<RecId>` field,
    /// [`crate::Cmd<C>`] (values of the comodel `C` only) for an `X2many<C>` field.
    #[must_use]
    pub fn commands<P: Model, T: crate::typed_rel::X2manyValue>(
        mut self,
        field: Field<P, T>,
        commands: impl IntoIterator<Item = T::Command>,
    ) -> Self
    where
        M: Inherits<P>,
    {
        self.raw.insert_commands(
            Cow::Borrowed(field.name()),
            commands.into_iter().map(T::command).collect(),
        );
        self
    }

    /// The untyped values.
    pub fn into_raw(self) -> RawVals {
        self.raw
    }

    /// The untyped values, borrowed.
    pub fn as_raw(&self) -> &RawVals {
        &self.raw
    }
}

impl<M> From<Vals<M>> for RawVals {
    fn from(vals: Vals<M>) -> Self {
        vals.raw
    }
}
