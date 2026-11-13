// SPDX-License-Identifier: MIT OR Apache-2.0
//! Multi-user CRUD acceptance matrix: every installed model against the
//! administrator, two single-company users and an anonymous caller. Models whose matrix is not
//! written yet are listed by name with the task that will write it.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "integration assertions"
)]
#[path = "support/multiuser_crud.rs"]
mod multiuser_crud;
#[path = "support/multiuser_fixture.rs"]
mod multiuser_fixture;
#[path = "support/native_http.rs"]
mod native_http;

use multiuser_crud::{ABSTRACT, Crud, MANIFEST, manifest_diff, manifest_problems};
use multiuser_fixture::run;

#[test]
fn manifest_is_well_formed() {
    let problems = manifest_problems();
    assert!(
        problems.is_empty(),
        "manifest problems:\n{}",
        problems.join("\n")
    );
    assert!(!ABSTRACT.is_empty());
}

#[test]
fn installed_models_match_the_manifest() {
    run(|fixture| {
        let installed = Crud::new(fixture).installed_models();
        let diff = manifest_diff(&installed);
        assert!(
            diff.is_empty(),
            "installed models differ from the manifest:\n{}",
            diff.join("\n")
        );
    });
}

#[test]
fn anonymous_is_refused_on_every_model() {
    run(|fixture| {
        let crud = Crud::new(fixture);
        let mismatches: Vec<String> = MANIFEST
            .iter()
            .flat_map(|case| crud.anonymous_probe(case))
            .collect();
        assert!(
            mismatches.is_empty(),
            "{} anonymous mismatches:\n{}",
            mismatches.len(),
            mismatches.join("\n")
        );
    });
}

#[test]
fn product_and_uom_crud_matrix() {
    run(|fixture| {
        let crud = Crud::new(fixture);
        let (mut mismatches, deferred) = crud.family_matrix();
        mismatches.extend(crud.mixed_batch());
        eprintln!(
            "multiuser CRUD: {deferred} authenticated cells deferred to the CRUD child tasks"
        );
        assert!(
            mismatches.is_empty(),
            "{} CRUD mismatches:\n{}",
            mismatches.len(),
            mismatches.join("\n")
        );
    });
}
