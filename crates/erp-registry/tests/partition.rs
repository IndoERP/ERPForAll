//! Leaf partitioning in the registry (–7.2, §7.8):
//! who may declare it, the leaf rule, the policy tables, the partition key and `server_set`.
//! Pure: no database.
#![allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]

use erp_fields::RangeBounds;
use erp_fields::{ErrorKind, OnDelete};
use erp_registry::{
    AddonDescriptor, DropReason, ExclusionDecl, FieldDecl, FieldDef, KeyDecl, KindDef,
    ManualFieldPolicy, ModelDecl, ModelKind, Overlay, PartitionDecl, PartitionInterval,
    PartitionRetain, Registry, RegistryBuilder, TableObjectDecl, TableObjectKind, TemporalKeyDecl,
};

static PART: AddonDescriptor = AddonDescriptor {
    name: "test_part",
    depends: &["base"],
    exact_only: false,
};
static PART_EXT: AddonDescriptor = AddonDescriptor {
    name: "test_part_ext",
    depends: &["test_part"],
    exact_only: false,
};

const EVENT: &[&str] = &["scale.event"];
const PLAIN: &[&str] = &["scale.plain"];

fn daily() -> PartitionDecl {
    PartitionDecl::new(PartitionInterval::Day, 2, PartitionRetain::Intervals(3))
}

/// `scale.event`, partitioned by day on `create_date`, with a plain char field.
fn event() -> ModelDecl {
    ModelDecl::new("test_part", "scale.event")
        .fields(const { &[FieldDecl::char("name")] })
        .partition(daily())
}

fn build(
    addons: &[&'static AddonDescriptor],
    decls: Vec<ModelDecl>,
) -> erp_fields::Result<Registry> {
    let decls: Vec<&'static ModelDecl> = decls
        .into_iter()
        .map(|d| &*Box::leak(Box::new(d)))
        .collect();
    RegistryBuilder::new(addons.iter().copied())
        .with_decls(decls)
        .build()
}

#[track_caller]
fn refused(decls: Vec<ModelDecl>, message: &str) {
    let error = build(&[&PART, &PART_EXT], decls).expect_err("the registry must refuse");
    assert_eq!(error.kind(), ErrorKind::Programming, "{error:?}");
    assert_eq!(error.message(), message);
}

/// A partitioned model is a leaf: a many2one to it, a many2many on it or to it and an `_inherits` of
/// it are refused naming the first referencing field; an extension may not partition a model it
/// does not own; a policy-source model is never partitioned; soft references are allowed.
#[test]
fn spec_ormx_025_leaf_rule_owner_only_and_policy_tables() {
    // (a) a many2one to the partitioned model
    refused(
        vec![
            event(),
            ModelDecl::new("test_part", "scale.ref")
                .fields(const { &[FieldDecl::many2one("event_id", "scale.event")] }),
        ],
        "model 'scale.event' is partitioned, so no field may reference it; 'scale.ref.event_id' \
         does",
    );
    // (b) a many2many on the partitioned model (its relation table would reference `id`) ...
    refused(
        vec![
            ModelDecl::new("test_part", "scale.tag").fields(const { &[FieldDecl::char("name")] }),
            ModelDecl::new("test_part", "scale.event")
                .fields(const { &[FieldDecl::many2many("tag_ids", "scale.tag")] })
                .partition(daily()),
        ],
        "model 'scale.event' is partitioned, so no field may reference it; 'scale.event.tag_ids' \
         does",
    );
    // ... and a many2many of another model to it
    refused(
        vec![
            event(),
            ModelDecl::new("test_part", "scale.other")
                .fields(const { &[FieldDecl::many2many("event_ids", "scale.event")] }),
        ],
        "model 'scale.event' is partitioned, so no field may reference it; \
         'scale.other.event_ids' does",
    );
    // (c) an `_inherits` of it (its delegate many2one)
    refused(
        vec![
            event(),
            ModelDecl::new("test_part", "scale.child")
                .inherits(const { &[("scale.event", "event_id")] })
                .fields(
                    const {
                        &[FieldDecl::many2one("event_id", "scale.event")
                            .required(true)
                            .ondelete(OnDelete::Cascade)]
                    },
                ),
        ],
        "model 'scale.event' is partitioned, so no field may reference it; \
         'scale.child.event_id' does",
    );
    // (d) an extension of a model it does not own
    refused(
        vec![
            ModelDecl::new("test_part", "scale.plain").fields(const { &[FieldDecl::char("name")] }),
            ModelDecl::extension("test_part_ext", "scale.plain", PLAIN).partition(daily()),
        ],
        "partitioning of 'scale.plain' can only be declared by its owner (test_part)",
    );
    // ... also when the owner partitions it already
    refused(
        vec![
            event(),
            ModelDecl::extension("test_part_ext", "scale.event", EVENT).partition(daily()),
        ],
        "partitioning of 'scale.event' can only be declared by its owner (test_part)",
    );
    // (e) a policy-source model
    refused(
        vec![
            ModelDecl::new("test_part", "ir.access")
                .fields(const { &[FieldDecl::char("name")] })
                .partition(daily()),
        ],
        "model 'ir.access' is a policy source and cannot be partitioned",
    );
    // soft references (`res_model`/`res_id`, a reference field) and a one2many the other way are
    // fine; the extension adds a field without touching the partitioning
    let registry = build(
        &[&PART, &PART_EXT],
        vec![
            event(),
            ModelDecl::extension("test_part_ext", "scale.event", EVENT)
                .fields(const { &[FieldDecl::many2one("partner_id", "scale.partner")] }),
            ModelDecl::new("test_part", "scale.partner").fields(
                const {
                    &[FieldDecl::one2many(
                        "event_ids",
                        "scale.event",
                        "partner_id",
                    )]
                },
            ),
            ModelDecl::new("test_part", "scale.note").fields(
                const {
                    &[
                        FieldDecl::char("res_model"),
                        FieldDecl::many2one_reference("res_id", "res_model"),
                        FieldDecl::reference("ref", &[("scale.event", "Event")]),
                    ]
                },
            ),
        ],
    )
    .unwrap();
    let spec = registry.model("scale.event").unwrap();
    let partition = spec.partition.expect("scale.event is partitioned");
    assert_eq!(partition.interval(), PartitionInterval::Day);
    assert_eq!(partition.premake(), 2);
    assert_eq!(partition.retain(), PartitionRetain::Intervals(3));
    assert_eq!(partition.key_name(), "create_date");
    assert_eq!(partition.key(), spec.field_id("create_date").unwrap());
    assert!(registry.model("scale.note").unwrap().partition.is_none());
}

/// required datetime field declared server_set is refused; server_set fields are readonly and not
/// copied. Inserts, refused caller values and the table's primary key: tests/partition.rs of
/// erp-test-models)
#[test]
fn spec_ormx_026_partition_key_must_be_server_time() {
    refused(
        vec![
            ModelDecl::new("test_part", "scale.other")
                .fields(const { &[FieldDecl::datetime("happened_at").required(true)] })
                .partition(daily().by("happened_at")),
        ],
        "partition key 'happened_at' of 'scale.other' must be create_date or a stored, required \
         datetime field declared server_set",
    );
    // server_set but not required
    refused(
        vec![
            ModelDecl::new("test_part", "scale.other")
                .fields(const { &[FieldDecl::datetime("happened_at").server_set(true)] })
                .partition(daily().by("happened_at")),
        ],
        "partition key 'happened_at' of 'scale.other' must be create_date or a stored, required \
         datetime field declared server_set",
    );
    // a field that does not exist
    refused(
        vec![
            ModelDecl::new("test_part", "scale.other")
                .fields(const { &[FieldDecl::char("name")] })
                .partition(daily().by("missing")),
        ],
        "partition key 'missing' of 'scale.other' must be create_date or a stored, required \
         datetime field declared server_set",
    );
    // the default key needs log access
    refused(
        vec![
            ModelDecl::new("test_part", "scale.other")
                .log_access(false)
                .fields(const { &[FieldDecl::char("name")] })
                .partition(daily()),
        ],
        "partitioned model 'scale.other' has no create_date (log_access is off): name a stored, \
         required datetime field declared server_set with by = \"<field>\"",
    );
    let registry = build(
        &[&PART],
        vec![
            ModelDecl::new("test_part", "scale.audit")
                .fields(
                    const {
                        &[
                            FieldDecl::datetime("recorded_at")
                                .required(true)
                                .server_set(true),
                            FieldDecl::char("what"),
                        ]
                    },
                )
                .partition(
                    PartitionDecl::new(PartitionInterval::Month, 2, PartitionRetain::All)
                        .by("recorded_at"),
                ),
        ],
    )
    .unwrap();
    let spec = registry.model("scale.audit").unwrap();
    let partition = spec.partition.unwrap();
    assert_eq!(partition.key_name(), "recorded_at");
    assert_eq!(partition.retain().intervals(), None);
    let recorded_at = spec.field("recorded_at").unwrap();
    assert!(recorded_at.server_set);
    assert!(recorded_at.readonly, "server_set fields are readonly");
    assert!(!recorded_at.copy, "server_set fields are not copied");
    assert!(!spec.field("what").unwrap().server_set);
}

/// `server_set` belongs on stored datetime fields that the ORM can fill: no compute, no related
/// path, no default.
#[test]
fn server_set_is_for_plain_stored_datetime_fields() {
    let message = |field: &str| {
        format!(
            "field 'scale.other.{field}': server_set is for stored datetime fields without a \
             compute, related path or default"
        )
    };
    for (decl, field) in [
        (FieldDecl::date("day").server_set(true), "day"),
        (FieldDecl::char("text").server_set(true), "text"),
        (
            FieldDecl::datetime("computed")
                .compute("_compute_x")
                .store(true)
                .server_set(true),
            "computed",
        ),
        (
            FieldDecl::datetime("unstored")
                .store(false)
                .server_set(true),
            "unstored",
        ),
        (
            FieldDecl::datetime("defaulted")
                .default(erp_registry::StaticValue::Str("2026-01-01 00:00:00"))
                .server_set(true),
            "defaulted",
        ),
    ] {
        let fields: &'static [FieldDecl] = Box::leak(Box::new([decl]));
        refused(
            vec![ModelDecl::new("test_part", "scale.other").fields(fields)],
            &message(field),
        );
    }
    // an unpartitioned model may have server_set fields too
    let registry = build(
        &[&PART],
        vec![
            ModelDecl::new("test_part", "scale.other")
                .fields(const { &[FieldDecl::datetime("seen_at").server_set(true)] }),
        ],
    )
    .unwrap();
    assert!(
        registry
            .model("scale.other")
            .unwrap()
            .field("seen_at")
            .unwrap()
            .server_set
    );
}

/// The window bounds, the model kind, a fill factor and the table name length are checked when the
/// registry is built.
#[test]
fn partition_parameters_are_validated() {
    let with = |partition: PartitionDecl| {
        vec![
            ModelDecl::new("test_part", "scale.other")
                .fields(const { &[FieldDecl::char("name")] })
                .partition(partition),
        ]
    };
    refused(
        with(PartitionDecl::new(
            PartitionInterval::Day,
            0,
            PartitionRetain::Intervals(3),
        )),
        "partition of 'scale.other': premake must be 1 to 1000 intervals (got 0)",
    );
    // upkeep may run up to one hour apart: an hourly window keeps two hours ahead
    refused(
        with(PartitionDecl::new(
            PartitionInterval::Hour,
            1,
            PartitionRetain::All,
        )),
        "partition of 'scale.other': an hourly partition needs premake of at least 2 intervals, \
         because upkeep may run up to one hour apart (got 1)",
    );
    build(
        &[&PART],
        with(PartitionDecl::new(
            PartitionInterval::Hour,
            2,
            PartitionRetain::All,
        )),
    )
    .expect("an hourly partition with premake 2 is accepted");
    refused(
        with(PartitionDecl::new(
            PartitionInterval::Hour,
            1_001,
            PartitionRetain::All,
        )),
        "partition of 'scale.other': premake must be 1 to 1000 intervals (got 1001)",
    );
    refused(
        with(PartitionDecl::new(
            PartitionInterval::Week,
            1,
            PartitionRetain::Intervals(0),
        )),
        "partition of 'scale.other': retain must be 1 to 1000 intervals or \"all\" (got 0)",
    );
    refused(
        with(PartitionDecl::new(
            PartitionInterval::Week,
            1,
            PartitionRetain::Intervals(u32::MAX),
        )),
        "partition of 'scale.other': retain must be 1 to 1000 intervals or \"all\" (got \
         4294967295)",
    );
    refused(
        vec![
            ModelDecl::new("test_part", "scale.other")
                .fillfactor(80)
                .fields(const { &[FieldDecl::char("name")] })
                .partition(daily()),
        ],
        "model 'scale.other' is partitioned: a fill factor cannot be set on a partitioned table",
    );
    refused(
        vec![
            ModelDecl::new("test_part", "scale.other")
                .kind(ModelKind::Abstract)
                .partition(daily()),
        ],
        "model 'scale.other' is abstract and cannot be partitioned: it has no table",
    );
    refused(
        vec![
            ModelDecl::new("test_part", "scale.other")
                .kind(ModelKind::Transient)
                .partition(daily()),
        ],
        "model 'scale.other' is transient and cannot be partitioned: its rows are removed by \
         age, not by retiring partitions",
    );
    let long = "t".repeat(52);
    let long: &'static str = Box::leak(long.into_boxed_str());
    refused(
        vec![
            ModelDecl::new("test_part", "scale.other")
                .table(long)
                .partition(daily()),
        ],
        &format!(
            "table '{long}' of partitioned model 'scale.other' is longer than 51 bytes, too long \
             for its partition names"
        ),
    );
    // 51 bytes is fine
    let ok: &'static str = Box::leak("t".repeat(51).into_boxed_str());
    build(
        &[&PART],
        vec![
            ModelDecl::new("test_part", "scale.other")
                .table(ok)
                .partition(daily()),
        ],
    )
    .unwrap();
}

/// A model inheriting a partitioned model under a new name gets its fields, not its partitioning:
/// the partitioning belongs to a table, like `_table`.
#[test]
fn a_new_model_does_not_inherit_the_partitioning() {
    let registry = build(
        &[&PART],
        vec![
            event(),
            ModelDecl::new("test_part", "scale.event.copy").inherit(EVENT),
        ],
    )
    .unwrap();
    assert!(registry.model("scale.event").unwrap().partition.is_some());
    let copy = registry.model("scale.event.copy").unwrap();
    assert!(copy.partition.is_none());
    assert!(copy.field("name").is_some());
}

/// A custom field referencing a partitioned model is a broken custom field: refused in a strict
/// build, left out and reported under `DropBroken`, so one bad `x_` field never stops the registry.
#[test]
fn a_custom_field_towards_a_partitioned_model_is_dropped_like_any_broken_one() {
    let mut overlay = Overlay::new();
    overlay
        .add_manual_field(
            "scale.note",
            FieldDef::new(
                "x_event_id",
                KindDef::Many2one {
                    comodel: "scale.event".into(),
                    ondelete: None,
                },
            ),
        )
        .unwrap();
    let decls = || -> Vec<&'static ModelDecl> {
        vec![
            Box::leak(Box::new(event())),
            Box::leak(Box::new(
                ModelDecl::new("test_part", "scale.note")
                    .fields(const { &[FieldDecl::char("name")] }),
            )),
        ]
    };
    let message = "model 'scale.event' is partitioned, so no field may reference it; 'scale.note.x_event_id' \
         does";
    let error = RegistryBuilder::new([&PART])
        .with_decls(decls())
        .overlay(&overlay)
        .build()
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Programming);
    assert_eq!(error.message(), message);
    let registry = RegistryBuilder::new([&PART])
        .with_decls(decls())
        .overlay(&overlay)
        .manual_fields(ManualFieldPolicy::DropBroken)
        .build()
        .unwrap();
    let dropped = registry.dropped_fields();
    assert_eq!(dropped.len(), 1);
    assert_eq!(
        (dropped[0].model(), dropped[0].field()),
        ("scale.note", "x_event_id")
    );
    assert_eq!(dropped[0].reason(), &DropReason::Refused);
    assert_eq!(dropped[0].error().message(), message);
    assert!(
        registry
            .model("scale.note")
            .unwrap()
            .field("x_event_id")
            .is_none()
    );
}

fn objects(list: Vec<TableObjectDecl>) -> &'static [TableObjectDecl] {
    Box::leak(list.into_boxed_slice())
}

/// `scale.slot`, monthly on the server-set `recorded_at`, with the given table objects.
fn slot(objects: &'static [TableObjectDecl]) -> ModelDecl {
    ModelDecl::new("test_part", "scale.slot")
        .fields(
            const {
                &[
                    FieldDecl::datetime("recorded_at")
                        .required(true)
                        .server_set(true),
                    FieldDecl::integer("resource"),
                    FieldDecl::datetime("start"),
                    FieldDecl::datetime("stop"),
                    FieldDecl::date_range("validity", RangeBounds::HalfOpen),
                    FieldDecl::datetime_range("window", RangeBounds::Closed),
                ]
            },
        )
        .partition(
            PartitionDecl::new(PartitionInterval::Month, 1, PartitionRetain::All).by("recorded_at"),
        )
        .table_objects(objects)
}

fn exclusion(name: &'static str, decl: ExclusionDecl) -> TableObjectDecl {
    TableObjectDecl {
        name,
        kind: TableObjectKind::Exclusion(decl),
    }
}

fn temporal(name: &'static str, decl: TemporalKeyDecl) -> TableObjectDecl {
    TableObjectDecl {
        name,
        kind: TableObjectKind::TemporalKey(decl),
    }
}

/// include the partition key, and a business key (an upsert arbiter) is refused at registry build
/// with the exact text of the design note's section 10; range fields are allowed on a partitioned
/// model, and keys including the partition key build)
#[test]
fn spec_ormx_026_objects_of_a_partitioned_model_include_the_partition_key() {
    let period = ("start", "stop");
    let bounds = RangeBounds::HalfOpen;
    // an exclusion without the partition key among its equal fields
    refused(
        vec![slot(objects(vec![exclusion(
            "_no_overlap",
            ExclusionDecl::new(period, bounds, "taken").equal(&["resource"]),
        )]))],
        "exclusion '_no_overlap' of partitioned model 'scale.slot' must include the partition key \
         'recorded_at'",
    );
    refused(
        vec![slot(objects(vec![exclusion(
            "_no_overlap",
            ExclusionDecl::new(period, bounds, "taken"),
        )]))],
        "exclusion '_no_overlap' of partitioned model 'scale.slot' must include the partition key \
         'recorded_at'",
    );
    // a WITHOUT OVERLAPS key without it
    refused(
        vec![slot(objects(vec![temporal(
            "_validity_key",
            TemporalKeyDecl::new("validity", "taken").fields(&["resource"]),
        )]))],
        "key '_validity_key' of partitioned model 'scale.slot' must include the partition key \
         'recorded_at'",
    );
    // a business key: refused whatever its fields, the partition key included
    for fields in [&["resource"][..], &["resource", "recorded_at"][..]] {
        let fields: &'static [&'static str] = Box::leak(fields.to_vec().into_boxed_slice());
        refused(
            vec![slot(objects(vec![TableObjectDecl {
                name: "_slot_key",
                kind: TableObjectKind::Key(KeyDecl::new(fields, "Unique.")),
            }]))],
            "business key '_slot_key' of partitioned model 'scale.slot' cannot be an upsert key: \
             a unique constraint of a partitioned table must include the partition key \
             'recorded_at', which the server sets on every insert, so no row could ever match it; \
             rows of a partitioned model are inserted with create, not upserted",
        );
    }
    // the keys and exclusions that include it build, range fields included
    let registry = build(
        &[&PART],
        vec![slot(objects(vec![
            exclusion(
                "_no_overlap",
                ExclusionDecl::new(period, bounds, "taken").equal(&["recorded_at", "resource"]),
            ),
            temporal(
                "_validity_key",
                TemporalKeyDecl::new("validity", "taken").fields(&["recorded_at", "resource"]),
            ),
        ]))],
    )
    .unwrap();
    let spec = registry.model("scale.slot").unwrap();
    assert!(spec.partition.is_some());
    assert!(spec.field("window").is_some());
    // the same objects on an unpartitioned model are none of this rule's business
    let unpartitioned = ModelDecl::new("test_part", "scale.plain_slot")
        .fields(const { &[FieldDecl::integer("resource")] })
        .table_objects(objects(vec![TableObjectDecl {
            name: "_slot_key",
            kind: TableObjectKind::Key(KeyDecl::new(&["resource"], "Unique.")),
        }]));
    build(&[&PART], vec![unpartitioned]).unwrap();
}

fn raw(name: &'static str, kind: TableObjectKind) -> TableObjectDecl {
    TableObjectDecl { name, kind }
}

/// partitioned model must list the partition key in their column list, refused at registry build
/// with the text of design section 10; lists that name it, any letter case, quoted or not, build;
/// plain indexes and checks are none of this rule's business)
#[test]
fn spec_ormx_026_unique_objects_of_a_partitioned_model_list_the_partition_key() {
    let constraint = |name, definition| {
        slot(objects(vec![raw(
            name,
            TableObjectKind::Constraint {
                definition,
                message: "Unique.",
            },
        )]))
    };
    let unique_index = |name, definition| {
        slot(objects(vec![raw(
            name,
            TableObjectKind::UniqueIndex {
                definition,
                message: "Unique.",
            },
        )]))
    };
    for definition in [
        "UNIQUE (resource)",
        "unique(resource) INCLUDE (recorded_at)",
        "PRIMARY KEY (resource)",
    ] {
        refused(
            vec![constraint("_resource_uniq", definition)],
            "unique constraint '_resource_uniq' of partitioned model 'scale.slot' must include \
             the partition key 'recorded_at'",
        );
    }
    for definition in [
        "(resource)",
        "USING btree (resource) WHERE recorded_at IS NOT NULL",
    ] {
        refused(
            vec![unique_index("_resource_idx", definition)],
            "unique index '_resource_idx' of partitioned model 'scale.slot' must include the \
             partition key 'recorded_at'",
        );
    }
    // lists that name the key build, and so do objects this rule does not cover
    let registry = build(
        &[&PART],
        vec![slot(objects(vec![
            raw(
                "_resource_uniq",
                TableObjectKind::Constraint {
                    definition: "UNIQUE (resource, recorded_at)",
                    message: "Unique.",
                },
            ),
            raw(
                "_quoted_uniq",
                TableObjectKind::Constraint {
                    definition: "Unique (\"recorded_at\", resource)",
                    message: "Unique.",
                },
            ),
            raw(
                "_resource_idx",
                TableObjectKind::UniqueIndex {
                    definition: "(RECORDED_AT, resource)",
                    message: "Unique.",
                },
            ),
            raw(
                "_positive",
                TableObjectKind::Constraint {
                    definition: "CHECK (resource > 0)",
                    message: "Positive.",
                },
            ),
            raw(
                "_plain_idx",
                TableObjectKind::Index {
                    definition: "(resource)",
                },
            ),
        ]))],
    )
    .unwrap();
    assert_eq!(registry.model("scale.slot").unwrap().table_objects.len(), 5);
}
