//! Index kinds at registry build (design): the field
//! types `brin` and `gin` accept, and the checks of typed model-level indexes. Pure: no database.
#![allow(clippy::unwrap_used, clippy::panic, reason = "tests")]

use erp_fields::{ErrorKind, IndexKind, RangeBounds};
use erp_registry::{
    AddonDescriptor, FieldDecl, IndexDecl, IndexDef, IndexMethod, MAX_INDEX_COLUMNS,
    MAX_INDEXES_PER_MODEL, ModelDecl, ModelDef, Registry, RegistryBuilder, TableObjectDecl,
    TableObjectKind,
};

static SCALE: AddonDescriptor = AddonDescriptor {
    name: "scale",
    depends: &["base"],
    exact_only: false,
};
static SCALE_EXT: AddonDescriptor = AddonDescriptor {
    name: "scale_ext",
    depends: &["scale"],
    exact_only: false,
};

fn build(decls: Vec<ModelDecl>) -> erp_fields::Result<Registry> {
    let decls: Vec<&'static ModelDecl> = decls
        .into_iter()
        .map(|d| &*Box::leak(Box::new(d)))
        .collect();
    RegistryBuilder::new([&SCALE, &SCALE_EXT])
        .with_decls(decls)
        .build()
}

fn leak<T>(items: Vec<T>) -> &'static [T] {
    Vec::leak(items)
}

/// `scale.x` with the given fields.
fn model(fields: Vec<FieldDecl>) -> ModelDecl {
    ModelDecl::new("scale", "scale.x").fields(leak(fields))
}

/// The registry error of `decls`, as `(kind, message)`.
fn refusal(decls: Vec<ModelDecl>) -> (ErrorKind, String) {
    let err = build(decls).unwrap_err();
    (err.kind(), err.message().to_owned())
}

#[test]
fn spec_ormx_023_wrong_field_kinds_are_registry_errors() {
    assert_eq!(
        refusal(vec![model(vec![
            FieldDecl::char("code").index(Some(IndexKind::Brin))
        ])]),
        (
            ErrorKind::Programming,
            "field 'scale.x.code': index = \"brin\" is for date, datetime, integer and bigint fields \
             (this field is char)"
                .to_owned()
        )
    );
    assert_eq!(
        refusal(vec![model(vec![
            FieldDecl::date("day").index(Some(IndexKind::Gin))
        ])]),
        (
            ErrorKind::Programming,
            "field 'scale.x.day': index = \"gin\" is for json fields (this field is date)"
                .to_owned()
        )
    );
    // an extension that sets a wrong kind on a field it does not declare is refused the same way
    assert_eq!(
        refusal(vec![
            model(vec![FieldDecl::float("qty")]),
            ModelDecl::extension("scale_ext", "scale.x", &["scale.x"]).fields(leak(vec![
                FieldDecl::float("qty").index(Some(IndexKind::Brin))
            ])),
        ]),
        (
            ErrorKind::Programming,
            "field 'scale.x.qty': index = \"brin\" is for date, datetime, integer and bigint fields \
             (this field is float)"
                .to_owned()
        )
    );
}

#[test]
fn brin_and_gin_on_their_types() {
    let registry = build(vec![
        model(vec![
            FieldDecl::date("day").index(Some(IndexKind::Brin)),
            FieldDecl::datetime("at").index(Some(IndexKind::Brin)),
            FieldDecl::integer("seq").index(Some(IndexKind::Brin)),
            FieldDecl::bigint("wide_seq").index(Some(IndexKind::Brin)),
            FieldDecl::json("payload").index(Some(IndexKind::Gin)),
            FieldDecl::char("label")
                .store(false)
                .compute("_compute_label")
                .index(Some(IndexKind::Btree)),
        ])
        .table_objects(leak(vec![TableObjectDecl::index(
            "_wide_brin",
            IndexDecl::new(&["wide_seq"]).method(IndexMethod::Brin),
        )])),
    ])
    .unwrap();
    let x = registry.model("scale.x").unwrap();
    assert!(x.table_objects.iter().any(|object| matches!(
        &object.kind,
        TableObjectKind::TypedIndex(index)
            if index.method == IndexMethod::Brin && index.fields == ["wide_seq"].as_slice()
    )));
    let kinds: Vec<_> = ["day", "at", "seq", "wide_seq", "payload", "label"]
        .iter()
        .map(|f| x.field(f).unwrap().index)
        .collect();
    assert_eq!(
        kinds,
        [
            Some(IndexKind::Brin),
            Some(IndexKind::Brin),
            Some(IndexKind::Brin),
            Some(IndexKind::Brin),
            Some(IndexKind::Gin),
            None
        ]
    );
}

fn booking_fields() -> Vec<FieldDecl> {
    vec![
        FieldDecl::many2one("resource_id", "scale.x"),
        FieldDecl::datetime("start"),
        FieldDecl::datetime("stop"),
        FieldDecl::char("state"),
        FieldDecl::json("payload"),
        FieldDecl::char("label")
            .store(false)
            .compute("_compute_label"),
    ]
}

/// `scale.x` with the booking fields and one typed index `_idx`.
fn with_index(index: IndexDecl) -> Vec<ModelDecl> {
    vec![model(booking_fields()).table_objects(leak(vec![TableObjectDecl::index("_idx", index)]))]
}

/// field among them)
#[test]
fn spec_ormx_023_typed_index_refusals() {
    let cases: Vec<(IndexDecl, String)> = vec![
        (
            IndexDecl::new(&["state", "start"]).include(&["start"]),
            "index '_idx' of 'scale.x': include field 'start' is already a key field".to_owned(),
        ),
        (
            IndexDecl::new(&["state", "state"]),
            "index '_idx' of 'scale.x': field 'state' is listed twice".to_owned(),
        ),
        (
            IndexDecl::new(&["state"]).include(&["stop", "stop"]),
            "index '_idx' of 'scale.x': field 'stop' is listed twice".to_owned(),
        ),
        (
            IndexDecl::new(&["start"])
                .method(IndexMethod::Brin)
                .include(&["state"]),
            "index '_idx' of 'scale.x': include is only allowed with method \"btree\" or \"gist\" \
             (this index uses \"brin\")"
                .to_owned(),
        ),
        (
            IndexDecl::new(&["state"]).method(IndexMethod::Brin),
            "index '_idx' of 'scale.x': method = \"brin\" is for date, datetime, integer and bigint \
             fields ('state' is char)"
                .to_owned(),
        ),
        (
            IndexDecl::new(&["start"]).method(IndexMethod::Gin),
            "index '_idx' of 'scale.x': method = \"gin\" is for json fields ('start' is datetime)"
                .to_owned(),
        ),
        (
            IndexDecl::new(&["payload"]).method(IndexMethod::Gist),
            "index '_idx' of 'scale.x': method = \"gist\" is for boolean, integer, float, \
             monetary, char, text, html, selection, date, datetime, many2one, \
             many2one_reference, reference, date_range and datetime_range fields ('payload' is json)"
                .to_owned(),
        ),
        (
            IndexDecl::new(&["start"]).method(IndexMethod::Trigram),
            "index '_idx' of 'scale.x': method = \"trigram\" is for char, text and html fields \
             ('start' is datetime)"
                .to_owned(),
        ),
        (
            IndexDecl::new(&["nope"]),
            "index '_idx' of 'scale.x': unknown field 'nope'".to_owned(),
        ),
        (
            IndexDecl::new(&["state"]).include(&["nope"]),
            "index '_idx' of 'scale.x': unknown field 'nope'".to_owned(),
        ),
        (
            IndexDecl::new(&["label"]),
            "index '_idx' of 'scale.x': field 'label' is not stored in the model's table"
                .to_owned(),
        ),
        (
            IndexDecl::new(&[]),
            "index '_idx' of 'scale.x' needs at least one field".to_owned(),
        ),
    ];
    for (index, message) in cases {
        assert_eq!(
            refusal(with_index(index)),
            (ErrorKind::Programming, message),
            "{index:?}"
        );
    }
}

#[test]
fn typed_index_column_limit() {
    // 33 integer fields, all in one index
    let names: Vec<&'static str> = (0..=MAX_INDEX_COLUMNS)
        .map(|i| &*Box::leak(format!("f{i}").into_boxed_str()))
        .collect();
    let fields: Vec<FieldDecl> = names.iter().copied().map(FieldDecl::integer).collect();
    let at_limit = IndexDecl::new(leak(names[..MAX_INDEX_COLUMNS].to_vec()));
    let over = IndexDecl::new(leak(names[1..].to_vec())).include(leak(vec![names[0]]));
    let ok = vec![
        model(fields.clone()).table_objects(leak(vec![TableObjectDecl::index("_idx", at_limit)])),
    ];
    assert!(build(ok).is_ok());
    let too_many =
        vec![model(fields).table_objects(leak(vec![TableObjectDecl::index("_idx", over)]))];
    assert_eq!(
        refusal(too_many),
        (
            ErrorKind::Programming,
            format!(
                "index '_idx' of 'scale.x' has {} columns; PostgreSQL allows at most \
                 {MAX_INDEX_COLUMNS}",
                MAX_INDEX_COLUMNS + 1
            )
        )
    );
}

/// `id` is a column even though its field has none of its own: a typed index may use it, as a key
/// or as a covering field (a queue's `(priority, run_at) INCLUDE (id)`, pg-features §3).
#[test]
fn typed_indexes_may_use_id() {
    let registry = build(vec![model(booking_fields()).table_objects(leak(vec![
        TableObjectDecl::index(
            "_cover",
            IndexDecl::new(&["state", "start"]).include(&["id"]),
        ),
        TableObjectDecl::index("_by_id", IndexDecl::new(&["id"]).method(IndexMethod::Brin)),
    ]))])
    .unwrap();
    assert_eq!(registry.model("scale.x").unwrap().table_objects.len(), 2);
}

/// An extension adds a typed index to a model it does not own and replaces another by name; the
/// composed object has the newest declaration's kind and name.
#[test]
fn typed_indexes_compose_by_name() {
    let registry = build(vec![
        model(booking_fields()).table_objects(leak(vec![TableObjectDecl::index(
            "_open",
            IndexDecl::new(&["state"]),
        )])),
        ModelDecl::extension("scale_ext", "scale.x", &["scale.x"]).table_objects(leak(vec![
            TableObjectDecl::index(
                "_open",
                IndexDecl::new(&["state", "start"]).include(&["resource_id"]),
            ),
            TableObjectDecl::index(
                "_by_stop",
                IndexDecl::new(&["stop"]).method(IndexMethod::Brin),
            ),
        ])),
    ])
    .unwrap();
    let x = registry.model("scale.x").unwrap();
    let objects: Vec<_> = x
        .table_objects
        .iter()
        .map(|o| (o.full_name.as_str(), o.name, o.kind))
        .map(|(full, name, kind)| match kind {
            TableObjectKind::TypedIndex(index) => (full, name, index),
            other => panic!("{other:?}"),
        })
        .collect();
    assert_eq!(
        objects,
        [
            (
                "scale_x_open",
                "_open",
                IndexDecl::new(&["state", "start"]).include(&["resource_id"])
            ),
            (
                "scale_x_by_stop",
                "_by_stop",
                IndexDecl::new(&["stop"]).method(IndexMethod::Brin)
            ),
        ]
    );
}

/// Runtime definitions (`models/*.toml`) intern their typed indexes like their fields: equal
/// definitions share one declaration, and the limits are checked first.
#[test]
fn runtime_index_definitions_are_interned_and_bounded() {
    let def = ModelDef::extension("scale", "scale.x").index(
        IndexDef::new("_by_day", ["day"])
            .method(IndexMethod::Brin)
            .include(Vec::<String>::new()),
    );
    let a = def.to_decl().unwrap();
    let b = def.clone().to_decl().unwrap();
    assert!(std::ptr::eq(a, b), "equal definitions intern once");
    let objects: Vec<_> = a.table_objects.iter().map(|o| (o.name, o.kind)).collect();
    let [(name, TableObjectKind::TypedIndex(index))] = objects.as_slice() else {
        panic!("unexpected table objects {objects:?}")
    };
    assert_eq!(
        (*name, *index),
        (
            "_by_day",
            IndexDecl::new(&["day"]).method(IndexMethod::Brin)
        )
    );
    // the filter is interned with the index
    let filtered = ModelDef::extension("scale", "scale.z")
        .index(IndexDef::new("_open", ["day"]).filter("[('state', '=', 'open')]"))
        .to_decl()
        .unwrap();
    let TableObjectKind::TypedIndex(index) = filtered.table_objects[0].kind else {
        panic!("not a typed index")
    };
    assert_eq!(index.filter, Some("[('state', '=', 'open')]"));
    // a definition without indexes keeps the empty slice
    let plain = ModelDef::extension("scale", "scale.y").to_decl().unwrap();
    assert!(plain.table_objects.is_empty());

    let refusal = |def: ModelDef| def.check_limits().unwrap_err().message().to_owned();
    let many = (0..=MAX_INDEXES_PER_MODEL).fold(ModelDef::extension("scale", "scale.x"), |d, i| {
        d.index(IndexDef::new(format!("_i{i}"), ["day"]))
    });
    assert_eq!(
        refusal(many),
        format!(
            "model 'scale.x': {} indexes, more than the limit of {MAX_INDEXES_PER_MODEL}",
            MAX_INDEXES_PER_MODEL + 1
        )
    );
    assert_eq!(
        refusal(ModelDef::extension("scale", "scale.x").index(IndexDef::new("by_day", ["day"]))),
        "model 'scale.x': index 'by_day': names of SQL objects in a model start with '_'"
    );
    assert_eq!(
        refusal(
            ModelDef::extension("scale", "scale.x")
                .index(IndexDef::new("_a", ["day"]))
                .index(IndexDef::new("_a", ["seq"]))
        ),
        "model 'scale.x': index '_a' is declared twice"
    );
    let long = "f".repeat(erp_registry::MAX_NAME_BYTES + 1);
    assert_eq!(
        refusal(ModelDef::extension("scale", "scale.x").index(IndexDef::new("_a", [long]))),
        format!(
            "model 'scale.x': index '_a': a field name is {} bytes long, more than the limit of {}",
            erp_registry::MAX_NAME_BYTES + 1,
            erp_registry::MAX_NAME_BYTES
        )
    );
}

/// A non-ASCII index name is kept as declared in messages and shortened at a character boundary
/// in the database name (the rule of every table object).
#[test]
fn non_ascii_index_names_are_shortened_on_a_character_boundary() {
    let name: &'static str = Box::leak(format!("_{}", "índice_über_".repeat(5)).into_boxed_str());
    let registry = build(vec![model(booking_fields()).table_objects(leak(vec![
        TableObjectDecl::index(name, IndexDecl::new(&["state"])),
    ]))])
    .unwrap();
    let object = &registry.model("scale.x").unwrap().table_objects[0];
    assert_eq!(object.name, name);
    let full = format!("scale_x_{}", &name[1..]);
    assert!(full.len() > 63, "{full}");
    assert_eq!(
        object.full_name,
        erp_registry::ident::shorten_identifier(&full)
    );
    assert!(object.full_name.len() <= 63);
    // the error messages name the declared index
    let err = build(vec![model(booking_fields()).table_objects(leak(vec![
        TableObjectDecl::index(name, IndexDecl::new(&["nope"])),
    ]))])
    .unwrap_err();
    assert_eq!(
        err.message(),
        format!("index '{name}' of 'scale.x': unknown field 'nope'")
    );
}

/// Generated index names stay within PostgreSQL's 63 bytes and stable: a long table and field name
/// (field-level index) or typed index name is shortened with a hash of the whole name, the same
/// input always gives the same name, and two names that differ only past the cut stay different.
#[test]
fn long_index_names_are_at_most_63_bytes_and_stable() {
    let table = "t".repeat(40);
    let a = erp_registry::ident::index_name(&table, &"f".repeat(40));
    let b = erp_registry::ident::index_name(&table, &format!("{}x", "f".repeat(39)));
    assert!(a.len() <= 63 && b.len() <= 63, "{a} / {b}");
    assert_ne!(a, b);
    assert_eq!(a, erp_registry::ident::index_name(&table, &"f".repeat(40)));
    // typed indexes: a long model table plus a long declared name
    let long: &'static str = Box::leak(format!("_{}", "n".repeat(70)).into_boxed_str());
    let other: &'static str = Box::leak(format!("_{}m", "n".repeat(69)).into_boxed_str());
    let registry = build(vec![model(booking_fields()).table_objects(leak(vec![
        TableObjectDecl::index(long, IndexDecl::new(&["state"])),
        TableObjectDecl::index(other, IndexDecl::new(&["stop"])),
    ]))])
    .unwrap();
    let objects = &registry.model("scale.x").unwrap().table_objects;
    assert!(objects.iter().all(|o| o.full_name.len() <= 63));
    assert_ne!(objects[0].full_name, objects[1].full_name);
    let again = build(vec![model(booking_fields()).table_objects(leak(vec![
        TableObjectDecl::index(long, IndexDecl::new(&["state"])),
    ]))])
    .unwrap();
    assert_eq!(
        again.model("scale.x").unwrap().table_objects[0].full_name,
        objects[0].full_name
    );
}

/// The filter text of a typed index is bounded and free of NUL bytes (it is compiled by the ORM,
/// which checks the domain itself).
#[test]
fn typed_index_filter_text_is_checked() {
    let (kind, message) = refusal(with_index(
        IndexDecl::new(&["state"]).filter("[('state', '=', 'a\0b')]"),
    ));
    assert_eq!(kind, ErrorKind::Programming);
    assert_eq!(
        message,
        "model 'scale.x' declares a text with a NUL byte: \"[('state', '=', 'a\\0b')]\""
    );
    let def = ModelDef::extension("m", "scale.x").index(
        IndexDef::new("_idx", ["state"]).filter("x".repeat(erp_registry::MAX_TEXT_BYTES + 1)),
    );
    assert_eq!(
        def.check_limits().unwrap_err().message(),
        format!(
            "model 'scale.x': exclusion, key or index text is {} bytes long, more than the limit \
             of {}",
            erp_registry::MAX_TEXT_BYTES + 1,
            erp_registry::MAX_TEXT_BYTES
        )
    );
}

/// The typed-index method rules for `brin` and `gin` are the field-level rules (one source).
#[test]
fn method_and_field_kind_rules_agree() {
    assert_eq!(
        IndexMethod::Brin.field_types(),
        IndexKind::Brin.field_types()
    );
    assert_eq!(IndexMethod::Gin.field_types(), IndexKind::Gin.field_types());
}

/// A table object name repeated inside one declaration is refused: the second would silently
/// replace the first. (Replacing an earlier addon's object by name is `typed_indexes_compose_by_name`.)
#[test]
fn a_repeated_index_name_in_one_declaration_is_refused() {
    let (kind, message) = refusal(vec![model(booking_fields()).table_objects(leak(vec![
        TableObjectDecl::index("_idx", IndexDecl::new(&["state"])),
        TableObjectDecl::index("_idx", IndexDecl::new(&["start"])),
    ]))]);
    assert_eq!(kind, ErrorKind::Programming);
    assert_eq!(
        message,
        "SQL object '_idx' of model 'scale.x' is declared twice by addon 'scale' (:0) \
         (database name 'scale_x_idx')"
    );
}

/// A `company_dependent` field is a `jsonb` column whatever its type: the index rules see `json`,
/// so `brin` on a company-dependent integer and a `gist`/`trigram` index over a company-dependent
/// char are refused at registry build, not by PostgreSQL at `CREATE INDEX`.
#[test]
fn company_dependent_fields_are_jsonb_for_index_rules() {
    assert_eq!(
        refusal(vec![model(vec![
            FieldDecl::integer("rank")
                .company_dependent(true)
                .index(Some(IndexKind::Brin))
        ])]),
        (
            ErrorKind::Programming,
            "field 'scale.x.rank': index = \"brin\" is for date, datetime, integer and bigint fields \
             (this field is integer, company_dependent: a jsonb column)"
                .to_owned()
        )
    );
    // gin suits the jsonb column
    build(vec![model(vec![
        FieldDecl::integer("rank")
            .company_dependent(true)
            .index(Some(IndexKind::Gin)),
    ])])
    .unwrap();
    let fields = vec![FieldDecl::char("label").company_dependent(true)];
    let (kind, message) = refusal(vec![model(fields).table_objects(leak(vec![
        TableObjectDecl::index(
            "_idx",
            IndexDecl::new(&["label"]).method(IndexMethod::Trigram),
        ),
    ]))]);
    assert_eq!(kind, ErrorKind::Programming);
    assert_eq!(
        message,
        "index '_idx' of 'scale.x': method = \"trigram\" is for char, text and html fields \
         ('label' is char, company_dependent: a jsonb column)"
    );
}

/// The cap on typed indexes holds for the composed model, not only per declaration: two addons
/// that each stay under it cannot together exceed it.
#[test]
fn the_typed_index_cap_is_per_composed_model() {
    let make = |prefix: &str| -> &'static [TableObjectDecl] {
        leak(
            (0..MAX_INDEXES_PER_MODEL / 2 + 1)
                .map(|i| {
                    let name: &'static str = Box::leak(format!("_{prefix}{i}").into_boxed_str());
                    TableObjectDecl::index(name, IndexDecl::new(&["state"]))
                })
                .collect(),
        )
    };
    let (kind, message) = refusal(vec![
        model(booking_fields()).table_objects(make("a")),
        ModelDecl::extension("scale_ext", "scale.x", &["scale.x"]).table_objects(make("b")),
    ]);
    assert_eq!(kind, ErrorKind::Programming);
    assert_eq!(
        message,
        format!(
            "model 'scale.x': {} typed indexes after composing every addon, more than the limit \
             of {MAX_INDEXES_PER_MODEL}",
            2 * (MAX_INDEXES_PER_MODEL / 2 + 1)
        )
    );
}

#[test]
fn brin_and_gin_need_a_stored_field_and_gist_takes_ranges() {
    let (kind, message) = refusal(vec![model(vec![
        FieldDecl::integer("n")
            .store(false)
            .compute("_compute_n")
            .index(Some(IndexKind::Brin)),
    ])]);
    assert_eq!(kind, ErrorKind::Programming);
    assert_eq!(
        message,
        "field 'scale.x.n': index = \"brin\" needs a stored field (this field has no column)"
    );
    let ranges = vec![FieldDecl::date_range("span", RangeBounds::HalfOpen)];
    build(vec![model(ranges).table_objects(leak(vec![
        TableObjectDecl::index("_span", IndexDecl::new(&["span"]).method(IndexMethod::Gist)),
    ]))])
    .unwrap();
}

/// Names in registry errors are clipped and escaped (model files are untrusted text), and a typed
/// index may not take the generated name of a field index.
#[test]
fn index_errors_escape_names_and_refuse_generated_name_clashes() {
    let (kind, message) = refusal(with_index(IndexDecl::new(&["no\npe"])));
    assert_eq!(kind, ErrorKind::Programming);
    assert_eq!(
        message,
        "index '_idx' of 'scale.x': unknown field 'no\\npe'"
    );
    let long = "x".repeat(100);
    let long: &'static str = Box::leak(long.into_boxed_str());
    let (_, message) = refusal(with_index(IndexDecl::new(leak(vec![long]))));
    assert_eq!(
        message,
        format!(
            "index '_idx' of 'scale.x': unknown field '{}…'",
            "x".repeat(64)
        )
    );
    // `scale_x__start_index` is the generated name of the index of `start`
    let fields = vec![FieldDecl::datetime("start").index(Some(IndexKind::Btree))];
    let (kind, message) = refusal(vec![model(fields).table_objects(leak(vec![
        TableObjectDecl::index("__start_index", IndexDecl::new(&["start"])),
    ]))]);
    assert_eq!(kind, ErrorKind::Programming);
    assert_eq!(
        message,
        "index '__start_index' of 'scale.x': its database name scale_x__start_index is the name \
         of the index of field 'start'; rename the index"
    );
}

/// Index names are schema-wide: table `stock` with `_quant_open` and table `stock_quant` with
/// `_open` both produce `stock_quant_open`; the registry refuses the pair, naming both models.
#[test]
fn two_models_may_not_produce_one_index_name() {
    let one = |name: &'static str, object: &'static str| {
        ModelDecl::new("scale", name)
            .fields(leak(vec![FieldDecl::char("state")]))
            .table_objects(leak(vec![TableObjectDecl::index(
                object,
                IndexDecl::new(&["state"]),
            )]))
    };
    let (kind, message) = refusal(vec![
        one("stock", "_quant_open"),
        one("stock.quant", "_open"),
    ]);
    assert_eq!(kind, ErrorKind::Programming);
    assert_eq!(
        message,
        "table object name 'stock_quant_open' is produced by both model 'stock' ('_quant_open' \
         on table 'stock') and model 'stock.quant' ('_open' on table 'stock_quant'); database \
         names of indexes and constraints are schema-wide, rename one"
    );
}

#[test]
fn spec_ormx_034_runtime_extension_cannot_change_native_field_index_kind() {
    let native = ModelDecl::new("scale", "scale.x")
        .fields(leak(vec![
            FieldDecl::date("day").index(Some(IndexKind::Btree)),
        ]))
        .at("models/scale.rs", 10);
    let mut runtime = ModelDecl::extension("scale_ext", "scale.x", &["scale.x"])
        .fields(leak(vec![
            FieldDecl::date("day").index(Some(IndexKind::Brin)),
        ]))
        .at("models/ext.toml", 5);
    runtime.runtime = true;

    assert_eq!(
        refusal(vec![native, runtime]),
        (
            ErrorKind::Programming,
            "field 'scale.x.day' has index kind 'btree' declared by native addon 'scale' \
             (models/scale.rs:10) and index kind 'brin' declared at runtime by addon 'scale_ext' \
             (models/ext.toml:5); a model file or overlay cannot change the index kind of a native field"
                .to_owned()
        )
    );

    // explicit index=false (index(None)) is also refused when native has an index
    let native = ModelDecl::new("scale", "scale.x")
        .fields(leak(vec![
            FieldDecl::date("day").index(Some(IndexKind::Btree)),
        ]))
        .at("models/scale.rs", 10);
    let mut runtime_false = ModelDecl::extension("scale_ext", "scale.x", &["scale.x"])
        .fields(leak(vec![FieldDecl::date("day").index(None)]))
        .at("models/ext.toml", 8);
    runtime_false.runtime = true;

    assert_eq!(
        refusal(vec![native, runtime_false]),
        (
            ErrorKind::Programming,
            "field 'scale.x.day' has index kind 'btree' declared by native addon 'scale' \
             (models/scale.rs:10) and index kind 'none' declared at runtime by addon 'scale_ext' \
             (models/ext.toml:8); a model file or overlay cannot change the index kind of a native field"
                .to_owned()
        )
    );

    // native many2one has default btree: changing to brin at runtime is refused too
    let native_m2o = ModelDecl::new("scale", "scale.x")
        .fields(leak(vec![FieldDecl::many2one("partner_id", "scale.x")]))
        .at("models/scale.rs", 15);
    let mut runtime_m2o = ModelDecl::extension("scale_ext", "scale.x", &["scale.x"])
        .fields(leak(vec![
            FieldDecl::many2one("partner_id", "scale.x").index(Some(IndexKind::Brin)),
        ]))
        .at("models/ext.toml", 12);
    runtime_m2o.runtime = true;

    assert_eq!(
        refusal(vec![native_m2o, runtime_m2o]),
        (
            ErrorKind::Programming,
            "field 'scale.x.partner_id' has index kind 'btree' declared by native addon 'scale' \
             (models/scale.rs:15) and index kind 'brin' declared at runtime by addon 'scale_ext' \
             (models/ext.toml:12); a model file or overlay cannot change the index kind of a native field"
                .to_owned()
        )
    );

    // same kind declared at runtime is accepted
    let native = ModelDecl::new("scale", "scale.x")
        .fields(leak(vec![
            FieldDecl::date("day").index(Some(IndexKind::Btree)),
        ]))
        .at("models/scale.rs", 10);
    let mut runtime_same = ModelDecl::extension("scale_ext", "scale.x", &["scale.x"])
        .fields(leak(vec![
            FieldDecl::date("day").index(Some(IndexKind::Btree)),
        ]))
        .at("models/ext.toml", 5);
    runtime_same.runtime = true;
    assert!(build(vec![native, runtime_same]).is_ok());

    // unindexed native field gaining an index at runtime is accepted
    let native = ModelDecl::new("scale", "scale.x")
        .fields(leak(vec![FieldDecl::date("day")]))
        .at("models/scale.rs", 10);
    let mut runtime_add = ModelDecl::extension("scale_ext", "scale.x", &["scale.x"])
        .fields(leak(vec![
            FieldDecl::date("day").index(Some(IndexKind::Brin)),
        ]))
        .at("models/ext.toml", 5);
    runtime_add.runtime = true;
    assert!(build(vec![native, runtime_add]).is_ok());

    // M1: two-step bypass: native btree -> runtime btree -> runtime brin is refused
    let native = ModelDecl::new("scale", "scale.x")
        .fields(leak(vec![
            FieldDecl::date("day").index(Some(IndexKind::Btree)),
        ]))
        .at("models/scale.rs", 10);
    let mut runtime_step1 = ModelDecl::extension("scale_ext", "scale.x", &["scale.x"])
        .fields(leak(vec![
            FieldDecl::date("day").index(Some(IndexKind::Btree)),
        ]))
        .at("models/ext_a.toml", 5);
    runtime_step1.runtime = true;
    let mut runtime_step2 = ModelDecl::extension("scale_ext", "scale.x", &["scale.x"])
        .fields(leak(vec![
            FieldDecl::date("day").index(Some(IndexKind::Brin)),
        ]))
        .at("models/ext_b.toml", 12);
    runtime_step2.runtime = true;
    assert_eq!(
        refusal(vec![native, runtime_step1, runtime_step2]),
        (
            ErrorKind::Programming,
            "field 'scale.x.day' has index kind 'btree' declared by native addon 'scale' \
             (models/scale.rs:10) and index kind 'brin' declared at runtime by addon 'scale_ext' \
             (models/ext_b.toml:12); a model file or overlay cannot change the index kind of a native field"
                .to_owned()
        )
    );

    // M2: runtime type change on a natively indexed field is refused
    let native = ModelDecl::new("scale", "scale.x")
        .fields(leak(vec![
            FieldDecl::date("day").index(Some(IndexKind::Btree)),
        ]))
        .at("models/scale.rs", 10);
    let mut runtime_type_change = ModelDecl::extension("scale_ext", "scale.x", &["scale.x"])
        .fields(leak(vec![FieldDecl::char("day")]))
        .at("models/ext.toml", 7);
    runtime_type_change.runtime = true;
    let (kind, message) = refusal(vec![native, runtime_type_change]);
    assert_eq!(kind, ErrorKind::Programming);
    assert_eq!(
        message,
        "field 'scale.x.day' has index kind 'btree' declared by native addon 'scale' \
         (models/scale.rs:10); a runtime extension cannot change the type of a natively indexed \
         field"
    );

    // S1: non-stored native many2one gaining index=false at runtime is accepted
    let native_unstored_m2o = ModelDecl::new("scale", "scale.x")
        .fields(leak(vec![
            FieldDecl::many2one("label_id", "scale.x")
                .store(false)
                .compute("_compute_label"),
        ]))
        .at("models/scale.rs", 20);
    let mut runtime_m2o_false = ModelDecl::extension("scale_ext", "scale.x", &["scale.x"])
        .fields(leak(vec![
            FieldDecl::many2one("label_id", "scale.x").index(None),
        ]))
        .at("models/ext.toml", 15);
    runtime_m2o_false.runtime = true;
    assert!(build(vec![native_unstored_m2o, runtime_m2o_false]).is_ok());

    // M1(a): runtime extension making a natively indexed field non-stored is refused
    let native = ModelDecl::new("scale", "scale.x")
        .fields(leak(vec![
            FieldDecl::date("day").index(Some(IndexKind::Btree)),
        ]))
        .at("models/scale.rs", 10);
    let mut runtime_non_stored = ModelDecl::extension("scale_ext", "scale.x", &["scale.x"])
        .fields(leak(vec![FieldDecl::date("day").store(false)]))
        .at("models/ext.toml", 8);
    runtime_non_stored.runtime = true;
    let (kind, message) = refusal(vec![native, runtime_non_stored]);
    assert_eq!(kind, ErrorKind::Programming);
    assert_eq!(
        message,
        "field 'scale.x.day' has index kind 'btree' declared by native addon 'scale' \
         (models/scale.rs:10); a runtime extension declared by addon 'scale_ext' (models/ext.toml:8) \
         cannot make a natively indexed field non-stored via store, compute, or related"
    );

    // M1(a): runtime extension setting compute without store=true on natively indexed field is refused
    let native = ModelDecl::new("scale", "scale.x")
        .fields(leak(vec![
            FieldDecl::date("day").index(Some(IndexKind::Btree)),
        ]))
        .at("models/scale.rs", 10);
    let mut runtime_compute = ModelDecl::extension("scale_ext", "scale.x", &["scale.x"])
        .fields(leak(vec![FieldDecl::date("day").compute("_compute_day")]))
        .at("models/ext.toml", 12);
    runtime_compute.runtime = true;
    let (kind, message) = refusal(vec![native, runtime_compute]);
    assert_eq!(kind, ErrorKind::Programming);
    assert_eq!(
        message,
        "field 'scale.x.day' has index kind 'btree' declared by native addon 'scale' \
         (models/scale.rs:10); a runtime extension declared by addon 'scale_ext' (models/ext.toml:12) \
         cannot make a natively indexed field non-stored via store, compute, or related"
    );

    // M1(a): runtime extension making a natively indexed field `related` (not stored) is refused
    let native = ModelDecl::new("scale", "scale.x")
        .fields(leak(vec![
            FieldDecl::date("day").index(Some(IndexKind::Btree)),
        ]))
        .at("models/scale.rs", 10);
    let mut runtime_related = ModelDecl::extension("scale_ext", "scale.x", &["scale.x"])
        .fields(leak(vec![FieldDecl::date("day").related("other_id.day")]))
        .at("models/ext.toml", 14);
    runtime_related.runtime = true;
    let (kind, message) = refusal(vec![native, runtime_related]);
    assert_eq!(kind, ErrorKind::Programming);
    assert_eq!(
        message,
        "field 'scale.x.day' has index kind 'btree' declared by native addon 'scale' \
         (models/scale.rs:10); a runtime extension declared by addon 'scale_ext' (models/ext.toml:14) \
         cannot make a natively indexed field non-stored via store, compute, or related"
    );

    // M1(b): native type change to Many2one resets and preserves default index against runtime change
    let native_a = ModelDecl::new("scale", "scale.x")
        .fields(leak(vec![FieldDecl::char("ref_id")]))
        .at("models/scale_a.rs", 10);
    let native_b = ModelDecl::extension("scale", "scale.x", &["scale.x"])
        .fields(leak(vec![FieldDecl::many2one("ref_id", "scale.x")]))
        .at("models/scale_b.rs", 15);
    let mut runtime_m2o_brin = ModelDecl::extension("scale_ext", "scale.x", &["scale.x"])
        .fields(leak(vec![
            FieldDecl::many2one("ref_id", "scale.x").index(Some(IndexKind::Brin)),
        ]))
        .at("models/ext.toml", 20);
    runtime_m2o_brin.runtime = true;
    let (kind, message) = refusal(vec![native_a, native_b, runtime_m2o_brin]);
    assert_eq!(kind, ErrorKind::Programming);
    assert_eq!(
        message,
        "field 'scale.x.ref_id' has index kind 'btree' declared by native addon 'scale' \
         (models/scale_b.rs:15) and index kind 'brin' declared at runtime by addon 'scale_ext' \
         (models/ext.toml:20); a model file or overlay cannot change the index kind of a native field"
    );

    // S1: explicit native index=false (kind 'none') cannot gain runtime index
    let native_explicit_false = ModelDecl::new("scale", "scale.x")
        .fields(leak(vec![FieldDecl::char("code").index(None)]))
        .at("models/scale.rs", 30);
    let mut runtime_add_to_false = ModelDecl::extension("scale_ext", "scale.x", &["scale.x"])
        .fields(leak(vec![
            FieldDecl::char("code").index(Some(IndexKind::Btree)),
        ]))
        .at("models/ext.toml", 25);
    runtime_add_to_false.runtime = true;
    let (kind, message) = refusal(vec![native_explicit_false, runtime_add_to_false]);
    assert_eq!(kind, ErrorKind::Programming);
    assert_eq!(
        message,
        "field 'scale.x.code' has index kind 'none' declared by native addon 'scale' \
         (models/scale.rs:30) and index kind 'btree' declared at runtime by addon 'scale_ext' \
         (models/ext.toml:25); a model file or overlay cannot change the index kind of a native field"
    );

    // native extension changing index kind is accepted (normal override order)
    let native = ModelDecl::new("scale", "scale.x")
        .fields(leak(vec![
            FieldDecl::date("day").index(Some(IndexKind::Btree)),
        ]))
        .at("models/scale.rs", 10);
    let native_ext = ModelDecl::extension("scale_ext", "scale.x", &["scale.x"])
        .fields(leak(vec![
            FieldDecl::date("day").index(Some(IndexKind::Brin)),
        ]))
        .at("models/scale_ext.rs", 5);
    assert!(build(vec![native, native_ext]).is_ok());
}

/// SEC-D2-2: check_object_names compares table object names against generated names of other tables
/// (_pkey, _fkey, _not_null, __index).
#[test]
fn table_object_clash_with_generated_names_is_refused() {
    let pkey_clash = vec![
        ModelDecl::new("scale", "stock")
            .fields(leak(vec![FieldDecl::char("name")]))
            .table_objects(leak(vec![TableObjectDecl::index(
                "_quant_pkey",
                IndexDecl::new(&["name"]),
            )])),
        ModelDecl::new("scale", "stock.quant").fields(leak(vec![FieldDecl::char("qty")])),
    ];
    let (kind, message) = refusal(pkey_clash);
    assert_eq!(kind, ErrorKind::Programming);
    assert_eq!(
        message,
        "table object name 'stock_quant_pkey' of model 'stock' ('_quant_pkey' on table 'stock') \
         clashes with the primary key of model 'stock.quant' on table 'stock_quant'; database \
         names of indexes and constraints are schema-wide, rename one"
    );

    // S2: a model's own table object clashing with its own generated pkey is refused
    let same_model_pkey = vec![
        ModelDecl::new("scale", "stock")
            .fields(leak(vec![FieldDecl::char("name")]))
            .table_objects(leak(vec![TableObjectDecl::index(
                "_pkey",
                IndexDecl::new(&["name"]),
            )])),
    ];
    let (kind, message) = refusal(same_model_pkey);
    assert_eq!(kind, ErrorKind::Programming);
    assert_eq!(
        message,
        "table object name 'stock_pkey' of model 'stock' ('_pkey' on table 'stock') clashes with \
         the primary key of model 'stock' on table 'stock'; database names of indexes and \
         constraints are schema-wide, rename one"
    );

    // S3: clash with identity sequence
    let seq_clash = vec![
        ModelDecl::new("scale", "stock")
            .fields(leak(vec![FieldDecl::char("name")]))
            .table_objects(leak(vec![TableObjectDecl::index(
                "_id_seq",
                IndexDecl::new(&["name"]),
            )])),
    ];
    let (kind, message) = refusal(seq_clash);
    assert_eq!(kind, ErrorKind::Programming);
    assert_eq!(
        message,
        "table object name 'stock_id_seq' of model 'stock' ('_id_seq' on table 'stock') clashes with \
         the identity sequence of model 'stock' on table 'stock'; database names of indexes and \
         constraints are schema-wide, rename one"
    );

    let fkey_clash = vec![
        ModelDecl::new("scale", "stock")
            .fields(leak(vec![FieldDecl::char("name")]))
            .table_objects(leak(vec![TableObjectDecl::index(
                "_quant_partner_id_fkey",
                IndexDecl::new(&["name"]),
            )])),
        ModelDecl::new("scale", "stock.quant")
            .fields(leak(vec![FieldDecl::many2one("partner_id", "stock")])),
    ];
    let (kind, message) = refusal(fkey_clash);
    assert_eq!(kind, ErrorKind::Programming);
    assert_eq!(
        message,
        "table object name 'stock_quant_partner_id_fkey' of model 'stock' ('_quant_partner_id_fkey' \
         on table 'stock') clashes with the foreign key of 'stock.quant.partner_id' on table \
         'stock_quant'; database names of indexes and constraints are schema-wide, rename one"
    );

    let idx_clash = vec![
        ModelDecl::new("scale", "stock")
            .fields(leak(vec![FieldDecl::char("name")]))
            .table_objects(leak(vec![TableObjectDecl::index(
                "_quant__lot_index",
                IndexDecl::new(&["name"]),
            )])),
        ModelDecl::new("scale", "stock.quant").fields(leak(vec![
            FieldDecl::char("lot").index(Some(IndexKind::Btree)),
        ])),
    ];
    let (kind, message) = refusal(idx_clash);
    assert_eq!(kind, ErrorKind::Programming);
    assert_eq!(
        message,
        "table object name 'stock_quant__lot_index' of model 'stock' ('_quant__lot_index' on \
         table 'stock') clashes with the field index of 'stock.quant.lot' on table 'stock_quant'; \
         database names of indexes and constraints are schema-wide, rename one"
    );

    // S3: m2m relation table pkey clash
    let m2m_clash = vec![
        ModelDecl::new("scale", "stock")
            .fields(leak(vec![FieldDecl::many2many("tag_ids", "scale.tag")])),
        ModelDecl::new("scale", "scale.tag").fields(leak(vec![FieldDecl::char("name")])),
        ModelDecl::new("scale", "scale_tag_stock")
            .fields(leak(vec![FieldDecl::char("name")]))
            .table_objects(leak(vec![TableObjectDecl::index(
                "_rel_pkey",
                IndexDecl::new(&["name"]),
            )])),
    ];
    let (kind, message) = refusal(m2m_clash);
    assert_eq!(kind, ErrorKind::Programming);
    assert_eq!(
        message,
        "table object name 'scale_tag_stock_rel_pkey' of model 'scale_tag_stock' ('_rel_pkey' on \
         table 'scale_tag_stock') clashes with the primary key of relation table 'scale_tag_stock_rel' \
         for m2m field 'stock.tag_ids'; database names of indexes and constraints are schema-wide, \
         rename one"
    );

    // L3: table object clashing with a generated name after pg_make_object_name truncation
    let long_table = "account_analytic_distribution_model_long_table_name_here_x";
    let pkey_name: &'static str = Box::leak(
        erp_registry::ident::pg_make_object_name(long_table, None, "pkey").into_boxed_str(),
    );
    // pkey_name truncated to 63 chars: suffix is _pkey (5 chars), so first 58 chars + _pkey
    assert_eq!(pkey_name.len(), 63);
    assert_eq!(&pkey_name[58..], "_pkey");
    let other_table = &pkey_name[..58];
    let l3_clash = vec![
        ModelDecl::new("scale", "scale.long")
            .table(long_table)
            .fields(leak(vec![FieldDecl::char("name")])),
        ModelDecl::new("scale", "scale.other")
            .table(other_table)
            .fields(leak(vec![FieldDecl::char("name")]))
            .table_objects(leak(vec![TableObjectDecl::index(
                "_pkey",
                IndexDecl::new(&["name"]),
            )])),
    ];
    let (kind, message) = refusal(l3_clash);
    assert_eq!(kind, ErrorKind::Programming);
    assert_eq!(
        message,
        format!(
            "table object name '{pkey_name}' of model 'scale.other' ('_pkey' on table '{other_table}') \
             clashes with the primary key of model 'scale.long' on table '{long_table}'; \
             database names of indexes and constraints are schema-wide, rename one"
        )
    );
}

// Translated fields support trigram expression indexes; other index kinds fail loudly.
#[test]
fn translated_field_indexes_support_trigram_only() {
    // Trigram index on translated field succeeds
    let reg = build(vec![model(vec![
        FieldDecl::char("name")
            .translate(true)
            .index(Some(IndexKind::Trigram)),
    ])]);
    assert!(
        reg.is_ok(),
        "trigram index on translated field should build successfully"
    );

    // Non-trigram indexes on translated fields are refused loudly
    for kind in [
        IndexKind::Btree,
        IndexKind::BtreeNotNull,
        IndexKind::Brin,
        IndexKind::Gin,
    ] {
        assert_eq!(
            refusal(vec![model(vec![
                FieldDecl::char("name").translate(true).index(Some(kind))
            ])]),
            (
                ErrorKind::Programming,
                "field 'scale.x.name': translated fields only support trigram indexes".to_owned()
            )
        );
    }
}

#[test]
fn translated_model_indexes_support_trigram_only() {
    // Trigram model index on translated field succeeds
    let trigram_idx = TableObjectDecl::index(
        "_translated_idx",
        IndexDecl::new(&["name"]).method(IndexMethod::Trigram),
    );
    let reg = build(vec![
        model(vec![FieldDecl::text("name").translate(true)]).table_objects(leak(vec![trigram_idx])),
    ]);
    assert!(
        reg.is_ok(),
        "trigram model index on translated field should build successfully"
    );

    // Non-trigram model indexes on translated fields are refused loudly
    for method in [
        IndexMethod::Btree,
        IndexMethod::Gin,
        IndexMethod::Brin,
        IndexMethod::Gist,
    ] {
        let index =
            TableObjectDecl::index("_translated_idx", IndexDecl::new(&["name"]).method(method));
        assert_eq!(
            refusal(vec![model(vec![FieldDecl::text("name").translate(true)])
                .table_objects(leak(vec![index]))]),
            (
                ErrorKind::Programming,
                "index '_translated_idx' of 'scale.x': field 'name' is translated; translated fields only support trigram indexes".to_owned()
            )
        );
    }
}
