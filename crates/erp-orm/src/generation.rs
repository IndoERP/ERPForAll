//! Immutable, registry-bound products supplied by the native composition layer.
use erp_fields::{Error, Result};
use erp_registry::Registry;
use rustc_hash::{FxHashMap, FxHashSet};
use std::any::{Any, TypeId};
use std::sync::Arc;

/// A composition product bound to one exact registry identity.
///
/// This conveys no principal, policy or database authority. A builder refuses an attachment
/// from another registry, even if its numeric generation happens to match. Rebuilds require
/// explicit new attachments; they never inherit these products from the previous generation.
pub struct GenerationAttachment<T: Any + Send + Sync> {
    registry: Arc<Registry>,
    value: Arc<T>,
}
impl<T: Any + Send + Sync> GenerationAttachment<T> {
    /// Bind an immutable composition product to the registry it was validated against.
    pub fn new(registry: Arc<Registry>, value: Arc<T>) -> Self {
        Self { registry, value }
    }
}

/// Immutable context available only while the ORM composition is still unpublished.
///
/// Runtime exposure is final here. No environment, transaction, pool or principal is exposed.
pub struct GenerationComposition<'a> {
    registry: &'a Arc<Registry>,
    exposure: &'a crate::rpc::ExposureTable,
}
impl GenerationComposition<'_> {
    /// Exact target registry, not an equivalent registry or a numeric generation key.
    pub fn registry(&self) -> &Arc<Registry> {
        self.registry
    }
    /// The validated runtime method/exposure table for the target registry.
    pub fn exposure(&self) -> &crate::rpc::ExposureTable {
        self.exposure
    }
}

/// Typed composition over immutable owned per-generation inputs, without request handles.
pub type GenerationCompose<T, I> = for<'a> fn(&GenerationComposition<'a>, &I) -> Result<Arc<T>>;

/// A factory bound to an exact registry and owned per-generation composition inputs.
///
/// Inputs must describe the actual installed set/dependency closure; the factory must validate
/// them. Native factories are trusted composition code, not a sandbox or an authority grant.
pub struct GenerationAttachmentFactory<T: Any + Send + Sync, I: Any + Send + Sync> {
    registry: Arc<Registry>,
    inputs: Arc<I>,
    compose: GenerationCompose<T, I>,
}
impl<T: Any + Send + Sync, I: Any + Send + Sync> GenerationAttachmentFactory<T, I> {
    /// Bind a typed factory and its owned inputs to the registry they were prepared for.
    pub fn new(registry: Arc<Registry>, inputs: Arc<I>, compose: GenerationCompose<T, I>) -> Self {
        Self {
            registry,
            inputs,
            compose,
        }
    }
}
trait ErasedFactory: Send + Sync {
    fn compose(
        self: Box<Self>,
        context: &GenerationComposition<'_>,
    ) -> Result<(TypeId, Arc<dyn Any + Send + Sync>)>;
}
impl<T: Any + Send + Sync, I: Any + Send + Sync> ErasedFactory
    for GenerationAttachmentFactory<T, I>
{
    fn compose(
        self: Box<Self>,
        context: &GenerationComposition<'_>,
    ) -> Result<(TypeId, Arc<dyn Any + Send + Sync>)> {
        check_identity(context.registry, &self.registry)?;
        let value = (self.compose)(context, &self.inputs)?;
        Ok((TypeId::of::<T>(), value))
    }
}
fn check_identity(target: &Arc<Registry>, prepared: &Arc<Registry>) -> Result<()> {
    if Arc::ptr_eq(target, prepared) {
        Ok(())
    } else {
        Err(Error::programming(
            "generation attachment registry identity does not match the ORM; rebuild and revalidate the composition product",
        ))
    }
}

#[derive(Default)]
pub(crate) struct GenerationAttachments {
    values: FxHashMap<TypeId, Arc<dyn Any + Send + Sync>>,
    registered: FxHashSet<TypeId>,
    factories: Vec<Box<dyn ErasedFactory>>,
}
impl GenerationAttachments {
    fn register<T: Any + Send + Sync>(&mut self) -> Result<()> {
        if self.registered.insert(TypeId::of::<T>()) {
            Ok(())
        } else {
            Err(Error::programming(
                "duplicate generation attachment: a composition product cannot replace another registration",
            ))
        }
    }
    pub(crate) fn insert<T: Any + Send + Sync>(
        &mut self,
        registry: &Arc<Registry>,
        attachment: GenerationAttachment<T>,
    ) -> Result<()> {
        check_identity(registry, &attachment.registry)?;
        self.register::<T>()?;
        self.insert_value(TypeId::of::<T>(), attachment.value)
    }
    pub(crate) fn factory<T: Any + Send + Sync, I: Any + Send + Sync>(
        &mut self,
        registry: &Arc<Registry>,
        factory: GenerationAttachmentFactory<T, I>,
    ) -> Result<()> {
        check_identity(registry, &factory.registry)?;
        self.register::<T>()?;
        self.factories.push(Box::new(factory));
        Ok(())
    }
    fn insert_value(&mut self, kind: TypeId, value: Arc<dyn Any + Send + Sync>) -> Result<()> {
        match self.values.entry(kind) {
            std::collections::hash_map::Entry::Vacant(slot) => {
                slot.insert(value);
                Ok(())
            }
            std::collections::hash_map::Entry::Occupied(_) => Err(Error::programming(
                "duplicate generation attachment during final composition",
            )),
        }
    }
    pub(crate) fn compose(
        &mut self,
        registry: &Arc<Registry>,
        exposure: &crate::rpc::ExposureTable,
    ) -> Result<()> {
        let context = GenerationComposition { registry, exposure };
        for factory in std::mem::take(&mut self.factories) {
            let (kind, value) = factory.compose(&context)?;
            self.insert_value(kind, value)?;
        }
        if self.values.len() != self.registered.len() {
            return Err(Error::programming(
                "generation composition omitted a required attachment",
            ));
        }
        Ok(())
    }
    pub(crate) fn get<T: Any + Send + Sync>(&self) -> Result<Arc<T>> {
        self.values.get(&TypeId::of::<T>()).cloned().ok_or_else(|| Error::programming("required generation attachment is unavailable; compose this registry before serving requests"))?
            .downcast::<T>().map_err(|_| Error::programming("generation attachment type invariant violated"))
    }
}
