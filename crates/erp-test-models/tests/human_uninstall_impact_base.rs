//! Lifecycle seam tests use a trusted test authorizer, not fake authentication evidence.
#![allow(clippy::unwrap_used, clippy::panic, reason = "test assertions")]
#[path = "../../erp-modules/tests/common/mod.rs"]
mod common;
use common::*;
use erp_modules::install::{InstallAuthorization, InstallAuthorizer};
use erp_modules::system::{ModuleSystem, OperationOutcome};
use erp_modules::{InitOptions, OPERATION_LOCK};
use erp_orm::{BoxFuture, GroupSet, PrincipalBuilder, Resolution, Transaction, UserId};
use erp_registry::AddonDescriptor;
use erp_testkit::TestDb;
use std::num::NonZeroU64;
use std::sync::atomic::{AtomicUsize, Ordering};

static NOTE: AddonDescriptor = AddonDescriptor {
    name: "note",
    depends: &["base"],
    exact_only: false,
};
struct Authorizer {
    deny: bool,
    uid: i64,
    calls: AtomicUsize,
}
impl InstallAuthorizer for Authorizer {
    fn authorize<'a>(
        &'a self,
        tx: &'a mut Transaction,
    ) -> BoxFuture<'a, erp_fields::Result<InstallAuthorization>> {
        Box::pin(async move {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let locked: bool = tx.query("SELECT EXISTS (SELECT 1 FROM pg_locks WHERE locktype='advisory' AND granted AND pid=pg_backend_pid() AND classid=$1::int8::oid AND objid=$2::int8::oid AND objsubid=2)", &[&i64::from(OPERATION_LOCK.0), &i64::from(OPERATION_LOCK.1)]).await?[0].get(0);
            assert!(locked, "authority must be checked under the lifecycle lock");
            assert!(tx.commit().await.is_err(), "authorizer must not own commit");
            if self.deny {
                return Err(erp_fields::Error::access("revoked system authority"));
            }
            let principal = PrincipalBuilder::user(UserId::new(self.uid).unwrap())
                .resolution(
                    Resolution::in_id_order(GroupSet::default(), [], NonZeroU64::MIN).unwrap(),
                )
                .build()
                .unwrap();
            InstallAuthorization::new(principal, "019a0000-0000-7000-8000-000000000001".into())
        })
    }
}

fn allowed() -> Authorizer {
    Authorizer {
        deny: false,
        uid: 2,
        calls: AtomicUsize::new(0),
    }
}

/// Production base declarations and linked SQL triggers, with an isolated minimal data manifest.
/// This does not claim coverage of the shipped base data files.
async fn installed_note_with_base() -> (TestDb, Addons, ModuleSystem, OperationOutcome) {
    let db = TestDb::create().await.unwrap();
    let addons = Addons::new();
    addons.add("note", "name='Note'\ndepends=['base']\n");
    let system = builder(&db, &addons)
        .code([&erp_addon_base::ADDON, &NOTE])
        .decls([model("note", "test.note")])
        .build()
        .unwrap();
    let init = system
        .initialize(InitOptions::new().with_skip_auto_install(true))
        .await
        .unwrap();
    let mut tx = init.orm.begin(erp_orm::SUPERUSER_ID).await.unwrap();
    tx.install_signal().await.unwrap();
    tx.batch("INSERT INTO res_users(id, name) VALUES (2, 'Administrator')")
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let installed = system
        .install_authorized(&[n("note")], &allowed())
        .await
        .unwrap();
    exec(
        &installed.orm,
        "INSERT INTO test_note(id, name) VALUES (11, 'private addon row')",
    )
    .await;
    assert_eq!(rows(&installed.orm, "SELECT c.relname, t.tgname FROM pg_trigger t JOIN pg_class c ON c.oid=t.tgrelid WHERE c.relname='ir_model_fields' AND t.tgname='ir_model_fields_policy_bump' AND NOT t.tgisinternal").await,
        vec![vec![Some("ir_model_fields".into()), Some("ir_model_fields_policy_bump".into())]]);
    assert_eq!(
        rows(
            &installed.orm,
            "SELECT count(*)::text FROM ir_model WHERE model='test.note'"
        )
        .await,
        vec![vec![Some("1".into())]]
    );
    (db, addons, system, installed)
}

#[tokio::test]
async fn production_base_unsupported_schema_is_explicitly_refused() {
    let (db, _addons, system, installed) = installed_note_with_base().await;
    let before = rows(
        &installed.orm,
        "SELECT id::text, model FROM ir_model_fields WHERE model='test.note' ORDER BY id",
    )
    .await;
    let diagnostic = "Uninstall impact cannot describe custom index support on table 'ir_dated_parameter'; unsupported until a later release";
    for error in [
        system
            .preview_uninstall_authorized(&[n("note")], &allowed())
            .await
            .unwrap_err(),
        system
            .uninstall_authorized(
                &[n("note")],
                &format!("uninstall-v1:{}", "0".repeat(64)),
                &allowed(),
            )
            .await
            .unwrap_err(),
    ] {
        match error {
            erp_modules::Error::Orm(source) => {
                assert_eq!(source.kind(), erp_fields::ErrorKind::Validation);
                assert_eq!(source.message(), diagnostic);
            }
            other => panic!("expected unsupported base-schema refusal, got {other:?}"),
        }
    }
    assert_eq!(
        rows(
            &installed.orm,
            "SELECT id::text, model FROM ir_model_fields WHERE model='test.note' ORDER BY id"
        )
        .await,
        before
    );
    assert!(table_exists(&installed.orm, "test_note").await);
    assert_eq!(state(&installed.orm, "note").await, "installed");
    assert_eq!(
        rows(
            &installed.orm,
            "SELECT count(*)::text FROM erp_module_lifecycle_audit WHERE operation='uninstall'"
        )
        .await,
        vec![vec![Some("0".into())]]
    );
    db.cleanup().await.unwrap();
}
