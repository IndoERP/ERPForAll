//! A primary-read entry of an addon that is not installed is ignored ((e).1,
//! Validation 6). Process-wide, so a binary of its own.
#![allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]

use erp_sdk::rpc::{Exposure, PrimaryRead};
use erp_testkit::Harness;

#[erp_sdk::__private::linkme::distributed_slice(erp_sdk::rpc::PRIMARY_READS)]
#[linkme(crate = erp_sdk::__private::linkme)]
static GHOST: PrimaryRead = PrimaryRead {
    addon: "ghost_addon",
    model: "no.such",
    method: "nothing",
};

#[tokio::test]
async fn an_entry_of_an_uninstalled_addon_is_ignored() {
    let h = Harness::builder([&erp_addon_base::ADDON, &erp_addon_mail::ADDON])
        .build()
        .await
        .expect("the build ignores the entry");
    let id = h.registry().model_id("mail.store").unwrap();
    match h.orm().exposure().get(id, "store_fetch") {
        Some(Exposure::Exposed(e)) => assert!(e.primary_read()),
        other => panic!("store_fetch is not exposed: {other:?}"),
    }
    h.cleanup().await.unwrap();
}
