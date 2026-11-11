//! Per-database overlays, custom (`x_`) models and fields, owned declarations and registry sharing
//! (§11; MODEL §4.12). Pure: no database.
#![allow(clippy::unwrap_used, reason = "tests")]

use std::sync::Arc;

use erp_fields::{Error, ErrorKind, FieldType, OnDelete};
use erp_registry::{
    AddonDescriptor, DATABASE_INTERN_QUOTA_BYTES, Databases, DefaultValue, FieldDecl, FieldDef,
    InternAccount, KindDef, MANUAL_ADDON, MAX_FIELDS_PER_MODEL, MAX_NAME_BYTES,
    MAX_SELECTION_ENTRIES, MAX_TEXT_BYTES, ManualModel, ModelDecl, ModelDef, Overlay, Registry,
    RegistryBuilder, RegistryCache,
};

static PARTNERS: AddonDescriptor = AddonDescriptor {
    name: "ov_partners",
    depends: &["base"],
    exact_only: false,
};
static SALES: AddonDescriptor = AddonDescriptor {
    name: "ov_sales",
    depends: &["ov_partners"],
    exact_only: false,
};

static PARTNER_DECLS: [ModelDecl; 2] = [
    ModelDecl::new("ov_partners", "res.partner")
        .description("Contact")
        .fields(const { &[FieldDecl::char("name")] }),
    ModelDecl::extension("ov_partners", "res.users", &["res.users"])
        .inherits(const { &[("res.partner", "partner_id")] })
        .fields(
            const {
                &[FieldDecl::many2one("partner_id", "res.partner")
                    .required(true)
                    .ondelete(OnDelete::Cascade)]
            },
        ),
];

static SALES_DECLS: [ModelDecl; 1] =
    [
        ModelDecl::extension("ov_sales", "res.partner", &["res.partner"])
            .fields(const { &[FieldDecl::integer("sale_count")] }),
    ];

fn decls() -> Vec<&'static ModelDecl> {
    PARTNER_DECLS.iter().chain(SALES_DECLS.iter()).collect()
}

fn build(addons: &[&'static AddonDescriptor], overlay: &Overlay) -> erp_fields::Result<Registry> {
    RegistryBuilder::new(addons.iter().copied())
        .linked(false)
        .with_decls(
            decls()
                .into_iter()
                .filter(|d| addons.iter().any(|a| a.name == d.addon)),
        )
        .overlay(overlay)
        .build()
}

fn char_field(name: &str) -> FieldDef {
    FieldDef::new(name, KindDef::Char { size: None })
}

fn names(registry: &Registry, model: &str) -> Vec<&'static str> {
    registry
        .model(model)
        .unwrap()
        .fields
        .iter()
        .map(|f| f.name)
        .collect()
}

/// Poll a future that never waits (the schema sync closures here are ready at once).
fn block_on<F: std::future::Future>(fut: F) -> F::Output {
    let mut fut = std::pin::pin!(fut);
    let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
    loop {
        if let std::task::Poll::Ready(value) = fut.as_mut().poll(&mut cx) {
            return value;
        }
    }
}

/// `Databases::set_overlay` with a schema sync that has nothing to do (no database here).
fn set_overlay(dbs: &Databases, db: &str, overlay: Overlay) -> erp_fields::Result<Arc<Registry>> {
    block_on(dbs.set_overlay(db, overlay, |_| async { Ok(()) }))
}

#[track_caller]
fn assert_error(err: &Error, kind: ErrorKind, message: &str) {
    assert_eq!(
        (err.kind(), err.message()),
        (kind, message),
        "unexpected error"
    );
}

/// field on its `_inherits` child; name rules and uniqueness refused. Columns: erp-test-models
/// `cross_addon`; the `ir.model.fields` records arrive with)
#[test]
fn spec_model_045_custom_fields_on_partner_and_inherited_child() {
    let mut overlay = Overlay::new();
    overlay
        .add_manual_field("res.partner", char_field("x_foo"))
        .unwrap();

    let xyz = overlay
        .add_manual_field("res.partner", char_field("xyz"))
        .unwrap_err();
    assert_error(
        &xyz,
        ErrorKind::Validation,
        "Custom fields must have a name that starts with 'x_'! (got 'xyz' on model 'res.partner')",
    );
    let spaced = overlay
        .add_manual_field("res.partner", char_field("x_foo bar"))
        .unwrap_err();
    assert_error(
        &spaced,
        ErrorKind::Validation,
        "Field names can only contain characters, digits and underscores (up to 63). \
         (got 'x_foo bar' on model 'res.partner')",
    );
    let dup = overlay
        .add_manual_field("res.partner", char_field("x_foo"))
        .unwrap_err();
    assert_error(
        &dup,
        ErrorKind::Validation,
        "Field names must be unique per model. (got 'x_foo' twice on model 'res.partner')",
    );

    let registry = build(&[&PARTNERS], &overlay).unwrap();
    let partner = registry.model("res.partner").unwrap();
    let x_foo = partner.field("x_foo").unwrap();
    assert!(x_foo.manual && x_foo.store && x_foo.has_column());
    assert!(x_foo.modules.is_empty(), "{:?}", x_foo.modules);
    // no `string` given: the label derives from the name, as for any field
    assert_eq!(x_foo.string, "X Foo");
    let users = registry.model("res.users").unwrap();
    let inherited = users.field("x_foo").unwrap();
    let link = inherited.inherited.unwrap();
    assert_eq!(registry.model_by_id(link.parent).name, "res.partner");
    assert!(!inherited.has_column() && !inherited.manual);
    // without the overlay the field does not exist
    let plain = build(&[&PARTNERS], &Overlay::new()).unwrap();
    assert!(plain.model("res.partner").unwrap().field("x_foo").is_none());
    assert!(plain.model("res.users").unwrap().field("x_foo").is_none());
}

/// `_rec_name`; the name rules are refused. `ir.model` records, the default `x_name` field and the
/// write rules arrive with)
#[test]
fn spec_model_044_custom_model() {
    let mut overlay = Overlay::new();
    overlay
        .add_manual_model(ManualModel::new("x_bananas").description("Bananas"))
        .unwrap();
    overlay
        .add_manual_field("x_bananas", char_field("x_name").string("Name"))
        .unwrap();
    let bad = overlay
        .add_manual_model(ManualModel::new("bananas"))
        .unwrap_err();
    assert_error(
        &bad,
        ErrorKind::Validation,
        "The model name must start with 'x_'.",
    );
    let upper = overlay
        .add_manual_model(ManualModel::new("x_Bananas"))
        .unwrap_err();
    assert_error(
        &upper,
        ErrorKind::Validation,
        "The model name can only contain lowercase characters, digits, underscores and dots. \
         (got 'x_Bananas')",
    );
    let twice = overlay
        .add_manual_model(ManualModel::new("x_bananas"))
        .unwrap_err();
    assert_error(
        &twice,
        ErrorKind::Validation,
        "Each model must have a unique name. (got 'x_bananas' twice)",
    );

    let registry = build(&[&PARTNERS], &overlay).unwrap();
    let bananas = registry.model("x_bananas").unwrap();
    assert!(bananas.custom && bananas.has_table());
    assert_eq!(bananas.table, "x_bananas");
    assert_eq!(bananas.description, "Bananas");
    let rec_name = bananas.rec_name.unwrap();
    assert_eq!(bananas.field_by_id(rec_name).name, "x_name");
    assert_eq!(bananas.field("x_name").unwrap().string, "Name");
    // audit fields like any regular model
    assert!(bananas.field("create_uid").is_some());
    assert!(!registry.addons().iter().any(|a| a.name == MANUAL_ADDON));
    // addon models are not custom
    assert!(!registry.model("res.partner").unwrap().custom);
}

/// addon model with `name` keeps it)
#[test]
fn spec_model_016_custom_model_rec_name_is_x_name() {
    let mut overlay = Overlay::new();
    overlay
        .add_manual_model(ManualModel::new("x_fruit"))
        .unwrap();
    overlay
        .add_manual_field("x_fruit", char_field("x_name"))
        .unwrap();
    overlay
        .add_manual_field("res.partner", char_field("x_name"))
        .unwrap();
    let registry = build(&[&PARTNERS], &overlay).unwrap();
    let fruit = registry.model("x_fruit").unwrap();
    assert_eq!(fruit.field_by_id(fruit.rec_name.unwrap()).name, "x_name");
    // res.partner has `name`: that stays its rec_name
    let partner = registry.model("res.partner").unwrap();
    assert_eq!(partner.field_by_id(partner.rec_name.unwrap()).name, "name");
}

/// An addon model with only `x_name` has no `_rec_name`: the fallback is for custom models.
#[test]
fn x_name_is_no_rec_name_on_addon_models() {
    let mut overlay = Overlay::new();
    overlay
        .add_addon_defs([ModelDef::new("ov_partners", "ov.thing").field(char_field("x_name"))])
        .unwrap();
    let registry = build(&[&PARTNERS], &overlay).unwrap();
    assert_eq!(registry.model("ov.thing").unwrap().rec_name, None);
}

/// archiving through it is exercised once `ir.model` rows exist)
#[test]
fn spec_model_017_custom_model_active_is_x_active() {
    let mut overlay = Overlay::new();
    overlay
        .add_manual_model(ManualModel::new("x_fruit"))
        .unwrap();
    overlay
        .add_manual_field("x_fruit", FieldDef::new("x_active", KindDef::Boolean))
        .unwrap();
    let registry = build(&[&PARTNERS], &overlay).unwrap();
    let fruit = registry.model("x_fruit").unwrap();
    assert_eq!(
        fruit.field_by_id(fruit.active_name.unwrap()).name,
        "x_active"
    );
}

/// replace a field an addon declares.
#[test]
fn spec_model_049_only_x_names_can_be_added() {
    let mut overlay = Overlay::new();
    let err = overlay
        .add_manual_field("res.partner", char_field("foo"))
        .unwrap_err();
    assert_error(
        &err,
        ErrorKind::Validation,
        "Custom fields must have a name that starts with 'x_'! (got 'foo' on model 'res.partner')",
    );
    // a code-declared `x_` name cannot be shadowed by a custom field
    static CODE_X: [ModelDecl; 1] =
        [
            ModelDecl::extension("ov_sales", "res.partner", &["res.partner"])
                .fields(const { &[FieldDecl::char("x_code")] }),
        ];
    overlay
        .add_manual_field("res.partner", char_field("x_code"))
        .unwrap();
    let err = RegistryBuilder::new([&PARTNERS, &SALES])
        .linked(false)
        .with_decls(decls())
        .with_decls(CODE_X.iter())
        .overlay(&overlay)
        .build()
        .unwrap_err();
    assert_error(
        &err,
        ErrorKind::Programming,
        "custom field 'x_code' on model 'res.partner' clashes with a field declared by an addon; \
         custom fields only add new 'x_' fields",
    );
}

/// goes away with the overlay)
#[test]
fn spec_model_015_custom_field_joins_the_field_set() {
    let mut overlay = Overlay::new();
    overlay
        .add_manual_field("res.partner", char_field("x_foo"))
        .unwrap();
    let registry = build(&[&PARTNERS, &SALES], &overlay).unwrap();
    let fields = names(&registry, "res.partner");
    let pos = |n: &str| fields.iter().position(|f| *f == n).unwrap();
    assert!(pos("name") < pos("sale_count") && pos("sale_count") < pos("x_foo"));
    assert_eq!(
        registry
            .model("res.partner")
            .unwrap()
            .field("sale_count")
            .unwrap()
            .modules,
        vec!["ov_sales"]
    );
    let without = build(&[&PARTNERS, &SALES], &Overlay::new()).unwrap();
    let mut expected = names(&without, "res.partner");
    expected.push("x_foo");
    let mut got = fields.clone();
    got.sort_unstable();
    expected.sort_unstable();
    assert_eq!(got, expected);
}

#[test]
fn custom_fields_on_unknown_models_or_comodels_fail_the_build() {
    let mut overlay = Overlay::new();
    overlay
        .add_manual_field("x_missing", char_field("x_a"))
        .unwrap();
    let err = build(&[&PARTNERS], &overlay).unwrap_err();
    assert_error(
        &err,
        ErrorKind::Programming,
        "Model 'x_missing' does not exist in registry.",
    );

    let mut overlay = Overlay::new();
    overlay
        .add_manual_field(
            "res.partner",
            FieldDef::new(
                "x_tag_id",
                KindDef::Many2one {
                    comodel: "x_tag".into(),
                    ondelete: None,
                },
            ),
        )
        .unwrap();
    let err = build(&[&PARTNERS], &overlay).unwrap_err();
    assert_error(
        &err,
        ErrorKind::Programming,
        "Field res.partner.x_tag_id refers to unknown model 'x_tag'",
    );
    // the comodel may be a custom model added after the field: custom models come first
    overlay.add_manual_model(ManualModel::new("x_tag")).unwrap();
    let registry = build(&[&PARTNERS], &overlay).unwrap();
    assert!(matches!(
        &registry.model("res.partner").unwrap().field("x_tag_id").unwrap().ty,
        FieldType::Many2one { comodel, .. } if comodel == "x_tag"
    ));
}

#[test]
fn addon_definitions_may_not_pose_as_custom() {
    let mut overlay = Overlay::new();
    let err = overlay
        .add_addon_defs([
            ModelDef::new("ov_partners", "ov.ok"),
            ModelDef::new(MANUAL_ADDON, "x_y"),
        ])
        .unwrap_err();
    assert_error(
        &err,
        ErrorKind::Programming,
        "model definition 'x_y' names the custom pseudo-addon '(manual)'; add custom models \
         with Overlay::add_manual_model",
    );
    // nothing of the refused batch was added
    assert!(overlay.is_empty());
    // and `with_decls` cannot smuggle one in either: the pseudo-addon is never in the set
    let decl = ModelDef::new("ov_partners", "ov.ok").to_decl().unwrap();
    assert!(!decl.custom);
}

/// Custom fields on a transient model take its kind (no "transforms the model" error).
#[test]
fn custom_field_on_a_transient_model() {
    let mut overlay = Overlay::new();
    overlay
        .add_manual_model(ManualModel::new("x_wizard").transient(true))
        .unwrap();
    overlay
        .add_manual_field("x_wizard", char_field("x_note"))
        .unwrap();
    let registry = build(&[&PARTNERS], &overlay).unwrap();
    let wizard = registry.model("x_wizard").unwrap();
    assert_eq!(wizard.kind, erp_registry::ModelKind::Transient);
    assert!(wizard.field("x_note").is_some());
}

/// Addon declarations held by an overlay (TOML models of a database-stored addon) compose with
/// their addon, before custom fields.
#[test]
fn overlay_addon_defs_compose_with_their_addon() {
    let mut overlay = Overlay::new();
    overlay
        .add_manual_field("res.partner", char_field("x_late"))
        .unwrap();
    overlay
        .add_addon_defs([ModelDef::extension("ov_sales", "res.partner")
            .field(FieldDef::new("level", KindDef::Integer).default(DefaultValue::Int(3)))])
        .unwrap();
    let registry = build(&[&PARTNERS, &SALES], &overlay).unwrap();
    let fields = names(&registry, "res.partner");
    let pos = |n: &str| fields.iter().position(|f| *f == n).unwrap();
    assert!(pos("sale_count") < pos("level") && pos("level") < pos("x_late"));
    let level = registry
        .model("res.partner")
        .unwrap()
        .field("level")
        .unwrap();
    assert_eq!(level.modules, vec!["ov_sales"]);
    assert!(!level.manual);
    // its addon must be installed
    let err = build(&[&PARTNERS], &overlay).unwrap_err();
    assert_error(
        &err,
        ErrorKind::Programming,
        "model definition 'res.partner' of addon 'ov_sales' belongs to addon 'ov_sales', which \
         is not in the addon set",
    );
}

/// Two overlays apply in call order; a custom field declared in both clashes loudly.
#[test]
fn several_overlays_apply_in_order() {
    let mut first = Overlay::new();
    first
        .add_manual_field("res.partner", char_field("x_a"))
        .unwrap();
    let mut second = Overlay::new();
    second
        .add_manual_field("res.partner", char_field("x_b"))
        .unwrap();
    let registry = RegistryBuilder::new([&PARTNERS])
        .linked(false)
        .with_decls(PARTNER_DECLS.iter())
        .overlay(&first)
        .overlay(&second)
        .build()
        .unwrap();
    let fields = names(&registry, "res.partner");
    let pos = |n: &str| fields.iter().position(|f| *f == n).unwrap();
    assert!(pos("x_a") < pos("x_b"));
    let err = RegistryBuilder::new([&PARTNERS])
        .linked(false)
        .with_decls(PARTNER_DECLS.iter())
        .overlay(&first)
        .overlay(&first)
        .build()
        .unwrap_err();
    assert_error(
        &err,
        ErrorKind::Programming,
        "custom field 'x_a' on model 'res.partner' is declared twice",
    );
}

/// Equal definitions intern to the same `&'static` declaration (no growth on rebuilds); floats
/// compare by bit pattern.
#[test]
fn owned_definitions_are_interned_once() {
    let def = |f: f64| {
        ModelDef::new("ov_partners", "x.interned")
            .field(char_field("a").default(DefaultValue::Str("z".into())))
            .field(
                FieldDef::new("f", KindDef::Float { digits: None }).default(DefaultValue::Float(f)),
            )
    };
    let a = def(1.5).to_decl().unwrap();
    assert!(std::ptr::eq(a, def(1.5).to_decl().unwrap()));
    assert!(!std::ptr::eq(a, def(2.5).to_decl().unwrap()));
    // NaN equals itself as a value (bitwise), but is no valid default; 0.0 and -0.0 differ
    assert_eq!(DefaultValue::Float(f64::NAN), DefaultValue::Float(f64::NAN));
    let err = def(f64::NAN).to_decl().unwrap_err();
    assert_error(
        &err,
        ErrorKind::Validation,
        "field 'f' on model 'x.interned': a non-finite float is not a valid default for a field \
         of type 'float'",
    );
    assert!(!std::ptr::eq(
        def(0.0).to_decl().unwrap(),
        def(-0.0).to_decl().unwrap()
    ));
    assert_eq!(a.fields.len(), 2);
    assert_eq!(a.addon, "ov_partners");
    assert_eq!(a.name, "x.interned");
}

#[test]
fn runtime_definitions_are_bounded() {
    let mut overlay = Overlay::new();
    // a label just under and just over the limit
    let ok = char_field("x_ok").string("é".repeat(MAX_TEXT_BYTES / 2));
    overlay.add_manual_field("res.partner", ok).unwrap();
    let big = char_field("x_big").string("a".repeat(MAX_TEXT_BYTES + 1));
    let err = overlay.add_manual_field("res.partner", big).unwrap_err();
    assert_error(
        &err,
        ErrorKind::Validation,
        &format!(
            "field 'x_big' on model 'res.partner': `string` is {} bytes long, more than the \
             limit of {MAX_TEXT_BYTES}",
            MAX_TEXT_BYTES + 1
        ),
    );
    // too many selection entries
    let values = (0..=MAX_SELECTION_ENTRIES)
        .map(|i| (i.to_string(), i.to_string()))
        .collect();
    let sel = FieldDef::new(
        "x_sel",
        KindDef::Selection {
            values: Some(values),
            add: Vec::new(),
            ondelete: Vec::new(),
        },
    );
    let err = overlay.add_manual_field("res.partner", sel).unwrap_err();
    assert_error(
        &err,
        ErrorKind::Validation,
        &format!(
            "field 'x_sel' on model 'res.partner': {} selection entries, more than the limit of \
             {MAX_SELECTION_ENTRIES}",
            MAX_SELECTION_ENTRIES + 1
        ),
    );
    // a model name that is too long or not ASCII
    let long = "x".repeat(MAX_NAME_BYTES + 1);
    let err = overlay
        .add_manual_field(long.clone(), char_field("x_a"))
        .unwrap_err();
    assert_error(
        &err,
        ErrorKind::Validation,
        &format!(
            "model name '{}…' is {} bytes long, more than the limit of {MAX_NAME_BYTES}",
            "x".repeat(64),
            MAX_NAME_BYTES + 1
        ),
    );
    let err = overlay
        .add_manual_model(ManualModel::new("x_café"))
        .unwrap_err();
    assert_error(
        &err,
        ErrorKind::Validation,
        "The model name can only contain lowercase characters, digits, underscores and dots. \
         (got 'x_café')",
    );
    // a definition with too many fields
    let mut wide = ModelDef::new("ov_partners", "ov.wide");
    for i in 0..=MAX_FIELDS_PER_MODEL {
        wide = wide.field(FieldDef::new(format!("f{i}"), KindDef::Integer));
    }
    let err = wide.to_decl().unwrap_err();
    assert_error(
        &err,
        ErrorKind::Validation,
        &format!(
            "model 'ov.wide': {} fields, more than the limit of {MAX_FIELDS_PER_MODEL}",
            MAX_FIELDS_PER_MODEL + 1
        ),
    );
    // an empty model name
    let err = ModelDef::new("ov_partners", "").check_limits().unwrap_err();
    assert_error(
        &err,
        ErrorKind::Validation,
        "The model name can only contain lowercase characters, digits, underscores and dots. \
         (got '')",
    );
}

/// An empty overlay changes nothing.
#[test]
fn empty_overlay_is_no_overlay() {
    let with = build(&[&PARTNERS, &SALES], &Overlay::new()).unwrap();
    let without = RegistryBuilder::new([&PARTNERS, &SALES])
        .linked(false)
        .with_decls(decls())
        .build()
        .unwrap();
    for model in ["res.partner", "res.users"] {
        assert_eq!(names(&with, model), names(&without, model));
    }
    assert!(Overlay::new().is_empty());
}

fn fixture_overlay() -> Overlay {
    // the cache is unlinked: the fixture declarations come in as overlay addon definitions
    let mut overlay = Overlay::new();
    overlay
        .add_addon_defs([ModelDef::new("ov_partners", "res.partner").field(char_field("name"))])
        .unwrap();
    overlay
}

/// databases with the same addon set and overlay share one registry; an overlay
/// stays in its database.
#[test]
fn registry_sharing_and_per_database_overlay() {
    let dbs = Databases::new(RegistryCache::unlinked());
    let a = dbs.load("db_a", &[&PARTNERS], fixture_overlay()).unwrap();
    let b = dbs.load("db_b", &[&PARTNERS], fixture_overlay()).unwrap();
    assert!(
        Arc::ptr_eq(&a, &b),
        "same addon set and overlay share one registry"
    );
    assert_eq!(dbs.cache().len(), 1);

    let mut custom = fixture_overlay();
    custom
        .add_manual_field("res.partner", char_field("x_foo"))
        .unwrap();
    let a2 = set_overlay(&dbs, "db_a", custom.clone()).unwrap();
    assert!(!Arc::ptr_eq(&a2, &b));
    assert_ne!(a2.generation(), b.generation());
    assert!(a2.model("res.partner").unwrap().field("x_foo").is_some());
    assert!(
        dbs.registry("db_b")
            .unwrap()
            .model("res.partner")
            .unwrap()
            .field("x_foo")
            .is_none()
    );
    assert!(Arc::ptr_eq(&dbs.registry("db_b").unwrap(), &b));
    assert_eq!(dbs.overlay("db_a").unwrap(), custom);

    // a failing overlay leaves the database on its current registry
    let mut broken = custom.clone();
    broken
        .add_manual_field("x_nowhere", char_field("x_a"))
        .unwrap();
    let err = set_overlay(&dbs, "db_a", broken).unwrap_err();
    assert_error(
        &err,
        ErrorKind::Programming,
        "Model 'x_nowhere' does not exist in registry.",
    );
    assert!(Arc::ptr_eq(&dbs.registry("db_a").unwrap(), &a2));
    assert_eq!(dbs.overlay("db_a").unwrap(), custom);

    // unknown database
    let err = set_overlay(&dbs, "db_c", Overlay::new()).unwrap_err();
    assert_error(
        &err,
        ErrorKind::Programming,
        "database 'db_c' has no loaded registry",
    );

    // dropping unused registries: the first registry is still held by `b` and db_b
    drop((a, a2));
    assert_eq!(dbs.cache().drop_unused(), 0);
    assert!(dbs.unload("db_a"));
    assert!(!dbs.unload("db_a"));
    assert_eq!(
        dbs.cache().drop_unused(),
        1,
        "db_a's custom registry is no longer used"
    );
    assert_eq!(dbs.cache().len(), 1);
    // reloading builds again: same declarations, new registry generation
    let again = set_overlay(&dbs, "db_b", custom).unwrap();
    assert!(again.model("res.partner").unwrap().field("x_foo").is_some());
}

/// The addon set is a set: order and duplicates do not make a new registry.
#[test]
fn cache_key_ignores_addon_order() {
    let dbs = Databases::new(RegistryCache::unlinked());
    let mut overlay = fixture_overlay();
    overlay
        .add_addon_defs([ModelDef::extension("ov_sales", "res.partner")
            .field(FieldDef::new("level", KindDef::Integer))])
        .unwrap();
    let one = dbs
        .load("db_a", &[&PARTNERS, &SALES], overlay.clone())
        .unwrap();
    let two = dbs
        .load("db_b", &[&SALES, &PARTNERS, &SALES], overlay)
        .unwrap();
    assert!(Arc::ptr_eq(&one, &two));
    assert_eq!(dbs.cache().len(), 1);
}

/// Concurrent loads of one key build once and share the result.
#[test]
fn concurrent_gets_share_one_registry() {
    let dbs = Arc::new(Databases::new(RegistryCache::unlinked()));
    let handles: Vec<_> = (0..8)
        .map(|i| {
            let dbs = Arc::clone(&dbs);
            std::thread::spawn(move || {
                dbs.load(&format!("db_{i}"), &[&PARTNERS], fixture_overlay())
                    .unwrap()
            })
        })
        .collect();
    let registries: Vec<Arc<Registry>> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    assert!(registries.windows(2).all(|w| Arc::ptr_eq(&w[0], &w[1])));
    assert_eq!(dbs.cache().len(), 1);
}

#[test]
fn manual_addon_is_not_an_addon() {
    static FAKE: AddonDescriptor = AddonDescriptor {
        name: MANUAL_ADDON,
        depends: &["base"],
        exact_only: false,
    };
    let err = RegistryBuilder::new([&FAKE])
        .linked(false)
        .build()
        .unwrap_err();
    assert_error(
        &err,
        ErrorKind::Programming,
        "'(manual)' names custom declarations of an overlay, not an addon",
    );
}

/// A static default must fit the field type.
#[test]
fn defaults_must_fit_the_field_type() {
    let mut overlay = Overlay::new();
    let cases: [(FieldDef, &str); 4] = [
        (
            FieldDef::new("x_n", KindDef::Integer).default(DefaultValue::Str("abc".into())),
            "field 'x_n' on model 'res.partner': a string is not a valid default for a field of \
             type 'integer'",
        ),
        (
            FieldDef::new(
                "x_p",
                KindDef::Many2one {
                    comodel: "res.partner".into(),
                    ondelete: None,
                },
            )
            .default(DefaultValue::Bool(true)),
            "field 'x_p' on model 'res.partner': a boolean is not a valid default for a field of \
             type 'many2one'",
        ),
        (
            FieldDef::new("x_f", KindDef::Float { digits: None })
                .default(DefaultValue::Float(f64::INFINITY)),
            "field 'x_f' on model 'res.partner': a non-finite float is not a valid default for a \
             field of type 'float'",
        ),
        (
            char_field("x_c").default(DefaultValue::Int(1)),
            "field 'x_c' on model 'res.partner': an integer is not a valid default for a field of \
             type 'char'",
        ),
    ];
    for (field, message) in cases {
        let err = overlay.add_manual_field("res.partner", field).unwrap_err();
        assert_error(&err, ErrorKind::Validation, message);
    }
    // what fits: False anywhere, an integer for a float, a string for a selection
    overlay
        .add_manual_field(
            "res.partner",
            FieldDef::new(
                "x_p",
                KindDef::Many2one {
                    comodel: "res.partner".into(),
                    ondelete: None,
                },
            )
            .default(DefaultValue::False),
        )
        .unwrap();
    overlay
        .add_manual_field(
            "res.partner",
            FieldDef::new("x_f", KindDef::Float { digits: None }).default(DefaultValue::Int(2)),
        )
        .unwrap();
    assert!(build(&[&PARTNERS], &overlay).is_ok());
}

/// Two models may not share a table when one of them is custom.
#[test]
fn custom_models_need_their_own_table() {
    let mut overlay = Overlay::new();
    overlay.add_manual_model(ManualModel::new("x_a.b")).unwrap();
    let err = overlay
        .add_manual_model(ManualModel::new("x_a_b"))
        .unwrap_err();
    assert_error(
        &err,
        ErrorKind::Validation,
        "custom models 'x_a.b' and 'x_a_b' would share the table 'x_a_b'",
    );
    // against an addon model
    let mut overlay = Overlay::new();
    overlay
        .add_manual_model(ManualModel::new("x_res.partner"))
        .unwrap();
    overlay
        .add_addon_defs([ModelDef::new("ov_partners", "x_res_partner")])
        .unwrap();
    let err = build(&[&PARTNERS], &overlay).unwrap_err();
    assert_error(
        &err,
        ErrorKind::Validation,
        "models 'x_res_partner' and 'x_res.partner' share the table 'x_res_partner'; a custom \
         model needs a table of its own",
    );
}

/// Only an overlay makes a model custom.
#[test]
fn custom_flag_outside_an_overlay_is_refused() {
    static FAKE: [ModelDecl; 1] = [{
        let mut decl = ModelDecl::new("ov_partners", "x_fake");
        decl.custom = true;
        decl
    }];
    let err = RegistryBuilder::new([&PARTNERS])
        .linked(false)
        .with_decls(PARTNER_DECLS.iter())
        .with_decls(FAKE.iter())
        .build()
        .unwrap_err();
    assert_error(
        &err,
        ErrorKind::Programming,
        "model definition 'x_fake' of addon 'ov_partners' is marked custom; custom models come \
         only from an Overlay",
    );
}

/// Many custom fields on one model: insertion order kept, few extra MRO entries.
#[test]
fn many_custom_fields_keep_their_order() {
    let mut overlay = Overlay::new();
    let expected: Vec<String> = (0..70).map(|i| format!("x_f{i:02}")).collect();
    for name in &expected {
        overlay
            .add_manual_field("res.partner", FieldDef::new(name.clone(), KindDef::Integer))
            .unwrap();
    }
    let registry = build(&[&PARTNERS], &overlay).unwrap();
    let fields = names(&registry, "res.partner");
    let custom: Vec<&str> = fields
        .iter()
        .copied()
        .filter(|f| f.starts_with("x_f"))
        .collect();
    assert_eq!(
        custom,
        expected.iter().map(String::as_str).collect::<Vec<_>>()
    );
    let partner = registry.model("res.partner").unwrap();
    let manual_entries = partner
        .mro
        .iter()
        .filter(|d| d.addon == MANUAL_ADDON)
        .count();
    assert_eq!(manual_entries, 3, "70 fields in chunks of 32");
}

/// The cache key includes each addon's dependencies.
#[test]
fn cache_key_includes_dependencies() {
    static FOO_A: AddonDescriptor = AddonDescriptor {
        name: "ov_foo",
        depends: &["base"],
        exact_only: false,
    };
    static FOO_B: AddonDescriptor = AddonDescriptor {
        name: "ov_foo",
        depends: &["base", "ov_partners"],
        exact_only: false,
    };
    let dbs = Databases::new(RegistryCache::unlinked());
    let one = dbs.load("db_a", &[&FOO_A], Overlay::new()).unwrap();
    let two = dbs
        .load("db_b", &[&PARTNERS, &FOO_B], Overlay::new())
        .unwrap();
    assert!(!Arc::ptr_eq(&one, &two));
    let err = dbs
        .load("db_c", &[&PARTNERS, &FOO_A, &FOO_B], Overlay::new())
        .unwrap_err();
    assert_error(
        &err,
        ErrorKind::Programming,
        "addon 'ov_foo' is listed twice with different dependencies ([\"base\"] and [\"base\", \
         \"ov_partners\"])",
    );
    // a failed build caches nothing
    let mut broken = Overlay::new();
    broken
        .add_manual_field("x_nowhere", char_field("x_a"))
        .unwrap();
    let err = dbs.load("db_c", &[&FOO_A], broken).unwrap_err();
    assert_error(
        &err,
        ErrorKind::Programming,
        "Model 'x_nowhere' does not exist in registry.",
    );
    assert!(dbs.registry("db_c").is_none());
    assert_eq!(dbs.cache().len(), 2);
}

/// Review item 1 (isolation): one database churning custom fields exhausts its own
/// intern quota; its registry stays, and another database still composes new declarations.
#[test]
fn intern_quota_is_per_database() {
    let dbs = Databases::new(RegistryCache::unlinked());
    let limit = 16 * 1024;
    dbs.set_account("db_a", Arc::new(InternAccount::with_limit(limit).unwrap()))
        .unwrap();
    let start = dbs.load("db_a", &[&PARTNERS], fixture_overlay()).unwrap();
    dbs.load("db_b", &[&PARTNERS], fixture_overlay()).unwrap();
    let churn = |db: &str, i: usize| {
        let mut overlay = fixture_overlay();
        overlay
            .add_manual_field("res.partner", char_field(&format!("x_{db}_churn_{i}")))
            .unwrap();
        set_overlay(&dbs, db, overlay)
    };
    let mut applied = 0usize;
    let err = loop {
        match churn("db_a", applied) {
            Ok(_) => applied += 1,
            Err(err) => break err,
        }
        assert!(applied < 1000, "the quota never tripped");
    };
    assert!(applied > 0, "the first change fits the quota");
    assert_error(
        &err,
        ErrorKind::ResourceExhausted,
        &format!(
            "the runtime model declarations of this database use more than {limit} bytes (its \
             intern quota); restart the server to release definitions that are no longer used"
        ),
    );
    let account = dbs.account_or_default("db_a");
    assert!(account.used() <= account.limit());
    // db_a keeps its last good registry
    let current = dbs.registry("db_a").unwrap();
    let field = format!("x_db_a_churn_{}", applied - 1);
    assert!(
        current
            .model("res.partner")
            .unwrap()
            .field(&field)
            .is_some()
    );
    assert!(!Arc::ptr_eq(&current, &start));
    // db_b is not affected: it composes more new declarations than db_a's whole quota
    for i in 0..applied * 3 {
        churn("db_b", i).unwrap();
    }
    assert!(dbs.account_or_default("db_b").used() > limit);
    // unloading does not reset db_a's account
    dbs.unload("db_a");
    dbs.load("db_a", &[&PARTNERS], fixture_overlay()).unwrap();
    let again = churn("db_a", 10_000).unwrap_err();
    assert_error(&again, ErrorKind::ResourceExhausted, err.message());
}

/// Review item 1: account limits can only be lowered.
#[test]
fn intern_account_limit_only_lowers() {
    assert_eq!(InternAccount::new().limit(), DATABASE_INTERN_QUOTA_BYTES);
    assert_eq!(InternAccount::with_limit(0).unwrap().limit(), 0);
    let err = InternAccount::with_limit(DATABASE_INTERN_QUOTA_BYTES + 1).unwrap_err();
    assert_error(
        &err,
        ErrorKind::Programming,
        &format!(
            "an intern account limit can only be lowered: {} bytes is more than \
             {DATABASE_INTERN_QUOTA_BYTES}",
            DATABASE_INTERN_QUOTA_BYTES + 1
        ),
    );
}

#[test]
fn spec_field_061_overlay_field_def_translate() {
    let mut overlay = Overlay::new();
    overlay
        .add_manual_field(
            "res.partner",
            FieldDef::new("x_trans_note", KindDef::Char { size: None }).translate(true),
        )
        .unwrap();
    let reg = build(&[&PARTNERS], &overlay).unwrap();
    let model = reg.model("res.partner").unwrap();
    let field = model.field("x_trans_note").unwrap();
    assert!(field.translate);
    assert_eq!(field.column(), Some(erp_fields::ColumnType::Jsonb));
}
