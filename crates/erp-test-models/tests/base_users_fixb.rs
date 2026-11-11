//! review fixes in `addons/base`: the regular-user
//! marker is required outside the base installation (F8), role values of the onchange (F8), a group
//! without a row is an error (F4), the user type groups are browsed in the caller's environment (S3).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

mod users_support;

use erp_orm::RawVals;
use erp_sdk::serde_json::json;
use erp_sdk::{Error, ErrorKind, Records, Transaction, Value};
use erp_testkit::base_install::set_base_installing;
use users_support::*;

const NO_REGULAR_MARKER: &str = "the group 'base.group_user_regular' does not exist in this \
    database; the rules of res.users and res.groups need it unless the base module is installing \
    (the data loader sets group_graph::begin_base_install)";

async fn create_group_raw(tx: &mut Transaction, name: &str) -> Result<Records, Error> {
    tx.env()
        .records("res.groups")
        .unwrap()
        .create_values(tx, &[map(&[("name", s(name))])])
        .await
}

#[tokio::test]
async fn a_missing_regular_marker_is_an_error_outside_the_base_installation() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let err = create_group_raw(&mut tx, "g").await.unwrap_err();
    assert_error(&err, ErrorKind::Programming, NO_REGULAR_MARKER);
    drop(tx);

    let mut tx = h.begin().await.unwrap();
    set_base_installing(&mut tx, true).unwrap();
    create_group_raw(&mut tx, "g").await.unwrap();
    drop(tx);

    // with the marker present nothing is missing
    let mut tx = h.begin().await.unwrap();
    named_group(&mut tx, "base.group_user_regular", "Is regular user").await;
    create_group_raw(&mut tx, "g").await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

fn unsaved_message(error: &Error) {
    assert_eq!(error.kind(), ErrorKind::Programming, "{error}");
    let message = error.to_string();
    assert!(
        message.starts_with("res.groups record <NewId 0x")
            && message
                .ends_with("> has no database row (a new record without an origin); save it first"),
        "{message}"
    );
}

/// `_is_light_groups`: an empty set answers true (nothing in it is heavy, Python's `all([])`); a
/// record that is not saved has no row, which is an error and not a dropped id (the answer would
/// be about the rest of the set). `_has_cycle` and `_reduce_to_light_groups` refuse it the same way.
#[tokio::test]
async fn groups_without_a_row_are_an_error_and_the_empty_set_is_light() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let t = user_types_and_admin(&mut tx).await;
    let heavy = group(&mut tx, "Heavy", &[t.internal]).await;

    let none = records(&tx, "res.groups", &[]);
    assert!(
        none.call::<(), bool>(&mut tx, "_is_light_groups", ())
            .await
            .unwrap()
    );
    let light = records(&tx, "res.groups", &[t.internal]);
    assert!(
        light
            .call::<(), bool>(&mut tx, "_is_light_groups", ())
            .await
            .unwrap()
    );
    let mixed = records(&tx, "res.groups", &[t.internal, heavy]);
    assert!(
        !mixed
            .call::<(), bool>(&mut tx, "_is_light_groups", ())
            .await
            .unwrap()
    );

    let groups = tx.env().records("res.groups").unwrap();
    let fresh = tx
        .new_record(&groups, RawVals::new(), None, None)
        .await
        .unwrap();
    let err = fresh
        .call::<(), bool>(&mut tx, "_is_light_groups", ())
        .await
        .unwrap_err();
    unsaved_message(&err);
    let err = fresh
        .call::<(String,), bool>(&mut tx, "_has_cycle", ("implied_ids".to_owned(),))
        .await
        .unwrap_err();
    unsaved_message(&err);
    let err = fresh
        .call::<(), Records>(&mut tx, "_reduce_to_light_groups", ())
        .await
        .unwrap_err();
    unsaved_message(&err);
    // a new record standing for a saved group (its origin) is that group
    let shadow = tx
        .new_record(
            &groups,
            RawVals::new(),
            Some(erp_sdk::RecId::from_real(heavy).unwrap()),
            None,
        )
        .await
        .unwrap();
    assert!(
        !shadow
            .call::<(), bool>(&mut tx, "_is_light_groups", ())
            .await
            .unwrap()
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

/// The user type groups come back in the environment the method was called on, not as a
/// superuser recordset.
#[tokio::test]
async fn user_type_groups_are_browsed_in_the_callers_environment() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let t = user_types_and_admin(&mut tx).await;
    let me = user(&mut tx, "me", &[t.internal]).await;
    let groups = tx.env().records("res.groups").unwrap().with_user(me);
    assert!(!groups.env().su());
    let types: Records = groups
        .call::<(), Records>(&mut tx, "_get_user_type_groups", ())
        .await
        .unwrap();
    assert_eq!(types.ids(), [t.internal, t.portal, t.public]);
    assert!(!types.env().su(), "the answer is not a superuser recordset");
    assert_eq!(types.env().uid(), me);
    // as superuser the answer stays elevated: the environment is the caller's
    let su = tx.env().sudo(true).records("res.groups").unwrap();
    let types = su
        .call::<(), Records>(&mut tx, "_get_user_type_groups", ())
        .await
        .unwrap();
    assert!(types.env().su());
    drop(tx);
    h.cleanup().await.unwrap();
}

fn onchange_spec() -> erp_orm::Spec {
    let value = to_value(json!({"group_ids": {}, "role": {}}));
    erp_orm::Spec::from_value(&value).unwrap()
}

fn to_value(json: erp_sdk::serde_json::Value) -> Value {
    use erp_sdk::serde_json::Value as J;
    match json {
        J::Null => Value::False,
        J::Bool(b) => Value::Bool(b),
        J::Number(n) => Value::Int(n.as_i64().unwrap()),
        J::String(t) => Value::str(t),
        J::Array(items) => Value::list(items.into_iter().map(to_value)),
        J::Object(m) => Value::Map(std::sync::Arc::new(
            m.into_iter().map(|(k, x)| (k, to_value(x))).collect(),
        )),
    }
}

async fn role_onchange(
    tx: &mut Transaction,
    caller: i64,
    role: Value,
    groups: &[i64],
) -> Result<Value, Error> {
    let users = tx.env().records("res.users").unwrap().with_user(caller);
    let mut form = erp_sdk::ValueMap::new();
    form.insert("name".to_owned(), s("form"));
    form.insert("login".to_owned(), s("form"));
    form.insert("role".to_owned(), role);
    form.insert("group_ids".to_owned(), set_cmd(groups));
    users
        .onchange(tx, form, vec!["role".to_owned()], onchange_spec())
        .await
}

/// An empty role changes nothing; any
/// other value is a mistake of the caller and an error, not a no-op. The
/// onchange also runs for a plain internal user, with no elevation.
#[tokio::test]
async fn role_onchange_refuses_values_that_are_not_roles() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let t = user_types_and_admin(&mut tx).await;
    named_group(&mut tx, "base.group_user_regular", "Is regular user").await;
    let me = user(&mut tx, "me", &[t.internal]).await;

    // an empty role: no group changes
    for empty in [Value::False, Value::Bool(false)] {
        let result = role_onchange(&mut tx, me, empty, &[t.internal])
            .await
            .unwrap();
        let Value::Map(result) = result else {
            panic!("{result:?}")
        };
        let changed = match result.get("value") {
            Some(Value::Map(value)) => value.contains_key("group_ids"),
            _ => false,
        };
        assert!(!changed, "{result:?}");
    }
    // a value that is not in the selection never reaches the method: the ORM refuses it first
    // (the method's own check is the second line of defence for callers that skip the ORM)
    let err = role_onchange(&mut tx, me, s("wizard"), &[t.internal])
        .await
        .unwrap_err();
    assert_error(
        &err,
        ErrorKind::Validation,
        "Wrong value for res.users.role: 'wizard'",
    );
    let err = role_onchange(&mut tx, me, int(5), &[t.internal])
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Validation, "{err}");
    // a real role works for the non-elevated caller
    let result = role_onchange(&mut tx, me, s("light_user"), &[t.internal])
        .await
        .unwrap();
    assert!(matches!(result, Value::Map(_)));
    drop(tx);
    h.cleanup().await.unwrap();
}
