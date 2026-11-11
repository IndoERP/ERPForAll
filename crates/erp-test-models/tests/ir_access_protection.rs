//! Generated-row immutability through public ORM dispatch; lifecycle emission is separate.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "test assertions"
)]

mod support;
use erp_registry::{
    AddonDescriptor, CompanyAccessDecl, CompanyBranch, EmptyCompany, FieldDecl, ModelDecl,
};
use erp_sdk::{ErrorKind, Records, Transaction, Value};
use erp_testkit::Harness;
use support::map;

static ADDON: AddonDescriptor = AddonDescriptor {
    name: "access_protection",
    depends: &["base"],
    exact_only: false,
};
static FIELDS: &[FieldDecl] = &[FieldDecl::many2one("company_id", "res.company")];
const DOC: ModelDecl = ModelDecl::new("access_protection", "protection.doc")
    .fields(FIELDS)
    .company_access(CompanyAccessDecl::direct(
        "company_id",
        CompanyBranch::Exact,
        EmptyCompany::Deny,
    ));
const SET_ERROR: &str = "ir.access rows marked generated are written by module installation and updates only: the company restriction of a model comes from its company_access declaration.";
const ROW_ERROR: &str = "The company restriction of protection.doc is generated from its company_access declaration (direct(field = \"company_id\", branch = \"exact\", empty = \"deny\"), declared by addon 'access_protection') and cannot be archived, changed or deleted; change the declaration and update the addon.";

async fn harness() -> Harness {
    Harness::builder([&erp_addon_base::ADDON, &erp_data::model_data::ADDON, &ADDON])
        .with_decls([erp_data::ir_model_data_decl(), DOC])
        .build()
        .await
        .unwrap()
}
async fn seed(tx: &mut Transaction) -> (i64, i64, i64) {
    erp_orm::reflection::reflect_models(tx).await.unwrap();
    let model: i64 = tx
        .query("SELECT id FROM ir_model WHERE model='protection.doc'", &[])
        .await
        .unwrap()[0]
        .get(0);
    let generated: i64 = tx.query("INSERT INTO ir_access(name, model_id, active, operation, domain, generated) VALUES ('generated', $1, true, 'crud', '[]', true) RETURNING id", &[&model]).await.unwrap()[0].get(0);
    let ordinary: i64 = tx.query("INSERT INTO ir_access(name, model_id, active, operation, domain, generated) VALUES ('ordinary', $1, true, 'crud', '[]', false) RETURNING id", &[&model]).await.unwrap()[0].get(0);
    (model, generated, ordinary)
}
fn access(tx: &Transaction, ids: &[i64]) -> Records {
    tx.env()
        .records("ir.access")
        .unwrap()
        .browse(ids.iter().copied())
        .unwrap()
}
fn refused(error: erp_sdk::Error, message: &str) {
    assert_eq!(error.kind(), ErrorKind::UserError, "{error}");
    assert_eq!(error.message(), message);
}

#[tokio::test]
async fn generated_rows_refuse_mutation_including_mixed_batches() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let (_, generated, ordinary) = seed(&mut tx).await;
    let rows = access(&tx, &[ordinary, generated]);
    for (field, value) in [
        ("active", Value::Bool(false)),
        ("domain", Value::str("[(1, '=', 1)]")),
        ("operation", Value::str("r")),
        ("group_id", Value::False),
        ("name", Value::str("renamed")),
    ] {
        refused(
            rows.write_values(&mut tx, &map(&[(field, value)]))
                .await
                .unwrap_err(),
            ROW_ERROR,
        );
    }
    refused(rows.unlink(&mut tx).await.unwrap_err(), ROW_ERROR);
    let stored = tx
        .query(
            "SELECT name, active, operation FROM ir_access ORDER BY id",
            &[],
        )
        .await
        .unwrap();
    assert_eq!(stored.len(), 2);
    for (row, name) in stored.iter().zip(["generated", "ordinary"]) {
        assert_eq!(row.get::<_, String>(0), name);
        assert!(row.get::<_, bool>(1));
        assert_eq!(row.get::<_, String>(2), "crud");
    }
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn generated_inputs_are_rejected_even_when_false() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let (model, _, ordinary) = seed(&mut tx).await;
    for value in [Value::Bool(true), Value::Bool(false)] {
        let vals = map(&[
            ("name", Value::str("new")),
            ("model_id", Value::Int(model)),
            ("operation", Value::str("crud")),
        ]);
        let mut marked = vals.clone();
        marked.insert("generated".into(), value.clone());
        refused(
            access(&tx, &[])
                .create_values(&mut tx, &[vals, marked])
                .await
                .unwrap_err(),
            SET_ERROR,
        );
        refused(
            access(&tx, &[ordinary])
                .write_values(&mut tx, &map(&[("generated", value)]))
                .await
                .unwrap_err(),
            SET_ERROR,
        );
    }
    assert_eq!(
        tx.query("SELECT count(*) FROM ir_access", &[])
            .await
            .unwrap()[0]
            .get::<_, i64>(0),
        2
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn archived_generated_rows_are_protected_and_copies_are_ordinary() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let (_, generated, ordinary) = seed(&mut tx).await;
    let source = access(&tx, &[generated]);
    let copied = source.copy(&mut tx, None).await.unwrap();
    assert_eq!(
        copied.value(&mut tx, "generated").await.unwrap(),
        Value::Bool(false)
    );
    copied
        .write_values(&mut tx, &map(&[("name", Value::str("copy"))]))
        .await
        .unwrap();
    copied.unlink(&mut tx).await.unwrap();
    access(&tx, &[ordinary]).unlink(&mut tx).await.unwrap();
    tx.execute(
        "UPDATE ir_access SET active=false WHERE id=$1",
        &[&generated],
    )
    .await
    .unwrap();
    refused(source.unlink(&mut tx).await.unwrap_err(), ROW_ERROR);
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn generated_context_default_is_rejected() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let (model, _, _) = seed(&mut tx).await;
    let rows = access(&tx, &[]).with_context([("default_generated", Value::Bool(true))]);
    refused(
        rows.create_values(
            &mut tx,
            &[map(&[
                ("name", Value::str("bad default")),
                ("model_id", Value::Int(model)),
                ("operation", Value::str("crud")),
            ])],
        )
        .await
        .unwrap_err(),
        SET_ERROR,
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn elevation_does_not_bypass_generated_protection() {
    let h = harness().await;
    let mut setup = h.begin().await.unwrap();
    let (_, generated, _) = seed(&mut setup).await;
    setup
        .execute(
            "INSERT INTO res_users(id,name,login,active) VALUES (2,'User','protect-user',true)",
            &[],
        )
        .await
        .unwrap();
    setup.commit().await.unwrap();
    let mut tx = h.begin_as(2).await.unwrap();
    for rows in [
        access(&tx, &[generated]),
        access(&tx, &[generated]).sudo(true),
    ] {
        refused(
            rows.write_values(&mut tx, &map(&[("active", Value::Bool(false))]))
                .await
                .unwrap_err(),
            ROW_ERROR,
        );
        refused(rows.unlink(&mut tx).await.unwrap_err(), ROW_ERROR);
    }
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn data_file_cannot_create_generated_rows() {
    use erp_data::load::{FileContext, LoadErrorKind, LoadSession, NoFiles, SessionOptions};
    use erp_data::{ConvertMode, XmlDataReader};
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let (model, _, _) = seed(&mut tx).await;
    let xml = format!(
        "<odoo><record id=\"forged\" model=\"ir.access\"><field name=\"name\">forged</field><field name=\"model_id\" eval=\"{model}\"/><field name=\"operation\">crud</field><field name=\"generated\" eval=\"True\"/></record></odoo>"
    );
    let mut reader = XmlDataReader::new(&xml, "data/forged.xml", "access_protection", false);
    let mut session = LoadSession::new(SessionOptions::new("20.0", std::sync::Arc::new(NoFiles)));
    let error = session
        .load_file(
            &mut tx,
            &FileContext::new("access_protection", "data/forged.xml", ConvertMode::Init),
            &mut reader,
        )
        .await
        .unwrap_err();
    assert_eq!(error.orm_kind(), Some(ErrorKind::UserError));
    match error.into_kind() {
        LoadErrorKind::Orm(error) => refused(error, SET_ERROR),
        other => panic!("unexpected loader error: {other:?}"),
    }
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn generated_stored_default_is_rejected() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let (model, _, _) = seed(&mut tx).await;
    let env = tx.env();
    erp_orm::ir_default::set(
        &mut tx,
        &env,
        "ir.access",
        "generated",
        &Value::Bool(true),
        None,
        None,
        None,
    )
    .await
    .unwrap();
    refused(
        access(&tx, &[])
            .create_values(
                &mut tx,
                &[map(&[
                    ("name", Value::str("bad default")),
                    ("model_id", Value::Int(model)),
                    ("operation", Value::str("crud")),
                ])],
            )
            .await
            .unwrap_err(),
        SET_ERROR,
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn ordinary_and_empty_recordsets_remain_editable() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let (_, _, ordinary) = seed(&mut tx).await;
    tx.execute(
        "UPDATE ir_access SET generated=NULL WHERE id=$1",
        &[&ordinary],
    )
    .await
    .unwrap();
    let row = access(&tx, &[ordinary]);
    row.write_values(&mut tx, &map(&[("name", Value::str("legacy"))]))
        .await
        .unwrap();
    assert_eq!(
        row.value(&mut tx, "name").await.unwrap(),
        Value::str("legacy")
    );
    row.unlink(&mut tx).await.unwrap();
    let empty = access(&tx, &[]);
    empty
        .write_values(&mut tx, &map(&[("generated", Value::Bool(true))]))
        .await
        .unwrap();
    empty.unlink(&mut tx).await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn copy_cannot_override_generated() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let (_, generated, _) = seed(&mut tx).await;
    refused(
        access(&tx, &[generated])
            .copy(
                &mut tx,
                Some(
                    map(&[("generated", Value::Bool(true))])
                        .into_iter()
                        .collect(),
                ),
            )
            .await
            .unwrap_err(),
        SET_ERROR,
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn generated_rows_without_a_resolved_declaration_still_refuse() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let (model, generated, _) = seed(&mut tx).await;
    tx.execute(
        "UPDATE ir_model SET model='protection.missing' WHERE id=$1",
        &[&model],
    )
    .await
    .unwrap();
    refused(
        access(&tx, &[generated]).unlink(&mut tx).await.unwrap_err(),
        "The generated company restriction of protection.missing has no registered model; update its owning addon before modifying access rows.",
    );
    tx.execute(
        "UPDATE ir_model SET model='ir.access' WHERE id=$1",
        &[&model],
    )
    .await
    .unwrap();
    refused(
        access(&tx, &[generated]).unlink(&mut tx).await.unwrap_err(),
        "The generated company restriction of ir.access has no restricting company_access declaration; update its owning addon before modifying access rows.",
    );
    assert_eq!(
        tx.query("SELECT count(*) FROM ir_access WHERE generated", &[])
            .await
            .unwrap()[0]
            .get::<_, i64>(0),
        1
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn stored_generated_flag_overrides_a_stale_record_cache() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let (_, generated, ordinary) = seed(&mut tx).await;
    let row = access(&tx, &[ordinary]);
    assert_eq!(
        row.value(&mut tx, "generated").await.unwrap(),
        Value::Bool(false)
    );
    tx.execute("DELETE FROM ir_access WHERE id=$1", &[&generated])
        .await
        .unwrap();
    tx.execute(
        "UPDATE ir_access SET generated=true WHERE id=$1",
        &[&ordinary],
    )
    .await
    .unwrap();
    refused(
        row.write_values(&mut tx, &map(&[("active", Value::Bool(false))]))
            .await
            .unwrap_err(),
        ROW_ERROR,
    );
    refused(row.unlink(&mut tx).await.unwrap_err(), ROW_ERROR);
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn csv_generated_column_is_refused() {
    use erp_data::load::{FileContext, LoadErrorKind, LoadSession, NoFiles, SessionOptions};
    use erp_data::{ConvertMode, CsvImportResult, parse_csv};
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let (model, _, _) = seed(&mut tx).await;
    let csv =
        format!("id,name,model_id/.id,operation,generated\nforged,forged,{model},crud,True\n");
    let CsvImportResult::Loaded(import) = parse_csv(
        &csv,
        "data/ir.access.csv",
        "access_protection",
        ConvertMode::Init,
    )
    .unwrap() else {
        panic!("expected CSV import");
    };
    let mut ops = vec![Ok(import.into_operation(false))].into_iter();
    let mut session = LoadSession::new(SessionOptions::new("20.0", std::sync::Arc::new(NoFiles)));
    let error = session
        .load_file(
            &mut tx,
            &FileContext::new("access_protection", "data/ir.access.csv", ConvertMode::Init),
            &mut ops,
        )
        .await
        .unwrap_err();
    match error.into_kind() {
        LoadErrorKind::CsvImport {
            module,
            file,
            messages,
        } => {
            assert_eq!(module, "access_protection");
            assert_eq!(file, "data/ir.access.csv");
            assert_eq!(messages, [SET_ERROR]);
        }
        other => panic!("unexpected CSV error: {other:?}"),
    }
    assert_eq!(
        tx.query("SELECT count(*) FROM ir_access", &[])
            .await
            .unwrap()[0]
            .get::<_, i64>(0),
        2
    );
    drop(tx);
    h.cleanup().await.unwrap();
}
