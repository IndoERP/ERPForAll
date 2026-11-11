//! Parity tests for `ir.default` and company-dependent field handling.
//! Specs:.
#![allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]

use support::currency_fixture;

mod support;

use std::sync::Arc;

use std::num::NonZeroU64;

use erp_fields::{ErrorKind, RecId, Value};
use erp_orm::{
    AccessPolicy, BoxFuture, CompanyId, FieldOp, Principal, PrincipalBuilder, PrincipalHint,
    PrincipalResolver, Resolution, ResolverStatement, Spec, ir_default,
};
use erp_sdk::serde_json;
use erp_sql::param::{Param, Scalar};
use erp_test_models::{base_users, test_defaults, test_orm};
use erp_testkit::FieldPolicy;
use erp_testkit::Harness;
use erp_testkit::tokio_postgres::Row;
use support::map;

async fn defaults_harness() -> Harness {
    Harness::builder([
        &base_users::ADDON,
        &erp_addon_base::ADDON,
        &test_defaults::ADDON,
        &test_orm::ADDON,
    ])
    .with_static_decls(crate::currency_fixture::decls())
    .build()
    .await
    .expect("harness")
}

async fn init_db(tx: &mut erp_orm::Transaction) -> i64 {
    let currency = erp_testkit::fixtures::seed_currency_in(tx).await.unwrap();
    tx.execute(
        "INSERT INTO res_company (id, name, currency_id) VALUES (1, 'Company 1', $1), (2, 'Company 2', $1) ON CONFLICT DO NOTHING",
        &[&currency],
    )
    .await
    .unwrap();
    tx.execute(
        "INSERT INTO res_users (id, name, login, active) VALUES (2, 'User 2', 'u2', true) ON CONFLICT DO NOTHING",
        &[],
    )
    .await
    .unwrap();
    currency
}

/// Currency fixtures use the database sequence and propagate its returned id to companies.
#[tokio::test]
async fn currency_fixture_uses_sequence_id() {
    let h = defaults_harness().await;
    let mut tx = h.begin().await.unwrap();
    tx.query(
        "SELECT setval(pg_get_serial_sequence('res_currency', 'id'), 40)",
        &[],
    )
    .await
    .unwrap();
    init_db(&mut tx).await;
    let rows = tx
        .query(
            "SELECT c.id::int8, co.currency_id::int8 FROM res_currency c \
             JOIN res_company co ON co.currency_id = c.id ORDER BY co.id",
            &[],
        )
        .await
        .unwrap();
    assert_eq!(rows.len(), 2);
    for row in rows {
        assert_eq!(row.get::<_, i64>(0), 41);
        assert_eq!(row.get::<_, i64>(1), 41);
    }
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_base_001_set_and_get_global_user_default() {
    let h = defaults_harness().await;
    let mut tx = h.begin().await.unwrap();
    let env = tx.env();

    // When: set global default for test_defaults.partner.ref_code
    ir_default::set(
        &mut tx,
        &env,
        "test_defaults.partner",
        "ref_code",
        &Value::str("GLOBAL_REF"),
        None,
        None,
        None,
    )
    .await
    .unwrap();

    // Then: ir_default::get returns 'GLOBAL_REF'
    let val = ir_default::get(
        &mut tx,
        &env,
        "test_defaults.partner",
        "ref_code",
        None,
        None,
        None,
    )
    .await
    .unwrap();
    assert_eq!(val, Some(Value::str("GLOBAL_REF")));

    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_base_002_precedence_of_user_and_company_defaults() {
    let h = defaults_harness().await;
    let mut tx = h.begin().await.unwrap();
    init_db(&mut tx).await;
    let env = tx.env();

    let u2 = RecId::from_real(2).unwrap();
    let c1 = RecId::from_real(1).unwrap();

    // Global default
    ir_default::set(
        &mut tx,
        &env,
        "test_defaults.partner",
        "ref_code",
        &Value::str("GLOBAL"),
        None,
        None,
        None,
    )
    .await
    .unwrap();

    // Company C1 default
    ir_default::set(
        &mut tx,
        &env,
        "test_defaults.partner",
        "ref_code",
        &Value::str("COMPANY"),
        None,
        Some(c1),
        None,
    )
    .await
    .unwrap();

    // User u2 default
    ir_default::set(
        &mut tx,
        &env,
        "test_defaults.partner",
        "ref_code",
        &Value::str("USER"),
        Some(u2),
        None,
        None,
    )
    .await
    .unwrap();

    // User u2 in company C1: user default takes precedence
    let env_u2 = tx
        .env()
        .with_user(2)
        .with_context([("allowed_company_ids", Value::list([Value::Int(1)]))]);
    let defs_u2 = ir_default::get_model_defaults(&mut tx, &env_u2, "test_defaults.partner", None)
        .await
        .unwrap();
    assert_eq!(defs_u2.get("ref_code"), Some(&Value::str("USER")));

    // Other user (e.g. u1) in company C1: company default takes precedence
    let env_u1 = tx
        .env()
        .with_user(1)
        .with_context([("allowed_company_ids", Value::list([Value::Int(1)]))]);
    let defs_u1 = ir_default::get_model_defaults(&mut tx, &env_u1, "test_defaults.partner", None)
        .await
        .unwrap();
    assert_eq!(defs_u1.get("ref_code"), Some(&Value::str("COMPANY")));

    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_base_005_conditional_user_defaults_via_change_default_onchange() {
    let h = defaults_harness().await;
    let mut tx = h.begin().await.unwrap();
    let env = tx.env();

    // Set conditional default: city='Brussels' when zip='1000'
    ir_default::set(
        &mut tx,
        &env,
        "test_defaults.partner",
        "city",
        &Value::str("Brussels"),
        None,
        None,
        Some("zip=1000"),
    )
    .await
    .unwrap();

    let records = tx.env().records("test_defaults.partner").unwrap();
    let partner = records
        .create_values(&mut tx, &[map(&[("zip", Value::str("1000"))])])
        .await
        .unwrap();

    // Call onchange on zip
    let spec = Spec::from_value(&Value::Map(Arc::new(erp_sdk::ValueMap::from([(
        "city".to_owned(),
        Value::Map(Arc::new(erp_sdk::ValueMap::default())),
    )]))))
    .unwrap();
    let result = partner
        .onchange(
            &mut tx,
            erp_sdk::ValueMap::from([("zip".to_owned(), Value::str("1000"))]),
            vec!["zip".to_string()],
            spec,
        )
        .await
        .unwrap();

    let city_val = match result {
        Value::Map(m) => match m.get("value") {
            Some(Value::Map(vm)) => vm.get("city").cloned(),
            _ => None,
        },
        _ => None,
    };
    assert_eq!(city_val, Some(Value::str("Brussels")));

    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_base_006_validation_of_model_field_and_value_on_set() {
    let h = defaults_harness().await;
    let mut tx = h.begin().await.unwrap();
    let env = tx.env();

    // Unknown model
    let err_model = ir_default::set(
        &mut tx,
        &env,
        "unknown_model",
        "f",
        &Value::Int(42),
        None,
        None,
        None,
    )
    .await
    .unwrap_err();
    assert_eq!(err_model.kind(), ErrorKind::Validation);
    assert_eq!(err_model.message(), "Invalid field unknown_model.f");

    // Unknown field on existing model
    let err_field = ir_default::set(
        &mut tx,
        &env,
        "test_defaults.partner",
        "unknown_field",
        &Value::Int(42),
        None,
        None,
        None,
    )
    .await
    .unwrap_err();
    assert_eq!(err_field.kind(), ErrorKind::Validation);
    assert_eq!(
        err_field.message(),
        "Invalid field test_defaults.partner.unknown_field"
    );

    let err_bounds = ir_default::set(
        &mut tx,
        &env,
        "test_defaults.partner",
        "color",
        &Value::Int(2_147_483_647),
        None,
        None,
        None,
    )
    .await
    .unwrap_err();
    assert_eq!(err_bounds.kind(), ErrorKind::Validation);
    assert_eq!(
        err_bounds.message(),
        "Invalid value for test_defaults.partner.color: 2147483647 is out of bounds (integers should be between -2,147,483,648 and 2,147,483,647)"
    );

    let err_bounds_min = ir_default::set(
        &mut tx,
        &env,
        "test_defaults.partner",
        "color",
        &Value::Int(-2_147_483_648),
        None,
        None,
        None,
    )
    .await
    .unwrap_err();
    assert_eq!(err_bounds_min.kind(), ErrorKind::Validation);
    assert_eq!(
        err_bounds_min.message(),
        "Invalid value for test_defaults.partner.color: -2147483648 is out of bounds (integers should be between -2,147,483,648 and 2,147,483,647)"
    );

    // Valid bound inside exclusive range
    ir_default::set(
        &mut tx,
        &env,
        "test_defaults.partner",
        "color",
        &Value::Int(2_147_483_646),
        None,
        None,
        None,
    )
    .await
    .unwrap();

    // Wrong type for integer field renders without quotes
    let err_type = ir_default::set(
        &mut tx,
        &env,
        "test_defaults.partner",
        "color",
        &Value::str("not_an_int"),
        None,
        None,
        None,
    )
    .await
    .unwrap_err();
    assert_eq!(err_type.kind(), ErrorKind::Validation);
    assert_eq!(
        err_type.message(),
        "Invalid value for test_defaults.partner.color: not_an_int"
    );

    // Missing model in get_model_defaults fails loudly
    let err_get_missing = ir_default::get_model_defaults(&mut tx, &env, "missing.model", None)
        .await
        .unwrap_err();
    assert_eq!(err_get_missing.kind(), ErrorKind::Validation);
    assert_eq!(
        err_get_missing.message(),
        "Model 'missing.model' is not in the registry"
    );

    // Corrupt JSON in database causes get_model_defaults to return Validation error naming field
    tx.execute(
        "UPDATE ir_default SET json_value = 'INVALID_JSON{' WHERE condition IS NULL",
        &[],
    )
    .await
    .unwrap();
    tx.invalidate_ormcache("ir.default");
    let err_corrupt = ir_default::get_model_defaults(&mut tx, &env, "test_defaults.partner", None)
        .await
        .unwrap_err();
    assert_eq!(err_corrupt.kind(), ErrorKind::Validation);
    assert!(
        err_corrupt
            .message()
            .contains("Invalid JSON default value for field 'color'")
    );

    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_base_008_multi_company_allowed_company_ids_context_filtering() {
    let h = defaults_harness().await;
    let mut tx = h.begin().await.unwrap();
    init_db(&mut tx).await;
    let env = tx.env();

    let ca = RecId::from_real(1).unwrap();
    let cb = RecId::from_real(2).unwrap();

    ir_default::set(
        &mut tx,
        &env,
        "test_defaults.partner",
        "ref_code",
        &Value::str("CADefault"),
        None,
        Some(ca),
        None,
    )
    .await
    .unwrap();

    ir_default::set(
        &mut tx,
        &env,
        "test_defaults.partner",
        "ref_code",
        &Value::str("CBDefault"),
        None,
        Some(cb),
        None,
    )
    .await
    .unwrap();

    // Query with allowed_company_ids=[2, 1] -> first id is company 2 (B)
    let env_cb = tx.env().with_context([(
        "allowed_company_ids",
        Value::list([Value::Int(2), Value::Int(1)]),
    )]);
    let defs = ir_default::get_model_defaults(&mut tx, &env_cb, "test_defaults.partner", None)
        .await
        .unwrap();
    assert_eq!(defs.get("ref_code"), Some(&Value::str("CBDefault")));

    drop(tx);
    h.cleanup().await.unwrap();
}

/// The users of the access tests: `USER` is in `base.group_user` only, `ADMIN` is also in
/// `base.group_system`. Companies 1 and 2 belong to both; each is allowed company 1 only.
const USER: i64 = 2;
const ADMIN: i64 = 3;
/// Only in `base.group_portal`.
const PORTAL: i64 = 4;
const FIELD: &str = "ref_code";
const MODEL: &str = "test_defaults.partner";

/// Resolves any user to groups the policy stated (none: membership is the policy's) and
/// companies 1 and 2, with company 1 allowed.
struct DefaultsResolver;

impl PrincipalResolver for DefaultsResolver {
    fn statement(&self, hint: &PrincipalHint) -> erp_sdk::Result<ResolverStatement> {
        Ok(ResolverStatement::new(
            "SELECT $1::int8".to_owned(),
            vec![Param::Scalar(Scalar::Int8(hint.subject().get()))],
        ))
    }

    fn finish(
        &self,
        hint: &PrincipalHint,
        _rows: &[Row],
        policy: &dyn AccessPolicy,
    ) -> erp_sdk::Result<Principal> {
        let resolution = Resolution::in_id_order(
            policy.groups(&[])?,
            vec![CompanyId::new(1).unwrap(), CompanyId::new(2).unwrap()],
            NonZeroU64::MIN,
        )?;
        Ok(PrincipalBuilder::user(hint.subject())
            .resolution(resolution)
            .companies(vec![CompanyId::new(1).unwrap()])
            .build()?)
    }

    fn re_resolve<'a>(
        &'a self,
        _tx: &'a mut erp_orm::Transaction,
        subjects: &'a [erp_orm::UserId],
    ) -> BoxFuture<'a, erp_sdk::Result<Vec<Resolution>>> {
        Box::pin(async move {
            subjects
                .iter()
                .map(|_| {
                    Ok(Resolution::in_id_order(
                        erp_orm::GroupSet::new([]),
                        vec![CompanyId::new(1).unwrap(), CompanyId::new(2).unwrap()],
                        NonZeroU64::MIN,
                    )?)
                })
                .collect()
        })
    }
}

/// A harness with the access test users in place (committed) and a field policy that denies
/// writing `city` to everybody outside superuser mode.
async fn access_harness() -> Harness {
    let policy = FieldPolicy::new()
        .member(USER, ["base.group_user"])
        .member(ADMIN, ["base.group_user", "base.group_system"])
        .member(PORTAL, ["base.group_portal"])
        .deny(MODEL, "city", FieldOp::Write);
    let h = Harness::builder([
        &base_users::ADDON,
        &erp_addon_base::ADDON,
        &test_defaults::ADDON,
        &test_orm::ADDON,
    ])
    .with_static_decls(crate::currency_fixture::decls())
    .policy(policy)
    .resolver(DefaultsResolver)
    .build()
    .await
    .expect("harness");
    h.run(async |tx| {
        let currency = init_db(tx).await;
        tx.execute(
            "INSERT INTO res_users (id, name, login, active) VALUES (3, 'User 3', 'u3', true), (4, 'User 4', 'u4', true) ON CONFLICT DO NOTHING",
            &[],
        )
        .await?;
        tx.execute(
            "INSERT INTO res_company (id, name, currency_id) VALUES (3, 'Company 3', $1) ON CONFLICT DO NOTHING",
            &[&currency],
        )
        .await?;
        Ok(())
    })
    .await
    .expect("seed");
    h
}

fn rec(id: i64) -> Option<RecId> {
    Some(RecId::from_real(id).unwrap())
}

/// The stored `ref_code` default of exactly this scope, read with raw SQL (not through
/// `ir_default::get`, which is unchecked and will follow the caller's rules).
async fn stored(
    tx: &mut erp_orm::Transaction,
    user: Option<RecId>,
    company: Option<RecId>,
) -> Option<Value> {
    stored_field(tx, FIELD, user, company).await
}

async fn stored_field(
    tx: &mut erp_orm::Transaction,
    field: &str,
    user: Option<RecId>,
    company: Option<RecId>,
) -> Option<Value> {
    let (user, company) = (user.map(RecId::get), company.map(RecId::get));
    let rows = tx
        .query(
            "SELECT d.json_value FROM ir_default d JOIN ir_model_fields f ON d.field_id = f.id \
             WHERE f.model = $1 AND f.name = $2 \
               AND d.user_id IS NOT DISTINCT FROM $3::int8 \
               AND d.company_id IS NOT DISTINCT FROM $4::int8 AND d.condition IS NULL",
            &[&MODEL, &field, &user, &company],
        )
        .await
        .unwrap();
    assert!(rows.len() <= 1, "one default per scope");
    rows.first().map(|r| {
        let text: String = r.get(0);
        ir_default::json_to_value(&serde_json::from_str(&text).unwrap())
    })
}

/// Seed `ref_code` defaults as the system (committed): `(user, company, value)`.
async fn seed(h: &Harness, rows: &[(Option<RecId>, Option<RecId>, &'static str)]) {
    let rows = rows.to_vec();
    h.run(async move |tx| {
        let env = tx.env();
        for (user, company, value) in &rows {
            ir_default::set(
                tx,
                &env,
                MODEL,
                FIELD,
                &Value::str(value),
                *user,
                *company,
                None,
            )
            .await?;
        }
        Ok(())
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn spec_base_009_non_system_user_cannot_create_global_default() {
    let h = access_harness().await;
    seed(&h, &[(None, None, "OLD")]).await;

    // the user in base.group_user only is refused, also when the global row exists already
    let mut tx = h.begin_as(USER).await.unwrap();
    let env = tx.env();
    let err = ir_default::set(
        &mut tx,
        &env,
        MODEL,
        FIELD,
        &Value::str("G"),
        None,
        None,
        None,
    )
    .await
    .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Access);
    assert_eq!(
        err.message(),
        format!("Only system administrators may create global defaults ({MODEL}.{FIELD})")
    );
    assert_eq!(stored(&mut tx, None, None).await, Some(Value::str("OLD")));
    drop(tx);

    // a member of base.group_system may
    let mut tx = h.begin_as(ADMIN).await.unwrap();
    let env = tx.env();
    ir_default::set(
        &mut tx,
        &env,
        MODEL,
        FIELD,
        &Value::str("G"),
        None,
        None,
        None,
    )
    .await
    .unwrap();
    assert_eq!(stored(&mut tx, None, None).await, Some(Value::str("G")));
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_base_151_non_system_user_cannot_set_another_users_default() {
    let h = access_harness().await;

    let mut tx = h.begin_as(USER).await.unwrap();
    let env = tx.env();
    let err = ir_default::set(
        &mut tx,
        &env,
        MODEL,
        FIELD,
        &Value::str("X"),
        rec(ADMIN),
        None,
        None,
    )
    .await
    .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Access);
    assert_eq!(
        err.message(),
        format!(
            "Only system administrators may set the default of another user: user {ADMIN} is \
             not user {USER} ({MODEL}.{FIELD})"
        )
    );
    assert_eq!(stored(&mut tx, rec(ADMIN), None).await, None);

    // the caller's own personal default is allowed
    ir_default::set(
        &mut tx,
        &env,
        MODEL,
        FIELD,
        &Value::str("MINE"),
        rec(USER),
        None,
        None,
    )
    .await
    .unwrap();
    assert_eq!(
        stored(&mut tx, rec(USER), None).await,
        Some(Value::str("MINE"))
    );
    drop(tx);

    // a member of base.group_system may set anyone's
    let mut tx = h.begin_as(ADMIN).await.unwrap();
    let env = tx.env();
    ir_default::set(
        &mut tx,
        &env,
        MODEL,
        FIELD,
        &Value::str("FOR_USER"),
        rec(USER),
        None,
        None,
    )
    .await
    .unwrap();
    assert_eq!(
        stored(&mut tx, rec(USER), None).await,
        Some(Value::str("FOR_USER"))
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

/// unchanged; the create half is the test above)
#[tokio::test]
async fn spec_base_151_existing_row_of_another_user_is_not_updated() {
    let h = access_harness().await;
    seed(&h, &[(rec(ADMIN), None, "OLD")]).await;
    let mut tx = h.begin_as(USER).await.unwrap();
    let env = tx.env();
    let err = ir_default::set(
        &mut tx,
        &env,
        MODEL,
        FIELD,
        &Value::str("NEW"),
        rec(ADMIN),
        None,
        None,
    )
    .await
    .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Access);
    assert_eq!(
        err.message(),
        format!(
            "Only system administrators may set the default of another user: user {ADMIN} is \
             not user {USER} ({MODEL}.{FIELD})"
        )
    );
    assert_eq!(
        stored(&mut tx, rec(ADMIN), None).await,
        Some(Value::str("OLD"))
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_base_152_default_company_must_be_an_allowed_company() {
    let h = access_harness().await;
    seed(
        &h,
        &[(rec(USER), rec(2), "OLD2"), (rec(USER), rec(3), "OLD3")],
    )
    .await;

    let mut tx = h.begin_as(USER).await.unwrap();
    let env = tx.env();
    // company 2 belongs to the subject but is not allowed in this environment, company 3 does
    // not belong to the subject; both are refused (also with a row there already and a new
    // value), the allowed company 1 is accepted
    let denied = |company: i64| {
        format!(
            "Only system administrators may set a default for a company that is not one of \
             your allowed companies: company {company} ({MODEL}.{FIELD})"
        )
    };
    for (foreign, old) in [(2, "OLD2"), (3, "OLD3")] {
        let err = ir_default::set(
            &mut tx,
            &env,
            MODEL,
            FIELD,
            &Value::str("C"),
            rec(USER),
            rec(foreign),
            None,
        )
        .await
        .unwrap_err();
        assert_eq!(err.kind(), ErrorKind::Access);
        assert_eq!(err.message(), denied(foreign));
        assert_eq!(
            stored(&mut tx, rec(USER), rec(foreign)).await,
            Some(Value::str(old))
        );
    }
    ir_default::set(
        &mut tx,
        &env,
        MODEL,
        FIELD,
        &Value::str("C1"),
        rec(USER),
        rec(1),
        None,
    )
    .await
    .unwrap();
    assert_eq!(
        stored(&mut tx, rec(USER), rec(1)).await,
        Some(Value::str("C1"))
    );
    drop(tx);

    let mut tx = h.begin_as(ADMIN).await.unwrap();
    let env = tx.env();
    ir_default::set(
        &mut tx,
        &env,
        MODEL,
        FIELD,
        &Value::str("C3"),
        None,
        rec(3),
        None,
    )
    .await
    .unwrap();
    assert_eq!(stored(&mut tx, None, rec(3)).await, Some(Value::str("C3")));
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_base_153_setting_a_default_needs_write_access_to_the_field() {
    let h = access_harness().await;
    let spec = h.orm().registry().model(MODEL).unwrap().description.clone();
    let expected = format!(
        "You do not have enough rights to access the field \"city\" on {spec} ({MODEL}). Please \
         contact your system administrator.\n\nOperation: write"
    );

    for (uid, user) in [(USER, rec(USER)), (ADMIN, rec(ADMIN))] {
        let mut tx = h.begin_as(uid).await.unwrap();
        let env = tx.env();
        let err = ir_default::set(
            &mut tx,
            &env,
            MODEL,
            "city",
            &Value::str("Paris"),
            user,
            None,
            None,
        )
        .await
        .unwrap_err();
        assert_eq!(err.kind(), ErrorKind::Access);
        assert_eq!(err.message(), expected);
        assert_eq!(stored_field(&mut tx, "city", user, None).await, None);
        drop(tx);
    }
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_base_154_non_system_discard_values_is_refused_for_rows_of_others() {
    let h = access_harness().await;
    let v1 = Value::str("V1");
    let v2 = Value::str("V2");
    seed(
        &h,
        &[
            (rec(USER), None, "V1"),
            (rec(ADMIN), None, "V1"),
            (None, None, "V1"),
            (rec(USER), rec(1), "V2"),
        ],
    )
    .await;

    // only the caller's own row matches: deleted
    let mut tx = h.begin_as(USER).await.unwrap();
    let env = tx.env();
    ir_default::discard_values(&mut tx, &env, MODEL, FIELD, std::slice::from_ref(&v2))
        .await
        .unwrap();
    assert_eq!(stored(&mut tx, rec(USER), rec(1)).await, None);

    // another user's and the global row match: refused, nothing deleted, own row included
    let err = ir_default::discard_values(&mut tx, &env, MODEL, FIELD, std::slice::from_ref(&v1))
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Access);
    assert_eq!(
        err.message(),
        format!(
            "Only system administrators may discard the defaults of other users or global \
             defaults ({MODEL}.{FIELD})"
        )
    );
    for user in [rec(USER), rec(ADMIN), None] {
        assert_eq!(stored(&mut tx, user, None).await, Some(v1.clone()));
    }
    drop(tx);

    // a member of base.group_system deletes all three
    let mut tx = h.begin_as(ADMIN).await.unwrap();
    let env = tx.env();
    ir_default::discard_values(&mut tx, &env, MODEL, FIELD, std::slice::from_ref(&v1))
        .await
        .unwrap();
    for user in [rec(USER), rec(ADMIN), None] {
        assert_eq!(stored(&mut tx, user, None).await, None);
    }
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_base_155_users_outside_base_group_user_cannot_manage_defaults() {
    let h = access_harness().await;
    seed(&h, &[(rec(PORTAL), None, "V1")]).await;
    let mut tx = h.begin_as(PORTAL).await.unwrap();
    let env = tx.env();

    let err = ir_default::set(
        &mut tx,
        &env,
        MODEL,
        FIELD,
        &Value::str("NEW"),
        rec(PORTAL),
        None,
        None,
    )
    .await
    .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Access);
    assert_eq!(
        err.message(),
        format!("Only internal users may set defaults ({MODEL}.{FIELD})")
    );
    let err = ir_default::discard_values(&mut tx, &env, MODEL, FIELD, &[Value::str("V1")])
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Access);
    assert_eq!(
        err.message(),
        format!("Only internal users may discard defaults ({MODEL}.{FIELD})")
    );
    // neither wrote nor deleted anything
    assert_eq!(
        stored(&mut tx, rec(PORTAL), None).await,
        Some(Value::str("V1"))
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

/// The test policy answers group questions like the real one: a group nobody named and an
/// empty name are errors, never `false`.
#[tokio::test]
async fn field_policy_has_groups_refuses_unknown_groups_and_empty_names() {
    let h = access_harness().await;
    let mut tx = h.begin_as(USER).await.unwrap();
    let env = tx.env();
    assert!(tx.has_groups(&env, "base.group_user").await.unwrap());
    assert!(!tx.has_groups(&env, "base.group_system").await.unwrap());
    assert!(
        !tx.has_groups(&env, "base.group_system,!base.group_user")
            .await
            .unwrap()
    );
    for spec in [
        "base.group_sytem",
        "!",
        "base.group_user,!",
        "base.group_user,,base.group_user",
    ] {
        let err = tx.has_groups(&env, spec).await.unwrap_err();
        assert_eq!(err.kind(), ErrorKind::Programming, "{spec}");
    }
    let err = tx.has_groups(&env, "base.group_sytem").await.unwrap_err();
    assert_eq!(
        err.message(),
        "FieldPolicy does not know the group 'base.group_sytem' in 'base.group_sytem'"
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_base_010_cache_invalidation_on_ir_default_mutations() {
    let h = defaults_harness().await;
    let mut tx = h.begin().await.unwrap();
    let env = tx.env();

    // Populate cache with initial default
    ir_default::set(
        &mut tx,
        &env,
        "test_defaults.partner",
        "ref_code",
        &Value::str("INIT"),
        None,
        None,
        None,
    )
    .await
    .unwrap();

    let defs1 = ir_default::get_model_defaults(&mut tx, &env, "test_defaults.partner", None)
        .await
        .unwrap();
    assert_eq!(defs1.get("ref_code"), Some(&Value::str("INIT")));

    // Mutate default to "UPDATED"
    ir_default::set(
        &mut tx,
        &env,
        "test_defaults.partner",
        "ref_code",
        &Value::str("UPDATED"),
        None,
        None,
        None,
    )
    .await
    .unwrap();

    // Reading again yields "UPDATED", confirming invalidation of cached defaults
    let defs2 = ir_default::get_model_defaults(&mut tx, &env, "test_defaults.partner", None)
        .await
        .unwrap();
    assert_eq!(defs2.get("ref_code"), Some(&Value::str("UPDATED")));

    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_crud_005_default_get_precedence() {
    let h = defaults_harness().await;
    let mut tx = h.begin().await.unwrap();
    let env = tx.env();

    let mixed = tx.env().records("test_orm.mixed").unwrap();

    // 1. Field default (float_default = 1.25 on model test_orm.mixed)
    let d1 = mixed
        .default_get(&mut tx, &["float_default"])
        .await
        .unwrap();
    assert_eq!(d1.get("float_default"), Some(&Value::Float(1.25)));

    // 2. ir.default override (float_default = 2.5)
    ir_default::set(
        &mut tx,
        &env,
        "test_orm.mixed",
        "float_default",
        &Value::Float(2.5),
        None,
        None,
        None,
    )
    .await
    .unwrap();

    let d2 = mixed
        .default_get(&mut tx, &["float_default"])
        .await
        .unwrap();
    assert_eq!(d2.get("float_default"), Some(&Value::Float(2.5)));

    // 3. Context default override: default_float_default = 1.0
    let env_ctx = tx
        .env()
        .with_context([("default_float_default", Value::Float(1.0))]);
    let mixed_ctx = env_ctx.records("test_orm.mixed").unwrap();
    let d3 = mixed_ctx
        .default_get(&mut tx, &["float_default"])
        .await
        .unwrap();
    assert_eq!(d3.get("float_default"), Some(&Value::Float(1.0)));

    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_crud_035_company_dependent_many2one_unlink() {
    let h = defaults_harness().await;
    let mut tx = h.begin().await.unwrap();

    let p_records = tx.env().records("test_defaults.partner").unwrap();
    let p1 = p_records
        .create_values(&mut tx, &[map(&[("name", Value::str("Partner1"))])])
        .await
        .unwrap();
    let p1_id = p1.ids()[0];

    let p2 = p_records
        .create_values(&mut tx, &[map(&[("name", Value::str("Partner2"))])])
        .await
        .unwrap();
    let p2_id = p2.ids()[0];

    let p3 = p_records
        .create_values(&mut tx, &[map(&[("name", Value::str("Partner3"))])])
        .await
        .unwrap();
    let p3_id = p3.ids()[0];

    // (a) pref_partner has ondelete = 'restrict'. Point item1 to p1 in company 1.
    tx.execute(
        r#"
        INSERT INTO test_defaults_item (name, pref_partner)
        VALUES ('Item1', jsonb_build_object('1', $1::int8))
        "#,
        &[&p1_id],
    )
    .await
    .unwrap();

    // Attempting to delete p1 without force_delete raises UserError
    let err_del_p1 = p1.unlink(&mut tx).await.unwrap_err();
    assert_eq!(err_del_p1.kind(), ErrorKind::UserError);
    assert!(err_del_p1.message().contains("cannot delete"));

    // (b) alt_partner has default policy (set null in jsonb). Point item2 to p2 in company 1.
    tx.execute(
        r#"
        INSERT INTO test_defaults_item (name, alt_partner)
        VALUES ('Item2', jsonb_build_object('1', $1::int8))
        "#,
        &[&p2_id],
    )
    .await
    .unwrap();

    // Deleting p2 succeeds and sets null in the jsonb
    p2.unlink(&mut tx).await.unwrap();
    let row = tx
        .query(
            "SELECT alt_partner FROM test_defaults_item WHERE name = 'Item2'",
            &[],
        )
        .await
        .unwrap();
    let raw_json: serde_json::Value = row[0].try_get(0).unwrap();
    assert_eq!(raw_json.get("1"), Some(&serde_json::Value::Null));

    // (c) p3 is set as ir.default fallback for pref_partner
    let env = tx.env();
    ir_default::set(
        &mut tx,
        &env,
        "test_defaults.item",
        "pref_partner",
        &Value::Int(p3_id),
        None,
        None,
        None,
    )
    .await
    .unwrap();

    // Deleting p3 without force_delete raises UserError
    let err_del_p3 = p3.unlink(&mut tx).await.unwrap_err();
    assert_eq!(err_del_p3.kind(), ErrorKind::UserError);
    assert!(err_del_p3.message().contains("default value"));

    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_grp_050_change_default_onchange() {
    let h = defaults_harness().await;
    let mut tx = h.begin().await.unwrap();
    let env = tx.env();

    // Set ir.default city='Brussels' when zip='1000'
    ir_default::set(
        &mut tx,
        &env,
        "test_defaults.partner",
        "city",
        &Value::str("Brussels"),
        None,
        None,
        Some("zip=1000"),
    )
    .await
    .unwrap();

    let defs_match =
        ir_default::get_model_defaults(&mut tx, &env, "test_defaults.partner", Some("zip=1000"))
            .await
            .unwrap();
    assert_eq!(defs_match.get("city"), Some(&Value::str("Brussels")));

    let defs_unmatch =
        ir_default::get_model_defaults(&mut tx, &env, "test_defaults.partner", Some("zip=2000"))
            .await
            .unwrap();
    assert_eq!(defs_unmatch.get("city"), None);

    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_field_075_company_dependent_ir_default_fallback() {
    let h = defaults_harness().await;
    let mut tx = h.begin().await.unwrap();
    init_db(&mut tx).await;
    let env = tx.env();

    let c1 = RecId::from_real(1).unwrap();

    // Global default: note = 'global_note'
    ir_default::set(
        &mut tx,
        &env,
        "test_defaults.item",
        "note",
        &Value::str("global_note"),
        None,
        None,
        None,
    )
    .await
    .unwrap();

    // Company 1 default: note = 'company1_note'
    ir_default::set(
        &mut tx,
        &env,
        "test_defaults.item",
        "note",
        &Value::str("company1_note"),
        None,
        Some(c1),
        None,
    )
    .await
    .unwrap();

    // Reading in company 1 resolves to 'company1_note'
    let env_c1 = tx
        .env()
        .with_context([("allowed_company_ids", Value::list([Value::Int(1)]))]);
    let defs_c1 = ir_default::get_model_defaults(&mut tx, &env_c1, "test_defaults.item", None)
        .await
        .unwrap();
    assert_eq!(defs_c1.get("note"), Some(&Value::str("company1_note")));

    // Reading in company 2 resolves to global fallback 'global_note'
    let env_c2 = tx
        .env()
        .with_context([("allowed_company_ids", Value::list([Value::Int(2)]))]);
    let defs_c2 = ir_default::get_model_defaults(&mut tx, &env_c2, "test_defaults.item", None)
        .await
        .unwrap();
    assert_eq!(defs_c2.get("note"), Some(&Value::str("global_note")));

    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_field_076_fallback_equal_company_values() {
    let h = defaults_harness().await;
    let mut tx = h.begin().await.unwrap();

    let items = tx.env().records("test_defaults.item").unwrap();
    let item = items
        .create_values(&mut tx, &[map(&[("name", Value::str("ItemUnset"))])])
        .await
        .unwrap();

    // With no company-dependent values set, jsonb column in database is NULL
    let row = tx
        .query(
            "SELECT note FROM test_defaults_item WHERE id = $1",
            &[&item.ids()[0]],
        )
        .await
        .unwrap();
    let note_val: Option<serde_json::Value> = row[0].try_get(0).unwrap();
    assert!(note_val.is_none() || note_val == Some(serde_json::Value::Null));

    drop(tx);
    h.cleanup().await.unwrap();
}

/// SM-10 of the merge review: the "used as a default value" check of an unlink does not
/// skip its refusal when its own query fails; the failure is the error of the unlink.
#[tokio::test]
async fn a_failed_default_usage_check_fails_the_unlink() {
    let h = defaults_harness().await;
    let mut tx = h.begin().await.unwrap();
    init_db(&mut tx).await;
    let env = tx.env();
    let partner = env
        .records("test_defaults.partner")
        .unwrap()
        .create_values(&mut tx, &[map(&[("name", Value::str("p"))])])
        .await
        .unwrap();
    tx.batch("DROP TABLE ir_default CASCADE").await.unwrap();
    let err = partner.unlink(&mut tx).await.unwrap_err();
    assert!(
        err.message().contains("ir_default"),
        "the error is the check's own: {err}"
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

/// In a registry where the base addon is not installed, ir.default is absent;
/// `is_available` returns false, pinning the predicate that avoids calling
/// `get_model_defaults` in defaults.rs, onchange.rs, and unlink.rs.
#[test]
fn ir_default_is_available_pins_no_base_skip() {
    let without_base = erp_registry::RegistryBuilder::new([])
        .linked(false)
        .build()
        .unwrap();
    assert!(!erp_orm::ir_default::is_available(&without_base));
    assert!(without_base.model("ir.default").is_none());

    let with_base = erp_registry::RegistryBuilder::new([&erp_addon_base::ADDON])
        .build()
        .unwrap();
    assert!(erp_orm::ir_default::is_available(&with_base));
    assert!(with_base.model("ir.default").is_some());
}

/// On a registry where the base addon is not installed, ir.default is absent;
/// `default_get` and `unlink` skip querying or discarding stored defaults while
/// continuing to resolve field-level defaults and execute unlinks cleanly.
#[tokio::test]
async fn default_get_without_base_addon_skips_ir_default_and_resolves_field_defaults() {
    let addons = [
        &base_users::ADDON,
        &erp_addon_base::ADDON,
        &test_defaults::ADDON,
        &test_orm::ADDON,
    ];
    let decls: Vec<_> = erp_registry::MODEL_DECLS
        .iter()
        .filter(|d| {
            d.name != "ir.default"
                && (addons.iter().any(|a| a.name == d.addon)
                    || d.addon == "base"
                    || d.addon == "web")
        })
        .collect();
    let currency = crate::currency_fixture::for_selected(
        crate::currency_fixture::has_currency_root(decls.iter().copied()),
    );

    let h = Harness::builder(addons)
        .linked(false)
        .with_static_decls(decls)
        .with_static_decls(currency)
        .build()
        .await
        .unwrap();
    assert!(!erp_orm::ir_default::is_available(h.registry()));
    assert!(h.registry().model("ir.default").is_none());

    let mut tx = h.begin().await.unwrap();
    let records = tx.env().records("test_orm.mixed").unwrap();

    // 1. default_get on a registry without ir.default skips ir.default and resolves field-level defaults
    let defaults = records
        .default_get(&mut tx, &["float_default"])
        .await
        .unwrap();
    assert_eq!(defaults.get("float_default"), Some(&Value::Float(1.25)));

    // 2. create record using defaults and unlink it; unlink skips ir.default cleanups
    let raw = records.vals_from_values(&defaults).unwrap();
    let created = records.create(&mut tx, vec![raw]).await.unwrap();
    assert_eq!(created.len(), 1);
    created.unlink(&mut tx).await.unwrap();

    let remaining = records
        .search(&mut tx, &erp_sdk::Domain::TRUE)
        .await
        .unwrap();
    assert!(remaining.is_empty());

    drop(tx);
    h.cleanup().await.unwrap();
}
