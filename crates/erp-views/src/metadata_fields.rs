//! Metadata field sets: which fields a view type's description needs metadata for.
//!
//! A set is a union of explicit validated handles, the references found in the arch/payload, and
//! registry predicates. The host intersects the union with the per-model field mask after
//! expansion; a declaration never widens access.
use crate::description_schema::{field_path, model_reference};
use crate::{FieldReference, ViewError, ViewResult};

const MAX_ENTRIES: usize = 256;

fn invalid(message: &str) -> ViewError {
    ViewError::InvalidDescription(message.to_owned())
}

/// A registry predicate over the root model's fields, resolved by the host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum FieldPredicate {
    /// Every field the caller may read (the search request's custom filter editor).
    AllReadable,
    /// Every readable numeric field.
    Numeric,
    /// Every readable field that can be grouped by.
    Groupable,
}
impl FieldPredicate {
    /// The stable predicate name.
    pub const fn name(self) -> &'static str {
        match self {
            Self::AllReadable => "all_readable",
            Self::Numeric => "numeric",
            Self::Groupable => "groupable",
        }
    }
    /// Resolve a predicate by name; unknown names refuse.
    pub fn from_name(name: &str) -> ViewResult<Self> {
        [Self::AllReadable, Self::Numeric, Self::Groupable]
            .into_iter()
            .find(|predicate| predicate.name() == name)
            .ok_or_else(|| invalid("Unknown metadata field predicate"))
    }
}

/// A validated metadata field set. Built only through [`MetadataFieldSet::builder`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MetadataFieldSet {
    explicit: Vec<(Option<String>, String)>,
    arch_references: bool,
    predicates: Vec<FieldPredicate>,
}
impl MetadataFieldSet {
    /// Start a declaration.
    pub fn builder() -> MetadataFieldSetBuilder {
        MetadataFieldSetBuilder::default()
    }
    /// Whether every field reference found in the built description is included.
    pub fn includes_arch_references(&self) -> bool {
        self.arch_references
    }
    /// The declared registry predicates, in declaration order.
    pub fn predicates(&self) -> &[FieldPredicate] {
        &self.predicates
    }
    /// The explicit handles, in declaration order, keeping their model scope.
    pub fn explicit(&self) -> impl Iterator<Item = FieldReference<'_>> + '_ {
        self.explicit.iter().map(|(model, path)| match model {
            None => FieldReference::RootPath(path),
            Some(model) => FieldReference::ModelPath { model, path },
        })
    }
}

/// Collects a declaration; [`MetadataFieldSetBuilder::finish`] validates all of it.
#[derive(Debug, Default)]
pub struct MetadataFieldSetBuilder {
    explicit: Vec<(Option<String>, String)>,
    arch_references: bool,
    predicates: Vec<FieldPredicate>,
    overflow: bool,
}
impl MetadataFieldSetBuilder {
    fn push(mut self, model: Option<&str>, path: &str) -> Self {
        if self.explicit.len() >= MAX_ENTRIES {
            self.overflow = true;
        } else {
            self.explicit
                .push((model.map(str::to_owned), path.to_owned()));
        }
        self
    }
    /// Add a field path resolved from the root model.
    pub fn root_path(self, path: &str) -> Self {
        self.push(None, path)
    }
    /// Add a field path resolved from this explicit model, never the root.
    pub fn model_path(self, model: &str, path: &str) -> Self {
        self.push(Some(model), path)
    }
    /// Include every field reference found in the built description.
    pub fn arch_references(mut self) -> Self {
        self.arch_references = true;
        self
    }
    /// Include a registry predicate.
    pub fn predicate(mut self, predicate: FieldPredicate) -> Self {
        if self.predicates.len() <= MAX_ENTRIES {
            self.predicates.push(predicate);
        }
        self
    }
    /// Validate and seal the set.
    pub fn finish(self) -> ViewResult<MetadataFieldSet> {
        if self.overflow {
            return Err(invalid("Metadata field set exceeds 256 entries"));
        }
        if self.explicit.is_empty() && !self.arch_references && self.predicates.is_empty() {
            return Err(invalid(
                "Metadata field set declares nothing; name a handle, arch references or a predicate",
            ));
        }
        for (index, (model, path)) in self.explicit.iter().enumerate() {
            if model
                .as_deref()
                .is_some_and(|model| !model_reference(model))
            {
                return Err(invalid("Metadata field set has an invalid model"));
            }
            if path.is_empty() || !field_path(path) {
                return Err(invalid("Metadata field set has an invalid field path"));
            }
            if self.explicit[..index].contains(&(model.clone(), path.clone())) {
                return Err(invalid("Metadata field set repeats a field handle"));
            }
        }
        for (index, predicate) in self.predicates.iter().enumerate() {
            if self.predicates[..index].contains(predicate) {
                return Err(invalid("Metadata field set repeats a predicate"));
            }
        }
        Ok(MetadataFieldSet {
            explicit: self.explicit,
            arch_references: self.arch_references,
            predicates: self.predicates,
        })
    }
}
