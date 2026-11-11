//! Security review of the integration (S5 to S12): tables an uninstall must keep, bounded
//! readers, operation limits and the modules model's guarded columns.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

mod common;

use common::*;
use erp_modules::error::Error;
use erp_modules::system::InitOptions;
use erp_registry::{FieldDecl, ModelDecl};
use erp_testkit::TestDb;

/// A model of `addon` on the explicit `table`.
fn model_on(addon: &'static str, name: &'static str, table: &'static str) -> &'static ModelDecl {
    let fields: &'static [FieldDecl] = Box::leak(vec![FieldDecl::char("name")].into_boxed_slice());
    Box::leak(Box::new(
        ModelDecl::new(addon, name)
            .description("Test model")
            .table(table)
            .fields(fields)
            .at(file!(), line!()),
    ))
}

// ---- S6: an uninstall never drops a table a kept model still maps to ------------------------

/// S6: model `test.drop` of `b` and model `test.keep` of `a` both map to `shared_t`; uninstalling
/// `b` would drop the kept model's table, so it is refused naming the table and both models, and
/// nothing changes.
#[tokio::test]
async fn uninstall_refuses_to_drop_a_table_a_kept_model_uses() {
    let addons = Addons::new();
    addons
        .add("a", "name = 'A'\n")
        .add("b", "name = 'B'\ndepends = ['a']\n");
    let db = TestDb::create().await.unwrap();
    let system = builder(&db, &addons)
        .decls([
            model_on("a", "test.keep", "shared_t"),
            model_on("b", "test.drop", "shared_t"),
        ])
        .build()
        .unwrap();
    let init = system
        .initialize(InitOptions::new().with_skip_auto_install(true))
        .await
        .unwrap();
    let env = su(&init.orm);
    let installed = system
        .button_immediate_install(&env, &[n("b")])
        .await
        .unwrap();
    assert!(table_exists(&installed.orm, "shared_t").await);
    let err = system
        .button_immediate_uninstall(&env, &[n("b")])
        .await
        .unwrap_err();
    assert!(
        matches!(&err, Error::SharedSchemaTable { table, dropped, kept, .. }
            if table == "shared_t" && dropped == "model test.drop" && kept == "model test.keep"),
        "{err:?}"
    );
    assert_eq!(
        err.to_string(),
        "Cannot remove the schema of modules b: table shared_t is used by model test.drop, which \
         is removed, and also by model test.keep, which stays"
    );
    assert!(table_exists(&installed.orm, "shared_t").await);
    assert_eq!(state(&installed.orm, "b").await, "installed");
    drop(installed);
    db.cleanup().await.unwrap();
}

/// S6: a removed model that maps to a table of the module system's own models (here
/// `ir_module_module`) is refused the same way; the module system's tables are never dropped.
#[tokio::test]
async fn uninstall_never_drops_a_table_of_the_module_system() {
    let addons = Addons::new();
    addons.add("b", "name = 'B'\n");
    let db = TestDb::create().await.unwrap();
    let system = builder(&db, &addons)
        .decls([model_on("b", "test.drop", "ir_module_module")])
        .build()
        .unwrap();
    let init = system
        .initialize(InitOptions::new().with_skip_auto_install(true))
        .await
        .unwrap();
    let env = su(&init.orm);
    let installed = system
        .button_immediate_install(&env, &[n("b")])
        .await
        .unwrap();
    let err = system
        .button_immediate_uninstall(&env, &[n("b")])
        .await
        .unwrap_err();
    assert_eq!(
        err.to_string(),
        "Cannot remove the schema of modules b: table ir_module_module is used by model \
         test.drop, which is removed, and also by model ir.module.module, which stays"
    );
    assert!(table_exists(&installed.orm, "ir_module_module").await);
    drop(installed);
    db.cleanup().await.unwrap();
}

// ---- S7: manifests are read with the bounded reader -----------------------------------------

use erp_modules::manifest::ModuleName;
use erp_modules::{AddonIndex, read_manifest_from_dir};

fn module(name: &str) -> ModuleName {
    ModuleName::new(name).unwrap()
}

/// S7: a `manifest.toml` that is a symbolic link is refused (also a dangling one: it must not
/// make the addon vanish), naming the file.
#[test]
fn a_symlinked_manifest_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    std::fs::write(outside.path().join("m.toml"), "name = 'X'\n").unwrap();
    let addon = dir.path().join("x");
    std::fs::create_dir_all(&addon).unwrap();
    std::os::unix::fs::symlink(outside.path().join("m.toml"), addon.join("manifest.toml")).unwrap();
    let err = read_manifest_from_dir(&addon, &module("x")).unwrap_err();
    assert!(
        matches!(&err, Error::DiscoveryError { path, .. } if *path == addon.join("manifest.toml")),
        "{err:?}"
    );
    assert_eq!(
        err.to_string(),
        format!(
            "Discovery error at '{}': manifest.toml is a symbolic link; a manifest must be a \
             regular file of the addon",
            addon.join("manifest.toml").display()
        )
    );
    // a link to nothing is refused too, not read as "no manifest"
    let dangling = dir.path().join("y");
    std::fs::create_dir_all(&dangling).unwrap();
    std::os::unix::fs::symlink(
        outside.path().join("gone.toml"),
        dangling.join("manifest.toml"),
    )
    .unwrap();
    let err = read_manifest_from_dir(&dangling, &module("y")).unwrap_err();
    assert!(matches!(&err, Error::DiscoveryError { .. }), "{err:?}");
    // discovery reports it as well
    let mut index = AddonIndex::new(true);
    index.add_dev_path(dir.path());
    let err = index.discover_entries().unwrap_err();
    assert!(
        err.to_string().contains("manifest.toml is a symbolic link"),
        "{err}"
    );
}

/// S7: a manifest over the size cap is refused with the cap in the message; one at the cap is
/// read (and then judged as a manifest).
#[test]
fn an_oversized_manifest_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let addon = dir.path().join("x");
    std::fs::create_dir_all(&addon).unwrap();
    let cap = usize::try_from(erp_modules::discovery::MAX_MANIFEST_BYTES).unwrap();
    let base = "name = 'X'\n# ";
    let padding = |len: usize| format!("{base}{}\n", "p".repeat(len - base.len() - 1));
    std::fs::write(addon.join("manifest.toml"), padding(cap + 1)).unwrap();
    let err = read_manifest_from_dir(&addon, &module("x")).unwrap_err();
    assert_eq!(
        err.to_string(),
        format!(
            "Discovery error at '{}': manifest.toml is larger than 262144 bytes",
            addon.join("manifest.toml").display()
        )
    );
    std::fs::write(addon.join("manifest.toml"), padding(cap)).unwrap();
    let manifest = read_manifest_from_dir(&addon, &module("x"))
        .unwrap()
        .unwrap();
    assert_eq!(manifest.name().as_str(), "x");
}

/// S7: a manifest that is a hard link is refused.
#[test]
fn a_hard_linked_manifest_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    std::fs::write(outside.path().join("m.toml"), "name = 'X'\n").unwrap();
    let addon = dir.path().join("x");
    std::fs::create_dir_all(&addon).unwrap();
    std::fs::hard_link(outside.path().join("m.toml"), addon.join("manifest.toml")).unwrap();
    let err = read_manifest_from_dir(&addon, &module("x")).unwrap_err();
    assert_eq!(
        err.to_string(),
        format!(
            "Discovery error at '{}': manifest.toml has 2 hard links; a manifest must belong to \
             the addon only",
            addon.join("manifest.toml").display()
        )
    );
}

// ---- S8: the module rows' state, version and demo flag are not writable through the ORM ----

/// S8: an ORM `write` or `create` that names `state`, `latest_version` or `demo` on
/// `ir.module.module` is refused (only the module system's own SQL writes them); other fields
/// (`sequence`) still go through.
#[tokio::test]
async fn module_rows_guard_state_version_and_demo() {
    use erp_fields::{ErrorKind, Value};
    use std::collections::BTreeMap;

    let addons = Addons::new();
    let db = TestDb::create().await.unwrap();
    let system = builder(&db, &addons).build().unwrap();
    let init = system
        .initialize(InitOptions::new().with_skip_auto_install(true))
        .await
        .unwrap();
    let mut tx = init.orm.begin(erp_orm::SUPERUSER_ID).await.unwrap();
    let modules = tx.env().records("ir.module.module").unwrap();
    let id: i64 = tx
        .query("SELECT id FROM ir_module_module WHERE name = 'base'", &[])
        .await
        .unwrap()[0]
        .get(0);
    let base = modules.browse([id]).unwrap();
    let one = |name: &str, value: Value| BTreeMap::from([(name.to_owned(), value)]);
    let err = base
        .write_values(&mut tx, &one("state", Value::str("to remove")))
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Access);
    assert_eq!(
        err.message(),
        "ir.module.module: field state is written only by the module system \
         (erp_modules::ModuleSystem operations)"
    );
    let both = BTreeMap::from([
        ("latest_version".to_owned(), Value::str("9.9")),
        ("demo".to_owned(), Value::Bool(true)),
    ]);
    let err = base.write_values(&mut tx, &both).await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Access);
    assert_eq!(
        err.message(),
        "ir.module.module: fields latest_version, demo are written only by the module system \
         (erp_modules::ModuleSystem operations)"
    );
    let mut create = one("name", Value::str("faked"));
    create.insert("state".to_owned(), Value::str("installed"));
    let err = modules.create_values(&mut tx, &[create]).await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Access);
    assert_eq!(
        err.message(),
        "ir.module.module: field state is written only by the module system \
         (erp_modules::ModuleSystem operations)"
    );
    // an unguarded field is still writable
    base.write_values(&mut tx, &one("sequence", Value::Int(7)))
        .await
        .unwrap();
    tx.rollback().await.unwrap();
    drop(tx);
    assert_eq!(state(&init.orm, "base").await, "installed");
    db.cleanup().await.unwrap();
}

// ---- S3: the post-commit queue is closed to every role but its owner ------------------------

/// S3: initialization revokes every privilege on the queue from `PUBLIC` (here a database whose
/// default privileges would have granted them), and `revoke_post_commit_access` takes them from
/// the request role; the owner and unknown input are refused.
#[tokio::test]
async fn the_post_commit_queue_is_closed_to_public_and_the_request_role() {
    use erp_fields::ErrorKind;

    let addons = Addons::new();
    let db = TestDb::create().await.unwrap();
    let client = db.pool().get().await.unwrap();
    client
        .batch_execute("ALTER DEFAULT PRIVILEGES IN SCHEMA public GRANT ALL ON TABLES TO PUBLIC")
        .await
        .unwrap();
    let system = builder(&db, &addons).build().unwrap();
    let init = system
        .initialize(InitOptions::new().with_skip_auto_install(true))
        .await
        .unwrap();
    let public_grants = |table: &'static str| {
        let sql = format!(
            "SELECT count(*)::text FROM pg_class c, aclexplode(c.relacl) a \
             WHERE c.oid = '{table}'::regclass AND a.grantee = 0"
        );
        let orm = init.orm.clone();
        async move { rows(&orm, &sql).await[0][0].clone().unwrap() }
    };
    assert_ne!(
        public_grants("ir_module_module").await,
        "0",
        "control: the default privileges reached the other tables"
    );
    assert_eq!(public_grants("ir_module_post_commit").await, "0");

    let role = format!("erp_pc_req_{}", std::process::id());
    client
        .batch_execute(&format!(
            "DROP ROLE IF EXISTS {role}; CREATE ROLE {role} NOLOGIN; \
             GRANT SELECT, INSERT ON ir_module_post_commit TO {role}"
        ))
        .await
        .unwrap();
    let can = |privilege: &'static str| {
        let sql = format!(
            "SELECT has_table_privilege('{role}', 'ir_module_post_commit', '{privilege}')::text"
        );
        let orm = init.orm.clone();
        async move { rows(&orm, &sql).await[0][0].clone().unwrap() }
    };
    assert_eq!(can("INSERT").await, "true");

    // a user transaction is refused, the System one revokes
    let mut user = init
        .orm
        .begin(erp_orm::UserId::new(2).unwrap())
        .await
        .unwrap();
    let err = erp_modules::revoke_post_commit_access(&mut user, &role)
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Access);
    assert_eq!(
        err.message(),
        "revoke_post_commit_access: only the System principal with full elevation may do this \
         (this one acts as user 2)"
    );
    user.rollback().await.unwrap();
    drop(user);
    assert_eq!(can("INSERT").await, "true", "nothing was revoked");
    let mut tx = init.orm.begin(erp_orm::SUPERUSER_ID).await.unwrap();
    erp_modules::revoke_post_commit_access(&mut tx, &role)
        .await
        .unwrap();
    // idempotent
    erp_modules::revoke_post_commit_access(&mut tx, &role)
        .await
        .unwrap();
    let owner: String = tx
        .query(
            "SELECT pg_get_userbyid(relowner)::text FROM pg_class \
             WHERE oid = 'ir_module_post_commit'::regclass",
            &[],
        )
        .await
        .unwrap()[0]
        .get(0);
    let err = erp_modules::revoke_post_commit_access(&mut tx, &owner)
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Programming);
    assert_eq!(
        err.message(),
        format!("role {owner:?} owns ir_module_post_commit: the request role must be another role")
    );
    let err = erp_modules::revoke_post_commit_access(&mut tx, "")
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Programming);
    assert_eq!(
        err.message(),
        "invalid role name \"\": 1 to 63 bytes without NUL"
    );
    tx.commit().await.unwrap();
    drop(tx);
    assert_eq!(can("INSERT").await, "false");
    assert_eq!(can("SELECT").await, "false");
    client
        .batch_execute(&format!("DROP ROLE {role}"))
        .await
        .unwrap();
    drop(client);
    db.cleanup().await.unwrap();
}
