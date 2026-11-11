//! Tracked fields at registry build (design
//! D6). Pure: no database.
#![allow(clippy::unwrap_used, clippy::panic, reason = "tests")]

use erp_fields::{ErrorKind, OnDelete};
use erp_registry::{
    AddonDescriptor, DEFAULT_TRACKING_SEQUENCE, FieldDecl, MAX_TRACKED_FIELDS, ModelDecl, Registry,
    RegistryBuilder,
};

static TRACK: AddonDescriptor = AddonDescriptor {
    name: "track",
    depends: &["base"],
    exact_only: false,
};
static TRACK_EXT: AddonDescriptor = AddonDescriptor {
    name: "track_ext",
    depends: &["track"],
    exact_only: false,
};

fn build(decls: Vec<ModelDecl>) -> erp_fields::Result<Registry> {
    let decls: Vec<&'static ModelDecl> = decls
        .into_iter()
        .map(|d| &*Box::leak(Box::new(d)))
        .collect();
    RegistryBuilder::new([&TRACK, &TRACK_EXT])
        .with_decls(decls)
        .build()
}

fn leak<T>(items: Vec<T>) -> &'static [T] {
    Vec::leak(items)
}

fn model(fields: Vec<FieldDecl>) -> ModelDecl {
    ModelDecl::new("track", "track.x").fields(leak(fields))
}

fn refusal(decls: Vec<ModelDecl>) -> (ErrorKind, String) {
    let err = build(decls).unwrap_err();
    (err.kind(), err.message().to_owned())
}

#[test]
fn spec_mail_049_tracking_attribute_is_accepted() {
    let registry = build(vec![model(vec![
        FieldDecl::char("name").tracking(DEFAULT_TRACKING_SEQUENCE),
        FieldDecl::integer("qty").tracking(20),
        FieldDecl::char("note"),
    ])])
    .unwrap();
    let spec = registry.model("track.x").unwrap();
    assert_eq!(spec.field("name").unwrap().tracking, Some(100));
    assert_eq!(spec.field("qty").unwrap().tracking, Some(20));
    assert_eq!(spec.field("note").unwrap().tracking, None);
}

/// re-declaration of a field)
#[test]
fn spec_mail_049_an_extension_can_switch_tracking_off_and_on() {
    let registry = build(vec![
        model(vec![
            FieldDecl::char("name").tracking(10),
            FieldDecl::char("code"),
        ]),
        ModelDecl::extension("track_ext", "track.x", &["track.x"]).fields(leak(vec![
            FieldDecl::char("name").untracked(),
            FieldDecl::char("code").tracking(5),
        ])),
    ])
    .unwrap();
    let spec = registry.model("track.x").unwrap();
    assert_eq!(spec.field("name").unwrap().tracking, None);
    assert_eq!(spec.field("code").unwrap().tracking, Some(5));
}

#[test]
fn spec_mail_055_tracked_fields_are_ordered_by_sequence_then_name() {
    let registry = build(vec![model(vec![
        FieldDecl::char("z").tracking(50),
        FieldDecl::char("b").tracking(10),
        FieldDecl::char("a").tracking(10),
        FieldDecl::char("plain"),
    ])])
    .unwrap();
    let spec = registry.model("track.x").unwrap();
    let names: Vec<&str> = spec
        .tracked
        .iter()
        .map(|id| spec.field_by_id(*id).name)
        .collect();
    assert_eq!(names, ["a", "b", "z"]);
}

/// D6: what the capture cannot serve is refused at registry build, with the field named.
#[test]
fn tracking_refusals_name_the_field_and_the_reason() {
    assert_eq!(
        refusal(vec![model(vec![FieldDecl::char("name").tracking(0)])]),
        (
            ErrorKind::Programming,
            "field 'track.x.name': tracking = 0 is refused (the reference ERP reads it as untracked in one \
             place and as tracked in another): use tracking = false to switch tracking off, or a \
             positive sequence"
                .to_owned()
        )
    );
    assert_eq!(
        refusal(vec![model(vec![
            FieldDecl::char("name").store(false).tracking(10)
        ])]),
        (
            ErrorKind::Programming,
            "field 'track.x.name': tracking needs a stored field (this field is not stored)"
                .to_owned()
        )
    );
    assert_eq!(
        refusal(vec![
            model(vec![FieldDecl::many2one("owner_id", "track.x")]),
            ModelDecl::new("track", "track.y").fields(leak(vec![
                FieldDecl::one2many("line_ids", "track.x", "owner_id").tracking(10)
            ])),
        ]),
        (
            ErrorKind::Programming,
            "field 'track.y.line_ids': tracking is not supported on a one2many field (it has no \
             value of its own: track the many2one of its lines)"
                .to_owned()
        )
    );
    let translated = refusal(vec![model(vec![
        FieldDecl::char("name").translate(true).tracking(10),
    ])]);
    assert_eq!(translated.0, ErrorKind::Programming);
    assert!(
        translated
            .1
            .starts_with("field 'track.x.name': tracking a translated or company-dependent field"),
        "{}",
        translated.1
    );
    let dependent = refusal(vec![model(vec![
        FieldDecl::integer("limit")
            .company_dependent(true)
            .tracking(10),
    ])]);
    assert!(
        dependent
            .1
            .starts_with("field 'track.x.limit': tracking a translated or company-dependent field"),
        "{}",
        dependent.1
    );
}

/// D6: a model tracks at most [`MAX_TRACKED_FIELDS`] fields (the limit itself is allowed).
#[test]
fn a_model_tracks_at_most_the_limit() {
    let fields = |n: usize| -> Vec<FieldDecl> {
        (0..n)
            .map(|i| {
                let name: &'static str = Box::leak(format!("f{i:04}").into_boxed_str());
                FieldDecl::integer(name).tracking(10)
            })
            .collect()
    };
    build(vec![model(fields(MAX_TRACKED_FIELDS))]).unwrap();
    let (kind, message) = refusal(vec![model(fields(MAX_TRACKED_FIELDS + 1))]);
    assert_eq!(kind, ErrorKind::Programming);
    assert_eq!(
        message,
        format!(
            "model 'track.x' tracks {} fields, more than the {MAX_TRACKED_FIELDS} a model may \
             track",
            MAX_TRACKED_FIELDS + 1
        )
    );
}

/// M1: a tracked stored computed field would never be captured (records are prepared by write,
/// create and upsert, not by a recompute), so the registry refuses it loudly.
#[test]
fn tracking_a_stored_computed_field_is_refused() {
    assert_eq!(
        refusal(vec![model(vec![
            FieldDecl::char("total")
                .compute("_compute_total")
                .store(true)
                .tracking(10)
        ])]),
        (
            ErrorKind::Programming,
            "field 'track.x.total': tracking a computed field is not supported yet \
             (a later release tracks fields written by write, create and upsert; tracking \
             recomputed fields belongs to a later slice)"
                .to_owned()
        )
    );
}

/// S2: the `_inherits` copy of a tracked parent field is not tracked (nothing is captured on the
/// child; the change is reported under the parent model), and `describe` must not claim it is.
#[test]
fn an_inherited_copy_of_a_tracked_field_is_not_tracked() {
    let registry = build(vec![
        model(vec![FieldDecl::char("name").tracking(10)]),
        ModelDecl::new("track", "track.child")
            .inherits(&[("track.x", "x_id")])
            .fields(leak(vec![
                FieldDecl::many2one("x_id", "track.x")
                    .required(true)
                    .ondelete(OnDelete::Cascade),
            ])),
    ])
    .unwrap();
    assert_eq!(
        registry
            .model("track.x")
            .unwrap()
            .field("name")
            .unwrap()
            .tracking,
        Some(10)
    );
    let child = registry.model("track.child").unwrap();
    assert!(child.field("name").unwrap().inherited.is_some());
    assert_eq!(child.field("name").unwrap().tracking, None);
    assert!(child.tracked.is_empty());
}

/// S1: a database-generated column is computed by the database, so no write captures it.
#[test]
fn tracking_a_generated_field_is_refused() {
    assert_eq!(
        refusal(vec![model(vec![
            FieldDecl::integer("a"),
            FieldDecl::integer("total").generated("a + a").tracking(10),
        ])]),
        (
            ErrorKind::Programming,
            "field 'track.x.total': tracking a database-generated field is not supported (the \
             database computes its value, no write of the record captures it)"
                .to_owned()
        )
    );
}
