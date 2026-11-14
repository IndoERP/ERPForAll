// SPDX-License-Identifier: MIT OR Apache-2.0
//! B6 native integrity declaration acceptance. Fixture models ship no default data.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]
use erp_fields::ErrorKind;
use erp_registry::*;

static ADDON: AddonDescriptor = AddonDescriptor {
    name: "test_integrity",
    depends: &["base"],
    exact_only: false,
};
fn leak<T>(v: T) -> &'static T {
    Box::leak(Box::new(v))
}
fn models() -> Vec<ModelDecl> {
    vec![
        ModelDecl::new(ADDON.name, "integrity.parent")
            .fields(const { &[FieldDecl::integer("company").required(true)] })
            .table_objects(
                const {
                    &[TableObjectDecl {
                        name: "_company_id",
                        kind: TableObjectKind::Key(KeyDecl::new(
                            &["company", "id"],
                            "Unique pair.",
                        )),
                    }]
                },
            ),
        ModelDecl::new(ADDON.name, "integrity.child").fields(
            const {
                &[
                    FieldDecl::integer("company").required(true),
                    FieldDecl::many2one("reference_id", "integrity.parent"),
                ]
            },
        ),
    ]
}
fn foreign(
    columns: &'static [&'static str],
    targets: &'static [&'static str],
) -> SqlForeignKeyDecl {
    SqlForeignKeyDecl::new(
        "Integrity$fk",
        "integrity.child",
        columns,
        "integrity.parent",
        targets,
        SqlForeignKeyTarget::Key("_company_id"),
    )
}
fn fk(f: SqlForeignKeyDecl) -> SqlObjectDecl {
    SqlObjectDecl::foreign_key(ADDON.name, "fk", f)
}
fn build(
    decls: Vec<SqlObjectDecl>,
    models: Vec<ModelDecl>,
    limits: SqlObjectLimits,
) -> erp_fields::Result<Registry> {
    let keys = Box::leak(
        decls
            .iter()
            .map(SqlObjectDecl::key)
            .collect::<Vec<_>>()
            .into_boxed_slice(),
    );
    RegistryBuilder::new([&ADDON])
        .with_decls(models.into_iter().map(leak))
        .require_sql_objects([leak(SqlObjectManifest::new(ADDON.name, keys))])
        .with_sql_objects(decls.into_iter().map(leak))
        .sql_object_limits(limits)
        .build()
}
fn refused(f: SqlForeignKeyDecl, reason: &str) {
    let error = build(vec![fk(f)], models(), SqlObjectLimits::default()).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Programming);
    assert_eq!(
        error.message(),
        format!("SQL object 'test_integrity/fk': {reason}")
    );
}
#[test]
fn composite_fk_resolves_and_invalid_vectors_fail() {
    let valid = foreign(&["company", "reference_id"], &["company", "id"])
        .on_delete(SqlForeignKeyAction::SetNull(&["reference_id"]));
    let registry = build(vec![fk(valid)], models(), SqlObjectLimits::default()).unwrap();
    let SqlObjectKind::ForeignKey(f) = registry.sql_objects().get(ADDON.name, "fk").unwrap().kind()
    else {
        panic!("foreign key")
    };
    assert_eq!(f.columns(), &["company", "reference_id"]);
    assert_eq!(f.target_columns(), &["company", "id"]);
    assert_eq!(f.model().table(), "integrity_child");
    assert_eq!(
        registry
            .model("integrity.parent")
            .unwrap()
            .business_key("_company_id")
            .unwrap()
            .arbitration_refusal(),
        Some("id")
    );
    refused(foreign(&[], &[]), "foreign key columns must not be empty");
    refused(
        foreign(&["company"], &["company", "id"]),
        "foreign key column arity differs",
    );
    refused(
        foreign(&["company", "company"], &["company", "id"]),
        "local column 'company' is listed more than once",
    );
    refused(
        foreign(&["company", "reference_id"], &["company", "company"]),
        "target column 'company' is listed more than once",
    );
    refused(
        valid.on_delete(SqlForeignKeyAction::SetNull(&[])),
        "SET NULL columns must not be empty",
    );
    refused(
        valid.on_delete(SqlForeignKeyAction::SetNull(&["missing"])),
        "SET NULL column 'missing' is not a local column",
    );
    refused(
        valid.on_delete(SqlForeignKeyAction::SetNull(&["company"])),
        "SET NULL column 'integrity.child.company' is required",
    );
    refused(
        valid.on_delete(SqlForeignKeyAction::SetNull(&[
            "reference_id",
            "reference_id",
        ])),
        "SET NULL column 'reference_id' is listed more than once",
    );
    refused(
        foreign(&["reference_id", "company"], &["company", "id"]),
        "columns 'integrity.child.reference_id' and 'integrity.parent.company' have incompatible types",
    );
    refused(
        foreign(&["missing", "reference_id"], &["company", "id"]),
        "field 'integrity.child.missing' is not an ordinary stored column",
    );
    refused(
        foreign(&["company", "reference_id"], &["id", "company"]),
        "target key '_company_id' does not match the ordered target columns",
    );
}
#[test]
fn primary_target_and_column_bound() {
    let primary = SqlForeignKeyDecl::new(
        "child_parent",
        "integrity.child",
        &["reference_id"],
        "integrity.parent",
        &["id"],
        SqlForeignKeyTarget::PrimaryKey,
    );
    build(vec![fk(primary)], models(), SqlObjectLimits::default()).unwrap();
    let mut limits = SqlObjectLimits::default();
    limits.foreign_key_columns = 1;
    let error = build(
        vec![fk(foreign(
            &["company", "reference_id"],
            &["company", "id"],
        ))],
        models(),
        limits,
    )
    .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Programming);
    assert_eq!(
        error.message(),
        "SQL object 'test_integrity/fk': foreign key columns limit 1 exceeded"
    );
}
#[test]
fn unlogged_options_require_policy_and_original_owner() {
    let options = SqlObjectDecl::table_options(
        ADDON.name,
        "table",
        SqlTableOptionsDecl::new("integrity.child", SqlTablePersistence::Unlogged),
    );
    let error = build(vec![options], models(), SqlObjectLimits::default()).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Programming);
    assert_eq!(
        error.message(),
        "SQL object 'test_integrity/table': unlogged table 'integrity.child' needs an explicit table grant policy"
    );
    let grant = SqlObjectDecl::table_grant(
        ADDON.name,
        "grant",
        SqlTableGrantDecl::new(
            "integrity.child",
            SqlBinding::Request,
            &[SqlTableOp::Insert],
        ),
    );
    let registry = build(vec![options, grant], models(), SqlObjectLimits::default()).unwrap();
    let SqlObjectKind::TableOptions(t) = registry
        .sql_objects()
        .get(ADDON.name, "table")
        .unwrap()
        .kind()
    else {
        panic!("options")
    };
    assert_eq!(t.persistence(), SqlTablePersistence::Unlogged);
}

#[test]
fn table_options_do_not_override_owner_or_duplicate_policy() {
    let options = SqlObjectDecl::table_options(
        ADDON.name,
        "table",
        SqlTableOptionsDecl::new("integrity.child", SqlTablePersistence::Logged),
    );
    let mut m = models();
    m[1].runtime = true;
    let e = build(vec![options], m, SqlObjectLimits::default()).unwrap_err();
    assert_eq!(e.kind(), ErrorKind::Programming);
    assert_eq!(
        e.message(),
        "SQL object 'test_integrity/table': table options for 'integrity.child' may only be declared by its original native owner"
    );
    let duplicate = SqlObjectDecl::table_options(
        ADDON.name,
        "second",
        SqlTableOptionsDecl::new("integrity.child", SqlTablePersistence::Logged),
    );
    let e = build(
        vec![options, duplicate],
        models(),
        SqlObjectLimits::default(),
    )
    .unwrap_err();
    assert_eq!(e.kind(), ErrorKind::Programming);
    assert_eq!(
        e.message(),
        "SQL object 'test_integrity/table': table options for 'integrity_child' is also declared by 'test_integrity/second'"
    );
}

#[test]
fn foreign_key_requires_the_target_key_contributor() {
    static KEY_ADDON: AddonDescriptor = AddonDescriptor {
        name: "test_integrity_key",
        depends: &["test_integrity"],
        exact_only: false,
    };
    let mut m = models();
    m[0].table_objects = &[];
    let extension = ModelDecl::extension(KEY_ADDON.name, "integrity.parent", &["integrity.parent"])
        .table_objects(
            const {
                &[TableObjectDecl {
                    name: "_company_id",
                    kind: TableObjectKind::Key(KeyDecl::new(&["company", "id"], "Unique pair.")),
                }]
            },
        );
    m.push(extension);
    let contributions: Vec<&'static ModelDecl> = m.into_iter().map(leak).collect();
    let error = RegistryBuilder::new([&ADDON, &KEY_ADDON])
        .with_decls(contributions.iter().copied())
        .require_sql_objects([leak(SqlObjectManifest::new(ADDON.name, &["fk"]))])
        .with_sql_objects([leak(fk(foreign(
            &["company", "reference_id"],
            &["company", "id"],
        )))])
        .linked(false)
        .build()
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Programming);
    assert_eq!(
        error.message(),
        "SQL object 'test_integrity/fk': target key '_company_id' is contributed by addon 'test_integrity_key', but addon 'test_integrity' does not depend on it"
    );
    static CONSUMER: AddonDescriptor = AddonDescriptor {
        name: "test_integrity_consumer",
        depends: &["test_integrity_key"],
        exact_only: false,
    };
    RegistryBuilder::new([&ADDON, &KEY_ADDON, &CONSUMER])
        .with_decls(contributions.iter().copied())
        .require_sql_objects([leak(SqlObjectManifest::new(CONSUMER.name, &["fk"]))])
        .with_sql_objects([leak(SqlObjectDecl::foreign_key(
            CONSUMER.name,
            "fk",
            foreign(&["company", "reference_id"], &["company", "id"]),
        ))])
        .linked(false)
        .build()
        .unwrap();
    // A matching key also owned by the reachable base addon does not depend on an override's message.
    RegistryBuilder::new([&ADDON, &KEY_ADDON])
        .with_decls(models().into_iter().map(leak).chain([contributions[2]]))
        .require_sql_objects([leak(SqlObjectManifest::new(ADDON.name, &["fk"]))])
        .with_sql_objects([leak(fk(foreign(
            &["company", "reference_id"],
            &["company", "id"],
        )))])
        .linked(false)
        .build()
        .unwrap();
}
