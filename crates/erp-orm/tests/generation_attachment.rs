//! Registry-generation services are immutable composition products, never request authority.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]
use erp_fields::ErrorKind;
use erp_orm::{Context, GenerationAttachment, Orm, SUPERUSER_ID, StaticPolicy};
use erp_registry::{AddonDescriptor, Registry, RegistryBuilder};
use std::sync::Arc;

fn registry() -> Arc<Registry> {
    Arc::new(
        RegistryBuilder::new(Vec::<&'static AddonDescriptor>::new())
            .build()
            .unwrap(),
    )
}
fn builder(registry: Arc<Registry>) -> erp_orm::OrmBuilder {
    Orm::builder(
        registry,
        erp_orm::pool::create_pool(tokio_postgres::Config::new(), 1).unwrap(),
    )
    .policy(StaticPolicy::SuperuserOnly)
}
#[derive(Debug)]
struct Descriptor(&'static str);

#[test]
fn generation_attachment_survives_principal_context_changes_but_not_rebuild() {
    let original = registry();
    let value = Arc::new(Descriptor("original"));
    let orm = builder(original.clone())
        .generation_attachment(GenerationAttachment::new(original.clone(), value.clone()))
        .build()
        .unwrap();
    let env = orm.env(SUPERUSER_ID, Context::new());
    assert!(Arc::ptr_eq(
        &value,
        &env.generation_attachment::<Descriptor>().unwrap()
    ));
    let changed = env
        .with_user(7)
        .with_debug(true)
        .with_context([("test", erp_fields::Value::Bool(true))]);
    assert_eq!(
        changed.generation_attachment::<Descriptor>().unwrap().0,
        "original"
    );
    let next = registry();
    let rebuilt = orm.rebuild(next.clone()).build().unwrap();
    let missing = rebuilt
        .env(SUPERUSER_ID, Context::new())
        .generation_attachment::<Descriptor>()
        .unwrap_err();
    assert_eq!(missing.kind(), ErrorKind::Programming);
    assert!(
        missing
            .message()
            .contains("generation attachment is unavailable")
    );
    // A discarded candidate cannot replace the old generation's services.
    assert_eq!(
        env.generation_attachment::<Descriptor>().unwrap().0,
        "original"
    );
    let stale = orm
        .rebuild(next)
        .generation_attachment(GenerationAttachment::new(original, value))
        .build();
    let Err(error) = stale else {
        panic!("stale registry service was accepted")
    };
    assert_eq!(error.kind(), ErrorKind::Programming);
    assert!(error.message().contains("registry identity"));
}

#[test]
fn generation_attachment_duplicate_registration_is_sticky() {
    let registry = registry();
    let Err(error) = builder(registry.clone())
        .generation_attachment(GenerationAttachment::new(
            registry.clone(),
            Arc::new(Descriptor("first")),
        ))
        .generation_attachment(GenerationAttachment::new(
            registry,
            Arc::new(Descriptor("second")),
        ))
        .build()
    else {
        panic!("duplicate service replaced an existing registration")
    };
    assert_eq!(error.kind(), ErrorKind::Programming);
    assert!(error.message().contains("duplicate generation attachment"));
}

static FACTORY_ADDON: AddonDescriptor = AddonDescriptor {
    name: "factory_test",
    depends: &["base"],
    exact_only: false,
};
static FACTORY_MODEL: erp_registry::ModelDecl =
    erp_registry::ModelDecl::new("factory_test", "factory.item");
struct FactoryInputs {
    installed: Vec<&'static AddonDescriptor>,
    method: &'static str,
}
fn factory_registry() -> Arc<Registry> {
    Arc::new(
        RegistryBuilder::new([&FACTORY_ADDON])
            .with_decls([&FACTORY_MODEL])
            .build()
            .unwrap(),
    )
}
fn compose_service(
    ctx: &erp_orm::GenerationComposition<'_>,
    inputs: &FactoryInputs,
) -> erp_fields::Result<Arc<Descriptor>> {
    if inputs.installed.len() != ctx.registry().addons().len()
        || !inputs
            .installed
            .iter()
            .zip(ctx.registry().addons())
            .all(|(expected, actual)| std::ptr::eq(*expected, *actual))
    {
        return Err(erp_fields::Error::programming(
            "factory installed closure mismatch",
        ));
    }
    let model = ctx.registry().model("factory.item").unwrap();
    if !matches!(
        ctx.exposure().get(model.id, inputs.method),
        Some(erp_orm::rpc::Exposure::Exposed(_))
    ) {
        return Err(erp_fields::Error::programming(format!(
            "factory method '{}' is not exposed",
            inputs.method
        )));
    }
    Ok(Arc::new(Descriptor("factory")))
}
fn factory(
    registry: &Arc<Registry>,
    method: &'static str,
) -> erp_orm::GenerationAttachmentFactory<Descriptor, FactoryInputs> {
    erp_orm::GenerationAttachmentFactory::new(
        registry.clone(),
        Arc::new(FactoryInputs {
            installed: registry.addons().to_vec(),
            method,
        }),
        compose_service,
    )
}

#[test]
fn factory_uses_final_exposure_and_owned_generation_inputs_without_inheritance() {
    let registry = factory_registry();
    let orm = builder(registry.clone())
        .generation_attachment_with(factory(&registry, "read"))
        .build()
        .unwrap();
    assert_eq!(
        orm.env(SUPERUSER_ID, Context::new())
            .generation_attachment::<Descriptor>()
            .unwrap()
            .0,
        "factory"
    );
    let next = factory_registry();
    let rebuilt = orm.rebuild(next).build().unwrap();
    let error = rebuilt
        .env(SUPERUSER_ID, Context::new())
        .generation_attachment::<Descriptor>()
        .unwrap_err();
    assert!(
        error
            .message()
            .contains("generation attachment is unavailable")
    );
}

#[test]
fn failed_generation_factory_cannot_publish_or_replace_the_old_generation() {
    let registry = factory_registry();
    let orm = builder(registry.clone())
        .generation_attachment(GenerationAttachment::new(
            registry,
            Arc::new(Descriptor("old")),
        ))
        .build()
        .unwrap();
    let next = factory_registry();
    let Err(error) = orm
        .rebuild(next.clone())
        .generation_attachment_with(factory(&next, "unknown_method"))
        .build()
    else {
        panic!("failed factory published an ORM")
    };
    assert_eq!(error.kind(), ErrorKind::Programming);
    assert!(error.message().contains("unknown_method"));
    assert_eq!(
        orm.env(SUPERUSER_ID, Context::new())
            .generation_attachment::<Descriptor>()
            .unwrap()
            .0,
        "old"
    );
}

#[test]
fn direct_factory_duplicates_and_wrong_registry_factories_refuse_before_publication() {
    let registry = factory_registry();
    let Err(error) = builder(registry.clone())
        .generation_attachment(GenerationAttachment::new(
            registry.clone(),
            Arc::new(Descriptor("direct")),
        ))
        .generation_attachment_with(factory(&registry, "unknown_method"))
        .build()
    else {
        panic!("duplicate factory published")
    };
    assert!(error.message().contains("duplicate generation attachment"));
    let Err(error) = builder(registry.clone())
        .generation_attachment_with(factory(&registry, "read"))
        .generation_attachment(GenerationAttachment::new(
            registry.clone(),
            Arc::new(Descriptor("direct")),
        ))
        .build()
    else {
        panic!("duplicate direct service published")
    };
    assert!(error.message().contains("duplicate generation attachment"));
    let Err(error) = builder(registry.clone())
        .generation_attachment_with(factory(&registry, "read"))
        .generation_attachment_with(factory(&registry, "unknown_method"))
        .build()
    else {
        panic!("duplicate factories published")
    };
    assert!(error.message().contains("duplicate generation attachment"));
    let wrong = factory_registry();
    let Err(error) = builder(registry)
        .generation_attachment_with(factory(&wrong, "read"))
        .build()
    else {
        panic!("wrong-registry factory published")
    };
    assert!(error.message().contains("registry identity"));
}

#[test]
fn immutable_inputs_with_an_incorrect_installed_closure_refuse() {
    let registry = factory_registry();
    let inputs = Arc::new(FactoryInputs {
        installed: Vec::new(),
        method: "read",
    });
    let factory =
        erp_orm::GenerationAttachmentFactory::new(registry.clone(), inputs, compose_service);
    let Err(error) = builder(registry)
        .generation_attachment_with(factory)
        .build()
    else {
        panic!("incorrect generation inputs published")
    };
    assert_eq!(error.kind(), ErrorKind::Programming);
    assert_eq!(error.message(), "factory installed closure mismatch");
}

#[test]
fn later_factory_failure_drops_earlier_private_products_without_publishing() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    // The counter observes test-resource destruction; it is not mutable module configuration.
    struct EarlyService(Arc<AtomicUsize>);
    impl Drop for EarlyService {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }
    struct ProbeInputs(Arc<AtomicUsize>);
    fn early(
        _: &erp_orm::GenerationComposition<'_>,
        inputs: &ProbeInputs,
    ) -> erp_fields::Result<Arc<EarlyService>> {
        Ok(Arc::new(EarlyService(Arc::clone(&inputs.0))))
    }
    let registry = factory_registry();
    let drops = Arc::new(AtomicUsize::new(0));
    let probe = erp_orm::GenerationAttachmentFactory::new(
        registry.clone(),
        Arc::new(ProbeInputs(Arc::clone(&drops))),
        early,
    );
    let Err(error) = builder(registry.clone())
        .generation_attachment_with(probe)
        .generation_attachment_with(factory(&registry, "unknown_method"))
        .build()
    else {
        panic!("partial generation was published")
    };
    assert!(error.message().contains("unknown_method"));
    assert_eq!(drops.load(Ordering::SeqCst), 1);
}
