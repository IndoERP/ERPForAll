//! The complete reference index of one payload walk, charged to the request's construction budget
//! before each growth, with owned storage for paths derived from expressions.
use crate::expression_references::Found;
use crate::{DescriptionBudget, DescriptionReservation, FieldReference, ViewError, ViewResult};
use std::borrow::Cow;
use std::mem::size_of;

/// One scoped path: borrowed from the payload or the schema when it was written there, owned when
/// derived by joining the segments of an expression's attribute chain.
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct Entry<'a> {
    // `None` sorts before `Some`, so root paths precede model paths exactly like `FieldReference`.
    model: Option<Cow<'a, str>>,
    path: Cow<'a, str>,
}

/// Complete scoped references with their construction-storage ownership.
///
/// Derived paths are owned by this index: [`SchemaReferences::fields`] borrows the index, never the
/// payload that produced them, so a path built from an expression outlives nothing it should not.
#[derive(Debug)]
pub struct SchemaReferences<'a> {
    entries: Vec<Entry<'a>>,
    _slots: DescriptionReservation,
    _strings: DescriptionReservation,
}
impl SchemaReferences<'_> {
    /// Every distinct declared field reference, preserving root versus explicit model scope.
    pub fn fields(&self) -> impl Iterator<Item = FieldReference<'_>> + '_ {
        self.entries.iter().map(|entry| match &entry.model {
            None => FieldReference::RootPath(&entry.path),
            Some(model) => FieldReference::ModelPath {
                model,
                path: &entry.path,
            },
        })
    }
}

fn refuse(budget: &DescriptionBudget, message: &str) -> ViewError {
    budget.poison();
    ViewError::InvalidDescription(message.to_owned())
}

/// Collects references while the walk runs. Each growth and each owned string is reserved against
/// the shared budget before it is allocated; the previous buffer is counted until it is released.
pub(crate) struct Collector<'a> {
    budget: DescriptionBudget,
    entries: Vec<Entry<'a>>,
    /// The entry buffer.
    slots: DescriptionReservation,
    /// Owned derived path strings.
    strings: DescriptionReservation,
}
impl<'a> Collector<'a> {
    pub(crate) fn new(budget: &DescriptionBudget) -> ViewResult<Self> {
        budget.check()?;
        Ok(Self {
            budget: budget.clone(),
            entries: Vec::new(),
            slots: budget.reserve(0)?,
            strings: budget.reserve(0)?,
        })
    }
    /// Make room for one more entry. The new buffer is reserved while the old one is still held,
    /// because a reallocation keeps both alive; the old reservation is released afterwards.
    fn room(&mut self) -> ViewResult<()> {
        let capacity = self.entries.capacity();
        if self.entries.len() < capacity {
            return Ok(());
        }
        let wanted = capacity.max(2).saturating_mul(2);
        let bytes = wanted
            .checked_mul(size_of::<Entry<'_>>())
            .ok_or_else(|| refuse(&self.budget, "Description reference storage overflow"))?;
        let mut grown = self.budget.reserve(bytes)?;
        if self.entries.try_reserve_exact(wanted - capacity).is_err() {
            return Err(refuse(
                &self.budget,
                "Description reference allocation failed",
            ));
        }
        if self.entries.capacity() > wanted {
            let extra = (self.entries.capacity() - wanted)
                .checked_mul(size_of::<Entry<'_>>())
                .ok_or_else(|| refuse(&self.budget, "Description reference storage overflow"))?;
            grown.grow(extra)?;
        }
        self.slots = grown;
        Ok(())
    }
    pub(crate) fn push_borrowed(
        &mut self,
        model: Option<&'a str>,
        path: &'a str,
    ) -> ViewResult<()> {
        self.room()?;
        self.entries.push(Entry {
            model: model.map(Cow::Borrowed),
            path: Cow::Borrowed(path),
        });
        Ok(())
    }
    /// Record a reference derived from an expression: the joined path is owned by the index.
    pub(crate) fn push_found(
        &mut self,
        scope: Option<&'a str>,
        parent_model: Option<&'a str>,
        found: Found<'_>,
    ) -> ViewResult<()> {
        let model = if found.parent { parent_model } else { scope };
        let overflow = || refuse(&self.budget, "Description reference storage overflow");
        let separators = found.parts.len().saturating_sub(1);
        let length = found
            .parts
            .iter()
            .try_fold(separators, |total, part| total.checked_add(part.len()))
            .ok_or_else(overflow)?;
        self.room()?;
        self.strings.grow(length)?;
        let mut path = String::new();
        if path.try_reserve_exact(length).is_err() {
            return Err(refuse(
                &self.budget,
                "Description reference allocation failed",
            ));
        }
        if path.capacity() > length {
            self.strings.grow(path.capacity() - length)?;
        }
        for (index, part) in found.parts.iter().enumerate() {
            if index > 0 {
                path.push('.');
            }
            path.push_str(part);
        }
        self.entries.push(Entry {
            model: model.map(Cow::Borrowed),
            path: Cow::Owned(path),
        });
        Ok(())
    }
    /// Preserve the criterion/value role before deduplication; same spellings may name two models.
    pub(crate) fn push_scoped_found(&mut self, model: &'a str, found: Found<'_>) -> ViewResult<()> {
        self.push_found(
            if found.criterion { Some(model) } else { None },
            None,
            found,
        )
    }
    pub(crate) fn finish(mut self) -> ViewResult<SchemaReferences<'a>> {
        self.entries.sort_unstable();
        self.entries.dedup();
        self.budget.check()?;
        Ok(SchemaReferences {
            entries: self.entries,
            _slots: self.slots,
            _strings: self.strings,
        })
    }
}
