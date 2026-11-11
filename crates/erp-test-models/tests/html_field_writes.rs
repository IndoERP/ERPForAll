//! Actual typed and dynamic host writes, diagnostics and rollback.
#![allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]
mod support;
use erp_sdk::{Error, ErrorKind, Vals, Value};
use erp_test_models::test_orm::Mixed;

#[tokio::test]
async fn typed_create_and_write_clean_before_storage_and_keep_reports() {
    let h = support::orm().await;
    let mut tx = h.begin().await.unwrap();
    let model = tx.env().model::<Mixed>().unwrap();
    let record = model
        .create(
            &mut tx,
            vec![Vals::new().set(Mixed::HTML, "<p onclick='x'>First</p>")],
        )
        .await
        .unwrap();
    assert_eq!(
        record.get(&mut tx, Mixed::HTML).await.unwrap().as_deref(),
        Some("<p>First</p>")
    );
    assert_eq!(tx.html_reports().len(), 1);
    assert_eq!(tx.html_reports()[0].model(), "test_orm.mixed");
    assert_eq!(tx.html_reports()[0].field(), "html");
    assert_eq!(
        tx.html_reports()[0].removals().entries()[0].name(),
        "onclick"
    );
    record
        .write(
            &mut tx,
            Vals::new().set(Mixed::HTML, "<p onmouseover='y'>Second</p>"),
        )
        .await
        .unwrap();
    tx.flush().await.unwrap();
    let client = h.db().client().await.unwrap();
    // The transaction itself owns the uncommitted value; a fresh connection reads after commit.
    let id = record.ids()[0];
    tx.commit().await.unwrap();
    assert!(tx.html_reports().is_empty());
    let row = client
        .query_one("SELECT html FROM test_orm_mixed WHERE id = $1", &[&id])
        .await
        .unwrap();
    assert_eq!(row.get::<_, String>(0), "<p>Second</p>");
    drop(client);
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn strict_write_refuses_and_cannot_commit_prior_changes() {
    let h = support::orm().await;
    let mut tx = h.begin().await.unwrap();
    let model = tx.env().model::<Mixed>().unwrap();
    let record = model
        .create(&mut tx, vec![Vals::new().set(Mixed::HTML, "<p>Kept</p>")])
        .await
        .unwrap();
    let id = record.ids()[0];
    tx.require_strict_html();
    let error = record
        .write(
            &mut tx,
            Vals::new().set(Mixed::HTML, "<p onclick='x'>Changed</p>"),
        )
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Validation);
    assert_eq!(
        error.message(),
        "test_orm.mixed.html: strict mode: the policy removes 1 item(s) in 1 group(s)"
    );
    let commit_error = tx.commit().await.unwrap_err();
    assert_eq!(commit_error.kind(), ErrorKind::Validation);
    assert_eq!(commit_error.message(), error.message());
    let client = h.db().client().await.unwrap();
    assert_eq!(
        client
            .query_one("SELECT count(*) FROM test_orm_mixed WHERE id = $1", &[&id])
            .await
            .unwrap()
            .get::<_, i64>(0),
        0
    );
    drop(client);
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn dynamic_writes_and_savepoint_reports_follow_rollback() {
    let h = support::orm().await;
    let mut tx = h.begin().await.unwrap();
    let records = tx.env().records("test_orm.mixed").unwrap();
    let values = std::collections::BTreeMap::from([(
        String::from("html"),
        Value::from("<p onclick='x'>Original</p>"),
    )]);
    let record = records.create_values(&mut tx, &[values]).await.unwrap();
    assert_eq!(tx.html_reports().len(), 1);
    let error = tx
        .savepoint(async |tx| {
            record
                .set_value(tx, "html", Value::from("<p onclick='y'>Discarded</p>"))
                .await?;
            assert_eq!(tx.html_reports().len(), 2);
            Err::<(), _>(Error::validation("intentional HTML savepoint rollback"))
        })
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Validation);
    assert_eq!(error.message(), "intentional HTML savepoint rollback");
    assert_eq!(tx.html_reports().len(), 1);
    assert_eq!(
        record.value(&mut tx, "html").await.unwrap(),
        Value::from("<p>Original</p>")
    );
    tx.commit().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

mod html_fixture {
    use erp_sdk::prelude::*;
    erp_sdk::addon!(name = "html_fixture", depends = ["base"]);
    #[model(name = "html.fixture", addon = "html_fixture")]
    pub struct Document {
        #[field(html, translate = true)]
        pub body: Option<Arc<str>>,
        #[field(html, sanitize = false)]
        pub raw: Option<Arc<str>>,
        #[field(html, translate = true, required = true)]
        pub title: Option<Arc<str>>,
    }
    #[model(name = "html.computed", addon = "html_fixture",
        keys = [key(name = "_code_key", fields = ["code"], message = "Code must be unique.")])]
    pub struct Computed {
        #[field(char, required)]
        pub code: Option<Arc<str>>,
        #[field(html)]
        pub body: Option<Arc<str>>,
        #[field(html, compute = "_body", store = true)]
        pub generated_body: Option<Arc<str>>,
        /// An editable related field with a weaker policy than its target.
        #[field(html, related = "body", readonly = false, sanitize = false)]
        pub body_alias: Option<Arc<str>>,
    }
    #[methods(Computed, addon = "html_fixture")]
    impl Computed {
        #[compute(depends = ["code"])]
        fn _body(batch: &mut Batch<'_, Computed>) -> Result<()> {
            for i in batch.indices() {
                batch.set(
                    i,
                    Computed::GENERATED_BODY,
                    "<p onclick='secret'>Computed</p>",
                )?;
            }
            Ok(())
        }
    }
}
async fn html_harness() -> erp_testkit::Harness {
    erp_testkit::Harness::builder([
        &erp_test_models::base_users::ADDON,
        &erp_addon_base::ADDON,
        &html_fixture::ADDON,
    ])
    .with_static_decls(support::currency_fixture::decls())
    .build()
    .await
    .unwrap()
}

#[tokio::test]
async fn translated_writes_report_language_and_raw_stays_untrusted_text() {
    use html_fixture::Document;
    let h = html_harness().await;
    let mut tx = h.begin().await.unwrap();
    let model = tx.env().model::<Document>().unwrap();
    let record = model
        .create(
            &mut tx,
            vec![
                Vals::new()
                    .set(Document::TITLE, "<p>Title</p>")
                    .set(Document::BODY, "<p onclick='x'>Body</p>")
                    .set(Document::RAW, "<p onclick='x'>Raw</p>"),
            ],
        )
        .await
        .unwrap();
    assert_eq!(tx.html_reports().len(), 1);
    assert_eq!(tx.html_reports()[0].language(), Some("en_US"));
    assert_eq!(
        record.get(&mut tx, Document::RAW).await.unwrap().as_deref(),
        Some("<p onclick='x'>Raw</p>")
    );
    let records = tx
        .env()
        .records("html.fixture")
        .unwrap()
        .browse(record.ids())
        .unwrap();
    records
        .update_field_translations(
            &mut tx,
            "body",
            [("en_US", Value::from("<p onmouseover='x'>Updated</p>"))],
        )
        .await
        .unwrap();
    assert_eq!(tx.html_reports().len(), 2);
    tx.flush().await.unwrap();
    let row = tx
        .query(
            "SELECT body, raw FROM html_fixture WHERE id = $1",
            &[&record.ids()[0]],
        )
        .await
        .unwrap()
        .remove(0);
    assert_eq!(
        row.get::<_, serde_json::Value>(0),
        serde_json::json!({"en_US":"<p>Updated</p>"})
    );
    assert_eq!(row.get::<_, String>(1), "<p onclick='x'>Raw</p>");
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn translated_html_errors_do_not_echo_content() {
    use erp_sdk::{Cell, RawVals};
    let h = html_harness().await;
    let mut tx = h.begin().await.unwrap();
    let model = tx.env().records("html.fixture").unwrap();
    let mut vals = RawVals::new();
    vals.insert("title".into(), Cell::Str("Title".into()));
    vals.insert(
        "body".into(),
        Cell::Json(std::sync::Arc::new(
            serde_json::json!({"invalid_language":"<script>secret-body</script>"}),
        )),
    );
    let error = model.create(&mut tx, vec![vals]).await.unwrap_err();
    assert!(
        !error.message().contains("secret-body"),
        "error disclosed body: {}",
        error.message()
    );
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn sanitized_required_translation_cannot_become_empty() {
    use html_fixture::Document;
    let h = html_harness().await;
    let mut tx = h.begin().await.unwrap();
    let model = tx.env().model::<Document>().unwrap();
    let error = model
        .create(
            &mut tx,
            vec![Vals::new().set(Document::TITLE, "<script>secret-body</script>")],
        )
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Validation);
    assert!(!error.message().contains("secret-body"));
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn saved_html_default_is_cleaned_and_reported() {
    let h = html_harness().await;
    let mut tx = h.begin().await.unwrap();
    let env = tx.env();
    erp_orm::ir_default::set(
        &mut tx,
        &env,
        "html.fixture",
        "body",
        &Value::from("<p onclick='secret'>Default</p>"),
        None,
        None,
        None,
    )
    .await
    .unwrap();
    assert_eq!(tx.html_reports().len(), 1);
    let row = tx
        .query("SELECT json_value FROM ir_default", &[])
        .await
        .unwrap()
        .remove(0);
    let json_text: String = row.get(0);
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&json_text).unwrap(),
        serde_json::json!("<p>Default</p>")
    );
    let records = env.records("html.fixture").unwrap();
    let defaults = records.default_get(&mut tx, &["body"]).await.unwrap();
    assert_eq!(defaults.get("body"), Some(&Value::from("<p>Default</p>")));
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn compute_and_virtual_cache_admit_html_with_reports() {
    use html_fixture::Computed;
    let h = html_harness().await;
    let mut tx = h.begin().await.unwrap();
    let model = tx.env().model::<Computed>().unwrap();
    let record = model
        .create(&mut tx, vec![Vals::new().set(Computed::CODE, "one")])
        .await
        .unwrap();
    assert_eq!(
        record
            .get(&mut tx, Computed::GENERATED_BODY)
            .await
            .unwrap()
            .as_deref(),
        Some("<p>Computed</p>")
    );
    assert!(
        tx.html_reports()
            .iter()
            .any(|r| r.field() == "generated_body")
    );
    let records = tx.env().records("html.computed").unwrap();
    let vals = records
        .vals_from_values(&std::collections::BTreeMap::from([
            ("code".to_owned(), Value::from("virtual")),
            ("body".to_owned(), Value::from("<p onclick='x'>Draft</p>")),
        ]))
        .unwrap();
    let virtual_record = tx.new_record(&records, vals, None, None).await.unwrap();
    assert_eq!(
        virtual_record.value(&mut tx, "body").await.unwrap(),
        Value::from("<p>Draft</p>")
    );
    assert!(tx.html_reports().iter().any(|r| r.field() == "body"));
    tx.rollback().await.unwrap();
    assert!(tx.html_reports().is_empty());
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn upsert_insert_and_update_use_reporting_admission() {
    use erp_sdk::Upsert;
    use html_fixture::Computed;
    let h = html_harness().await;
    let mut tx = h.begin().await.unwrap();
    let model = tx.env().model::<Computed>().unwrap();
    for body in ["<p onclick='x'>First</p>", "<p onclick='x'>Second</p>"] {
        model
            .upsert(
                &mut tx,
                Upsert::on(Computed::CODE_KEY)
                    .update([Computed::BODY])
                    .rows(vec![
                        Vals::new()
                            .set(Computed::CODE, "same")
                            .set(Computed::BODY, body),
                    ]),
            )
            .await
            .unwrap();
    }
    let rows = tx
        .query("SELECT body FROM html_computed WHERE code = 'same'", &[])
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].get::<_, String>(0), "<p>Second</p>");
    assert_eq!(
        tx.html_reports()
            .iter()
            .filter(|r| r.field() == "body")
            .count(),
        2
    );
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn onchange_reports_do_not_outlive_the_onchange() {
    use html_fixture::Computed;
    let h = html_harness().await;
    let mut tx = h.begin().await.unwrap();
    // one stored write before the onchange: its report must survive the onchange
    tx.env()
        .model::<Computed>()
        .unwrap()
        .create(
            &mut tx,
            vec![
                Vals::new()
                    .set(Computed::CODE, "stored")
                    .set(Computed::BODY, "<p onclick='x'>Stored</p>"),
            ],
        )
        .await
        .unwrap();
    let before = tx.html_reports().len();
    assert!(before >= 1);
    let records = tx.env().records("html.computed").unwrap();
    let values: erp_sdk::ValueMap = [
        ("code".to_owned(), Value::from("draft")),
        ("body".to_owned(), Value::from("<p onclick='x'>Draft</p>")),
    ]
    .into_iter()
    .collect();
    let spec = erp_orm::Spec::new().field("code").field("body");
    let result = records
        .onchange(&mut tx, values, vec!["body".to_owned()], spec)
        .await
        .unwrap();
    assert_eq!(
        tx.html_reports().len(),
        before,
        "reports of a draft outlived the onchange"
    );
    // the onchange did admit the draft: the value it returns is the cleaned one
    let Value::Map(result) = result else {
        panic!("onchange result is not a map: {result:?}")
    };
    if let Some(Value::Map(value)) = result.get("value")
        && let Some(body) = value.get("body")
    {
        assert_eq!(body, &Value::from("<p>Draft</p>"));
    }
    // taking reports inside a savepoint is refused: the savepoint may still roll back
    let error = tx
        .savepoint(async |tx| tx.take_html_reports().map(|_| ()))
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Programming);
    assert_eq!(tx.html_reports().len(), before);
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

async fn stored_body(tx: &mut erp_orm::Transaction, id: i64) -> String {
    tx.query("SELECT body FROM html_computed WHERE id = $1", &[&id])
        .await
        .unwrap()
        .remove(0)
        .get::<_, String>(0)
}

#[tokio::test]
async fn related_html_write_keeps_the_target_policy() {
    use html_fixture::Computed;
    let h = html_harness().await;
    let mut tx = h.begin().await.unwrap();
    let model = tx.env().model::<Computed>().unwrap();
    let record = model
        .create(
            &mut tx,
            vec![
                Vals::new()
                    .set(Computed::CODE, "alias")
                    .set(Computed::BODY_ALIAS, "<p onclick='x'>Created</p>"),
            ],
        )
        .await
        .unwrap();
    tx.flush().await.unwrap();
    let id = record.ids()[0];
    assert_eq!(stored_body(&mut tx, id).await, "<p>Created</p>");
    record
        .write(
            &mut tx,
            Vals::new().set(
                Computed::BODY_ALIAS,
                "<p><img src='x' onerror='y'>Written</p>",
            ),
        )
        .await
        .unwrap();
    tx.flush().await.unwrap();
    let body = stored_body(&mut tx, id).await;
    assert!(!body.contains("onerror"), "{body}");
    assert!(body.contains("Written"), "{body}");
    assert!(tx.html_reports().iter().any(|r| r.field() == "body"));
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn context_default_html_is_cleaned_before_the_cache() {
    let h = html_harness().await;
    let mut tx = h.begin().await.unwrap();
    let records = tx
        .env()
        .records("html.computed")
        .unwrap()
        .with_context([("default_body", Value::from("<p onclick='x'>Context</p>"))]);
    let draft = tx
        .new_record(&records, erp_sdk::RawVals::new(), None, None)
        .await
        .unwrap();
    assert_eq!(
        draft.value(&mut tx, "body").await.unwrap(),
        Value::from("<p>Context</p>")
    );
    assert!(tx.html_reports().iter().any(|r| r.field() == "body"));
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}
