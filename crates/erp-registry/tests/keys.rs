//! Business keys and row versions at registry build (D3,
//! §4.1, §10). Pure: no database.
#![allow(clippy::unwrap_used, reason = "tests")]

use erp_fields::{ErrorKind, FieldType, OnDelete};
use erp_registry::{
    AddonDescriptor, FieldDecl, KeyDecl, KindDecl, ModelDecl, ROW_VERSION, Registry,
    RegistryBuilder, TableObjectDecl, TableObjectKind,
};

static KEYS: AddonDescriptor = AddonDescriptor {
    name: "keys",
    depends: &["base"],
    exact_only: false,
};
static KEYS_EXT: AddonDescriptor = AddonDescriptor {
    name: "keys_ext",
    depends: &["keys"],
    exact_only: false,
};

const ITEM: &[&str] = &["keys.item"];

fn key(name: &'static str, decl: KeyDecl) -> TableObjectDecl {
    TableObjectDecl {
        name,
        kind: TableObjectKind::Key(decl),
    }
}

fn leak<T>(value: T) -> &'static T {
    Box::leak(Box::new(value))
}

fn objects(list: Vec<TableObjectDecl>) -> &'static [TableObjectDecl] {
    Box::leak(list.into_boxed_slice())
}

fn base_item() -> ModelDecl {
    ModelDecl::new("keys", "keys.item")
        .description("Key Item")
        .fields(
            const {
                &[
                    FieldDecl::char("code").required(true),
                    FieldDecl::many2one("company_id", "keys.company"),
                    FieldDecl::float("weight"),
                    FieldDecl::json("payload"),
                    FieldDecl::text("notes"),
                    FieldDecl::html("body"),
                    FieldDecl::char("label").compute("_compute_label"),
                    FieldDecl::one2many("line_ids", "keys.line", "item_id"),
                ]
            },
        )
}

fn company() -> ModelDecl {
    ModelDecl::new("keys", "keys.company").fields(const { &[FieldDecl::char("name")] })
}

fn line() -> ModelDecl {
    ModelDecl::new("keys", "keys.line")
        .fields(const { &[FieldDecl::many2one("item_id", "keys.item")] })
}

fn build(decls: Vec<ModelDecl>) -> erp_fields::Result<Registry> {
    RegistryBuilder::new([&KEYS, &KEYS_EXT])
        .with_decls(decls.into_iter().map(leak))
        .build()
}

fn build_err(decls: Vec<ModelDecl>) -> erp_fields::Error {
    build(decls).unwrap_err()
}

/// A key is composed with the other table objects, resolved to field ids, `NULLS NOT DISTINCT`
/// unless declared otherwise; an extension replaces it by name (later declaration wins).
#[test]
fn keys_are_composed_and_resolved() {
    let registry = build(vec![
        base_item().table_objects(objects(vec![key(
            "_code_key",
            KeyDecl::new(
                &["code", "company_id"],
                "The code must be unique per company.",
            ),
        )])),
        company(),
        line(),
    ])
    .unwrap();
    let item = registry.model("keys.item").unwrap();
    let k = item.business_key("_code_key").unwrap();
    assert_eq!(k.full_name, "keys_item_code_key");
    assert_eq!(
        k.fields,
        vec![
            item.field_id("code").unwrap(),
            item.field_id("company_id").unwrap()
        ]
    );
    assert!(!k.nulls_distinct);
    assert_eq!(k.arbitration_refusal(), None);
    assert!(item.business_key("_other").is_none());

    let replaced = build(vec![
        base_item().table_objects(objects(vec![key(
            "_code_key",
            KeyDecl::new(&["code", "company_id"], "first"),
        )])),
        ModelDecl::extension("keys_ext", "keys.item", ITEM).table_objects(objects(vec![key(
            "_code_key",
            KeyDecl::new(&["code"], "second").nulls_distinct(true),
        )])),
        company(),
        line(),
    ])
    .unwrap();
    let item = replaced.model("keys.item").unwrap();
    let k = item.business_key("_code_key").unwrap();
    assert_eq!((k.message, k.nulls_distinct), ("second", true));
    assert_eq!(k.fields, vec![item.field_id("code").unwrap()]);
    assert_eq!(k.arbitration_refusal(), Some("nulls_distinct"));
    assert_eq!(item.business_keys().len(), 1);
}

/// Key fields must be stored columns of the model's own table of an exactly comparable kind,
/// named once, with a message (design note §3.1).
#[test]
fn key_fields_are_checked() {
    let cases: Vec<(KeyDecl, String)> = vec![
        (
            KeyDecl::new(&[], "m"),
            "key '_k' of model 'keys.item' has no fields".to_owned(),
        ),
        (
            KeyDecl::new(&["code"], " "),
            "key '_k' of model 'keys.item' needs a message (shown when a row would duplicate the \
             key)"
                .to_owned(),
        ),
        (
            KeyDecl::new(&["nope"], "m"),
            "key '_k' of model 'keys.item': unknown field 'nope'".to_owned(),
        ),
        (
            KeyDecl::new(&["code", "code"], "m"),
            "key '_k' of model 'keys.item' names field 'code' twice".to_owned(),
        ),
        (
            KeyDecl::new(&["label"], "m"),
            "key '_k' of model 'keys.item': field 'label' is not stored; key fields must be \
             columns of the model's own table"
                .to_owned(),
        ),
        (
            KeyDecl::new(&["weight"], "m"),
            "key '_k' of model 'keys.item': field 'weight' (float) cannot be a key field: a float \
             without digits compares inexactly; give it digits or use another field"
                .to_owned(),
        ),
        (
            KeyDecl::new(&["payload"], "m"),
            "key '_k' of model 'keys.item': field 'payload' (json) cannot be a key field: JSON \
             documents have no exact key equality"
                .to_owned(),
        ),
        (
            KeyDecl::new(&["notes"], "m"),
            "key '_k' of model 'keys.item': field 'notes' (text) cannot be a key field: text and \
             HTML fields are unbounded, and a btree entry over about 2.7 kB fails with a bare \
             database error; give the key a char field with a size"
                .to_owned(),
        ),
        (
            KeyDecl::new(&["body"], "m"),
            "key '_k' of model 'keys.item': field 'body' (html) cannot be a key field: text and \
             HTML fields are unbounded, and a btree entry over about 2.7 kB fails with a bare \
             database error; give the key a char field with a size"
                .to_owned(),
        ),
        (
            KeyDecl::new(&["line_ids"], "m"),
            "key '_k' of model 'keys.item': field 'line_ids' (one2many) cannot be a key field: \
             x2many fields have no column"
                .to_owned(),
        ),
        (
            KeyDecl::new(&["id"], "m"),
            "key '_k' of model 'keys.item': field 'id' (integer) cannot be a key field: the id is \
             unique already"
                .to_owned(),
        ),
    ];
    for (decl, message) in cases {
        let err = build_err(vec![
            base_item().table_objects(objects(vec![key("_k", decl)])),
            company(),
            line(),
        ]);
        assert_eq!(err.kind(), ErrorKind::Programming, "{message}");
        assert_eq!(err.message(), message);
    }
}

/// A key over a field the model only inherits through `_inherits` is refused.
#[test]
fn keys_over_inherited_fields_are_refused() {
    let err = build_err(vec![
        company(),
        line(),
        base_item(),
        ModelDecl::new("keys", "keys.child")
            .inherits(&[("keys.item", "item_id")])
            .fields(
                const {
                    &[FieldDecl::many2one("item_id", "keys.item")
                        .required(true)
                        .ondelete(OnDelete::Cascade)]
                },
            )
            .table_objects(objects(vec![key("_k", KeyDecl::new(&["code"], "m"))])),
    ]);
    assert_eq!(err.kind(), ErrorKind::Programming);
    assert_eq!(
        err.message(),
        "key '_k' of model 'keys.child': field 'code' is inherited through _inherits; key fields \
         must be columns of the model's own table"
    );
}

/// `row_version = true` adds a readonly, required, not copied `row_version` field of its own
/// kind; a later declaration cannot turn it off, and no declaration may declare the field
/// itself (design note §4.1).
#[test]
fn row_version_declaration() {
    let registry = build(vec![
        base_item().row_version(true),
        // a later declaration repeating it changes nothing
        ModelDecl::extension("keys_ext", "keys.item", ITEM).row_version(true),
        company(),
        line(),
    ])
    .unwrap();
    let item = registry.model("keys.item").unwrap();
    let fid = item.row_version.unwrap();
    let field = item.field_by_id(fid);
    assert_eq!(field.name, ROW_VERSION);
    assert_eq!(field.ty, FieldType::RowVersion);
    assert_eq!(
        (field.readonly, field.required, field.copy, field.store),
        (true, true, false, true)
    );
    assert_eq!(field.ty.type_name(), "integer");
    assert!(
        registry
            .model("keys.company")
            .unwrap()
            .row_version
            .is_none()
    );
    assert!(
        registry
            .model("keys.company")
            .unwrap()
            .field(ROW_VERSION)
            .is_none()
    );

    // an extension may turn it on
    let registry = build(vec![
        base_item(),
        ModelDecl::extension("keys_ext", "keys.item", ITEM).row_version(true),
        company(),
        line(),
    ])
    .unwrap();
    assert!(registry.model("keys.item").unwrap().row_version.is_some());

    let err = build_err(vec![
        base_item().row_version(true),
        ModelDecl::extension("keys_ext", "keys.item", ITEM)
            .row_version(false)
            .at("ext.rs", 7),
        company(),
        line(),
    ]);
    assert_eq!(err.kind(), ErrorKind::Programming);
    assert_eq!(
        err.message(),
        "model definition 'keys.item' of addon 'keys_ext' (ext.rs:7) sets row_version = false on \
         model 'keys.item', but model definition 'keys.item' of addon 'keys' already turned it \
         on; a row version cannot be turned off once declared"
    );

    let err = build_err(vec![
        base_item().row_version(true),
        ModelDecl::extension("keys_ext", "keys.item", ITEM)
            .fields(const { &[FieldDecl::integer("row_version")] }),
        company(),
        line(),
    ]);
    assert_eq!(err.kind(), ErrorKind::Programming);
    assert_eq!(
        err.message(),
        "model definition 'keys.item' of addon 'keys_ext' declares field 'row_version' on model \
         'keys.item': the name is reserved to the field the registry adds for row_version = true"
    );

    // without row_version the name is free
    let registry = build(vec![
        base_item(),
        ModelDecl::extension("keys_ext", "keys.item", ITEM)
            .fields(const { &[FieldDecl::integer("row_version")] }),
        company(),
        line(),
    ])
    .unwrap();
    let item = registry.model("keys.item").unwrap();
    assert!(item.row_version.is_none());
    assert_eq!(item.field(ROW_VERSION).unwrap().ty, FieldType::Integer);

    let err = build_err(vec![
        base_item().fields(const { &[FieldDecl::new("version", KindDecl::RowVersion)] }),
        company(),
        line(),
    ]);
    assert_eq!(err.kind(), ErrorKind::Programming);
    assert_eq!(
        err.message(),
        "model definition 'keys.item' of addon 'keys' declares field 'version' on model \
         'keys.item': the row version kind is reserved to the field the registry adds for \
         row_version = true"
    );
}
