//! The linked topic registry refuses a contribution that names two addons ((a).4). The
//! contribution is process-wide, so this test has a binary of its own.
#![allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]

use erp_addon_mail::store_fetch::{
    StoreTopic, TOPIC_SLOT, TopicFuture, TopicInput, TopicOutput, TopicRegistry,
};
use erp_sdk::Transaction;
use erp_sdk::extensions::Extension;

fn noop<'a>(
    _tx: &'a mut Transaction,
    _i: &'a TopicInput<'a>,
    _out: &'a mut TopicOutput,
) -> TopicFuture<'a> {
    Box::pin(async { Ok(()) })
}

static TOPIC: StoreTopic = StoreTopic::new("mail", "x.topic", noop);

#[erp_sdk::__private::linkme::distributed_slice(erp_sdk::extensions::EXTENSIONS)]
#[linkme(crate = erp_sdk::__private::linkme)]
static LYING: Extension = Extension::new(TOPIC_SLOT, "other_addon", &TOPIC);

#[test]
fn a_contribution_that_names_two_addons_is_refused() {
    let error = TopicRegistry::linked().unwrap_err();
    assert_eq!(error.kind(), erp_sdk::ErrorKind::Programming);
    assert_eq!(
        error.message(),
        "store topic 'x.topic' is contributed by addon 'other_addon' but names addon 'mail'"
    );
    let error = TopicRegistry::require_linked(&[]).unwrap_err();
    assert_eq!(error.kind(), erp_sdk::ErrorKind::Programming);
}
