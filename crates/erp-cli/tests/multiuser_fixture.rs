// SPDX-License-Identifier: MIT OR Apache-2.0
//! Preconditions for the later exhaustive multi-user HTTP acceptance suites.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "integration assertions"
)]
#[path = "support/multiuser_fixture.rs"]
mod multiuser_fixture;
#[path = "support/native_http.rs"]
mod native_http;

use multiuser_fixture::{Subject, error, run, run_with_setup, success};
use serde_json::json;
use std::{
    collections::BTreeSet,
    panic::{AssertUnwindSafe, catch_unwind},
    sync::{Arc, Mutex},
};

#[test]
fn installed_multiuser_profile_has_real_product_thread_composition() {
    run(|fixture| {
        assert_eq!(
            fixture.installed(),
            &BTreeSet::from_iter(
                [
                    "base",
                    "contacts",
                    "mail",
                    "product",
                    "product_mail",
                    "uom",
                    "web"
                ]
                .into_iter()
                .map(str::to_owned)
            )
        );
        for model in ["product.template", "product.product"] {
            let fields = success(&fixture.rpc(Subject::Admin, model, "fields_get", json!({})));
            for field in ["message_ids", "message_follower_ids"] {
                assert!(
                    fields.get(field).is_some(),
                    "{model}.{field} missing from installed registry"
                );
            }
        }
        for model in ["mail.message", "uom.uom", "res.partner"] {
            let fields = success(&fixture.rpc(Subject::Admin, model, "fields_get", json!({})));
            assert!(
                fields.as_object().is_some_and(|fields| !fields.is_empty()),
                "{model} metadata missing"
            );
        }
    });
}

#[test]
fn multiuser_subjects_have_verified_groups_and_disjoint_companies() {
    run(|fixture| {
        let mut ids = BTreeSet::new();
        for subject in [Subject::Admin, Subject::A, Subject::B] {
            let user = fixture.user(subject);
            assert_ne!(user.id, erp_orm::SUPERUSER_ID.get());
            assert!(ids.insert(user.id), "subjects share an identity");
            let row = success(&fixture.rpc(Subject::Admin, "res.users", "read", json!({
                "ids":[user.id], "fields":["active", "share", "group_ids", "all_group_ids", "company_id", "company_ids"]
            })));
            let row = &row[0];
            assert_eq!(row["active"], true);
            assert_eq!(row["share"], false);
            let explicit: BTreeSet<i64> = row["group_ids"]
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v.as_i64().unwrap())
                .collect();
            let effective: BTreeSet<i64> = row["all_group_ids"]
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v.as_i64().unwrap())
                .collect();
            assert_eq!(
                explicit,
                BTreeSet::from([fixture.group(match subject {
                    Subject::Admin => "group_system",
                    Subject::A | Subject::B => "group_user",
                    Subject::Anonymous => unreachable!(),
                })])
            );
            assert!(effective.contains(&fixture.group("group_user")));
            for group in ["group_system", "group_erp_manager"] {
                assert_eq!(
                    effective.contains(&fixture.group(group)),
                    subject == Subject::Admin
                );
            }
            assert_eq!(row["company_ids"], json!(user.companies));
            assert_eq!(
                row["company_id"]
                    .as_i64()
                    .or_else(|| row["company_id"][0].as_i64()),
                Some(user.company)
            );
            let bootstrap = fixture.bootstrap(subject);
            assert_eq!(bootstrap["identity"]["uid"], user.id);
            assert_eq!(bootstrap["current_company_id"], user.company);
            assert_eq!(bootstrap["allowed_company_ids"], json!(user.companies));
        }
        assert_ne!(
            fixture.user(Subject::A).company,
            fixture.user(Subject::B).company
        );
        assert_eq!(
            fixture.user(Subject::A).companies,
            vec![fixture.user(Subject::A).company]
        );
        assert_eq!(
            fixture.user(Subject::B).companies,
            vec![fixture.user(Subject::B).company]
        );
    });
}

#[test]
fn interleaved_sessions_preserve_identity_and_anonymous_is_unauthenticated() {
    run(|fixture| {
        assert!(
            fixture.sessions_are_distinct(),
            "real logins reused a session"
        );
        for subject in [
            Subject::A,
            Subject::B,
            Subject::Admin,
            Subject::B,
            Subject::A,
        ] {
            let id = fixture.user(subject).id;
            let row = success(&fixture.rpc(
                subject,
                "res.users",
                "read",
                json!({"ids":[id], "fields":["id", "login"]}),
            ));
            assert_eq!(row[0]["id"], id);
            assert_eq!(row[0]["login"], fixture.user(subject).login);
            assert_eq!(fixture.bootstrap(subject)["identity"]["uid"], id);
        }
        error(
            &fixture.rpc(
                Subject::Anonymous,
                "res.users",
                "read",
                json!({
                    "ids":[fixture.user(Subject::A).id], "fields":["id"]
                }),
            ),
            401,
            "werkzeug.exceptions.Unauthorized",
            "User not authenticated, use an API Key with a Bearer Authorization header.",
        );
    });
}

#[test]
fn settings_subject_has_administrative_access_without_full_elevation() {
    run(|fixture| {
        let companies = success(&fixture.rpc(Subject::Admin, "res.company", "read", json!({
            "ids":[fixture.user(Subject::A).company, fixture.user(Subject::B).company], "fields":["id", "name"]
        })));
        assert_eq!(companies.as_array().unwrap().len(), 2);
        // A failure must never dump a successful response containing the actual secret.
        let denied = fixture.rpc(
            Subject::Admin,
            "ir.config_parameter",
            "get_str",
            json!({"key":"database.secret"}),
        );
        error(
            &denied,
            403,
            "erp.exceptions.AccessError",
            "ir.config_parameter database.secret is readable and writable only in superuser mode, never over RPC",
        );
    });
}

#[test]
fn fixtures_are_isolated_and_cleanup_preserves_the_other_fixture() {
    let outer = run(|first| {
        let first_probe = first.cleanup_probe();
        let second_probe = run(move |second| {
            let second_probe = second.cleanup_probe();
            assert_ne!(first_probe.database, second_probe.database);
            assert_ne!(first_probe.role, second_probe.role);
            assert_ne!(first_probe.directory, second_probe.directory);
            second_probe
        });
        second_probe.assert_removed();
        assert_eq!(
            first.bootstrap(Subject::A)["identity"]["uid"],
            first.user(Subject::A).id
        );
        first.cleanup_probe()
    });
    outer.assert_removed();
}

#[test]
fn setup_and_body_panics_keep_the_failure_and_cleanup_owned_resources() {
    for fail_in_setup in [true, false] {
        let probe = Arc::new(Mutex::new(None));
        let setup_probe = Arc::clone(&probe);
        let body_probe = Arc::clone(&probe);
        let failure = catch_unwind(AssertUnwindSafe(move || {
            run_with_setup(
                move |native| {
                    if fail_in_setup {
                        *setup_probe.lock().unwrap() =
                            Some(multiuser_fixture::CleanupProbe::capture(native));
                        panic!("intentional multiuser setup failure");
                    }
                },
                move |fixture| {
                    *body_probe.lock().unwrap() = Some(fixture.cleanup_probe());
                    panic!("intentional multiuser body failure");
                },
            );
        }))
        .expect_err("intentional failure was swallowed");
        let message = failure.downcast_ref::<&str>().unwrap();
        assert_eq!(
            *message,
            if fail_in_setup {
                "intentional multiuser setup failure"
            } else {
                "intentional multiuser body failure"
            }
        );
        probe.lock().unwrap().take().unwrap().assert_removed();
    }
}
