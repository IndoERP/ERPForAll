//! The generated company restrictions of the models a test acts on, for tests that compile an
//! access policy themselves, and the raw fixture users and
//! company those tests act as.
//!
//! Module installation writes one generated `ir.access` row per restricted model
//! (`erp_modules`' emission); a test that builds the policy from rows it inserted, or from rows
//! it builds in memory, has none, and a restricted model without its generated row is denied to
//! every principal that is not the superuser (`erp_security::snapshot`, drift). These helpers add
//! the rows of the named models and nothing else, so the other models of the test keep their
//! behaviour. A model that has no generated restriction in the registry is a mistake of the test
//! and panics with its name: nothing is skipped silently.
#![allow(
    dead_code,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "test helpers"
)]

use erp_registry::Registry;
use erp_security::company_access::{GeneratedRestriction, generate};
use erp_security::rows::{RuleId, RuleRow};
use erp_testkit::tokio_postgres::Client;
use erp_testkit::tokio_postgres::types::ToSql;

/// The models of `base` that declare a company restriction and that tests commonly act on.
pub const PARTNER_AND_USERS: [&str; 2] = ["res.partner", "res.users"];

/// The generated restriction of each of `models`, in the order given.
///
/// # Panics
///
/// When `generate` fails, or when a model has no generated restriction in `registry` (it is not
/// in the registry, is unrestricted, or has no declaration).
pub fn generated_of(registry: &Registry, models: &[&str]) -> Vec<GeneratedRestriction> {
    let all = generate(registry).expect("generate the company restrictions of the registry");
    models
        .iter()
        .map(|model| {
            all.iter()
                .find(|g| g.model_name() == *model)
                .unwrap_or_else(|| {
                    panic!("the registry generates no company restriction for model {model}")
                })
                .clone()
        })
        .collect()
}

/// The generated policy rows of `models` for an in-memory policy (`PolicyRows::builder(..)
/// .rules(..)`): a `crud` row without group, carrying the generated text, marked generated.
/// Ids count up from `first_id`, which must be above every id the test uses for its own rules.
///
/// # Panics
///
/// As [`generated_of`]; also when `first_id` is not positive.
pub fn generated_rules(registry: &Registry, models: &[&str], first_id: i64) -> Vec<RuleRow> {
    generated_of(registry, models)
        .iter()
        .zip(first_id..)
        .map(|(g, id)| {
            RuleRow::new(
                RuleId::new(id).expect("a positive rule id"),
                g.name(),
                g.model_name(),
                "crud",
            )
            .expect("a generated rule row")
            .with_domain(g.text())
            .with_generated()
        })
        .collect()
}

/// Insert the generated restriction of each of `models` into the policy tables of the database
/// behind `client`, as the installation emission does: the `ir_access` row (`crud`, no group,
/// active, generated) and the `ir_model.company_restriction` ledger text. The `ir_model` row of a
/// model is used when it exists and created otherwise, so call this after the test inserted its
/// own `ir_model` rows (a second row of the same model would be read twice).
///
/// # Panics
///
/// As [`generated_of`]; also on any database error.
pub async fn insert_generated(client: &Client, registry: &Registry, models: &[&str]) {
    for g in generated_of(registry, models) {
        let found = client
            .query_opt(
                "SELECT id::int8 FROM ir_model WHERE model = $1 ORDER BY id LIMIT 1",
                &[&g.model_name()],
            )
            .await
            .unwrap();
        let model_id: i64 = match found {
            Some(row) => row.get(0),
            None => client
                .query_one(
                    "INSERT INTO ir_model (name, model, state, transient) \
                     VALUES ($1, $1, 'base', false) RETURNING id::int8",
                    &[&g.model_name()],
                )
                .await
                .unwrap()
                .get(0),
        };
        // a second call for the same model changes nothing: a model has one generated row
        client
            .execute(
                "INSERT INTO ir_access (name, active, model_id, operation, domain, generated) \
                 SELECT $1::text, true, $2::int8, 'crud', $3::text, true \
                  WHERE NOT EXISTS (SELECT 1 FROM ir_access WHERE model_id = $2::int8 AND generated)",
                &[&g.name(), &model_id, &g.text()],
            )
            .await
            .unwrap();
        client
            .execute(
                "UPDATE ir_model SET company_restriction = $2::text WHERE id = $1::int8",
                &[&model_id, &g.text()],
            )
            .await
            .unwrap();
    }
}

/// Whether a raw fixture user is internal or external (portal, public).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UserKind {
    /// `share = false`: escapes the company restriction of `res.users`; its own
    /// contact is not a share contact.
    Internal,
    /// `share` and its contact's `partner_share` take their fail-closed database default `true`
    /// : the company test applies to them.
    External,
}

/// The company the fixture users of a test belong to: one active `res_company` row with a
/// currency of its own (`erp_testkit::fixtures::seed_currency`).
///
/// # Panics
///
/// On any database error.
pub async fn fixture_company(client: &Client) -> i64 {
    let currency = erp_testkit::fixtures::seed_currency(client).await.unwrap();
    client
        .query_one(
            "INSERT INTO res_company (name, currency_id, active) \
             VALUES ('Fixture Company', $1, true) RETURNING id::int8",
            &[&currency],
        )
        .await
        .unwrap()
        .get(0)
}

/// Insert a raw fixture user named and logged in as `login`, member of `groups`, with `company`
/// as its default and only allowed company (`res_company_users_rel`); `share` follows `kind`.
///
/// A user without a company resolves to an empty active-company set, which admits no record of a
/// company-restricted model, its own user and contact included; a user that leaves
/// `share` out is an external user. Rows only, as a fixture writes them: no sudo, no
/// change to a record rule. Pass the internal group in `groups` where the test has groups.
///
/// # Panics
///
/// On any database error.
pub async fn fixture_user(
    client: &Client,
    login: &str,
    groups: &[i64],
    kind: UserKind,
    company: i64,
) -> i64 {
    insert_user(client, login, groups, kind, company, None).await
}

/// [`fixture_user`] with a contact of its own (`partner_id`), named as the user; returns
/// `(user, partner)`. The contact has no company; it is a share contact unless the user is
/// internal.
///
/// # Panics
///
/// On any database error.
pub async fn fixture_user_with_partner(
    client: &Client,
    login: &str,
    groups: &[i64],
    kind: UserKind,
    company: i64,
) -> (i64, i64) {
    let sql = match kind {
        UserKind::Internal => {
            "INSERT INTO res_partner (name, active, partner_share) VALUES ($1, true, false) \
             RETURNING id::int8"
        }
        UserKind::External => {
            "INSERT INTO res_partner (name, active) VALUES ($1, true) RETURNING id::int8"
        }
    };
    let partner: i64 = client.query_one(sql, &[&login]).await.unwrap().get(0);
    let user = insert_user(client, login, groups, kind, company, Some(partner)).await;
    (user, partner)
}

async fn insert_user(
    client: &Client,
    login: &str,
    groups: &[i64],
    kind: UserKind,
    company: i64,
    partner: Option<i64>,
) -> i64 {
    let mut columns = vec!["name", "login", "active", "company_id"];
    let mut values = vec!["$1", "$1", "true", "$2"];
    let mut params: Vec<&(dyn ToSql + Sync)> = vec![&login, &company];
    if kind == UserKind::Internal {
        columns.push("share");
        values.push("false");
    }
    if let Some(partner) = partner.as_ref() {
        columns.push("partner_id");
        values.push("$3");
        params.push(partner);
    }
    let uid: i64 = client
        .query_one(
            &format!(
                "INSERT INTO res_users ({}) VALUES ({}) RETURNING id::int8",
                columns.join(", "),
                values.join(", ")
            ),
            &params,
        )
        .await
        .unwrap()
        .get(0);
    client
        .execute(
            "INSERT INTO res_company_users_rel (user_id, cid) VALUES ($1, $2)",
            &[&uid, &company],
        )
        .await
        .unwrap();
    for group in groups {
        client
            .execute(
                "INSERT INTO res_groups_users_rel (uid, gid) VALUES ($1, $2)",
                &[&uid, group],
            )
            .await
            .unwrap();
    }
    uid
}
