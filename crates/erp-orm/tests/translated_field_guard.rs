//! Declaration metadata and ORM construction are supported for translated fields.
//! Search and order on translated fields are supported.
#![allow(clippy::unwrap_used, clippy::panic, reason = "tests")]

use std::sync::Arc;

use erp_orm::{Orm, OrmBuilder};
use erp_registry::{AddonDescriptor, FieldDecl, ModelDecl, RegistryBuilder};

fn builder(field: FieldDecl) -> OrmBuilder {
    static ADDON: AddonDescriptor = AddonDescriptor {
        name: "translation_guard",
        depends: &[],
        exact_only: false,
    };
    let fields = Vec::leak(vec![field]);
    let decl = Box::leak(Box::new(
        ModelDecl::new("translation_guard", "translation.guard").fields(fields),
    ));
    let registry = RegistryBuilder::new([&ADDON])
        .with_decls([&*decl])
        .build()
        .unwrap();
    let pool = erp_orm::pool::create_pool(tokio_postgres::Config::new(), 1).unwrap();
    Orm::builder(Arc::new(registry), pool)
}

#[test]
fn translated_fields_now_build() {
    for field in [
        FieldDecl::char("name"),
        FieldDecl::text("name"),
        FieldDecl::html("name"),
    ] {
        for store in [true, false] {
            for install in [true, false] {
                let builder = builder(field.translate(true).store(store));
                let result = if install {
                    builder.build_with_base_install_grant().map(|(orm, _)| orm)
                } else {
                    builder.build()
                };
                assert!(
                    result.is_ok(),
                    "translated field should build: store={store}, install={install}"
                );
            }
        }
    }
}

#[test]
fn untranslated_fields_still_build() {
    assert!(
        builder(FieldDecl::char("name").translate(false))
            .build_with_base_install_grant()
            .is_ok()
    );
    assert!(
        builder(FieldDecl::char("name").translate(false))
            .build()
            .is_ok()
    );
}

#[test]
fn search_and_order_supported_for_translated_fields() {
    use erp_domain::{Domain, Operator};
    use erp_sql::filter::{Unaccent, WhereContext, plan_where, render_where};
    use erp_sql::order::compile_order;

    static ADDON: AddonDescriptor = AddonDescriptor {
        name: "translation_guard",
        depends: &[],
        exact_only: false,
    };
    let decl = Box::leak(Box::new(
        ModelDecl::new("translation_guard", "translation.guard").fields(Vec::leak(vec![
            FieldDecl::char("name").translate(true).store(true),
        ])),
    ));
    let registry = RegistryBuilder::new([&ADDON])
        .with_decls([&*decl])
        .build()
        .unwrap();
    let spec = registry.model("translation.guard").unwrap();

    // Order supported in en_US (default)
    let order_en = compile_order(&registry, spec, "name asc", None).unwrap();
    assert_eq!(
        order_en.sql,
        "\"translation_guard\".\"name\"->>'en_US' ASC, \"translation_guard\".\"id\""
    );

    // Order supported with explicit language
    let order_fr = compile_order(&registry, spec, "name asc", Some("fr_FR")).unwrap();
    assert_eq!(
        order_fr.sql,
        "COALESCE(\"translation_guard\".\"name\"->>'fr_FR', \"translation_guard\".\"name\"->>'en_US') ASC, \"translation_guard\".\"id\""
    );

    // Search supported in en_US (default)
    let domain = Domain::condition("name", Operator::Like, "%test%").unwrap();
    let ctx_en = WhereContext {
        registry: &registry,
        model: spec,
        unaccent: Unaccent::Off,
        lang: None,
    };
    let plan_en = plan_where(&domain, &ctx_en).unwrap();
    let rendered_en = render_where(&plan_en.key, &registry);
    assert_eq!(
        rendered_en.sql,
        "\"translation_guard\".\"name\"->>'en_US' LIKE $1::text"
    );

    // Search supported with explicit language
    let ctx_fr = ctx_en.with_lang(Some("fr_FR"));
    let plan_fr = plan_where(&domain, &ctx_fr).unwrap();
    let rendered_fr = render_where(&plan_fr.key, &registry);
    assert_eq!(
        rendered_fr.sql,
        "COALESCE(\"translation_guard\".\"name\"->>'fr_FR', \"translation_guard\".\"name\"->>'en_US') LIKE $1::text"
    );
}

#[test]
fn malformed_language_is_refused_by_sql_compilers() {
    use erp_domain::{Domain, Operator};
    use erp_fields::ErrorKind;
    use erp_sql::filter::{Unaccent, WhereContext, plan_where};
    static ADDON: AddonDescriptor = AddonDescriptor {
        name: "langs",
        depends: &[],
        exact_only: false,
    };
    let decl = Box::leak(Box::new(
        ModelDecl::new("langs", "langs.record")
            .fields(Vec::leak(vec![FieldDecl::char("name").translate(true)])),
    ));
    let registry = RegistryBuilder::new([&ADDON])
        .with_decls([&*decl])
        .build()
        .unwrap();
    let spec = registry.model("langs.record").unwrap();
    let domain = Domain::condition("name", Operator::Eq, "X").unwrap();
    for code in ["x\0y".to_owned(), "x' OR TRUE--".to_owned(), "x".repeat(65)] {
        let ctx = WhereContext::new(&registry, spec, Unaccent::Off).with_lang(Some(&code));
        let err = plan_where(&domain, &ctx).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::UserError);
        assert_eq!(err.message(), format!("Invalid language code: {code}"));
        let err = erp_sql::order::compile_order(&registry, spec, "name", Some(&code)).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::UserError);
        assert_eq!(err.message(), format!("Invalid language code: {code}"));
    }
}
