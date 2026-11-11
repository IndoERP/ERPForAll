//! [`AccessCtx`]: what `#[access_domain]` overrides and grant sources see of a principal (DN §2.2).

use erp_domain::{PrincipalParam, Value};
use erp_fields::{Error, Result};
use erp_registry::{ModelSpec, Registry};

use crate::principal::{ElevationKey, GroupId, GroupSet, KindClass, QualifiedXmlId};

/// The kind of an elevation, without its scope's contents.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ElevationKind {
    /// No elevation.
    None,
    /// A scoped elevation block.
    Scoped,
    /// Superuser mode.
    Full,
}

impl ElevationKind {
    /// The kind of an elevation key.
    pub fn of(key: &ElevationKey) -> Self {
        match key {
            ElevationKey::None => Self::None,
            ElevationKey::Scoped(_) => Self::Scoped,
            ElevationKey::Full => Self::Full,
        }
    }
}

/// Resolves group external ids for [`AccessCtx::has_group`]; implemented by the policy snapshot
/// (its group graph knows the external ids).
pub trait GroupLookup: Send + Sync {
    /// The group with external id `xmlid` (`module.name`), or `None` when the snapshot has no such
    /// group.
    ///
    /// # Errors
    ///
    /// A lookup that cannot be answered.
    fn group_id(&self, xmlid: &QualifiedXmlId) -> Result<Option<GroupId>>;
}

/// The parts of an [`AccessCtx`] ([`AccessCtx::new`]).
#[derive(Clone, Copy)]
#[non_exhaustive]
pub struct AccessCtxParts<'a> {
    /// The principal's effective groups.
    pub groups: &'a GroupSet,
    /// The principal's kind class.
    pub class: KindClass,
    /// The elevation kind.
    pub elevation: ElevationKind,
    /// The model decided on.
    pub model: &'a ModelSpec,
    /// The registry.
    pub registry: &'a Registry,
    /// External-id resolution of the snapshot's groups.
    pub lookup: &'a dyn GroupLookup,
}

impl<'a> AccessCtxParts<'a> {
    /// The parts of a context.
    pub fn new(
        groups: &'a GroupSet,
        class: KindClass,
        elevation: ElevationKind,
        model: &'a ModelSpec,
        registry: &'a Registry,
        lookup: &'a dyn GroupLookup,
    ) -> Self {
        Self {
            groups,
            class,
            elevation,
            model,
            registry,
            lookup,
        }
    }
}

/// What an `#[access_domain]` override or a [`super::GrantSource`] may know of the principal: its
/// groups, kind class and elevation kind, and the model; no uid, no company values, no context,
/// no I/O (R-M1). Principal values enter a domain only as parameters ([`AccessCtx::param`]), so an
/// override's result is a function of the memo key (model, op, group set, shape).
#[derive(Clone, Copy)]
pub struct AccessCtx<'a> {
    parts: AccessCtxParts<'a>,
}

impl<'a> AccessCtx<'a> {
    /// A context over `parts` (built by the policy snapshot).
    pub fn new(parts: AccessCtxParts<'a>) -> Self {
        Self { parts }
    }

    /// Whether the principal's effective groups contain the group `xmlid` (`module.name`).
    ///
    /// # Errors
    ///
    /// `Programming` when `xmlid` is malformed or names no group of the snapshot (never `false`:
    /// a typo must not silently deny or allow).
    pub fn has_group(&self, xmlid: &str) -> Result<bool> {
        let parsed = QualifiedXmlId::parse(xmlid)?;
        match self.parts.lookup.group_id(&parsed)? {
            Some(group) => Ok(self.parts.groups.contains(group)),
            None => Err(Error::programming(format!(
                "has_group('{xmlid}') in an access override of '{}': no such group",
                self.parts.model.name
            ))),
        }
    }

    /// The principal's kind class.
    pub fn class(&self) -> KindClass {
        self.parts.class
    }

    /// The elevation kind (no scope contents).
    pub fn elevation(&self) -> ElevationKind {
        self.parts.elevation
    }

    /// The model decided on.
    pub fn model(&self) -> &'a ModelSpec {
        self.parts.model
    }

    /// The registry.
    pub fn registry(&self) -> &'a Registry {
        self.parts.registry
    }

    /// A parameter leaf for `param` (never its value).
    pub fn param(&self, param: PrincipalParam) -> Value {
        Value::Param(param)
    }
}

impl std::fmt::Debug for AccessCtx<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AccessCtx")
            .field("groups", &self.parts.groups)
            .field("class", &self.parts.class)
            .field("elevation", &self.parts.elevation)
            .field("model", &self.parts.model.name)
            .finish_non_exhaustive()
    }
}
