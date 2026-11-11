//! CORE prerequisite probe: native method-backed selections must expose choices and preserve callable conversion semantics.
//! This generic fixture does not claim any whole PARTNER acceptance case.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

use std::collections::BTreeMap;

use erp_sdk::prelude::*;
use erp_test_models::base_users;
use erp_testkit::Harness;

static HOOK_EFFECTS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

mod choices {
    use erp_sdk::prelude::*;

    erp_sdk::addon!(name = "test_partner_selection_probe", depends = ["base"]);

    #[model(
        name = "test_partner_selection_probe.choice",
        addon = "test_partner_selection_probe"
    )]
    pub struct Choice {
        #[field(selection, selection = [("static", "Static")])]
        pub fixed: Option<Arc<str>>,
        #[field(selection, selection_method = "_writing_function")]
        pub writing_function: Option<Arc<str>>,
        #[field(selection, selection_method = "_commit_hook")]
        pub commit_hook: Option<Arc<str>>,
        #[field(selection, selection_method = "_choices")]
        pub language: Option<Arc<str>>,
        #[field(selection, selection_method = "_mutating")]
        pub mutating: Option<Arc<str>>,
        #[field(selection, selection_method = "_duplicate_keys")]
        pub duplicate: Option<Arc<str>>,
        #[field(selection, selection_method = "_oversized")]
        pub oversized: Option<Arc<str>>,
        #[field(selection, selection_method = "_recursive_a")]
        pub loop_a: Option<Arc<str>>,
        #[field(selection, selection_method = "_recursive_b")]
        pub loop_b: Option<Arc<str>>,
    }

    #[methods(Choice, addon = "test_partner_selection_probe")]
    impl Choice {
        #[method]
        async fn _writing_function(
            _this: &RecordSet<Choice>,
            tx: &mut Transaction,
        ) -> Result<Value> {
            tx.query("SELECT probe_selection_write()", &[]).await?;
            Ok(Value::list([]))
        }

        #[method]
        async fn _commit_hook(_this: &RecordSet<Choice>, tx: &mut Transaction) -> Result<Value> {
            tx.after_commit(|| {
                super::HOOK_EFFECTS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            });
            // An ordinary signature must be refused before this callback is registered.
            Ok(Value::str("malformed"))
        }
        #[selection]
        async fn _choices(
            _this: &RecordSet<Choice>,
            ctx: &mut erp_sdk::SelectionContext<'_>,
        ) -> Result<()> {
            ctx.emit("en_US", "English")
        }

        #[method]
        async fn _mutating(_this: &RecordSet<Choice>, tx: &mut Transaction) -> Result<Value> {
            tx.execute("CREATE TEMPORARY TABLE probe_test_mutation (x int)", &[])
                .await?;
            Ok(Value::list([Value::list([
                Value::str("x"),
                Value::str("X"),
            ])]))
        }

        #[selection]
        async fn _duplicate_keys(
            _this: &RecordSet<Choice>,
            ctx: &mut erp_sdk::SelectionContext<'_>,
        ) -> Result<()> {
            ctx.emit("dup", "First")?;
            ctx.emit("dup", "Second")
        }

        #[selection]
        async fn _oversized(
            _this: &RecordSet<Choice>,
            ctx: &mut erp_sdk::SelectionContext<'_>,
        ) -> Result<()> {
            for i in 0..4097 {
                ctx.emit(&format!("k_{i}"), &format!("Label {i}"))?;
            }
            Ok(())
        }

        #[selection]
        async fn _recursive_a(
            _this: &RecordSet<Choice>,
            ctx: &mut erp_sdk::SelectionContext<'_>,
        ) -> Result<()> {
            ctx.resolve("test_partner_selection_probe.choice", "loop_b")
                .await
        }

        #[selection]
        async fn _recursive_b(
            _this: &RecordSet<Choice>,
            ctx: &mut erp_sdk::SelectionContext<'_>,
        ) -> Result<()> {
            ctx.resolve("test_partner_selection_probe.choice", "loop_a")
                .await
        }
    }
}

#[tokio::test]
async fn method_selection_metadata_resolves_specialized_provider() {
    let h = Harness::builder([&base_users::ADDON, &choices::ADDON])
        .build()
        .await
        .unwrap();
    let mut tx = h.begin().await.unwrap();
    let records = tx
        .env()
        .records("test_partner_selection_probe.choice")
        .unwrap();
    let actual = records
        .fields_get(&mut tx, Some(&["language"]), Some(&["selection"]))
        .await
        .unwrap()["language"]["selection"]
        .clone();
    drop(tx);
    h.cleanup().await.unwrap();
    assert_eq!(
        actual,
        Value::list([Value::list([Value::str("en_US"), Value::str("English")])]),
        "fields_get must execute the specialized selection provider"
    );
}

#[tokio::test]
async fn method_selection_preserves_callable_selection_conversion() {
    let h = Harness::builder([&base_users::ADDON, &choices::ADDON])
        .build()
        .await
        .unwrap();
    let mut tx = h.begin().await.unwrap();
    let records = tx
        .env()
        .records("test_partner_selection_probe.choice")
        .unwrap();
    let provider = records
        .fields_get(&mut tx, Some(&["language"]), Some(&["selection"]))
        .await
        .unwrap()["language"]["selection"]
        .clone();
    assert_eq!(
        provider,
        Value::list([Value::list([Value::str("en_US"), Value::str("English")])])
    );
    let created = records
        .create_values(
            &mut tx,
            &[BTreeMap::from([(
                "language".to_owned(),
                Value::str("not_installed"),
            )])],
        )
        .await
        .unwrap();
    let stored = created.value(&mut tx, "language").await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
    assert_eq!(stored, Value::str("not_installed"));
}

#[tokio::test]
async fn method_selection_filtered_out_by_attributes_does_not_dispatch() {
    let h = Harness::builder([&base_users::ADDON, &choices::ADDON])
        .build()
        .await
        .unwrap();
    let mut tx = h.begin().await.unwrap();
    let records = tx
        .env()
        .records("test_partner_selection_probe.choice")
        .unwrap();
    // mutating provider would fail if dispatched, but since selection is not requested, it succeeds
    let res = records
        .fields_get(&mut tx, Some(&["mutating"]), Some(&["string", "type"]))
        .await
        .unwrap();
    assert_eq!(res["mutating"]["type"], Value::str("selection"));
    assert!(!res["mutating"].contains_key("selection"));
    drop(tx);
    h.cleanup().await.unwrap();
}

// The accepted capability API removed the synchronous describe facade. Retain the
// relevant static-metadata contract through the current async entry: static choices
// need no provider dispatch or database statement.
#[tokio::test]
async fn static_selection_describe_needs_no_query() {
    let h = Harness::builder([&base_users::ADDON, &choices::ADDON])
        .build()
        .await
        .unwrap();
    let mut tx = h.begin().await.unwrap();
    let records = tx
        .env()
        .records("test_partner_selection_probe.choice")
        .unwrap();
    let before = tx.query_count();
    let metadata = records
        .fields_get(&mut tx, Some(&["fixed"]), Some(&["selection"]))
        .await
        .unwrap();
    assert_eq!(
        metadata["fixed"]["selection"],
        Value::list([Value::list([Value::str("static"), Value::str("Static")])])
    );
    assert_eq!(tx.query_count(), before);
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn method_selection_refuses_mutating_provider() {
    let h = Harness::builder([&base_users::ADDON, &choices::ADDON])
        .build()
        .await
        .unwrap();
    let mut tx = h.begin().await.unwrap();
    let records = tx
        .env()
        .records("test_partner_selection_probe.choice")
        .unwrap();
    let err = records
        .fields_get(&mut tx, Some(&["mutating"]), Some(&["selection"]))
        .await
        .unwrap_err();
    assert_eq!(err.kind(), erp_sdk::ErrorKind::Programming);
    assert_eq!(
        err.message(),
        "selection 'test_partner_selection_probe.choice.mutating' provider '_mutating': provider requires the specialized SelectionContext signature"
    );
    let rows = tx
        .query(
            "SELECT to_regclass('pg_temp.probe_test_mutation') IS NULL",
            &[],
        )
        .await
        .unwrap();
    assert!(
        rows[0].get::<_, bool>(0),
        "ordinary mutation provider must not execute"
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn method_selection_refuses_duplicate_keys() {
    let h = Harness::builder([&base_users::ADDON, &choices::ADDON])
        .build()
        .await
        .unwrap();
    let mut tx = h.begin().await.unwrap();
    let records = tx
        .env()
        .records("test_partner_selection_probe.choice")
        .unwrap();
    let err = records
        .fields_get(&mut tx, Some(&["duplicate"]), Some(&["selection"]))
        .await
        .unwrap_err();
    assert_eq!(err.kind(), erp_sdk::ErrorKind::Programming);
    assert_eq!(
        err.message(),
        "selection 'test_partner_selection_probe.choice.duplicate' provider '_duplicate_keys': duplicate key 'dup'"
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn method_selection_refuses_oversized_pairs() {
    let h = Harness::builder([&base_users::ADDON, &choices::ADDON])
        .build()
        .await
        .unwrap();
    let mut tx = h.begin().await.unwrap();
    let records = tx
        .env()
        .records("test_partner_selection_probe.choice")
        .unwrap();
    let err = records
        .fields_get(&mut tx, Some(&["oversized"]), Some(&["selection"]))
        .await
        .unwrap_err();
    assert_eq!(err.kind(), erp_sdk::ErrorKind::ResourceExhausted);
    assert_eq!(
        err.message(),
        "selection 'test_partner_selection_probe.choice.oversized' provider '_oversized': pair limit exceeded"
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn method_selection_refuses_cycle_or_recursion() {
    let h = Harness::builder([&base_users::ADDON, &choices::ADDON])
        .build()
        .await
        .unwrap();
    let mut tx = h.begin().await.unwrap();
    let records = tx
        .env()
        .records("test_partner_selection_probe.choice")
        .unwrap();
    let err = records
        .fields_get(&mut tx, Some(&["loop_a"]), Some(&["selection"]))
        .await
        .unwrap_err();
    assert_eq!(err.kind(), erp_sdk::ErrorKind::Programming);
    assert_eq!(
        err.message(),
        "selection 'test_partner_selection_probe.choice.loop_a' provider '_recursive_a': recursive provider resolution"
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn selection_writing_function_cannot_persist_effects() {
    let h = Harness::builder([&base_users::ADDON, &choices::ADDON])
        .build()
        .await
        .unwrap();
    let mut tx = h.begin().await.unwrap();
    tx.execute("CREATE TABLE probe_selection_effect (x integer)", &[])
        .await
        .unwrap();
    tx.execute("CREATE FUNCTION probe_selection_write() RETURNS integer LANGUAGE SQL VOLATILE AS 'INSERT INTO probe_selection_effect VALUES (1) RETURNING x'", &[]).await.unwrap();
    let records = tx
        .env()
        .records("test_partner_selection_probe.choice")
        .unwrap();
    let result = records
        .fields_get(&mut tx, Some(&["writing_function"]), Some(&["selection"]))
        .await;
    let rows = tx
        .query("SELECT count(*) FROM probe_selection_effect", &[])
        .await
        .unwrap();
    assert_eq!(
        rows[0].get::<_, i64>(0),
        0,
        "a metadata provider must leave no database changes"
    );
    let err = result.unwrap_err();
    assert_eq!(err.kind(), erp_sdk::ErrorKind::Programming);
    assert_eq!(
        err.message(),
        "selection 'test_partner_selection_probe.choice.writing_function' provider '_writing_function': provider requires the specialized SelectionContext signature"
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn caught_selection_refusal_cannot_register_commit_hooks() {
    let h = Harness::builder([&base_users::ADDON, &choices::ADDON])
        .build()
        .await
        .unwrap();
    let mut tx = h.begin().await.unwrap();
    let records = tx
        .env()
        .records("test_partner_selection_probe.choice")
        .unwrap();
    let before = HOOK_EFFECTS.load(std::sync::atomic::Ordering::SeqCst);
    let err = records
        .fields_get(&mut tx, Some(&["commit_hook"]), Some(&["selection"]))
        .await
        .unwrap_err();
    assert_eq!(err.kind(), erp_sdk::ErrorKind::Programming);
    assert_eq!(
        err.message(),
        "selection 'test_partner_selection_probe.choice.commit_hook' provider '_commit_hook': provider requires the specialized SelectionContext signature"
    );
    tx.commit().await.unwrap();
    assert_eq!(
        HOOK_EFFECTS.load(std::sync::atomic::Ordering::SeqCst),
        before,
        "provider-created hooks must not escape a caught metadata refusal"
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

// Integration regression for the base provider after the SelectionContext API merge.
// This checks the catalog projection, not a whole partner acceptance case.
#[tokio::test]
async fn partner_language_metadata_projects_active_catalog_in_name_order() {
    let h = Harness::builder([&base_users::ADDON, &erp_addon_base::ADDON])
        .build()
        .await
        .unwrap();
    let mut tx = h.begin().await.unwrap();
    let languages = tx.env().records("res.lang").unwrap();
    for (code, name, active) in [
        ("zz_ZZ", "Zulu", true),
        ("aa_AA", "Alpha", true),
        ("hh_HH", "Hidden", false),
    ] {
        languages
            .create(
                &mut tx,
                vec![
                    RawVals::new()
                        .set_cell("code", Cell::Str(code.into()))
                        .set_cell("name", Cell::Str(name.into()))
                        .set_cell("url_code", Cell::Str(code.into()))
                        .set_cell("active", Cell::Bool(active)),
                ],
            )
            .await
            .unwrap();
    }
    let partners = tx.env().records("res.partner").unwrap();
    let metadata = partners
        .fields_get(&mut tx, Some(&["lang"]), Some(&["selection"]))
        .await
        .unwrap();
    assert_eq!(
        metadata["lang"]["selection"],
        Value::list([
            Value::list([Value::str("aa_AA"), Value::str("Alpha")]),
            Value::list([Value::str("zz_ZZ"), Value::str("Zulu")]),
        ])
    );
    drop(tx);
    h.cleanup().await.unwrap();
}
