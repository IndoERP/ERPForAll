//! Relational values typed by their comodel (review item 14).
//!
//! A field declared `Many2one<Partner>` or `X2many<Line>` (instead of the untyped
//! `Option<RecId>` / `Vec<RecId>`) carries its comodel in the type, so that:
//!
//! - [`Vals::commands`] on it only accepts [`Cmd<Line>`] commands, and [`Cmd::create`] /
//!   [`Cmd::update`] only accept `Vals<Line>` (values of another model do not compile);
//! - [`RecordSet::related`] returns `RecordSet<Line>` / `RecordSet<Partner>`;
//! - a `RecordSet<Partner>` (empty or singleton) can be written to a `Many2one<Partner>`.
//!
//! ```compile_fail
//! # use erp_orm::{Cmd, Model, Vals};
//! # enum Line {} impl Model for Line { const MODEL_NAME: &'static str = "l"; const KIND: erp_registry::ModelKind = erp_registry::ModelKind::Regular; }
//! # enum Tag {} impl Model for Tag { const MODEL_NAME: &'static str = "t"; const KIND: erp_registry::ModelKind = erp_registry::ModelKind::Regular; }
//! // a Tag's values cannot create a Line
//! let command: Cmd<Line> = Cmd::create(Vals::<Tag>::new());
//! ```
//!
//! ```
//! # use erp_orm::{Cmd, Model, Vals};
//! # enum Line {} impl Model for Line { const MODEL_NAME: &'static str = "l"; const KIND: erp_registry::ModelKind = erp_registry::ModelKind::Regular; }
//! // a Line's values create a Line
//! let command: Cmd<Line> = Cmd::create(Vals::<Line>::new());
//! ```
//!
//! The untyped forms stay available for fields declared with `Option<RecId>` / `Vec<RecId>` and
//! on the dynamic surface (`Command`, `Records`).

use std::marker::PhantomData;

use erp_fields::kinds;
use erp_fields::{Cell, Error, FieldValue, IntoFieldValue, RecId, RefType, Result, ValueFor};

use crate::commands::Command;
use crate::records::{Field, Inherits, Model, RecordSet};
use crate::vals::Vals;

/// A many2one value whose comodel is `C`: the target's id, or none.
pub struct Many2one<C> {
    id: Option<RecId>,
    _c: PhantomData<fn() -> C>,
}

impl<C> Many2one<C> {
    /// A value pointing at `id` (or at nothing).
    pub const fn new(id: Option<RecId>) -> Self {
        Self {
            id,
            _c: PhantomData,
        }
    }

    /// The target's id.
    pub const fn id(&self) -> Option<RecId> {
        self.id
    }

    /// No target.
    pub const fn is_none(&self) -> bool {
        self.id.is_none()
    }

    /// A target.
    pub const fn is_some(&self) -> bool {
        self.id.is_some()
    }
}

impl<C> Clone for Many2one<C> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<C> Copy for Many2one<C> {}

impl<C> PartialEq for Many2one<C> {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
    }
}

impl<C> Eq for Many2one<C> {}

impl<C> std::hash::Hash for Many2one<C> {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.id.hash(state);
    }
}

impl<C> std::fmt::Debug for Many2one<C> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Many2one({:?})", self.id)
    }
}

impl<C: Model> FieldValue for Many2one<C> {
    fn from_cell(cell: &Cell) -> Result<Self> {
        Option::<RecId>::from_cell(cell).map(Self::new)
    }

    fn into_cell(self) -> Cell {
        self.id.into_cell()
    }
}

impl<C: Model> ValueFor<kinds::Many2one> for Many2one<C> {}

impl<C: Model> RefType for Many2one<C> {
    fn field_kinds() -> &'static [&'static str] {
        &["many2one"]
    }

    fn comodel() -> Option<&'static str> {
        Some(C::MODEL_NAME)
    }
}

impl<C: Model> IntoFieldValue<Many2one<C>> for RecId {
    fn into_field_value(self) -> Many2one<C> {
        Many2one::new(Some(self))
    }
}

impl<C: Model> IntoFieldValue<Many2one<C>> for Option<RecId> {
    fn into_field_value(self) -> Many2one<C> {
        Many2one::new(self)
    }
}

/// An x2many value whose comodel is `C`: the linked ids in the comodel's order.
pub struct X2many<C> {
    ids: Vec<RecId>,
    _c: PhantomData<fn() -> C>,
}

impl<C> X2many<C> {
    /// A value listing `ids` (written as `SET`).
    pub fn new(ids: Vec<RecId>) -> Self {
        Self {
            ids,
            _c: PhantomData,
        }
    }

    /// The linked ids.
    pub fn ids(&self) -> &[RecId] {
        &self.ids
    }

    /// The number of linked records.
    pub fn len(&self) -> usize {
        self.ids.len()
    }

    /// No linked record.
    pub fn is_empty(&self) -> bool {
        self.ids.is_empty()
    }
}

impl<C> Clone for X2many<C> {
    fn clone(&self) -> Self {
        Self::new(self.ids.clone())
    }
}

impl<C> PartialEq for X2many<C> {
    fn eq(&self, other: &Self) -> bool {
        self.ids == other.ids
    }
}

impl<C> std::fmt::Debug for X2many<C> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "X2many({:?})", self.ids)
    }
}

impl<C: Model> FieldValue for X2many<C> {
    fn from_cell(cell: &Cell) -> Result<Self> {
        Vec::<RecId>::from_cell(cell).map(Self::new)
    }

    fn into_cell(self) -> Cell {
        self.ids.into_cell()
    }
}

impl<C: Model> ValueFor<kinds::X2many> for X2many<C> {}

impl<C: Model> RefType for X2many<C> {
    fn field_kinds() -> &'static [&'static str] {
        &["one2many", "many2many"]
    }

    fn comodel() -> Option<&'static str> {
        Some(C::MODEL_NAME)
    }
}

/// An x2many command for a field whose comodel is `C`: its values are `Vals<C>`.
pub struct Cmd<C> {
    command: Command,
    _c: PhantomData<fn() -> C>,
}

impl<C: Model> Cmd<C> {
    fn wrap(command: Command) -> Self {
        Self {
            command,
            _c: PhantomData,
        }
    }

    /// `CREATE` with values of the comodel.
    pub fn create(vals: Vals<C>) -> Self {
        Self::wrap(Command::create(vals.into_raw()))
    }

    /// `UPDATE` of one of the field's records.
    pub fn update(id: RecId, vals: Vals<C>) -> Self {
        Self::wrap(Command::update(id, vals.into_raw()))
    }

    /// `DELETE`.
    pub fn delete(id: RecId) -> Self {
        Self::wrap(Command::delete(id))
    }

    /// `UNLINK`.
    pub fn unlink(id: RecId) -> Self {
        Self::wrap(Command::unlink(id))
    }

    /// `LINK`.
    pub fn link(id: RecId) -> Self {
        Self::wrap(Command::link(id))
    }

    /// `CLEAR`.
    pub fn clear() -> Self {
        Self::wrap(Command::clear())
    }

    /// `SET`.
    pub fn set(ids: impl IntoIterator<Item = RecId>) -> Self {
        Self::wrap(Command::set(ids))
    }

    /// The untyped command.
    pub fn into_command(self) -> Command {
        self.command
    }
}

/// The Rust types of x2many fields and the commands they take: `Vec<RecId>` takes untyped
/// [`Command`]s, `X2many<C>` takes [`Cmd<C>`].
pub trait X2manyValue: FieldValue {
    /// The command type.
    type Command;
    /// The untyped command.
    fn command(command: Self::Command) -> Command;
}

impl X2manyValue for Vec<RecId> {
    type Command = Command;
    fn command(command: Command) -> Command {
        command
    }
}

impl<C: Model> X2manyValue for X2many<C> {
    type Command = Cmd<C>;
    fn command(command: Cmd<C>) -> Command {
        command.into_command()
    }
}

/// Relational field types that name their comodel.
pub trait TypedRelation: FieldValue {
    /// The comodel.
    type Comodel: Model;
}

impl<C: Model> TypedRelation for Many2one<C> {
    type Comodel = C;
}

impl<C: Model> TypedRelation for X2many<C> {
    type Comodel = C;
}

impl<M: Model> RecordSet<M> {
    /// The records reached through a typed relational field (`order.partner_id`,
    /// `order.line_ids`), as a recordset of the comodel; see [`crate::Records::related`].
    pub async fn related<P: Model, T: TypedRelation>(
        &self,
        tx: &mut crate::Transaction,
        field: Field<P, T>,
    ) -> Result<RecordSet<T::Comodel>>
    where
        M: Inherits<P>,
    {
        let records = self.as_records().related(tx, field.name()).await?;
        if records.model_name() != T::Comodel::MODEL_NAME {
            return Err(Error::programming(format!(
                "{}.{} points to {}, not to {}",
                self.as_records().model_name(),
                field.name(),
                records.model_name(),
                T::Comodel::MODEL_NAME
            )));
        }
        Ok(RecordSet::unchecked(records))
    }
}
