// SPDX-License-Identifier: MIT OR Apache-2.0
//! `owner_only_stored_fields` (contract §7): only a protected model's owner
//! and its co-owners add or change materialized columns, whatever surface the declaration uses.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

use erp_fields::{ErrorKind, Result};
use erp_registry::{
    AddonDescriptor, DropReason, FieldDecl, FieldDef, GeneratedStorage, KindDef, ManualFieldPolicy,
    ModelDecl, ModelDef, ModelKind, Overlay, Registry, RegistryBuilder,
};

fn addon(name: &str) -> &'static AddonDescriptor {
    static MIX: AddonDescriptor = desc("mix", &["base"]);
    static ACCT: AddonDescriptor = desc("acct", &["mix"]);
    static ACCT_X: AddonDescriptor = desc("acct_x", &["acct"]);
    static OTHER: AddonDescriptor = desc("other", &["acct"]);
    static LATE: AddonDescriptor = desc("late", &["other"]);
    static STRAY: AddonDescriptor = desc("stray", &["mix"]);
    match name {
        "mix" => &MIX,
        "acct" => &ACCT,
        "acct_x" => &ACCT_X,
        "other" => &OTHER,
        "late" => &LATE,
        "stray" => &STRAY,
        _ => panic!("no test addon {name}"),
    }
}

const fn desc(name: &'static str, depends: &'static [&'static str]) -> AddonDescriptor {
    AddonDescriptor {
        name,
        depends,
        exact_only: false,
    }
}

static MIXIN: ModelDecl = ModelDecl::new("mix", "m.mixin")
    .kind(ModelKind::Abstract)
    .fields(&[
        FieldDecl::char("mixed"),
        FieldDecl::integer("ma"),
        FieldDecl::integer("mb"),
        FieldDecl::many2one("muser_id", "res.users"),
    ]);
static REF: ModelDecl = ModelDecl::new("acct", "acct.ref").fields(&[FieldDecl::char("name")]);
static TAG: ModelDecl = ModelDecl::new("acct", "acct.tag").fields(&[FieldDecl::char("name")]);
static MOVE: ModelDecl = ModelDecl::new("acct", "acct.move")
    .inherit(&["m.mixin"])
    .owner_only_stored_fields(&["acct_x"])
    .fields(&[
        FieldDecl::char("name"),
        FieldDecl::integer("a"),
        FieldDecl::integer("b"),
        FieldDecl::integer("total").generated("a + b"),
        FieldDecl::many2one("ref_id", "acct.ref"),
        FieldDecl::many2many("tag_ids", "acct.tag").relation(
            "acct_move_tag_rel",
            "move_id",
            "tag_id",
        ),
    ]);

fn leak<T>(value: T) -> &'static T {
    Box::leak(Box::new(value))
}

fn fields(list: Vec<FieldDecl>) -> &'static [FieldDecl] {
    Box::leak(list.into_boxed_slice())
}

/// An in-place extension of `acct.move` by `addon`.
fn ext(addon: &'static str, list: Vec<FieldDecl>) -> &'static ModelDecl {
    leak(ModelDecl::extension(addon, "acct.move", &["acct.move"]).fields(fields(list)))
}

fn build_with(
    addons: &[&str],
    decls: &[&'static ModelDecl],
    overlay: Option<&Overlay>,
    policy: ManualFieldPolicy,
) -> Result<Registry> {
    let mut all: Vec<&'static ModelDecl> = vec![&MIXIN, &REF, &TAG, &MOVE];
    all.extend_from_slice(decls);
    // `acct.move` names `acct_x`: a co-owner must be a known addon, so it is always installed
    let mut names = vec!["mix", "acct", "acct_x"];
    names.extend_from_slice(addons);
    let mut builder = RegistryBuilder::new(names.into_iter().map(addon))
        .linked(false)
        .with_decls(all)
        .manual_fields(policy);
    if let Some(overlay) = overlay {
        builder = builder.overlay(overlay);
    }
    builder.build()
}

fn build(addons: &[&str], decls: &[&'static ModelDecl]) -> Result<Registry> {
    build_with(addons, decls, None, ManualFieldPolicy::Strict)
}

fn refusal(addon: &str, field: &str, declared_by: &str) -> String {
    format!(
        "addon '{addon}' cannot add or change stored field 'acct.move.{field}'; owner is 'acct'; \
         declared by {declared_by}"
    )
}

fn def_of(model: &str, addon: &str) -> String {
    format!("model definition '{model}' of addon '{addon}'")
}

fn assert_refused(result: Result<Registry>, message: &str) {
    let error = result.expect_err("the build must refuse");
    assert_eq!(error.kind(), ErrorKind::Programming, "{error}");
    assert_eq!(error.message(), message);
}

/// The materialized column kinds an addon can introduce natively on `acct.move`, by name.
fn materialized(prefix: &'static str) -> Vec<FieldDecl> {
    kinds(prefix, "acct.ref", "ref_id.name", "a + b")
}

/// The same kinds on the mixin `m.mixin`, whose own fields they use.
fn materialized_on_mixin(prefix: &'static str) -> Vec<FieldDecl> {
    kinds(prefix, "res.users", "muser_id.login", "ma + mb")
}

fn kinds(
    prefix: &'static str,
    comodel: &'static str,
    related: &'static str,
    generated: &'static str,
) -> Vec<FieldDecl> {
    let name = |s: &str| -> &'static str { Box::leak(format!("{prefix}_{s}").into_boxed_str()) };
    vec![
        FieldDecl::char(name("plain")),
        FieldDecl::many2one(name("ref_id"), comodel),
        FieldDecl::char(name("computed")).compute("_c").store(true),
        FieldDecl::char(name("related"))
            .related(related)
            .store(true),
        FieldDecl::integer(name("stored_gen"))
            .generated(generated)
            .generated_storage(GeneratedStorage::Stored),
        FieldDecl::char(name("translated")).translate(true),
    ]
}

/// An in-place extension of the owner's mixin `m.mixin` by `addon`.
fn mixin_ext(addon: &'static str, list: Vec<FieldDecl>) -> &'static ModelDecl {
    leak(
        ModelDecl::extension(addon, "m.mixin", &["m.mixin"])
            .kind(ModelKind::Abstract)
            .fields(fields(list)),
    )
}

#[test]
fn spec_ormx_055_owners_control_materialized_field_additions() {
    // native: the owner and the co-owner add every column kind
    let owner = ext("acct", materialized("own"));
    let co = ext("acct_x", materialized("co"));
    let registry = build(&["acct_x"], &[owner, co]).unwrap();
    let model = registry.model("acct.move").unwrap();
    for field in materialized("own").iter().chain(&materialized("co")) {
        let spec = model.field(field.name).unwrap();
        assert!(spec.has_column() && !spec.is_virtual(), "{}", field.name);
    }

    // native: an unrelated addon is refused for each kind, naming addon, model and field
    for field in materialized("u") {
        let name = field.name;
        assert_refused(
            build(&["other"], &[ext("other", vec![field])]),
            &refusal("other", name, &def_of("acct.move", "other")),
        );
    }

    // runtime: the owner's model file adds a column; an unrelated addon's model file is refused
    let runtime = |addon: &str, field: FieldDef| {
        let mut overlay = Overlay::new();
        overlay
            .add_addon_defs([ModelDef::extension(addon, "acct.move").field(field)])
            .unwrap();
        overlay
    };
    let char_def = |name: &str| FieldDef::new(name, KindDef::Char { size: None });
    let own = runtime("acct", char_def("own_runtime"));
    build_with(&[], &[], Some(&own), ManualFieldPolicy::Strict).unwrap();
    for field in [
        char_def("u_runtime"),
        FieldDef::new(
            "u_runtime",
            KindDef::Many2one {
                comodel: "acct.ref".to_owned(),
                ondelete: None,
            },
        ),
        char_def("u_runtime").compute("_c").store(true),
        char_def("u_runtime").related("ref_id.name").store(true),
        FieldDef::new("u_runtime", KindDef::Integer)
            .generated("a + b")
            .generated_storage(GeneratedStorage::Stored),
        char_def("u_runtime").translate(true),
    ] {
        let overlay = runtime("other", field);
        assert_refused(
            build_with(&["other"], &[], Some(&overlay), ManualFieldPolicy::Strict),
            &refusal("other", "u_runtime", &def_of("acct.move", "other")),
        );
    }

    // overlay: a custom stored field is refused, even for the owner's database
    for field in [
        char_def("x_note"),
        char_def("x_note").related("ref_id.name").store(true),
        FieldDef::new("x_note", KindDef::Integer)
            .generated("a + b")
            .generated_storage(GeneratedStorage::Stored),
    ] {
        let mut overlay = Overlay::new();
        overlay.add_manual_field("acct.move", field).unwrap();
        assert_refused(
            build_with(&[], &[], Some(&overlay), ManualFieldPolicy::Strict),
            &refusal("(manual)", "x_note", &def_of("acct.move", "(manual)")),
        );
    }

    // inherited: the owner's mixin extended by the co-owner adds every kind; by others, none
    let registry = build(
        &["acct_x"],
        &[mixin_ext("acct_x", materialized_on_mixin("cm"))],
    )
    .unwrap();
    let model = registry.model("acct.move").unwrap();
    for field in materialized_on_mixin("cm") {
        let spec = model.field(field.name).unwrap();
        assert!(spec.has_column() && !spec.is_virtual(), "{}", field.name);
    }
    for field in materialized_on_mixin("s") {
        let name = field.name;
        assert_refused(
            build(&["stray"], &[mixin_ext("stray", vec![field])]),
            &refusal("stray", name, &def_of("m.mixin", "stray")),
        );
    }

    // inherited: a mixin the unrelated addon adds brings its stored field under that addon's name
    let mixin = leak(
        ModelDecl::new("other", "o.mixin")
            .kind(ModelKind::Abstract)
            .fields(const { &[FieldDecl::char("o_mixed")] }),
    );
    let edge = leak(ModelDecl::extension(
        "other",
        "acct.move",
        &["acct.move", "o.mixin"],
    ));
    assert_refused(
        build(&["other"], &[mixin, edge]),
        &refusal("other", "o_mixed", &def_of("o.mixin", "other")),
    );

    // policy replacement: an extension cannot set, replace or widen the policy
    let takeover = leak(
        ModelDecl::extension("other", "acct.move", &["acct.move"])
            .owner_only_stored_fields(&["other"]),
    );
    assert_refused(
        build(&["other"], &[takeover]),
        "model definition 'acct.move' of addon 'other' cannot set owner_only_stored_fields on \
         model 'acct.move'; only its owner 'acct' declares the policy",
    );

    // storage override: an unrelated addon cannot dematerialize or retype an owner column
    for (field, name) in [
        (FieldDecl::char("name").store(false), "name"),
        (FieldDecl::text("name"), "name"),
        (FieldDecl::char("name").size(8), "name"),
        (FieldDecl::char("name").compute("_c"), "name"),
        (
            FieldDecl::integer("total").generated_storage(GeneratedStorage::Stored),
            "total",
        ),
        (FieldDecl::integer("total").generated("a + a"), "total"),
        (FieldDecl::many2one("ref_id", "acct.tag"), "ref_id"),
    ] {
        assert_refused(
            build(&["other"], &[ext("other", vec![field])]),
            &refusal("other", name, &def_of("acct.move", "other")),
        );
    }
}

#[test]
fn mixin_provenance_follows_the_edge_and_the_extension() {
    let registry = build(&[], &[]).unwrap();
    let mixed = registry.model("acct.move").unwrap().field("mixed").unwrap();
    assert!(mixed.has_column());

    assert_refused(
        build(
            &["stray"],
            &[mixin_ext("stray", vec![FieldDecl::char("stray_mixed")])],
        ),
        &refusal("stray", "stray_mixed", &def_of("m.mixin", "stray")),
    );
    // an unrelated addon repeating a mixin the owner already brings is refused too (plan choice)
    let repeat = leak(ModelDecl::extension(
        "other",
        "acct.move",
        &["acct.move", "m.mixin"],
    ));
    assert_refused(
        build(&["other"], &[repeat]),
        &refusal("other", "mixed", &def_of("m.mixin", "mix")),
    );
}

#[test]
fn runtime_mixin_edges_and_dropped_custom_fields() {
    let mixin = leak(
        ModelDecl::new("other", "o.mixin")
            .kind(ModelKind::Abstract)
            .fields(const { &[FieldDecl::char("o_mixed")] }),
    );
    let mut def = ModelDef::extension("other", "acct.move");
    def.inherit.push("o.mixin".to_owned());
    let mut overlay = Overlay::new();
    overlay.add_addon_defs([def]).unwrap();
    assert_refused(
        build_with(
            &["other"],
            &[mixin],
            Some(&overlay),
            ManualFieldPolicy::Strict,
        ),
        &refusal("other", "o_mixed", &def_of("o.mixin", "other")),
    );

    let mut overlay = Overlay::new();
    for name in ["x_stored", "x_projection"] {
        let field = FieldDef::new(name, KindDef::Char { size: None });
        let field = if name == "x_projection" {
            field.related("ref_id.name")
        } else {
            field
        };
        overlay.add_manual_field("acct.move", field).unwrap();
    }
    let registry = build_with(&[], &[], Some(&overlay), ManualFieldPolicy::DropBroken).unwrap();
    let model = registry.model("acct.move").unwrap();
    assert!(model.field("x_stored").is_none());
    assert!(model.field("x_projection").is_some());
    let dropped = registry.dropped_fields();
    assert_eq!(dropped.len(), 1);
    assert_eq!(
        (dropped[0].model(), dropped[0].field()),
        ("acct.move", "x_stored")
    );
    assert_eq!(dropped[0].reason(), &DropReason::Refused);
    assert_eq!(
        dropped[0].error().message(),
        refusal("(manual)", "x_stored", &def_of("acct.move", "(manual)"))
    );
}

#[test]
fn policy_declaration_is_validated() {
    let model = |co: &'static [&'static str]| {
        leak(
            ModelDecl::new("other", "o.doc")
                .owner_only_stored_fields(co)
                .fields(const { &[FieldDecl::char("name")] }),
        )
    };
    let refused = |addons: &[&str], decl: &'static ModelDecl, message: &str| {
        assert_refused(build(addons, &[decl]), message);
    };
    let at = "model definition 'o.doc' of addon 'other'";
    refused(
        &["other"],
        model(&["other"]),
        &format!("{at}: co-owner 'other' of owner_only_stored_fields is the owner itself"),
    );
    refused(
        &["other", "late"],
        model(&["late", "late"]),
        &format!("{at}: co-owner 'late' of owner_only_stored_fields is listed twice"),
    );
    refused(
        &["other"],
        model(&[""]),
        &format!("{at}: co-owner '' of owner_only_stored_fields is not an addon name"),
    );
    refused(
        &["other"],
        model(&["émile"]),
        &format!("{at}: co-owner 'émile' of owner_only_stored_fields is not an addon name"),
    );
    refused(
        &["other"],
        model(&["(manual)"]),
        &format!("{at}: co-owner '(manual)' of owner_only_stored_fields is not an addon name"),
    );
    refused(
        &["other", "stray"],
        model(&["stray"]),
        &format!(
            "{at}: co-owner 'stray' of owner_only_stored_fields does not depend on the owner \
             'other'"
        ),
    );
    // an addon neither installed nor linked is unknown; an installed dependent co-owner is admitted
    refused(
        &["other"],
        model(&["late"]),
        &format!(
            "{at}: co-owner 'late' of owner_only_stored_fields is not a known addon (neither in \
             the addon set nor linked into this build)"
        ),
    );
    build(&["other", "late"], &[model(&["late"])]).unwrap();
    // a native addon linked into the build but not installed has no effect
    RegistryBuilder::new(["mix", "acct", "acct_x", "other"].into_iter().map(addon))
        .with_decls([&MIXIN, &REF, &TAG, &MOVE, model(&["ghost"])])
        .build()
        .unwrap();

    let mut abstract_model = ModelDecl::new("other", "o.abstract")
        .kind(ModelKind::Abstract)
        .owner_only_stored_fields(&[]);
    refused(
        &["other"],
        leak(abstract_model),
        "model definition 'o.abstract' of addon 'other' sets owner_only_stored_fields on an \
         abstract model; only a concrete model has stored columns",
    );
    abstract_model = ModelDecl::new("other", "o.runtime").owner_only_stored_fields(&[]);
    abstract_model.runtime = true;
    refused(
        &["other"],
        leak(abstract_model),
        "model definition 'o.runtime' of addon 'other' cannot set owner_only_stored_fields: the \
         policy is declared only in native addon code",
    );
}

#[test]
fn labels_stay_open_and_unprotected_models_stay_open() {
    let relabel = ext(
        "other",
        vec![
            FieldDecl::char("name").string("Label").help("Help"),
            FieldDecl::integer("total").help("Virtual sum"),
        ],
    );
    let registry = build(&["other"], &[relabel]).unwrap();
    assert_eq!(
        registry
            .model("acct.move")
            .unwrap()
            .field("name")
            .unwrap()
            .string,
        "Label"
    );
    let open = leak(ModelDecl::new("other", "o.doc").fields(const { &[FieldDecl::char("name")] }));
    let open_ext = leak(
        ModelDecl::extension("late", "o.doc", &["o.doc"])
            .fields(const { &[FieldDecl::char("extra")] }),
    );
    let registry = build(&["other", "late"], &[open, open_ext]).unwrap();
    assert!(
        registry
            .model("o.doc")
            .unwrap()
            .field("extra")
            .unwrap()
            .has_column()
    );
}

#[test]
fn unrelated_row_version_is_refused() {
    let rv = leak(ModelDecl::extension("other", "acct.move", &["acct.move"]).row_version(true));
    assert_refused(
        build(&["other"], &[rv]),
        &refusal("other", "row_version", &def_of("acct.move", "other")),
    );
    let own = leak(ModelDecl::extension("acct_x", "acct.move", &["acct.move"]).row_version(true));
    build(&["acct_x"], &[own]).unwrap();
}

#[test]
fn spec_ormx_056_nonmaterialized_extensions_and_companions_remain_possible() {
    let companion = leak(ModelDecl::new("other", "other.note").fields(
        const {
            &[
                FieldDecl::many2one("move_id", "acct.move"),
                FieldDecl::char("text"),
            ]
        },
    ));
    let extension = ext(
        "other",
        vec![
            FieldDecl::char("u_computed").compute("_c"),
            FieldDecl::char("u_related").related("ref_id.name"),
            FieldDecl::integer("u_virtual").generated("a + b"),
            FieldDecl::one2many("u_note_ids", "other.note", "move_id"),
            FieldDecl::many2many("u_tag_ids", "acct.tag").relation(
                "other_move_tag_rel",
                "move_id",
                "tag_id",
            ),
        ],
    );
    let registry = build(&["other"], &[companion, extension]).unwrap();
    let model = registry.model("acct.move").unwrap();
    for name in ["u_computed", "u_related", "u_note_ids", "u_tag_ids"] {
        assert!(!model.field(name).unwrap().has_column(), "{name}");
    }
    let virtual_field = model.field("u_virtual").unwrap();
    assert!(virtual_field.store && virtual_field.is_virtual());

    // turning those additions into columns is refused, by the addon that does it
    let declared = def_of("acct.move", "late");
    for (field, name) in [
        (FieldDecl::char("u_computed").store(true), "u_computed"),
        (FieldDecl::char("u_related").store(true), "u_related"),
        (
            FieldDecl::integer("u_virtual").generated_storage(GeneratedStorage::Stored),
            "u_virtual",
        ),
    ] {
        assert_refused(
            build(
                &["other", "late"],
                &[companion, extension, ext("late", vec![field])],
            ),
            &refusal("late", name, &declared),
        );
    }
    // and by the addon that added them
    assert_refused(
        build(
            &["other"],
            &[ext(
                "other",
                vec![FieldDecl::char("u_computed").compute("_c").store(true)],
            )],
        ),
        &refusal("other", "u_computed", &def_of("acct.move", "other")),
    );
}

#[test]
fn guarded_relation_tables_follow_their_owner_group() {
    // an unrelated inverse many2many reusing the owner's relation table is refused
    let steal = leak(
        ModelDecl::extension("other", "acct.tag", &["acct.tag"]).fields(
            const {
                &[FieldDecl::many2many("move_ids", "acct.move").relation(
                    "acct_move_tag_rel",
                    "tag_id",
                    "move_id",
                )]
            },
        ),
    );
    assert_refused(
        build(&["other"], &[steal]),
        "addon 'other' cannot add or change stored field 'acct.tag.move_ids'; owner is 'acct'; \
         declared by model definition 'acct.tag' of addon 'other'; relation table \
         'acct_move_tag_rel' belongs to 'acct.move.tag_ids'",
    );
    // a co-owner may share it
    let shared = leak(
        ModelDecl::extension("acct_x", "acct.tag", &["acct.tag"]).fields(
            const {
                &[FieldDecl::many2many("move_ids", "acct.move").relation(
                    "acct_move_tag_rel",
                    "tag_id",
                    "move_id",
                )]
            },
        ),
    );
    build(&["acct_x"], &[shared]).unwrap();
    // an unrelated addon cannot change the guarded relation itself
    let rewire = ext(
        "other",
        vec![FieldDecl::many2many("tag_ids", "acct.tag").relation(
            "acct_move_tag_rel",
            "move_id",
            "other_tag_id",
        )],
    );
    assert_refused(
        build(&["other"], &[rewire]),
        &refusal("other", "tag_ids", &def_of("acct.move", "other")),
    );
    // a companion relation an unrelated addon created stays with it
    let companion = ext(
        "other",
        vec![FieldDecl::many2many("u_tag_ids", "acct.tag").relation(
            "other_move_tag_rel",
            "move_id",
            "tag_id",
        )],
    );
    let take = ext(
        "late",
        vec![FieldDecl::many2many("u_tag_ids", "acct.tag").relation(
            "late_move_tag_rel",
            "move_id",
            "tag_id",
        )],
    );
    assert_refused(
        build(&["other", "late"], &[companion, take]),
        &refusal("late", "u_tag_ids", &def_of("acct.move", "late")),
    );
}

/// A native declaration of an addon that the tests never install: linked, so a known addon.
#[erp_registry::linkme::distributed_slice(erp_registry::MODEL_DECLS)]
#[linkme(crate = erp_registry::linkme)]
static GHOST: ModelDecl = ModelDecl::new("ghost", "ghost.thing");

#[test]
fn protected_tables_cannot_be_taken_by_another_model() {
    let shadow = |addon: &'static str, table: &'static str| {
        leak(
            ModelDecl::new(addon, "x.shadow")
                .table(table)
                .fields(const { &[FieldDecl::char("shadow_note")] }),
        )
    };
    assert_refused(
        build(&["other"], &[shadow("other", "acct_move")]),
        "addon 'other' cannot store model 'x.shadow' in table 'acct_move'; owner is 'acct'; \
         declared by model definition 'x.shadow' of addon 'other'; table 'acct_move' belongs to \
         'acct.move'",
    );
    assert_refused(
        build(&["other"], &[shadow("other", "acct_move_tag_rel")]),
        "addon 'other' cannot store model 'x.shadow' in table 'acct_move_tag_rel'; owner is \
         'acct'; declared by model definition 'x.shadow' of addon 'other'; relation table \
         'acct_move_tag_rel' belongs to 'acct.move.tag_ids'",
    );
    // the co-owner may share the owner's table
    let registry = build(&["acct_x"], &[shadow("acct_x", "acct_move")]).unwrap();
    assert_eq!(registry.model("x.shadow").unwrap().table, "acct_move");

    // a many2many of another addon whose relation is the owner's own table
    let m2m = leak(
        ModelDecl::extension("other", "acct.tag", &["acct.tag"]).fields(
            const {
                &[FieldDecl::many2many("move_ids", "acct.move").relation(
                    "acct_move",
                    "tag_id",
                    "move_id",
                )]
            },
        ),
    );
    assert_refused(
        build(&["other"], &[m2m]),
        "addon 'other' cannot add or change stored field 'acct.tag.move_ids'; owner is 'acct'; \
         declared by model definition 'acct.tag' of addon 'other'; table 'acct_move' belongs to \
         'acct.move'",
    );
}
