//! Broken custom fields: the `DropBroken` build mode leaves them out and reports them (
//! amending). Pure: no database.
#![allow(clippy::unwrap_used, reason = "tests")]

use std::sync::Arc;

use erp_fields::{Error, ErrorKind, OnDelete};
use erp_registry::{
    AddonDescriptor, Databases, DropReason, DroppedField, FieldDecl, FieldDef, InternAccount,
    KindDef, ManualFieldPolicy, ManualModel, ModelDecl, Overlay, Registry, RegistryBuilder,
    RegistryCache,
};

static PARTNERS: AddonDescriptor = AddonDescriptor {
    name: "dr_partners",
    depends: &["base"],
    exact_only: false,
};
static SALES: AddonDescriptor = AddonDescriptor {
    name: "dr_sales",
    depends: &["dr_partners"],
    exact_only: false,
};

static PARTNER_DECLS: [ModelDecl; 2] = [
    ModelDecl::new("dr_partners", "res.partner")
        .description("Contact")
        .fields(const { &[FieldDecl::char("name")] }),
    ModelDecl::extension("dr_partners", "res.users", &["res.users"])
        .inherits(const { &[("res.partner", "partner_id")] })
        .fields(
            const {
                &[FieldDecl::many2one("partner_id", "res.partner")
                    .required(true)
                    .ondelete(OnDelete::Cascade)]
            },
        ),
];

/// Version 1 of `dr_sales`: a model `dr.order` of its own.
static SALES_V1: [ModelDecl; 1] = [ModelDecl::new("dr_sales", "dr.order")
    .description("Order")
    .fields(const { &[FieldDecl::char("name")] })];

/// Version 2 of `dr_sales`: `dr.order` is gone, and `res.partner` gets a field `x_code` (the name
/// a database already uses for a custom field).
static SALES_V2: [ModelDecl; 1] =
    [
        ModelDecl::extension("dr_sales", "res.partner", &["res.partner"])
            .fields(const { &[FieldDecl::char("x_code")] }),
    ];

fn builder(sales: &'static [ModelDecl], overlay: &Overlay) -> RegistryBuilder {
    RegistryBuilder::new([&PARTNERS, &SALES])
        .linked(false)
        .with_decls(PARTNER_DECLS.iter().chain(sales.iter()))
        .overlay(overlay)
}

fn strict(sales: &'static [ModelDecl], overlay: &Overlay) -> erp_fields::Result<Registry> {
    builder(sales, overlay).build()
}

fn dropping(sales: &'static [ModelDecl], overlay: &Overlay) -> erp_fields::Result<Registry> {
    builder(sales, overlay)
        .manual_fields(ManualFieldPolicy::DropBroken)
        .build()
}

fn char_field(name: &str) -> FieldDef {
    FieldDef::new(name, KindDef::Char { size: None })
}

fn many2one(name: &str, comodel: &str) -> FieldDef {
    FieldDef::new(
        name,
        KindDef::Many2one {
            comodel: comodel.into(),
            ondelete: None,
        },
    )
}

#[track_caller]
fn assert_error(err: &Error, kind: ErrorKind, message: &str) {
    assert_eq!(
        (err.kind(), err.message()),
        (kind, message),
        "unexpected error"
    );
}

/// `(model, field, reason, error kind, error message)` of each dropped field, in report order.
fn report(registry: &Registry) -> Vec<(&str, &str, DropReason, ErrorKind, String)> {
    registry
        .dropped_fields()
        .iter()
        .map(|d: &DroppedField| {
            (
                d.model(),
                d.field(),
                d.reason().clone(),
                d.error().kind(),
                d.error().message().to_owned(),
            )
        })
        .collect()
}

fn has_field(registry: &Registry, model: &str, field: &str) -> bool {
    registry
        .model(model)
        .is_some_and(|m| m.field(field).is_some())
}

/// No declaration of the model's method resolution order still carries the field.
fn in_any_decl(registry: &Registry, model: &str, field: &str) -> bool {
    registry
        .model(model)
        .unwrap()
        .mro
        .iter()
        .any(|d| d.fields.iter().any(|f| f.name == field))
}

#[test]
fn spec_model_048_broken_custom_fields_are_dropped_and_setup_succeeds() {
    let mut overlay = Overlay::new();
    overlay
        .add_manual_field("res.partner", char_field("x_kept"))
        .unwrap();
    // a custom many2one whose comodel does not exist (yet)
    overlay
        .add_manual_field("res.partner", many2one("x_tag_id", "x_tag"))
        .unwrap();
    // a custom related field through it
    overlay
        .add_manual_field(
            "res.partner",
            char_field("x_tag_name").related("x_tag_id.x_name"),
        )
        .unwrap();
    // and one on the `_inherits` child, through the inherited copy
    overlay
        .add_manual_field(
            "res.users",
            char_field("x_user_tag").related("x_tag_id.x_name"),
        )
        .unwrap();

    // strict (the default): the build fails, as before
    let err = strict(&SALES_V1, &overlay).unwrap_err();
    assert_error(
        &err,
        ErrorKind::Programming,
        "Field res.partner.x_tag_id refers to unknown model 'x_tag'",
    );

    let registry = dropping(&SALES_V1, &overlay).unwrap();
    assert_eq!(
        report(&registry),
        vec![
            (
                "res.partner",
                "x_tag_id",
                DropReason::UnknownComodel {
                    comodel: "x_tag".into()
                },
                ErrorKind::Programming,
                err.message().to_owned(),
            ),
            (
                "res.partner",
                "x_tag_name",
                DropReason::Related {
                    path: "x_tag_id.x_name".into()
                },
                ErrorKind::Programming,
                "Field res.partner.x_tag_name: related path 'x_tag_id.x_name' goes through \
                 res.partner.x_tag_id, whose comodel 'x_tag' is not in the registry"
                    .into(),
            ),
            (
                "res.users",
                "x_user_tag",
                DropReason::Related {
                    path: "x_tag_id.x_name".into()
                },
                ErrorKind::Programming,
                "Field res.users.x_user_tag: related path 'x_tag_id.x_name' goes through \
                 res.users.x_tag_id, whose comodel 'x_tag' is not in the registry"
                    .into(),
            ),
        ]
    );
    for (model, field) in [
        ("res.partner", "x_tag_id"),
        ("res.partner", "x_tag_name"),
        ("res.users", "x_tag_id"),
        ("res.users", "x_user_tag"),
    ] {
        assert!(!has_field(&registry, model, field), "{model}.{field}");
        assert!(!in_any_decl(&registry, model, field), "{model}.{field}");
    }
    let kept = registry
        .model("res.partner")
        .unwrap()
        .field("x_kept")
        .unwrap();
    assert!(kept.manual && kept.has_column());
    assert!(
        has_field(&registry, "res.users", "x_kept"),
        "inherited copy"
    );

    // once the comodel exists, nothing is dropped and the fields are there
    overlay.add_manual_model(ManualModel::new("x_tag")).unwrap();
    overlay
        .add_manual_field("x_tag", char_field("x_name"))
        .unwrap();
    let fixed = dropping(&SALES_V1, &overlay).unwrap();
    assert!(fixed.dropped_fields().is_empty());
    assert!(has_field(&fixed, "res.partner", "x_tag_name"));
    assert!(has_field(&fixed, "res.users", "x_user_tag"));
    let strict_fixed = strict(&SALES_V1, &overlay).unwrap();
    assert!(strict_fixed.dropped_fields().is_empty());

    overlay
        .add_manual_field(
            "res.partner",
            FieldDef::new(
                "x_amount",
                KindDef::Monetary {
                    currency_field: None,
                },
            ),
        )
        .unwrap();
    let err = strict(&SALES_V1, &overlay).unwrap_err();
    assert_error(
        &err,
        ErrorKind::Programming,
        "Field res.partner.x_amount with unknown currency_field None",
    );
    let registry = dropping(&SALES_V1, &overlay).unwrap();
    assert_eq!(
        report(&registry),
        vec![(
            "res.partner",
            "x_amount",
            DropReason::Refused,
            ErrorKind::Programming,
            err.message().to_owned(),
        )]
    );
    assert!(!has_field(&registry, "res.partner", "x_amount"));
    assert!(has_field(&registry, "res.partner", "x_tag_name"));
}

#[test]
fn custom_field_declared_by_a_newer_addon_version_is_dropped() {
    let mut overlay = Overlay::new();
    overlay
        .add_manual_field("res.partner", char_field("x_code").string("Custom code"))
        .unwrap();
    overlay
        .add_manual_field("res.partner", char_field("x_code_copy").related("x_code"))
        .unwrap();

    let v1 = strict(&SALES_V1, &overlay).unwrap();
    let field = v1.model("res.partner").unwrap().field("x_code").unwrap();
    assert!(field.manual && field.modules.is_empty());

    let err = strict(&SALES_V2, &overlay).unwrap_err();
    assert_error(
        &err,
        ErrorKind::Programming,
        "custom field 'x_code' on model 'res.partner' clashes with a field declared by an addon; \
         custom fields only add new 'x_' fields",
    );

    let v2 = dropping(&SALES_V2, &overlay).unwrap();
    assert_eq!(
        report(&v2),
        vec![(
            "res.partner",
            "x_code",
            DropReason::DeclaredByAddon {
                addons: vec!["dr_sales"]
            },
            ErrorKind::Programming,
            err.message().to_owned(),
        )]
    );
    let field = v2.model("res.partner").unwrap().field("x_code").unwrap();
    assert!(!field.manual);
    assert_eq!(field.modules, vec!["dr_sales"]);
    assert_eq!(
        field.string, "X Code",
        "the custom label is gone with the custom field"
    );
    // a custom related field on the name now follows the addon field (same type: kept)
    assert!(has_field(&v2, "res.partner", "x_code_copy"));
    assert!(
        !v2.model("res.partner")
            .unwrap()
            .mro
            .iter()
            .any(|d| d.addon == erp_registry::MANUAL_ADDON
                && d.fields.iter().any(|f| f.name == "x_code")),
        "no custom declaration still carries x_code"
    );
}

/// The model a custom field extends is gone (the addon was removed or a newer version dropped the
/// model): every custom field on it is dropped.
#[test]
fn custom_fields_of_a_removed_model_are_dropped() {
    let mut overlay = Overlay::new();
    overlay
        .add_manual_field("dr.order", char_field("x_a"))
        .unwrap();
    overlay
        .add_manual_field("res.partner", char_field("x_b"))
        .unwrap();
    overlay
        .add_manual_field("dr.order", FieldDef::new("x_c", KindDef::Integer))
        .unwrap();
    assert!(strict(&SALES_V1, &overlay).is_ok());

    let err = strict(&SALES_V2, &overlay).unwrap_err();
    assert_error(
        &err,
        ErrorKind::Programming,
        "Model 'dr.order' does not exist in registry.",
    );
    let v2 = dropping(&SALES_V2, &overlay).unwrap();
    assert_eq!(
        report(&v2),
        ["x_a", "x_c"]
            .into_iter()
            .map(|f| (
                "dr.order",
                f,
                DropReason::UnknownModel,
                ErrorKind::Programming,
                err.message().to_owned()
            ))
            .collect::<Vec<_>>()
    );
    assert!(v2.model("dr.order").is_none());
    assert!(has_field(&v2, "res.partner", "x_b"));
}

#[test]
fn custom_related_fields_are_checked() {
    let mut overlay = Overlay::new();
    overlay
        .add_manual_field(
            "res.partner",
            FieldDef::new("x_name_len", KindDef::Integer).related("name"),
        )
        .unwrap();
    let err = strict(&SALES_V1, &overlay).unwrap_err();
    assert_error(
        &err,
        ErrorKind::Programming,
        "Type of related field res.partner.x_name_len (integer) is inconsistent with \
         res.partner.name (char)",
    );

    let cases: [(FieldDef, DropReason, &str); 3] = [
        (
            FieldDef::new("x_name_len", KindDef::Integer).related("name"),
            DropReason::Related {
                path: "name".into(),
            },
            "Type of related field res.partner.x_name_len (integer) is inconsistent with \
             res.partner.name (char)",
        ),
        (
            char_field("x_nowhere").related("no_such_field"),
            DropReason::Related {
                path: "no_such_field".into(),
            },
            "Field res.partner.x_nowhere: related path 'no_such_field': field 'no_such_field' \
             does not exist on model 'res.partner'",
        ),
        (
            char_field("x_through_char").related("name.name"),
            DropReason::Related {
                path: "name.name".into(),
            },
            "Field res.partner.x_through_char: related path 'name.name' goes through \
             res.partner.name, which is not a many2one, one2many or many2many field",
        ),
    ];
    for (field, reason, message) in cases {
        let name = field.name.clone();
        let mut overlay = Overlay::new();
        overlay.add_manual_field("res.partner", field).unwrap();
        let err = strict(&SALES_V1, &overlay).unwrap_err();
        assert_error(&err, ErrorKind::Programming, message);
        let registry = dropping(&SALES_V1, &overlay).unwrap();
        assert_eq!(
            report(&registry),
            vec![(
                "res.partner",
                name.as_str(),
                reason,
                ErrorKind::Programming,
                message.to_owned()
            )]
        );
    }

    // a related path through a relation and an inherited field resolves
    let mut overlay = Overlay::new();
    overlay
        .add_manual_field(
            "res.users",
            char_field("x_partner_name").related("partner_id.name"),
        )
        .unwrap();
    overlay
        .add_manual_field("res.users", char_field("x_own_name").related("name"))
        .unwrap();
    let registry = strict(&SALES_V1, &overlay).unwrap();
    assert!(has_field(&registry, "res.users", "x_partner_name"));
    assert!(has_field(&registry, "res.users", "x_own_name"));
}

/// A cascade over several rounds: a custom many2one is dropped (missing comodel), so the custom
/// one2many using it as inverse loses its inverse and is dropped in the next round.
#[test]
fn drops_cascade_through_one2many_inverses() {
    let mut overlay = Overlay::new();
    overlay
        .add_manual_model(ManualModel::new("x_line"))
        .unwrap();
    overlay
        .add_manual_field("x_line", many2one("x_owner_id", "x_owner"))
        .unwrap();
    overlay
        .add_manual_field(
            "x_line",
            FieldDef::new(
                "x_line_ids",
                KindDef::One2many {
                    comodel: "x_line".into(),
                    inverse: "x_owner_id".into(),
                },
            ),
        )
        .unwrap();
    overlay
        .add_manual_field("x_line", char_field("x_name"))
        .unwrap();
    let registry = dropping(&SALES_V1, &overlay).unwrap();
    assert_eq!(
        report(&registry),
        vec![
            (
                "x_line",
                "x_owner_id",
                DropReason::UnknownComodel {
                    comodel: "x_owner".into()
                },
                ErrorKind::Programming,
                "Field x_line.x_owner_id refers to unknown model 'x_owner'".into(),
            ),
            (
                "x_line",
                "x_line_ids",
                DropReason::Refused,
                ErrorKind::Programming,
                "'x_owner_id' declared in x_line.x_line_ids does not exist on 'x_line'.".into(),
            ),
        ]
    );
    let fields: Vec<&str> = registry
        .model("x_line")
        .unwrap()
        .fields
        .iter()
        .filter(|f| f.name.starts_with("x_"))
        .map(|f| f.name)
        .collect();
    assert_eq!(fields, vec!["x_name"]);
}

/// Errors not tied to one custom field still fail the build in `DropBroken`.
#[test]
fn errors_not_tied_to_one_custom_field_still_fail() {
    // the same custom field from two overlays
    let mut a = Overlay::new();
    a.add_manual_field("res.partner", char_field("x_twice"))
        .unwrap();
    let err = builder(&SALES_V1, &a)
        .overlay(&a)
        .manual_fields(ManualFieldPolicy::DropBroken)
        .build()
        .unwrap_err();
    assert_error(
        &err,
        ErrorKind::Programming,
        "custom field 'x_twice' on model 'res.partner' is declared twice",
    );

    // an addon declaration with an unknown comodel
    static BROKEN_ADDON: [ModelDecl; 1] =
        [
            ModelDecl::extension("dr_sales", "res.partner", &["res.partner"])
                .fields(const { &[FieldDecl::many2one("order_id", "dr.nowhere")] }),
        ];
    let mut overlay = Overlay::new();
    overlay
        .add_manual_field("res.partner", many2one("x_bad_id", "x_nowhere"))
        .unwrap();
    let err = dropping(&BROKEN_ADDON, &overlay).unwrap_err();
    assert_error(
        &err,
        ErrorKind::Programming,
        "Field res.partner.order_id refers to unknown model 'dr.nowhere'",
    );

    // the database's intern quota
    let mut overlay = Overlay::new().with_account(Arc::new(InternAccount::with_limit(0).unwrap()));
    overlay
        .add_manual_field(
            "res.partner",
            many2one("x_quota_only_m2o", "x_quota_nowhere"),
        )
        .unwrap();
    let err = dropping(&SALES_V1, &overlay).unwrap_err();
    assert_error(
        &err,
        ErrorKind::ResourceExhausted,
        "the runtime model declarations of this database use more than 0 bytes (its intern \
         quota); restart the server to release definitions that are no longer used",
    );
}

/// Strict is the default; `DropBroken` with nothing broken builds the same registry and reports
/// nothing; an empty overlay reports nothing.
#[test]
fn nothing_broken_nothing_reported() {
    let mut overlay = Overlay::new();
    overlay
        .add_manual_field("res.partner", char_field("x_fine"))
        .unwrap();
    let a = strict(&SALES_V1, &overlay).unwrap();
    let b = dropping(&SALES_V1, &overlay).unwrap();
    assert!(a.dropped_fields().is_empty() && b.dropped_fields().is_empty());
    let names = |r: &Registry| -> Vec<(&str, Vec<&str>)> {
        r.models()
            .iter()
            .map(|m| (m.name, m.fields.iter().map(|f| f.name).collect()))
            .collect()
    };
    assert_eq!(names(&a), names(&b));
    assert_eq!(ManualFieldPolicy::default(), ManualFieldPolicy::Strict);
    let empty = dropping(&SALES_V2, &Overlay::new()).unwrap();
    assert!(empty.dropped_fields().is_empty());
}

/// Many broken fields among many good ones: all dropped in report order, the good ones keep
/// their order (chunks of custom fields are rebuilt without the dropped ones).
#[test]
fn many_broken_fields_keep_the_good_ones_in_order() {
    let mut overlay = Overlay::new();
    let mut good = Vec::new();
    let mut bad = Vec::new();
    for i in 0..300 {
        if i % 3 == 0 {
            let name = format!("x_many_bad_{i}");
            overlay
                .add_manual_field("res.partner", many2one(&name, "x_many_nowhere"))
                .unwrap();
            bad.push(name);
        } else {
            let name = format!("x_many_good_{i}");
            overlay
                .add_manual_field("res.partner", char_field(&name))
                .unwrap();
            good.push(name);
        }
    }
    let registry = dropping(&SALES_V1, &overlay).unwrap();
    let dropped: Vec<&str> = registry
        .dropped_fields()
        .iter()
        .map(|d| d.field())
        .collect();
    assert_eq!(dropped, bad.iter().map(String::as_str).collect::<Vec<_>>());
    let kept: Vec<&str> = registry
        .model("res.partner")
        .unwrap()
        .fields
        .iter()
        .filter(|f| f.manual)
        .map(|f| f.name)
        .collect();
    assert_eq!(kept, good.iter().map(String::as_str).collect::<Vec<_>>());
}

/// `Databases::load_with` remembers the policy: `set_overlay` drops too, the cache never shares a
/// strict and a dropping build, and `load` (strict) of the same overlay still fails loudly.
#[test]
fn databases_keep_the_policy_of_the_load() {
    let dbs = Databases::new(RegistryCache::unlinked());
    let mut overlay = Overlay::new();
    overlay
        .add_manual_field("res.users", many2one("x_db_tag_id", "x_db_tag"))
        .unwrap();

    let err = dbs.load("db_strict", &[], overlay.clone()).unwrap_err();
    assert_error(
        &err,
        ErrorKind::Programming,
        "Field res.users.x_db_tag_id refers to unknown model 'x_db_tag'",
    );
    assert!(dbs.registry("db_strict").is_none());

    let loaded = dbs
        .load_with(
            "db_drop",
            &[],
            overlay.clone(),
            ManualFieldPolicy::DropBroken,
        )
        .unwrap();
    assert_eq!(
        loaded
            .dropped_fields()
            .iter()
            .map(|d| (d.model(), d.field()))
            .collect::<Vec<_>>(),
        vec![("res.users", "x_db_tag_id")]
    );
    // the dropping build is not served to a strict load of the same key
    let err = dbs.load("db_strict", &[], overlay.clone()).unwrap_err();
    assert_error(
        &err,
        ErrorKind::Programming,
        "Field res.users.x_db_tag_id refers to unknown model 'x_db_tag'",
    );

    // a later overlay change keeps dropping the broken field and adds the good one
    let mut changed = overlay.clone();
    changed
        .add_manual_field("res.users", char_field("x_db_badge"))
        .unwrap();
    let after =
        futures_block_on(dbs.set_overlay("db_drop", changed, |_| async { Ok(()) })).unwrap();
    assert!(has_field(&after, "res.users", "x_db_badge"));
    assert!(!has_field(&after, "res.users", "x_db_tag_id"));
    assert_eq!(after.dropped_fields().len(), 1);
    assert!(Arc::ptr_eq(&dbs.registry("db_drop").unwrap(), &after));
}

/// Poll a future that never waits (the schema sync closure here is ready at once).
fn futures_block_on<F: std::future::Future>(fut: F) -> F::Output {
    let mut fut = std::pin::pin!(fut);
    let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
    loop {
        if let std::task::Poll::Ready(v) = fut.as_mut().poll(&mut cx) {
            return v;
        }
    }
}

/// A model inheriting a model under a new name (`_inherit` without `_name` = parent) gets the
/// parent's custom fields too; a refused custom field is reported once, on the model it was
/// declared on, and leaves both models.
#[test]
fn custom_fields_seen_through_prototype_inheritance_are_dropped_once() {
    static COPY: [ModelDecl; 1] = [ModelDecl::new("dr_sales", "dr.partner.copy")
        .description("Partner copy")
        .inherit(&["res.partner"])];
    let mut overlay = Overlay::new();
    overlay
        .add_manual_field("res.partner", many2one("x_proto_id", "x_proto_nowhere"))
        .unwrap();
    overlay
        .add_manual_field("res.partner", char_field("x_proto_kept"))
        .unwrap();
    let build = |policy| {
        RegistryBuilder::new([&PARTNERS, &SALES])
            .linked(false)
            .with_decls(PARTNER_DECLS.iter().chain(COPY.iter()))
            .overlay(&overlay)
            .manual_fields(policy)
            .build()
    };
    let err = build(ManualFieldPolicy::Strict).unwrap_err();
    assert_error(
        &err,
        ErrorKind::Programming,
        "Field res.partner.x_proto_id refers to unknown model 'x_proto_nowhere'",
    );

    let registry = build(ManualFieldPolicy::DropBroken).unwrap();
    assert_eq!(
        registry
            .dropped_fields()
            .iter()
            .map(|d| (d.model(), d.field(), d.reason().clone()))
            .collect::<Vec<_>>(),
        vec![(
            "res.partner",
            "x_proto_id",
            DropReason::UnknownComodel {
                comodel: "x_proto_nowhere".into()
            }
        )]
    );
    for model in ["res.partner", "dr.partner.copy"] {
        assert!(!has_field(&registry, model, "x_proto_id"), "{model}");
        assert!(has_field(&registry, model, "x_proto_kept"), "{model}");
    }
}

/// `set_overlay` on a database loaded with `DropBroken` refuses an overlay whose build drops a
/// custom field the current registry does not drop (a new broken field, or a field the change
/// breaks), with that field's strict error: nothing is synchronized and the database keeps its
/// registry. Fields the current registry drops already carry over (review item 1).
#[test]
fn set_overlay_refuses_a_custom_field_it_would_newly_drop() {
    let dbs = Databases::new(RegistryCache::unlinked());
    let mut overlay = Overlay::new();
    overlay
        .add_manual_field("res.users", many2one("x_old_tag_id", "x_old_nowhere"))
        .unwrap();
    overlay
        .add_manual_model(ManualModel::new("x_kind"))
        .unwrap();
    overlay
        .add_manual_field("res.users", many2one("x_kind_id", "x_kind"))
        .unwrap();
    let loaded = dbs
        .load_with(
            "db_new",
            &[],
            overlay.clone(),
            ManualFieldPolicy::DropBroken,
        )
        .unwrap();
    assert_eq!(loaded.dropped_fields().len(), 1);
    let entries = dbs.cache().len();

    let refused = |changed: Overlay, message: &str| {
        let synced = std::cell::Cell::new(false);
        let err = futures_block_on(dbs.set_overlay("db_new", changed, |_| {
            synced.set(true);
            async { Ok(()) }
        }))
        .unwrap_err();
        assert_error(&err, ErrorKind::Programming, message);
        assert!(
            !synced.get(),
            "nothing is synchronized for a refused overlay"
        );
        assert!(Arc::ptr_eq(&dbs.registry("db_new").unwrap(), &loaded));
        assert_eq!(
            dbs.cache().len(),
            entries,
            "the refused build is not cached"
        );
    };

    // a new custom field that cannot be set up
    let mut added = overlay.clone();
    added
        .add_manual_field("res.users", many2one("x_new_id", "x_new_nowhere"))
        .unwrap();
    refused(
        added,
        "Field res.users.x_new_id refers to unknown model 'x_new_nowhere'",
    );

    // a change that breaks a field the current registry has: its comodel is no longer declared
    let mut broken = Overlay::new();
    broken
        .add_manual_field("res.users", many2one("x_old_tag_id", "x_old_nowhere"))
        .unwrap();
    broken
        .add_manual_field("res.users", many2one("x_kind_id", "x_kind"))
        .unwrap();
    refused(
        broken,
        "Field res.users.x_kind_id refers to unknown model 'x_kind'",
    );

    // the field dropped already carries over; the database can still change
    let mut fine = overlay.clone();
    fine.add_manual_field("res.users", char_field("x_badge"))
        .unwrap();
    let after = futures_block_on(dbs.set_overlay("db_new", fine, |_| async { Ok(()) })).unwrap();
    assert!(has_field(&after, "res.users", "x_badge"));
    assert_eq!(
        after
            .dropped_fields()
            .iter()
            .map(|d| (d.model(), d.field()))
            .collect::<Vec<_>>(),
        vec![("res.users", "x_old_tag_id")]
    );
}

/// A custom field declared in two overlays is an error of the overlays, not of one field: the
/// build fails with "declared twice" in both modes, also when the field is broken (review item
/// 3: it was dropped once per overlay and the duplicate went unnoticed).
#[test]
fn a_broken_custom_field_declared_in_two_overlays_fails_the_build() {
    let both = |sales: &'static [ModelDecl], a: &Overlay, b: &Overlay, policy| {
        builder(sales, a).overlay(b).manual_fields(policy).build()
    };
    // on a model that is gone
    let mut a = Overlay::new();
    a.add_manual_field("dr.order", char_field("x_gone"))
        .unwrap();
    let mut b = Overlay::new();
    b.add_manual_field("res.partner", char_field("x_other"))
        .unwrap();
    b.add_manual_field("dr.order", char_field("x_gone"))
        .unwrap();
    // clashing with a field a newer addon version declares
    let mut c = Overlay::new();
    c.add_manual_field("res.partner", char_field("x_code"))
        .unwrap();
    let cases = [
        (
            &a,
            &b,
            "custom field 'x_gone' on model 'dr.order' is declared twice",
        ),
        (
            &c,
            &c,
            "custom field 'x_code' on model 'res.partner' is declared twice",
        ),
    ];
    for (first, second, message) in cases {
        for policy in [ManualFieldPolicy::Strict, ManualFieldPolicy::DropBroken] {
            let err = both(&SALES_V2, first, second, policy).unwrap_err();
            assert_error(&err, ErrorKind::Programming, message);
        }
    }

    // a model inheriting another under a new name, the field broken on the parent: the
    // declaration on the child is a second declaration of it, as when nothing is broken. The
    // round that finds it also drops the parent's field; the error must not be lost when the
    // next round composes without it
    static COPY_V2: [ModelDecl; 2] = [
        ModelDecl::extension("dr_sales", "res.partner", &["res.partner"])
            .fields(const { &[FieldDecl::char("x_code")] }),
        ModelDecl::new("dr_sales", "dr.partner.copy")
            .description("Partner copy")
            .inherit(&["res.partner"]),
    ];
    static COPY_V1: [ModelDecl; 1] = [ModelDecl::new("dr_sales", "dr.partner.copy")
        .description("Partner copy")
        .inherit(&["res.partner"])];
    let mut inherited = Overlay::new();
    inherited
        .add_manual_field("res.partner", char_field("x_code"))
        .unwrap();
    inherited
        .add_manual_field("dr.partner.copy", char_field("x_code"))
        .unwrap();
    let twice = "custom field 'x_code' on model 'dr.partner.copy' is declared twice";
    let err = builder(&COPY_V1, &inherited).build().unwrap_err();
    assert_error(&err, ErrorKind::Programming, twice);
    let err = builder(&COPY_V2, &inherited)
        .manual_fields(ManualFieldPolicy::DropBroken)
        .build()
        .unwrap_err();
    assert_error(&err, ErrorKind::Programming, twice);
}
