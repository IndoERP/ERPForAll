//! Elevation (DN §4): `None`, a named scoped block, or `Full` (`sudo`).
//!
//! BASE-0 lands the values. Opening a block (`Transaction::elevated`, `elevate!`), the block
//! registry, narrowing checks and audit come with, which also adds the constructor of
//! [`BlockId`].

use std::num::NonZeroU64;
use std::sync::Arc;

use erp_registry::{FieldId, ModelId};

use super::error::PrincipalError;
use super::ids::QualifiedXmlId;
use crate::access::Operation;

/// Most items one elevation scope names.
pub const MAX_ELEVATION_ITEMS: usize = 256;
/// Most fields or lifted rules one scope item names.
pub const MAX_ITEM_ENTRIES: usize = 1_024;

/// The elevation of a principal.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Elevation {
    /// The principal's own rights.
    None,
    /// A named block opening the listed models, fields and operations (DN §4).
    Scoped {
        /// What the block opens.
        scope: Arc<ElevationScope>,
        /// The block, registered open in its transaction.
        block: BlockId,
    },
    /// Superuser mode (`sudo()`, `System`): no access checks.
    Full,
}

impl Elevation {
    /// `Full`.
    pub fn is_full(&self) -> bool {
        matches!(self, Self::Full)
    }
}

/// A set of [`Operation`]s.
///
/// ```
/// use erp_orm::{Operation, OperationSet};
/// let ops = OperationSet::of([Operation::Read, Operation::Write]);
/// assert!(ops.contains(Operation::Write) && !ops.contains(Operation::Unlink));
/// assert_eq!(ops.iter().collect::<Vec<_>>(), [Operation::Read, Operation::Write]);
/// ```
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct OperationSet(u8);

const ALL_OPS: [Operation; 4] = [
    Operation::Read,
    Operation::Write,
    Operation::Create,
    Operation::Unlink,
];

impl OperationSet {
    /// No operation.
    pub const EMPTY: Self = Self(0);

    fn bit(op: Operation) -> u8 {
        match op {
            Operation::Read => 1,
            Operation::Write => 2,
            Operation::Create => 4,
            Operation::Unlink => 8,
        }
    }

    /// The set of `ops` (repeats are the same operation).
    pub fn of(ops: impl IntoIterator<Item = Operation>) -> Self {
        Self(ops.into_iter().fold(0, |acc, op| acc | Self::bit(op)))
    }

    /// Whether `op` is in the set.
    pub fn contains(self, op: Operation) -> bool {
        self.0 & Self::bit(op) != 0
    }

    /// Whether the set is empty.
    pub fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// Whether every operation of `self` is in `other`.
    pub fn is_subset(self, other: Self) -> bool {
        self.0 & !other.0 == 0
    }

    /// The operations, in the order read, write, create, unlink.
    pub fn iter(self) -> impl Iterator<Item = Operation> {
        ALL_OPS.into_iter().filter(move |op| self.contains(*op))
    }
}

/// Fields a scope item opens.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum FieldAllow {
    /// Every field (except `groups='.'` fields, which only `Full` reaches).
    All,
    /// These fields (sorted, unique, not empty).
    Named(Box<[FieldId]>),
}

impl FieldAllow {
    /// Only `fields`.
    ///
    /// # Errors
    ///
    /// [`PrincipalError::InvalidElevationScope`] for an empty list, a duplicate or more than
    /// [`MAX_ITEM_ENTRIES`] fields.
    pub fn named(fields: impl IntoIterator<Item = FieldId>) -> Result<Self, PrincipalError> {
        let list = sorted_unique(fields, "field", |f| f.index().to_string())?;
        if list.is_empty() {
            return Err(invalid(
                "an empty field list opens nothing (leave the fields out to open them all)"
                    .to_owned(),
            ));
        }
        Ok(Self::Named(list))
    }
}

fn invalid(problem: String) -> PrincipalError {
    PrincipalError::InvalidElevationScope { problem }
}

fn sorted_unique<T: Ord>(
    items: impl IntoIterator<Item = T>,
    what: &str,
    show: impl Fn(&T) -> String,
) -> Result<Box<[T]>, PrincipalError> {
    let mut list = Vec::new();
    for item in items {
        if list.len() == MAX_ITEM_ENTRIES {
            return Err(invalid(format!(
                "more than {MAX_ITEM_ENTRIES} {what} entries"
            )));
        }
        list.push(item);
    }
    list.sort_unstable();
    if let Some(pair) = list.windows(2).find(|w| w[0] == w[1]) {
        return Err(invalid(format!(
            "{what} {} is listed twice",
            show(&pair[0])
        )));
    }
    Ok(list.into_boxed_slice())
}

/// One model an elevation scope opens: operations, fields, and the non-company record rules it
/// lifts (DN §4, R-M5). Carries no capabilities (D7, R-S12).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct ScopeItem {
    model: ModelId,
    ops: OperationSet,
    fields: FieldAllow,
    lift: Box<[QualifiedXmlId]>,
}

impl ScopeItem {
    /// `model` for `ops`, every field, nothing lifted.
    ///
    /// # Errors
    ///
    /// [`PrincipalError::InvalidElevationScope`] when `ops` is empty.
    pub fn new(model: ModelId, ops: OperationSet) -> Result<Self, PrincipalError> {
        if ops.is_empty() {
            return Err(invalid(format!(
                "the item for model {} opens no operation",
                model.index()
            )));
        }
        Ok(Self {
            model,
            ops,
            fields: FieldAllow::All,
            lift: Box::default(),
        })
    }

    /// Open only `fields`.
    #[must_use]
    pub fn fields(mut self, fields: FieldAllow) -> Self {
        self.fields = fields;
        self
    }

    /// Lift these restriction rules (company rules can never be lifted; PRINCIPAL checks that at
    /// block entry).
    ///
    /// # Errors
    ///
    /// [`PrincipalError::InvalidElevationScope`] for a duplicate or more than
    /// [`MAX_ITEM_ENTRIES`] rules.
    pub fn lift(
        mut self,
        rules: impl IntoIterator<Item = QualifiedXmlId>,
    ) -> Result<Self, PrincipalError> {
        self.lift = sorted_unique(rules, "lifted rule", |r| format!("'{r}'"))?;
        Ok(self)
    }

    /// The model.
    pub fn model(&self) -> ModelId {
        self.model
    }

    /// The operations.
    pub fn ops(&self) -> OperationSet {
        self.ops
    }

    /// The fields.
    pub fn field_allow(&self) -> &FieldAllow {
        &self.fields
    }

    /// The lifted rules (sorted).
    pub fn lifted(&self) -> &[QualifiedXmlId] {
        &self.lift
    }
}

/// What a scoped elevation block opens: items sorted by model, one per model (`Eq + Hash` by
/// value, so it can key memos; DN §2.1).
///
/// ```
/// use erp_orm::{ElevationScope, Operation, OperationSet, ScopeItem};
/// # fn ids(r: &erp_registry::Registry) -> Option<(erp_registry::ModelId, erp_registry::ModelId)> {
/// #     Some((r.model_id("base")?, r.model_id("res.users")?))
/// # }
/// # let registry = erp_registry::RegistryBuilder::new([]).build()?;
/// # let (a, b) = ids(&registry).ok_or_else(|| erp_fields::Error::programming("models"))?;
/// let scope = ElevationScope::new([
///     ScopeItem::new(b, OperationSet::of([Operation::Read]))?,
///     ScopeItem::new(a, OperationSet::of([Operation::Read, Operation::Write]))?,
/// ])?;
/// assert_eq!(scope.items()[0].model(), a.min(b));
/// # Ok::<(), erp_fields::Error>(())
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct ElevationScope {
    items: Box<[ScopeItem]>,
}

impl ElevationScope {
    /// A scope of `items`.
    ///
    /// # Errors
    ///
    /// [`PrincipalError::InvalidElevationScope`] for no items, two items for one model, or more
    /// than [`MAX_ELEVATION_ITEMS`] items.
    pub fn new(items: impl IntoIterator<Item = ScopeItem>) -> Result<Self, PrincipalError> {
        let mut list: Vec<ScopeItem> = Vec::new();
        for item in items {
            if list.len() == MAX_ELEVATION_ITEMS {
                return Err(invalid(format!("more than {MAX_ELEVATION_ITEMS} items")));
            }
            list.push(item);
        }
        if list.is_empty() {
            return Err(invalid("a scope with no item opens nothing".to_owned()));
        }
        list.sort_unstable_by_key(|i| i.model);
        if let Some(pair) = list.windows(2).find(|w| w[0].model == w[1].model) {
            return Err(invalid(format!(
                "model {} has two items (merge them into one)",
                pair[0].model.index()
            )));
        }
        Ok(Self {
            items: list.into_boxed_slice(),
        })
    }

    /// The items, sorted by model.
    pub fn items(&self) -> &[ScopeItem] {
        &self.items
    }
}

/// Serial of a transaction in this process (assigns it; never reused).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TxSerial(NonZeroU64);

impl TxSerial {
    /// The raw serial.
    pub fn get(self) -> u64 {
        self.0.get()
    }
}

/// An elevation block: its transaction and its number there. An environment whose block is not
/// open in the transaction it is used in is refused (DN §4). Only the transaction
/// creates them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct BlockId {
    tx: TxSerial,
    seq: u32,
}

impl BlockId {
    /// The transaction.
    pub fn tx(self) -> TxSerial {
        self.tx
    }

    /// The block's number in its transaction.
    pub fn seq(self) -> u32 {
        self.seq
    }
}

#[cfg(test)]
mod tests {
    use erp_registry::{Registry, RegistryBuilder};

    use super::*;

    fn registry() -> Registry {
        RegistryBuilder::new([]).build().unwrap()
    }

    fn message(e: PrincipalError) -> String {
        assert_eq!(e.kind(), erp_fields::ErrorKind::Programming);
        e.to_string()
    }

    #[test]
    fn operation_sets() {
        assert!(OperationSet::EMPTY.is_empty());
        let rw = OperationSet::of([Operation::Write, Operation::Read, Operation::Read]);
        assert_eq!(
            rw.iter().collect::<Vec<_>>(),
            [Operation::Read, Operation::Write]
        );
        assert!(OperationSet::of([Operation::Read]).is_subset(rw));
        assert!(!rw.is_subset(OperationSet::of([Operation::Read])));
        assert!(OperationSet::EMPTY.is_subset(OperationSet::EMPTY));
        let all = OperationSet::of(ALL_OPS);
        assert_eq!(all.iter().count(), 4);
    }

    #[test]
    fn scope_items_and_scopes_validate() {
        let r = registry();
        let base = r.model_id("base").unwrap();
        let users = r.model_id("res.users").unwrap();
        let read = OperationSet::of([Operation::Read]);
        assert_eq!(
            message(ScopeItem::new(base, OperationSet::EMPTY).unwrap_err()),
            format!(
                "invalid elevation scope: the item for model {} opens no operation",
                base.index()
            )
        );
        assert_eq!(
            message(ElevationScope::new([]).unwrap_err()),
            "invalid elevation scope: a scope with no item opens nothing"
        );
        let dup = ElevationScope::new([
            ScopeItem::new(users, read).unwrap(),
            ScopeItem::new(base, read).unwrap(),
            ScopeItem::new(users, OperationSet::of([Operation::Write])).unwrap(),
        ])
        .unwrap_err();
        assert_eq!(
            message(dup),
            format!(
                "invalid elevation scope: model {} has two items (merge them into one)",
                users.index()
            )
        );
        let name = users_field(&r, "name");
        let login = users_field(&r, "login");
        assert_eq!(
            message(FieldAllow::named([]).unwrap_err()),
            "invalid elevation scope: an empty field list opens nothing (leave the fields out \
             to open them all)"
        );
        assert_eq!(
            message(FieldAllow::named([name, name]).unwrap_err()),
            format!(
                "invalid elevation scope: field {} is listed twice",
                name.index()
            )
        );
        let rule = QualifiedXmlId::parse("base.rule").unwrap();
        assert_eq!(
            message(
                ScopeItem::new(users, read)
                    .unwrap()
                    .lift([rule.clone(), rule.clone()])
                    .unwrap_err()
            ),
            "invalid elevation scope: lifted rule 'base.rule' is listed twice"
        );
        let item = ScopeItem::new(users, read)
            .unwrap()
            .fields(FieldAllow::named([login, name]).unwrap())
            .lift([QualifiedXmlId::parse("m.b").unwrap(), rule.clone()])
            .unwrap();
        assert_eq!(item.lifted()[0], rule);
        let a = ElevationScope::new([item.clone(), ScopeItem::new(base, read).unwrap()]).unwrap();
        let b = ElevationScope::new([ScopeItem::new(base, read).unwrap(), item]).unwrap();
        assert_eq!(a, b, "input order does not matter");
        assert_eq!(a.items()[0].model(), base.min(users));
        let rules: Vec<QualifiedXmlId> = (0..=MAX_ITEM_ENTRIES)
            .map(|i| QualifiedXmlId::parse(&format!("m.r{i}")).unwrap())
            .collect();
        assert_eq!(
            message(
                ScopeItem::new(base, read)
                    .unwrap()
                    .lift(rules.iter().cloned())
                    .unwrap_err()
            ),
            "invalid elevation scope: more than 1024 lifted rule entries"
        );
        assert!(
            ScopeItem::new(base, read)
                .unwrap()
                .lift(rules[..MAX_ITEM_ENTRIES].iter().cloned())
                .is_ok()
        );
    }

    fn users_field(r: &Registry, name: &str) -> FieldId {
        r.model("res.users").unwrap().field(name).unwrap().id
    }
}
