//! Native composition must publish the installed web addon's H18 declarations.
#![allow(clippy::unwrap_used, clippy::expect_used, reason = "test assertions")]

use erp_cli::node::Node;
use erp_sdk::Context;
use erp_sdk::views::PublishedViewTypes;

/// declarations and binds the published table to that root's exact registry)
#[tokio::test]
async fn native_serving_root_publishes_web_view_types() {
    // Opening the composition does not connect; loading installed data is a separate operation.
    let pool = erp_orm::pool::create_pool(tokio_postgres::Config::new(), 1).unwrap();
    let addons = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("addons");
    let node = Node::open(pool, &[addons], None).unwrap();
    let env = node.system().system_env(Context::new());
    let published = env.generation_attachment::<PublishedViewTypes>().unwrap();
    published.require_registry(env.registry()).unwrap();
    let keys: Vec<_> = published
        .declarations()
        .map(|(_, declaration)| declaration.key())
        .collect();
    assert_eq!(keys, ["form", "list", "pivot", "search"]);
    for key in keys {
        assert!(std::ptr::eq(
            published.owner(key).unwrap(),
            &erp_addon_web::ADDON
        ));
    }
}
