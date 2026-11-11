//! A primary-read declaration that names a method which is not an exposed read method refuses the
//! exposure build. The entry is
//! process-wide, so this test has a binary of its own.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

use erp_sdk::rpc::PrimaryRead;
use erp_testkit::Harness;

#[erp_sdk::__private::linkme::distributed_slice(erp_sdk::rpc::PRIMARY_READS)]
#[linkme(crate = erp_sdk::__private::linkme)]
static NOT_A_METHOD: PrimaryRead = PrimaryRead {
    addon: "mail",
    model: "mail.store",
    method: "no_such_method",
};

#[tokio::test]
async fn a_primary_read_of_a_missing_method_refuses_the_build() {
    let error = Harness::builder([&erp_addon_base::ADDON, &erp_addon_mail::ADDON])
        .build()
        .await
        .err()
        .expect("the build must refuse the entry");
    let text = error.to_string();
    assert!(
        text.contains(
            "primary read 'mail.store.no_such_method' (addon 'mail') is not an exposed method with \
             effect = read"
        ),
        "{text}"
    );
}
