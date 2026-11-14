// SPDX-License-Identifier: MIT OR Apache-2.0
//! Restrictive removal preserves exact quoted identities and overloaded signatures.
#![allow(clippy::unwrap_used, reason = "tests")]
use erp_registry::*;
static ADDON: AddonDescriptor = AddonDescriptor {
    name: "drop_test",
    depends: &["base"],
    exact_only: false,
};
fn leak<T>(v: T) -> &'static T {
    Box::leak(Box::new(v))
}
#[test]
fn drop_uses_exact_signature_and_never_cascade_or_if_exists() {
    let function = leak(SqlObjectDecl::function(
        "drop_test",
        "function",
        SqlFunctionDecl::new(
            "Mixed$Function",
            SqlLanguage::Sql,
            SqlResult::Scalar(SqlScalar::Text),
            "SELECT $1",
        )
        .schema("App$Schema")
        .args(&[
            SqlType::Scalar(SqlScalar::Text),
            SqlType::Array(SqlScalar::Integer),
        ]),
    ));
    let reg = RegistryBuilder::new([&ADDON])
        .linked(false)
        .require_sql_objects([leak(SqlObjectManifest::new("drop_test", &["function"]))])
        .with_sql_objects([function])
        .build()
        .unwrap();
    assert_eq!(
        erp_sql::sqlobj::drop_object(
            reg.sql_objects().get("drop_test", "function").unwrap(),
            "ignored"
        )
        .unwrap(),
        Some("DROP FUNCTION \"App$Schema\".\"Mixed$Function\"(text, integer[]) RESTRICT".into())
    );
}
#[test]
fn trigger_drop_qualifies_the_table_and_quotes_every_identifier() {
    let f = leak(SqlObjectDecl::function(
        "drop_test",
        "function",
        SqlFunctionDecl::new(
            "guard",
            SqlLanguage::PlPgSql,
            SqlResult::Trigger,
            "BEGIN RETURN NEW; END",
        ),
    ));
    let t = leak(SqlObjectDecl::trigger(
        "drop_test",
        "trigger",
        SqlTriggerDecl::new(
            "Mixed$Trigger",
            "drop.item",
            ("drop_test", "function"),
            SqlTiming::Before,
            &[SqlEvent::Insert],
            SqlLevel::Row,
        ),
    ));
    let reg = RegistryBuilder::new([&ADDON])
        .linked(false)
        .with_decls([leak(ModelDecl::new("drop_test", "drop.item"))])
        .require_sql_objects([leak(SqlObjectManifest::new(
            "drop_test",
            &["function", "trigger"],
        ))])
        .with_sql_objects([f, t])
        .build()
        .unwrap();
    assert_eq!(
        erp_sql::sqlobj::drop_object(
            reg.sql_objects().get("drop_test", "trigger").unwrap(),
            "App$Schema"
        )
        .unwrap(),
        Some("DROP TRIGGER \"Mixed$Trigger\" ON \"App$Schema\".\"drop_item\" RESTRICT".into())
    );
}
