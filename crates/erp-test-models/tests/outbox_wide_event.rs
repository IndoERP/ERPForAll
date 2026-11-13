// SPDX-License-Identifier: MIT OR Apache-2.0
//! the registry's 128-name ceiling agrees with what the producer emits.
#![allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]

use erp_fields::Cell;
use erp_orm::{BeginOptions, OperationIdentity, RawVals, SUPERUSER_ID};
use erp_registry::{AddonDescriptor, FieldDecl, MAX_EVENT_FIELD_NAMES, ModelDecl};
use erp_test_models::base_users;
use erp_testkit::Harness;

static ADDON: AddonDescriptor = AddonDescriptor {
    name: "test_wide_event",
    depends: &["base"],
    exact_only: false,
};
const MODEL: &str = "test.wide.event";
/// Declared integer fields; the registry adds the log-access columns (checked below against the
/// built spec, so a change of that set fails the test rather than going unnoticed).
const DECLARED: usize = 124;

fn declared_fields() -> &'static [FieldDecl] {
    let fields: Vec<FieldDecl> = (0..DECLARED)
        .map(|i| FieldDecl::integer(Box::leak(format!("f{i}").into_boxed_str())))
        .collect();
    Box::leak(fields.into_boxed_slice())
}

#[tokio::test]
async fn all_fields_create_of_a_128_column_model_writes_one_event_with_128_names() {
    let h = Harness::builder([&base_users::ADDON, &ADDON])
        .with_decls([ModelDecl::new("test_wide_event", MODEL)
            .eventful(true)
            .fields(declared_fields())])
        .build()
        .await
        .unwrap();
    let countable: std::collections::BTreeSet<String> = h
        .registry()
        .model(MODEL)
        .unwrap()
        .fields
        .iter()
        .filter(|f| f.has_column() && f.inherited.is_none() && f.name != "id")
        .map(|f| f.name.to_owned())
        .collect();
    assert_eq!(countable.len(), MAX_EVENT_FIELD_NAMES);
    let mut tx = h.begin().await.unwrap();
    tx.install_event_schema().await.unwrap();
    tx.commit().await.unwrap();

    let mut tx = h
        .orm()
        .begin_opts(
            BeginOptions::new(SUPERUSER_ID)
                .operation_identity(OperationIdentity::generate().unwrap()),
        )
        .await
        .unwrap();
    let mut vals = RawVals::new();
    for i in 0..DECLARED {
        vals = vals.set_cell(format!("f{i}"), Cell::Int(i32::try_from(i).unwrap()));
    }
    tx.env()
        .records(MODEL)
        .unwrap()
        .create(&mut tx, vec![vals])
        .await
        .unwrap();
    tx.commit().await.unwrap();

    let mut check = h.begin().await.unwrap();
    let rows = check
        .query(
            "SELECT field_names FROM erp_event.ir_event_outbox WHERE model = $1",
            &[&MODEL],
        )
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    let names: Vec<String> = rows[0].get(0);
    assert_eq!(names.len(), MAX_EVENT_FIELD_NAMES);
    // the producer's set is the registry's set, not only the same size
    assert_eq!(
        names.into_iter().collect::<std::collections::BTreeSet<_>>(),
        countable
    );
    check.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}
