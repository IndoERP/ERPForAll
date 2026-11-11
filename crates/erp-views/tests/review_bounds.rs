//! Regressions for independent review RT-1 / RT-2.
#![allow(clippy::unwrap_used, clippy::panic, reason = "test assertions")]
use erp_views::{LimitKind, ViewCatalogue, ViewDescriptor, ViewError, ViewLimits};
use std::error::Error as _;

#[test]
fn root_only_combinations_spend_the_batch_work_budget() {
    let limits = ViewLimits::default()
        .lowered(LimitKind::BatchSteps, 1)
        .unwrap();
    let catalogue =
        ViewCatalogue::with_limits(vec![ViewDescriptor::new(1, "root", "m", "<form/>")], limits)
            .unwrap();
    let error = catalogue.combine_many(&[1, 1]).unwrap_err();
    assert_eq!(
        error,
        ViewError::InView {
            view_id: 1,
            view_name: "root".to_owned(),
            source: Box::new(ViewError::LimitExceeded {
                limit: LimitKind::BatchSteps,
                max: 1,
                actual: Some(2),
            }),
        }
    );
    assert_eq!(error.to_string(), "error in view 'root' (id 1)");
    assert_eq!(
        error.source().unwrap().to_string(),
        "evaluation steps per batch of combinations limit exceeded: maximum 1, reached 2"
    );
}

#[test]
fn catalogue_bytes_include_names_models_and_arches() {
    let limits = ViewLimits::default()
        .lowered(LimitKind::CatalogueBytes, 10)
        .unwrap();
    let error =
        ViewCatalogue::with_limits(vec![ViewDescriptor::new(1, "root", "m", "<form/>")], limits)
            .unwrap_err();
    assert_eq!(
        error,
        ViewError::LimitExceeded {
            limit: LimitKind::CatalogueBytes,
            max: 10,
            actual: Some(12),
        }
    );
    assert_eq!(
        error.to_string(),
        "text bytes in one catalogue limit exceeded: maximum 10, reached 12"
    );
}

#[test]
fn retained_outputs_share_one_byte_budget() {
    let limits = ViewLimits::default()
        .lowered(LimitKind::BatchBytes, 4096)
        .unwrap();
    let catalogue =
        ViewCatalogue::with_limits(vec![ViewDescriptor::new(1, "root", "m", "<form/>")], limits)
            .unwrap();
    assert_eq!(catalogue.combine_many(&[1]).unwrap().len(), 1);
    let error = catalogue.combine_many(&[1; 20]).unwrap_err();
    // Storage capacities depend on the platform's layout. Assert the actual observed charge
    // crossed the cap and retain the complete diagnostic, rather than pinning a pointer size.
    let ViewError::LimitExceeded {
        limit,
        max,
        actual: Some(actual),
    } = &error
    else {
        panic!("expected a retained-byte limit, got {error:?}");
    };
    assert_eq!(*limit, LimitKind::BatchBytes);
    assert_eq!(*max, 4096);
    assert!(*actual > *max);
    assert_eq!(
        error.to_string(),
        format!(
            "retained bytes per batch of combinations limit exceeded: maximum 4096, reached {actual}"
        )
    );
}

#[test]
fn catalogue_byte_limit_accepts_its_exact_boundary() {
    let limits = ViewLimits::default()
        .lowered(LimitKind::CatalogueBytes, 12)
        .unwrap();
    let catalogue =
        ViewCatalogue::with_limits(vec![ViewDescriptor::new(1, "root", "m", "<form/>")], limits)
            .unwrap();
    assert_eq!(catalogue.combine(1).unwrap().to_xml(), "<form/>");
}

#[test]
fn configured_group_reference_limit_is_enforced_across_attributes() {
    use erp_views::validate::{ArchIssue, group_refs_with_limits};
    let limits = ViewLimits::default()
        .lowered(LimitKind::CheckGroupRefs, 1)
        .unwrap();
    let catalogue = ViewCatalogue::with_limits(
        vec![ViewDescriptor::new(
            1,
            "root",
            "m",
            "<form><a groups=\"base.a\"/><b groups=\"base.a,base.b\"/></form>",
        )],
        limits,
    )
    .unwrap();
    let combined = catalogue.combine(1).unwrap();
    let error = group_refs_with_limits(combined.arch(), catalogue.limits()).unwrap_err();
    assert_eq!(
        error,
        ArchIssue::TooManyGroupReferences {
            count: 2,
            max: 1,
            origin: combined.arch().root().children().nth(1).unwrap().origin(),
        }
    );
    assert_eq!(
        error.to_string(),
        "The view lists 2 distinct groups, more than the 1 allowed"
    );
}
