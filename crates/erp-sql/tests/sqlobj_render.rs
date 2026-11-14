// SPDX-License-Identifier: MIT OR Apache-2.0
//! Rendering of addon-owned SQL objects (slice B3): DDL text, the core-rendered
//! search path (A1), the dollar quote and the canonical hash payloads. Pure: no database.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

use erp_fields::ErrorKind;
use erp_registry::{
    AddonDescriptor, FieldDecl, ModelDecl, Registry, RegistryBuilder, SqlColumnSet,
    SqlCompareColumn, SqlConditionDecl, SqlDeferral, SqlDep, SqlEvent, SqlFunction,
    SqlFunctionDecl, SqlLanguage, SqlLevel, SqlObject, SqlObjectDecl, SqlObjectKind,
    SqlObjectManifest, SqlResult, SqlScalar, SqlTiming, SqlTrigger, SqlTriggerDecl, SqlType,
};
use erp_sql::sqlobj;

static ADDON: AddonDescriptor = AddonDescriptor {
    name: "test_sqr",
    depends: &["base"],
    exact_only: false,
};

/// The model; hostile identifiers (`$`, upper case) are in the object names.
const MODEL: &str = "sqr.move";

fn leak<T>(value: T) -> &'static T {
    Box::leak(Box::new(value))
}

fn compare_body(columns: &[SqlCompareColumn]) -> String {
    let list: Vec<String> = columns
        .iter()
        .map(|c| format!("{} {}", c.quoted(), c.sql_type()))
        .collect();
    format!(
        "BEGIN RETURN '{}'; END",
        list.join(", ").replace('\'', "''")
    )
}

fn decls() -> Vec<SqlObjectDecl> {
    vec![
        SqlObjectDecl::function(
            "test_sqr",
            "fn_helper",
            SqlFunctionDecl::new(
                "sqr$Helper",
                SqlLanguage::Sql,
                SqlResult::SetOf(SqlScalar::TimestampTz),
                "SELECT now() WHERE $1 <> '$$' AND $2 IS NOT NULL -- $erp$ and $erp",
            )
            .schema("Sq$r")
            .args(&[
                SqlType::Scalar(SqlScalar::Text),
                SqlType::Array(SqlScalar::Integer),
            ]),
        ),
        SqlObjectDecl::function(
            "test_sqr",
            "fn_guard",
            SqlFunctionDecl::new(
                "sqr_guard",
                SqlLanguage::PlPgSql,
                SqlResult::Trigger,
                "BEGIN RETURN NULL; END",
            )
            .deps(&[
                SqlDep::Object {
                    addon: "test_sqr",
                    key: "fn_helper",
                },
                SqlDep::Model(MODEL),
            ]),
        ),
        SqlObjectDecl::function(
            "test_sqr",
            "fn_compare",
            SqlFunctionDecl::catalog_derived(
                "sqr_compare",
                SqlLanguage::PlPgSql,
                SqlResult::Scalar(SqlScalar::Text),
                SqlColumnSet::new(MODEL, &["note"]),
                compare_body,
            ),
        ),
        SqlObjectDecl::trigger(
            "test_sqr",
            "trg_ins",
            SqlTriggerDecl::new(
                "Sqr$ins",
                MODEL,
                ("test_sqr", "fn_guard"),
                SqlTiming::After,
                &[SqlEvent::Insert],
                SqlLevel::Statement,
            )
            .new_table("New$rows"),
        ),
        SqlObjectDecl::trigger(
            "test_sqr",
            "trg_upd",
            SqlTriggerDecl::new(
                "sqr_upd",
                MODEL,
                ("test_sqr", "fn_guard"),
                SqlTiming::After,
                &[SqlEvent::Update],
                SqlLevel::Statement,
            )
            .old_table("old_rows")
            .new_table("new_rows"),
        ),
        SqlObjectDecl::trigger(
            "test_sqr",
            "trg_del",
            SqlTriggerDecl::new(
                "sqr_del",
                MODEL,
                ("test_sqr", "fn_guard"),
                SqlTiming::After,
                &[SqlEvent::Delete],
                SqlLevel::Statement,
            )
            .old_table("old_rows"),
        ),
        SqlObjectDecl::trigger(
            "test_sqr",
            "trg_trunc",
            SqlTriggerDecl::new(
                "sqr_trunc",
                MODEL,
                ("test_sqr", "fn_guard"),
                SqlTiming::Before,
                &[SqlEvent::Truncate],
                SqlLevel::Statement,
            ),
        ),
        SqlObjectDecl::trigger(
            "test_sqr",
            "trg_row",
            SqlTriggerDecl::new(
                "sqr_row",
                MODEL,
                ("test_sqr", "fn_guard"),
                SqlTiming::Before,
                &[SqlEvent::Insert],
                SqlLevel::Row,
            ),
        ),
        SqlObjectDecl::trigger(
            "test_sqr",
            "trg_check",
            SqlTriggerDecl::new(
                "sqr_check",
                MODEL,
                ("test_sqr", "fn_guard"),
                SqlTiming::After,
                &[SqlEvent::Insert],
                SqlLevel::Row,
            )
            .constraint(SqlDeferral::InitiallyDeferred),
        ),
        SqlObjectDecl::condition(
            "test_sqr",
            "cond",
            SqlConditionDecl::new("ERP01", "sqr_cond", "Refused.")
                .kind(erp_registry::SqlConditionKind::Validation),
        ),
    ]
}

fn registry() -> Registry {
    let decls: Vec<&'static SqlObjectDecl> = decls().into_iter().map(leak).collect();
    let keys: Vec<&'static str> = decls.iter().map(|d| d.key()).collect();
    let manifest: &'static SqlObjectManifest = leak(SqlObjectManifest::new(
        "test_sqr",
        Box::leak(keys.into_boxed_slice()),
    ));
    let model: &'static ModelDecl = leak(
        ModelDecl::new("test_sqr", MODEL)
            .fields(const { &[FieldDecl::char("name"), FieldDecl::char("note")] }),
    );
    RegistryBuilder::new([&ADDON])
        .with_decls([model])
        .require_sql_objects([manifest])
        .with_sql_objects(decls)
        .linked(false)
        .build()
        .unwrap()
}

fn object<'r>(registry: &'r Registry, key: &str) -> &'r SqlObject {
    registry.sql_objects().get("test_sqr", key).unwrap()
}

fn function<'r>(registry: &'r Registry, key: &str) -> &'r SqlFunction {
    match object(registry, key).kind() {
        SqlObjectKind::Function(f) => f,
        other => panic!("{key} is not a function: {other:?}"),
    }
}

fn trigger<'r>(registry: &'r Registry, key: &str) -> &'r SqlTrigger {
    match object(registry, key).kind() {
        SqlObjectKind::Trigger(t) => t,
        other => panic!("{key} is not a trigger: {other:?}"),
    }
}

#[test]
fn spec_ormx_040_rendering_quotes_identifiers() {
    let registry = registry();
    let helper = function(&registry, "fn_helper");
    let guard = function(&registry, "fn_guard");
    assert_eq!(
        sqlobj::signature(helper),
        format!(
            "{}.{}(text, integer[])",
            erp_sql::quote("Sq$r"),
            erp_sql::quote("sqr$Helper")
        )
    );
    let schemas = sqlobj::search_path_schemas(
        registry.sql_objects(),
        object(&registry, "fn_guard"),
        "pu\"b",
    );
    // own schema, then the helper's schema, then the model's (the current schema)
    assert_eq!(
        schemas,
        vec!["public".to_owned(), "Sq$r".to_owned(), "pu\"b".to_owned()]
    );
    let created = sqlobj::create_function(guard, guard.body(), &schemas, false).unwrap();
    assert_eq!(
        created,
        format!(
            "CREATE FUNCTION {} RETURNS trigger LANGUAGE plpgsql VOLATILE SECURITY INVOKER \
             {} AS $erp$BEGIN RETURN NULL; END$erp$",
            sqlobj::signature(guard),
            sqlobj::search_path_clause(&schemas)
        )
    );
    let replaced = sqlobj::create_function(guard, guard.body(), &schemas, true).unwrap();
    assert_eq!(
        replaced,
        created.replacen("CREATE FUNCTION", "CREATE OR REPLACE FUNCTION", 1)
    );
    let table = erp_sql::quote("sqr_move");
    let run = format!("EXECUTE FUNCTION {}", sqlobj::signature(guard));
    let expected = [
        (
            "trg_ins",
            format!(
                "CREATE TRIGGER {} AFTER INSERT ON {table} REFERENCING NEW TABLE AS {} FOR EACH \
                 STATEMENT {run}",
                erp_sql::quote("Sqr$ins"),
                erp_sql::quote("New$rows")
            ),
        ),
        (
            "trg_upd",
            format!(
                "CREATE TRIGGER \"sqr_upd\" AFTER UPDATE ON {table} REFERENCING OLD TABLE AS \
                 \"old_rows\" NEW TABLE AS \"new_rows\" FOR EACH STATEMENT {run}"
            ),
        ),
        (
            "trg_del",
            format!(
                "CREATE TRIGGER \"sqr_del\" AFTER DELETE ON {table} REFERENCING OLD TABLE AS \
                 \"old_rows\" FOR EACH STATEMENT {run}"
            ),
        ),
        (
            "trg_trunc",
            format!(
                "CREATE TRIGGER \"sqr_trunc\" BEFORE TRUNCATE ON {table} FOR EACH STATEMENT {run}"
            ),
        ),
        (
            "trg_row",
            format!("CREATE TRIGGER \"sqr_row\" BEFORE INSERT ON {table} FOR EACH ROW {run}"),
        ),
        (
            "trg_check",
            format!(
                "CREATE CONSTRAINT TRIGGER \"sqr_check\" AFTER INSERT ON {table} DEFERRABLE \
                 INITIALLY DEFERRED FOR EACH ROW {run}"
            ),
        ),
    ];
    for (key, sql) in expected {
        let t = trigger(&registry, key);
        assert_eq!(sqlobj::create_trigger(t, guard), sql, "{key}");
        assert_eq!(
            sqlobj::enable_always(t),
            format!(
                "ALTER TABLE {table} ENABLE ALWAYS TRIGGER {}",
                erp_sql::quote(t.name())
            )
        );
    }
}

/// A1: `pg_catalog` first, the declared schemas quoted and de-duplicated, `pg_temp` last.
#[test]
fn search_path_clause_is_core_rendered() {
    let schemas = vec!["public".to_owned(), "a\"b".to_owned()];
    assert_eq!(
        sqlobj::search_path_clause(&schemas),
        "SET search_path = pg_catalog, \"public\", \"a\"\"b\", pg_temp"
    );
    let registry = registry();
    // an independent function: only its own schema
    assert_eq!(
        sqlobj::search_path_schemas(
            registry.sql_objects(),
            object(&registry, "fn_helper"),
            "public"
        ),
        vec!["Sq$r".to_owned()]
    );
    // the current schema equal to the own schema appears once
    assert_eq!(
        sqlobj::search_path_schemas(
            registry.sql_objects(),
            object(&registry, "fn_compare"),
            "public"
        ),
        vec!["public".to_owned()]
    );
}

/// The dollar-quote tag never occurs in the body, nor across the body's end.
#[test]
fn dollar_quote_tag_is_absent_from_the_body() {
    for body in [
        "SELECT 1",
        "SELECT '$$'",
        "SELECT '$erp$'",
        "SELECT '$erp$ $erp1$'",
        "SELECT 1 -- ends with $erp",
        "SELECT 1 -- ends with $erp1",
        "$erp",
        "",
    ] {
        let quoted = sqlobj::dollar_quote(body).unwrap();
        let end = quoted
            .find('$')
            .map(|i| quoted[i + 1..].find('$').unwrap() + i + 2)
            .unwrap();
        let tag = &quoted[..end];
        assert!(tag.starts_with("$erp") && tag.ends_with('$'), "{tag}");
        assert_eq!(&quoted[tag.len()..quoted.len() - tag.len()], body);
        assert!(quoted.ends_with(tag));
        // the first occurrence of the tag after the opening one is the closing one
        assert_eq!(
            quoted[tag.len()..].find(tag),
            Some(body.len()),
            "{body:?} quoted as {quoted:?}"
        );
    }
}

/// The canonical payloads: versioned, length-delimited, exact body bytes; the shape covers
/// the shape only.
#[test]
fn canonical_payloads_are_versioned_and_exact() {
    let registry = registry();
    let helper = object(&registry, "fn_helper");
    let schemas = sqlobj::search_path_schemas(registry.sql_objects(), helper, "public");
    let f = function(&registry, "fn_helper");
    let declaration = sqlobj::declaration_payload(helper, f.body(), &schemas);
    let prefix = format!("erp-sqlobj:{}:", sqlobj::FORMAT_VERSION);
    assert!(declaration.starts_with(&prefix), "{declaration}");
    let rest = &declaration[prefix.len()..];
    let (len, json) = rest.split_once(':').unwrap();
    assert_eq!(len.parse::<usize>().unwrap(), json.len());
    let values: Vec<Option<String>> = serde_json::from_str(json).unwrap();
    assert!(values.contains(&Some(f.body().to_owned())));
    // CRLF and trailing whitespace are body bytes
    let crlf = sqlobj::declaration_payload(helper, &format!("{}\r\n ", f.body()), &schemas);
    assert_ne!(crlf, declaration);
    let shape = sqlobj::shape_payload(helper);
    assert_eq!(sqlobj::shape_change(helper, &shape).unwrap(), None);
    let guard = object(&registry, "fn_guard");
    assert_eq!(
        sqlobj::shape_change(guard, &shape).unwrap(),
        Some("function schema")
    );
    let error = sqlobj::shape_change(guard, "erp-sqlobj:1:2:[]x").unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Database);
    assert_eq!(
        error.message(),
        "SQL object 'test_sqr/fn_guard': the recorded shape is not a canonical payload of format 1"
    );
    let columns = vec![
        SqlCompareColumn::new("na\"me", "character varying"),
        SqlCompareColumn::new("id", "integer"),
    ];
    let payload = sqlobj::columns_payload(&columns);
    let swapped = sqlobj::columns_payload(&[columns[1].clone(), columns[0].clone()]);
    assert_ne!(payload, swapped, "column order is semantic");
    assert_eq!(
        columns[0].quoted(),
        erp_sql::quote("na\"me"),
        "the render callback receives quoted names"
    );
}

#[test]
fn condition_kind_changes_declaration_but_not_physical_identity() {
    let build = |kind| {
        let declaration = leak(SqlObjectDecl::condition(
            "test_sqr",
            "kind",
            SqlConditionDecl::new("ERP10", "sqr_kind", "Refused.").kind(kind),
        ));
        RegistryBuilder::new([&ADDON])
            .linked(false)
            .require_sql_objects([leak(SqlObjectManifest::new("test_sqr", &["kind"]))])
            .with_sql_objects([declaration])
            .build()
            .unwrap()
    };
    let validation = build(erp_registry::SqlConditionKind::Validation);
    let programming = build(erp_registry::SqlConditionKind::Programming);
    let a = object(&validation, "kind");
    let b = object(&programming, "kind");
    assert_eq!(sqlobj::physical_identity(a), sqlobj::physical_identity(b));
    assert_eq!(sqlobj::shape_payload(a), sqlobj::shape_payload(b));
    assert_ne!(
        sqlobj::declaration_payload(a, "", &[]),
        sqlobj::declaration_payload(b, "", &[])
    );
}
