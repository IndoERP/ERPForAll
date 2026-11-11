//! An item of the wrong type under an extension slot is an error naming addon and slot, never
//! skipped ((a).3, Validation 1). Process-wide, so a binary of its own.
#![allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]

use erp_addon_mail::store_fetch::{StoreTopic, TOPIC_SLOT, TopicRegistry};
use erp_sdk::extensions::Extension;

static NOT_A_TOPIC: u32 = 7;

#[erp_sdk::__private::linkme::distributed_slice(erp_sdk::extensions::EXTENSIONS)]
#[linkme(crate = erp_sdk::__private::linkme)]
static WRONG: Extension = Extension::new(TOPIC_SLOT, "mail", &NOT_A_TOPIC);

#[test]
fn an_item_of_the_wrong_type_is_an_error() {
    let message = "addon 'mail' contributes an item of the wrong type to the extension slot \
                   'mail.store_topic'";
    let error = erp_sdk::extensions::items::<StoreTopic>(TOPIC_SLOT).unwrap_err();
    assert_eq!(error.kind(), erp_sdk::ErrorKind::Programming);
    assert_eq!(error.message(), message);
    assert_eq!(TopicRegistry::linked().unwrap_err().message(), message);
}
