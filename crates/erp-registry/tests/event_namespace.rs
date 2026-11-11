//! The closed event table namespace (A2-D5, plan S1): who may place a model in
//! the event schema, what such a model refuses, and the native `server_set` timestamp that only it
//! may carry. Pure: no database.
#![allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]

use erp_fields::{ErrorKind, OnDelete};
use erp_registry::{
    AddonDescriptor, DropReason, FieldDecl, FieldDef, KindDef, ManualFieldPolicy, ModelDecl,
    Overlay, PartitionDecl, PartitionInterval, PartitionRetain, Registry, RegistryBuilder,
    TableNamespace,
};

static ADDON: AddonDescriptor = AddonDescriptor {
    name: "test_event",
    depends: &["base"],
    exact_only: false,
};

fn leak(decl: ModelDecl) -> &'static ModelDecl {
    Box::leak(Box::new(decl))
}

fn daily() -> PartitionDecl {
    PartitionDecl::new(PartitionInterval::Day, 7, PartitionRetain::Intervals(30))
}

/// The leaf event model: a required, server-set `timestamptz` partition key, no magic columns.
fn event() -> ModelDecl {
    ModelDecl::new("test_event", "test.event")
        .log_access(false)
        .fields(
            const {
                &[
                    FieldDecl::timestamptz("recorded_at")
                        .required(true)
                        .server_set(true),
                    FieldDecl::char("name"),
                ]
            },
        )
        .partition(daily().by("recorded_at"))
}

fn build_core(
    event_decls: Vec<ModelDecl>,
    addon_decls: Vec<ModelDecl>,
) -> erp_fields::Result<Registry> {
    RegistryBuilder::new([&ADDON])
        .linked(false)
        .with_core_event_decls(event_decls.into_iter().map(leak))
        .with_decls(addon_decls.into_iter().map(leak))
        .build()
}

#[track_caller]
fn refused(result: erp_fields::Result<Registry>, message: &str) {
    let error = result.expect_err("the registry must refuse");
    assert_eq!(error.kind(), ErrorKind::Programming, "{error:?}");
    assert_eq!(error.message(), message);
}

/// Only the core declaration path selects the event namespace: the same declaration given through the
/// addon path stays in the current schema, and an addon cannot reach the schema through `_table`.
#[test]
fn addon_model_cannot_select_event_namespace() {
    let registry = build_core(vec![event()], vec![]).unwrap();
    let model = registry.model("test.event").unwrap();
    assert_eq!(model.namespace, TableNamespace::Event);
    assert_eq!(model.sql_table(), "\"erp_event\".\"test_event\"");
    // generic schema sync, uninstall and vacuum loops skip it
    assert!(model.has_table() && !model.has_generic_table());
    // the bare quoted accessor stays the table alone: only `sql_table` is schema-qualified
    assert_eq!(model.quoted_table(), "\"test_event\"");

    // a model given through the addon path is an ordinary model in the current schema
    let plain = leak(
        ModelDecl::new("test_event", "test.plain").fields(const { &[FieldDecl::char("name")] }),
    );
    let registry = RegistryBuilder::new([&ADDON])
        .linked(false)
        .with_decls([plain])
        .build()
        .unwrap();
    let model = registry.model("test.plain").unwrap();
    assert_eq!(model.namespace, TableNamespace::Current);
    assert_eq!(model.sql_table(), "\"test_plain\"");
    assert!(model.has_generic_table());

    // a dotted `_table` is one identifier to the registry and is refused, never read as schema.table
    let forged = leak(ModelDecl::new("test_event", "test.forged").table("erp_event.outbox"));
    let error = RegistryBuilder::new([&ADDON])
        .linked(false)
        .with_decls([forged])
        .build()
        .expect_err("a dotted table name must be refused");
    assert_eq!(error.kind(), ErrorKind::Validation, "{error:?}");
    assert_eq!(
        error.message(),
        "Invalid characters in table name 'erp_event.outbox'"
    );
}

/// The event model is a closed leaf: no extension (addon or runtime overlay), no `_inherit` from it, no
/// `_inherits`, no many2one or many2many reaching it, no many2many of its own.
#[test]
fn event_model_refuses_inherit_extension_and_inbound_relations() {
    // an addon extension of the event model
    static EXT_FIELDS: [FieldDecl; 1] = [FieldDecl::char("extra")];
    refused(
        build_core(
            vec![event()],
            vec![
                ModelDecl::extension("test_event", "test.event", &["test.event"])
                    .fields(&EXT_FIELDS),
            ],
        ),
        "model definition 'test.event' of addon 'test_event' extends 'test.event', which is in the \
         closed event namespace; its owner's declaration is the only one it takes",
    );
    // a new model inheriting from it
    refused(
        build_core(
            vec![event()],
            vec![ModelDecl::new("test_event", "test.child").inherit(&["test.event"])],
        ),
        "model definition 'test.child' of addon 'test_event' inherits from 'test.event', which is in \
         the closed event namespace",
    );
    // `_inherits` delegation to it
    refused(
        build_core(
            vec![event()],
            vec![
                ModelDecl::new("test_event", "test.delegating")
                    .inherits(&[("test.event", "event_id")])
                    .fields(
                        const {
                            &[FieldDecl::many2one("event_id", "test.event")
                                .required(true)
                                .ondelete(OnDelete::Cascade)]
                        },
                    ),
            ],
        ),
        "model 'test.event' is in the closed event namespace, so no field may reference it; \
         'test.delegating.event_id' does",
    );
    // a many2one reaching it
    refused(
        build_core(
            vec![event()],
            vec![
                ModelDecl::new("test_event", "test.pointer")
                    .fields(const { &[FieldDecl::many2one("event_id", "test.event")] }),
            ],
        ),
        "model 'test.event' is in the closed event namespace, so no field may reference it; \
         'test.pointer.event_id' does",
    );
    // a many2many reaching it
    refused(
        build_core(
            vec![event()],
            vec![
                ModelDecl::new("test_event", "test.tagged")
                    .fields(const { &[FieldDecl::many2many("events", "test.event")] }),
            ],
        ),
        "model 'test.event' is in the closed event namespace, so no field may reference it; \
         'test.tagged.events' does",
    );
    // a many2many of its own (its relation table would live outside the event schema)
    refused(
        build_core(
            vec![
                ModelDecl::new("test_event", "test.event")
                    .log_access(false)
                    .fields(
                        const {
                            &[
                                FieldDecl::timestamptz("recorded_at")
                                    .required(true)
                                    .server_set(true),
                                FieldDecl::many2many("tags", "res.users"),
                            ]
                        },
                    )
                    .partition(daily().by("recorded_at")),
            ],
            vec![],
        ),
        "model 'test.event' is in the closed event namespace and takes no relational field; \
         'test.event.tags' is one",
    );
    // a soft reference (model name as data) is not a relation and stays allowed
    build_core(
        vec![event()],
        vec![ModelDecl::new("test_event", "test.soft").fields(
            const {
                &[
                    FieldDecl::char("res_model"),
                    FieldDecl::many2one_reference("res_id", "res_model"),
                ]
            },
        )],
    )
    .unwrap();

    // a runtime overlay cannot add a custom field to it: refused strictly, dropped like any
    // broken custom field otherwise
    let mut overlay = Overlay::new();
    overlay
        .add_manual_field(
            "test.event",
            FieldDef::new("x_note", KindDef::Char { size: None }),
        )
        .unwrap();
    let overlaid = |policy| {
        RegistryBuilder::new([&ADDON])
            .linked(false)
            .with_core_event_decls([leak(event())])
            .overlay(&overlay)
            .manual_fields(policy)
            .build()
    };
    let message = "model definition 'test.event' of addon '(manual)' extends 'test.event', which is \
                   in the closed event namespace; its owner's declaration is the only one it takes";
    refused(overlaid(ManualFieldPolicy::Strict), message);
    let registry = overlaid(ManualFieldPolicy::DropBroken).unwrap();
    let dropped = registry.dropped_fields();
    assert_eq!(dropped.len(), 1);
    assert_eq!(
        (dropped[0].model(), dropped[0].field()),
        ("test.event", "x_note")
    );
    assert_eq!(*dropped[0].reason(), DropReason::Refused);
    assert!(
        registry
            .model("test.event")
            .unwrap()
            .field("x_note")
            .is_none()
    );
}

/// A native `server_set` timestamptz outside the event namespace is refused, naming the slice that adds
/// it; a UUID or xid8 `server_set` is refused everywhere in this step.
#[test]
fn native_server_set_outside_event_namespace_is_refused() {
    let error = RegistryBuilder::new([&ADDON])
        .linked(false)
        .with_decls([leak(ModelDecl::new("test_event", "test.stamped").fields(
            const { &[FieldDecl::timestamptz("recorded_at").server_set(true)] },
        ))])
        .build()
        .expect_err("native server_set outside the event namespace must be refused");
    assert_eq!(error.kind(), ErrorKind::Programming, "{error:?}");
    assert_eq!(
        error.message(),
        "field 'test.stamped.recorded_at': a native timestamptz server_set is admitted only in the \
         closed event namespace (a later release); server_set on public models has no slice that adds it"
    );
    // uuid / xid8 keep the existing refusal, event namespace or not
    refused(
        build_core(
            vec![
                ModelDecl::new("test_event", "test.event")
                    .log_access(false)
                    .fields(const { &[FieldDecl::uuid("event_id").server_set(true)] }),
            ],
            vec![],
        ),
        "field 'test.event.event_id': server_set is for stored datetime fields without a compute, \
         related path or default",
    );
}

/// A `timestamptz` server-set required field is a partition key only for the event namespace; a plain
/// `timestamptz` key is refused there too.
#[test]
fn timestamptz_server_set_partition_key_admitted_only_for_event_namespace() {
    let registry = build_core(vec![event()], vec![]).unwrap();
    let model = registry.model("test.event").unwrap();
    let partition = model.partition.as_ref().unwrap();
    assert_eq!(partition.key_name(), "recorded_at");

    // the key must still be required and server-set: a caller-supplied timestamptz is refused
    refused(
        build_core(
            vec![
                ModelDecl::new("test_event", "test.event")
                    .log_access(false)
                    .fields(const { &[FieldDecl::timestamptz("recorded_at").required(true)] })
                    .partition(daily().by("recorded_at")),
            ],
            vec![],
        ),
        "partition key 'recorded_at' of 'test.event' must be create_date or a stored, required \
         datetime field declared server_set",
    );

    // outside the event namespace a timestamptz key is not a partition key
    let error = RegistryBuilder::new([&ADDON])
        .linked(false)
        .with_decls([leak(
            ModelDecl::new("test_event", "test.scale")
                .log_access(false)
                .fields(const { &[FieldDecl::timestamptz("recorded_at").required(true)] })
                .partition(daily().by("recorded_at")),
        )])
        .build()
        .expect_err("a timestamptz partition key outside the event namespace must be refused");
    assert_eq!(error.kind(), ErrorKind::Programming, "{error:?}");
    assert_eq!(
        error.message(),
        "partition key 'recorded_at' of 'test.scale' must be create_date or a stored, required \
         datetime field declared server_set"
    );
}
