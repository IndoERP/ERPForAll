//! Functional construction admission, not a product performance measurement.
use erp_views::{DescriptionBudget, LimitKind, ViewError};

#[test]
fn simultaneous_description_allocations_share_one_sticky_budget() {
    let budget = DescriptionBudget::with_limit(10).unwrap();
    let _arch = budget.reserve(7).unwrap();
    let callback = budget.clone();
    let error = callback
        .reserve(4)
        .expect_err("aggregate admission refused");
    assert!(matches!(
        error,
        ViewError::LimitExceeded {
            limit: LimitKind::DescriptionBuildBytes,
            max: 10,
            actual: Some(11)
        }
    ));
    assert!(
        budget.reserve(0).is_err(),
        "caught error must not restore admission"
    );
}

#[test]
fn released_storage_is_reusable_and_overflow_never_wraps() {
    let budget = DescriptionBudget::with_limit(10).unwrap();
    let arch = budget.reserve(10).unwrap();
    drop(arch);
    let _payload = budget.reserve(8).unwrap();
    assert!(budget.reserve(usize::MAX).is_err());
    assert!(budget.reserve(1).is_err());
    assert!(DescriptionBudget::with_limit(0).is_err());
    assert!(DescriptionBudget::with_limit(32 * 1024 * 1024 + 1).is_err());
}

#[test]
fn concurrent_participants_cannot_publish_after_another_participant_refuses() {
    use std::sync::{Arc, Barrier};
    let budget = DescriptionBudget::with_limit(10).unwrap();
    let barrier = Arc::new(Barrier::new(3));
    let workers: Vec<_> = (0..2)
        .map(|_| {
            let shared = budget.clone();
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                let reservation = shared.reserve(6);
                barrier.wait();
                let admitted = reservation.is_ok();
                assert!(shared.check().is_err());
                admitted
            })
        })
        .collect();
    barrier.wait();
    let admitted = workers
        .into_iter()
        .map(|worker| usize::from(worker.join().unwrap()))
        .sum::<usize>();
    assert!(
        admitted <= 1,
        "concurrent participants exceeded the shared allowance"
    );
    assert!(budget.check().is_err());
}
