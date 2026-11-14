//! Stored label migration acceptance.
//!
//! The always-run test is explicitly a current-schema lifecycle fixture with an
//! old stored value/version marker. It is NOT genuine previous-binary evidence.
//! The ignored native-CLI test below installs with a pinned previous binary and
//! verifies the old formula before the candidate upgrade; no marker rewind there.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "fixture assertions"
)]

#[path = "support/runtime_addons.rs"]
mod runtime_addons;

use std::{
    collections::BTreeMap,
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Command, Output},
};

use erp_modules::{
    AddonIndex, ModuleStep,
    manifest::ModuleName,
    migrations::{MigrationImpl, MigrationStage},
    system::{InitOptions, ModuleSystem},
    version::SemverVersion,
};
use erp_sdk::{BoxFuture, Context, Error, Result, Transaction};
use erp_testkit::{
    TestDb,
    tokio_postgres::{Client, config::Host},
};

// This native fixture is active only in the focused current-schema tests below.
// Source text is deliberately distinct from diagnostics to catch contact-data leaks.
mod bound_fixture {
    use erp_addon_base::ResPartner;
    use erp_sdk::prelude::*;

    erp_sdk::addon!(name = "test_partner_label_bounds", depends = ["base"]);

    #[extend(ResPartner, addon = "test_partner_label_bounds")]
    pub struct LabelBoundsFixture;

    #[methods(ResPartner, addon = "test_partner_label_bounds")]
    impl LabelBoundsFixture {
        #[method(override)]
        async fn _get_complete_name(
            this: &RecordSet<ResPartner>,
            tx: &mut Transaction,
            sup: Super,
        ) -> Result<Vec<Arc<str>>> {
            let references = this.mapped(tx, ResPartner::REF).await?;
            let mut labels: Vec<Arc<str>> = sup.call(tx, this.as_records(), ()).await?;
            for (reference, label) in references.into_iter().zip(&mut labels) {
                match reference.as_deref() {
                    Some("label-bound-raise") => {
                        return Err(Error::validation(
                            "Contact label output exceeds the 1048576-byte limit",
                        ));
                    }
                    Some("label-bound-output") => *label = Arc::from("x".repeat(1_048_577)),
                    Some("label-bound-programming") => {
                        return Err(Error::programming(
                            "Contact label output exceeds the 1048576-byte limit",
                        ));
                    }
                    Some("label-bound-other-validation") => {
                        return Err(Error::validation("fixture unrelated helper refusal"));
                    }
                    _ => {} // All other fixture records use the ordinary native helper.
                }
            }
            Ok(labels)
        }
    }
}

const OLD_SOURCE: &str = "d63b60d7489c1deee4ee8ec1a7f47bbb7fbb293e";
const EXPECTED_LABEL: &str = "Tenant Parent, Tenant Child";
const OLD_LABEL: &str = "Tenant Child";

fn addons() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("addons")
}

fn current_base_version() -> String {
    erp_modules::manifest::Manifest::from_toml(
        "base",
        &fs::read_to_string(addons().join("base/manifest.toml")).unwrap(),
    )
    .unwrap()
    .version()
    .to_string()
}

fn late_check<'a>(tx: &'a mut Transaction, _from: &'a SemverVersion) -> BoxFuture<'a, Result<()>> {
    Box::pin(async move {
        tx.flush().await?;
        let row = tx.query("SELECT p.complete_name, f.refuse FROM labels_upgrade_fixture f JOIN res_partner p ON p.id=f.child", &[]).await?;
        if row.len() != 1 {
            return Err(Error::programming(
                "label upgrade fixture expected one migration probe",
            ));
        }
        let label: String = row[0]
            .try_get(0)
            .map_err(|e| Error::db(format!("label upgrade fixture value: {e}")))?;
        eprintln!("label_migration_probe actual={label:?} expected={EXPECTED_LABEL:?}");
        if label != EXPECTED_LABEL {
            return Err(Error::programming(
                "label upgrade fixture observed stale complete_name after post migration",
            ));
        }
        tx.execute("UPDATE labels_upgrade_fixture SET reached=true", &[])
            .await?;
        let refuse: bool = row[0]
            .try_get(1)
            .map_err(|e| Error::db(format!("label upgrade fixture refusal: {e}")))?;
        if refuse {
            return Err(Error::validation("fixture late label failure"));
        }
        Ok(())
    })
}

static LATE_CHECK: MigrationImpl = MigrationImpl::new(
    "base",
    "1.2",
    MigrationStage::End,
    "fixture-labels-late-check",
    late_check,
);

async fn current_system(db: &TestDb) -> ModuleSystem {
    let mut index = AddonIndex::new(true);
    index.add_dev_path(addons());
    let identities = index
        .discover_entries()
        .unwrap()
        .into_iter()
        .map(|(name, entry)| {
            let snapshot = erp_modules::artifact_identity::ArtifactSnapshot::capture(
                name.as_str(),
                Path::new(entry.source_location()),
            )
            .unwrap();
            (name, snapshot.digest().to_owned())
        })
        .collect();
    ModuleSystem::builder(db.pool().clone(), index)
        .native_artifact_identities(identities)
        .require_core_tables(erp_addon_base::partner_hierarchy::core_tables())
        .code([&erp_addon_base::ADDON])
        .migrations([&LATE_CHECK])
        .build()
        .unwrap()
}

async fn bound_fixture_system(db: &TestDb, fixture_root: &Path) -> ModuleSystem {
    let mut index = AddonIndex::new(true);
    index.add_dev_path(addons());
    index.add_dev_path(fixture_root);
    let identities = index
        .discover_entries()
        .unwrap()
        .into_iter()
        .map(|(name, entry)| {
            let snapshot = erp_modules::artifact_identity::ArtifactSnapshot::capture(
                name.as_str(),
                Path::new(entry.source_location()),
            )
            .unwrap();
            (name, snapshot.digest().to_owned())
        })
        .collect();
    ModuleSystem::builder(db.pool().clone(), index)
        .native_artifact_identities(identities)
        .require_core_tables(erp_addon_base::partner_hierarchy::core_tables())
        .code([&erp_addon_base::ADDON, &bound_fixture::ADDON])
        .build()
        .unwrap()
}

async fn assert_bound_migration_identifies_partner_and_rolls_back(mode: &str) {
    let db = TestDb::create().await.unwrap();
    let fixture_root = tempfile::tempdir().unwrap();
    let fixture_root_path = fixture_root.path().canonicalize().unwrap();
    let fixture = fixture_root_path.join("test_partner_label_bounds");
    fs::create_dir(&fixture).unwrap();
    fs::write(
        fixture.join("manifest.toml"),
        "name='Partner label bounds fixture'\nversion='1.0'\ndepends=['base']\ndata=[]\n",
    )
    .unwrap();
    let system = bound_fixture_system(&db, &fixture_root_path).await;
    let initialized = system
        .initialize(InitOptions::new().with_skip_auto_install(true))
        .await
        .unwrap();
    assert!(initialized.post_commit.is_clean());
    let env = system.system_env(Context::new());
    let installed = system
        .button_immediate_install(
            &env,
            &[ModuleName::new("test_partner_label_bounds").unwrap()],
        )
        .await
        .unwrap();
    assert!(installed.post_commit.is_clean());
    let client = db.client().await.unwrap();
    // This fixture deliberately seeds CURRENT-schema stale labels and rewinds the
    // version marker. It does not claim a genuine prior-binary installation.
    client.execute(
        "INSERT INTO res_partner (name, type, active, is_company, partner_share, commercial_company_name, complete_name, ref) SELECT 'Private contact ' || n::text, 'other', false, false, true, '', 'stale label', 'label-bound-window' FROM generate_series(1, 270) AS n",
        &[],
    ).await.unwrap();
    client
        .execute(
            "UPDATE res_partner SET commercial_partner_id=id WHERE ref='label-bound-window'",
            &[],
        )
        .await
        .unwrap();
    let before: Vec<(i64, String)> = client
        .query(
            "SELECT id, complete_name FROM res_partner WHERE ref='label-bound-window' ORDER BY id",
            &[],
        )
        .await
        .unwrap()
        .into_iter()
        .map(|row| (row.get(0), row.get(1)))
        .collect();
    assert_eq!(before.len(), 270);
    let offender = before.last().unwrap().0;
    client
        .execute(
            "UPDATE res_partner SET ref=$1 WHERE id=$2",
            &[&mode, &offender],
        )
        .await
        .unwrap();
    client.execute("INSERT INTO ir_model_data (module, name, model, res_id, noupdate) VALUES ('test_partner_label_bounds', 'offender', 'res.partner', $1, true)", &[&offender]).await.unwrap();
    client
        .execute(
            "UPDATE ir_module_module SET latest_version='1.1.0' WHERE name='base'",
            &[],
        )
        .await
        .unwrap();
    let failure = match system
        .button_immediate_upgrade(&env, &[ModuleName::new("base").unwrap()])
        .await
    {
        Err(error) => error,
        Ok(outcome) => {
            drop(outcome);
            panic!("oversized migration label unexpectedly committed")
        }
    };
    let source = match failure {
        erp_modules::Error::StepFailed {
            module,
            step: ModuleStep::Migration {
                stage: MigrationStage::Post,
            },
            source,
            ..
        } => {
            assert_eq!(module.as_str(), "base");
            match *source {
                erp_modules::Error::MigrationFailed {
                    version,
                    stage,
                    source,
                    ..
                } => {
                    assert_eq!(version, SemverVersion::new(1, 2, 0));
                    assert_eq!(stage, MigrationStage::Post);
                    source
                }
                other => panic!("expected migration failure, got {other:?}"),
            }
        }
        other => panic!("expected post migration failure, got {other:?}"),
    };
    match mode {
        "label-bound-programming" => {
            assert_eq!(source.kind(), erp_sdk::ErrorKind::Programming);
            assert_eq!(
                source.message(),
                "Contact label output exceeds the 1048576-byte limit"
            );
        }
        "label-bound-other-validation" => {
            assert_eq!(source.kind(), erp_sdk::ErrorKind::Validation);
            assert_eq!(source.message(), "fixture unrelated helper refusal");
        }
        "label-bound-raise" | "label-bound-output" => {
            assert_eq!(source.kind(), erp_sdk::ErrorKind::Validation);
            assert_eq!(
                source.message(),
                format!(
                    "base 1.2 label recomputation failed for res.partner {offender}: Contact label output exceeds the 1048576-byte limit"
                )
            );
        }
        _ => panic!("unsupported bound fixture mode"),
    }
    assert!(!source.message().contains("Private contact"));
    assert_eq!(module_version(&client).await, "1.1.0");
    let after: Vec<(i64, String)> = client.query(
        "SELECT id, complete_name FROM res_partner WHERE ref IN ('label-bound-window', $1) ORDER BY id",
        &[&mode],
    ).await.unwrap().into_iter().map(|row| (row.get(0), row.get(1))).collect();
    assert_eq!(
        after, before,
        "earlier windows and the offender roll back together"
    );
    assert!(
        client
            .query_one(
                "SELECT NOT active FROM res_partner WHERE id=$1",
                &[&offender]
            )
            .await
            .unwrap()
            .get::<_, bool>(0)
    );
    assert_eq!(client.query_one("SELECT res_id FROM ir_model_data WHERE module='test_partner_label_bounds' AND name='offender'", &[]).await.unwrap().get::<_, i64>(0), offender);
    drop(client);
    drop(installed);
    drop(initialized);
    drop(system);
    db.cleanup().await.unwrap();
}

#[tokio::test]
async fn current_schema_migration_helper_size_refusal_names_partner_and_rolls_back() {
    assert_bound_migration_identifies_partner_and_rolls_back("label-bound-raise").await;
}

#[tokio::test]
async fn current_schema_migration_oversized_output_names_partner_and_rolls_back() {
    assert_bound_migration_identifies_partner_and_rolls_back("label-bound-output").await;
}

#[tokio::test]
async fn current_schema_migration_preserves_programming_error_kind() {
    assert_bound_migration_identifies_partner_and_rolls_back("label-bound-programming").await;
}

#[tokio::test]
async fn current_schema_migration_preserves_unrelated_validation_message() {
    assert_bound_migration_identifies_partner_and_rolls_back("label-bound-other-validation").await;
}

async fn label(client: &Client, child: i64) -> String {
    client
        .query_one(
            "SELECT complete_name FROM res_partner WHERE id=$1",
            &[&child],
        )
        .await
        .unwrap()
        .get(0)
}

async fn module_version(client: &Client) -> String {
    client
        .query_one(
            "SELECT latest_version FROM ir_module_module WHERE name='base'",
            &[],
        )
        .await
        .unwrap()
        .get(0)
}

async fn child_identity(client: &Client, child: i64) -> (String, String, String, String, bool) {
    let row = client
        .query_one(
            "SELECT name, email, phone, street, active FROM res_partner WHERE id=$1",
            &[&child],
        )
        .await
        .unwrap();
    (row.get(0), row.get(1), row.get(2), row.get(3), row.get(4))
}

async fn contact_count(client: &Client) -> i64 {
    client
        .query_one("SELECT count(*) FROM res_partner", &[])
        .await
        .unwrap()
        .get(0)
}

async fn window_ids(client: &Client) -> Vec<i64> {
    client
        .query(
            "SELECT id FROM res_partner WHERE ref='fixture-label-window' ORDER BY id",
            &[],
        )
        .await
        .unwrap()
        .into_iter()
        .map(|row| row.get(0))
        .collect()
}

async fn assert_window_labels(client: &Client, migrated: bool) {
    let rows = client
        .query(
            "SELECT id, name, complete_name, active FROM res_partner WHERE ref='fixture-label-window' ORDER BY id",
            &[],
        )
        .await
        .unwrap();
    assert_eq!(
        rows.len(),
        270,
        "every archived fixture sibling is retained"
    );
    for row in rows {
        let id: i64 = row.get(0);
        let name: String = row.get(1);
        let label: String = row.get(2);
        let expected = if migrated {
            format!("Tenant Parent, {name}")
        } else {
            name
        };
        assert_eq!(label, expected, "archived sibling id={id}");
        assert!(!row.get::<_, bool>(3), "archived flag retained for id={id}");
    }
}

fn assert_late_failure(error: erp_modules::Error) {
    let outer_text = error.to_string();
    let inner = match error {
        erp_modules::Error::StepFailed {
            module,
            step,
            file,
            source,
        } => {
            assert_eq!(module.as_str(), "base");
            assert_eq!(
                step,
                ModuleStep::Migration {
                    stage: MigrationStage::End
                }
            );
            assert_eq!(file.as_deref(), Some(LATE_CHECK.location().as_str()));
            assert_eq!(
                outer_text,
                format!(
                    "Module \"base\": end migration '{}' failed",
                    LATE_CHECK.location()
                )
            );
            *source
        }
        other => panic!("expected late migration step failure, got {other:?}"),
    };
    let text = inner.to_string();
    match inner {
        erp_modules::Error::MigrationFailed {
            module,
            version,
            stage,
            name,
            source,
        } => {
            assert_eq!(module.as_str(), "base");
            assert_eq!(version, SemverVersion::new(1, 2, 0));
            assert_eq!(stage, MigrationStage::End);
            assert_eq!(name, "fixture-labels-late-check");
            assert_eq!(source.kind(), erp_sdk::ErrorKind::Validation);
            assert_eq!(source.message(), "fixture late label failure");
            assert_eq!(
                text,
                "Module 'base': migration 'fixture-labels-late-check' (version 1.2.0, stage end) failed"
            );
        }
        other => panic!("expected wrapped migration failure, got {other:?}"),
    }
}

#[test]
fn current_schema_lifecycle_fixture_recomputes_labels_preserves_edits_and_rolls_back_late_failure()
{
    std::thread::Builder::new()
        .name("partner-label-lifecycle-2mib".to_owned())
        .stack_size(2 * 1024 * 1024)
        .spawn(|| {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            let scenario = std::pin::pin!(partner_label_lifecycle_scenario());
            runtime.block_on(scenario);
        })
        .unwrap()
        .join()
        .unwrap();
}

async fn partner_label_lifecycle_scenario() {
    let db = TestDb::create().await.unwrap();
    let system = current_system(&db).await;
    let initialized = system
        .initialize(InitOptions::new().with_skip_auto_install(true))
        .await
        .unwrap();
    assert!(initialized.post_commit.is_clean());
    let client = db.client().await.unwrap();
    // Intentionally synthesize prior stored values in the CURRENT schema. The
    // genuine old-CLI test supplies historical installation evidence separately.
    let root: i64 = client.query_one("INSERT INTO res_partner (name, type, active, is_company, partner_share, complete_name, commercial_company_name) VALUES ('', 'other', false, false, true, '', '') RETURNING id", &[]).await.unwrap().get(0);
    client
        .execute(
            "UPDATE res_partner SET commercial_partner_id=id WHERE id=$1",
            &[&root],
        )
        .await
        .unwrap();
    let parent: i64 = client.query_one("INSERT INTO res_partner (name, type, active, is_company, partner_share, parent_id, commercial_partner_id, commercial_company_name, complete_name) VALUES ('Tenant Parent', 'contact', true, false, true, $1, $1, '', 'Tenant Parent') RETURNING id", &[&root]).await.unwrap().get(0);
    let child: i64 = client.query_one("INSERT INTO res_partner (name, type, active, is_company, partner_share, parent_id, commercial_partner_id, commercial_company_name, complete_name, email, phone, street) VALUES ('Tenant Child', 'contact', false, false, true, $1, $2, '', $3, 'tenant@example.com', 'tenant phone', 'Tenant street') RETURNING id", &[&parent, &root, &OLD_LABEL]).await.unwrap().get(0);
    client.execute("INSERT INTO ir_model_data (module, name, model, res_id, noupdate) VALUES ('test_labels_fixture', 'tenant_child', 'res.partner', $1, true)", &[&child]).await.unwrap();
    // More than one 256-record migration window, with archived records on both
    // sides of the boundary. These are deliberately seeded old-formula values.
    client.execute("INSERT INTO res_partner (name, type, active, is_company, partner_share, parent_id, commercial_partner_id, commercial_company_name, complete_name, email, phone, street, ref) SELECT 'Window ' || n::text, 'contact', false, false, true, $1, $2, '', 'Window ' || n::text, 'window' || n::text || '@example.com', 'original window phone', 'Original window street', 'fixture-label-window' FROM generate_series(0, 269) AS n", &[&parent, &root]).await.unwrap();
    let before_window_ids = window_ids(&client).await;
    assert_eq!(before_window_ids.len(), 270);
    let tail = *before_window_ids.last().unwrap();
    client.execute("UPDATE res_partner SET email='tail-tenant@example.com', phone='tail tenant phone', street='Tail tenant street' WHERE id=$1", &[&tail]).await.unwrap();
    client.execute("INSERT INTO ir_model_data (module, name, model, res_id, noupdate) VALUES ('test_labels_fixture', 'tenant_window_tail', 'res.partner', $1, true)", &[&tail]).await.unwrap();
    let before_tail_identity = child_identity(&client, tail).await;
    assert_window_labels(&client, false).await;
    client.batch_execute("CREATE TABLE labels_upgrade_fixture (child bigint NOT NULL, refuse boolean NOT NULL, reached boolean NOT NULL)").await.unwrap();
    client
        .execute(
            "INSERT INTO labels_upgrade_fixture VALUES ($1, true, false)",
            &[&child],
        )
        .await
        .unwrap();
    // This rewind selects the migration in a focused fixture; it is deliberately
    // not called a previous-binary or previous-schema upgrade.
    client
        .execute(
            "UPDATE ir_module_module SET latest_version='1.1.0' WHERE name='base'",
            &[],
        )
        .await
        .unwrap();
    let before_identity = child_identity(&client, child).await;
    let before_count = contact_count(&client).await;
    assert_eq!(label(&client, child).await, OLD_LABEL);
    let env = system.system_env(Context::new());
    let base = ModuleName::new("base").unwrap();
    let failure = match system
        .button_immediate_upgrade(&env, std::slice::from_ref(&base))
        .await
    {
        Err(error) => error,
        Ok(outcome) => {
            drop(outcome);
            panic!("late migration refusal unexpectedly committed")
        }
    };
    assert_late_failure(failure);
    assert_eq!(label(&client, child).await, OLD_LABEL);
    assert_eq!(module_version(&client).await, "1.1.0");
    assert_eq!(child_identity(&client, child).await, before_identity);
    assert_eq!(contact_count(&client).await, before_count);
    assert_eq!(window_ids(&client).await, before_window_ids);
    assert_eq!(child_identity(&client, tail).await, before_tail_identity);
    assert_window_labels(&client, false).await;
    assert!(
        !client
            .query_one("SELECT reached FROM labels_upgrade_fixture", &[])
            .await
            .unwrap()
            .get::<_, bool>(0)
    );
    client
        .execute("UPDATE labels_upgrade_fixture SET refuse=false", &[])
        .await
        .unwrap();
    let upgraded = system
        .button_immediate_upgrade(&env, std::slice::from_ref(&base))
        .await
        .unwrap();
    assert!(upgraded.post_commit.is_clean());
    drop(upgraded);
    assert_eq!(label(&client, child).await, EXPECTED_LABEL);
    assert_eq!(module_version(&client).await, current_base_version());
    assert_eq!(child_identity(&client, child).await, before_identity);
    assert_eq!(contact_count(&client).await, before_count);
    assert_eq!(window_ids(&client).await, before_window_ids);
    assert_eq!(child_identity(&client, tail).await, before_tail_identity);
    assert_window_labels(&client, true).await;
    assert_eq!(client.query_one("SELECT res_id FROM ir_model_data WHERE module='test_labels_fixture' AND name='tenant_child'", &[]).await.unwrap().get::<_, i64>(0), child);
    assert_eq!(client.query_one("SELECT res_id FROM ir_model_data WHERE module='test_labels_fixture' AND name='tenant_window_tail'", &[]).await.unwrap().get::<_, i64>(0), tail);
    client
        .execute("UPDATE labels_upgrade_fixture SET reached=false", &[])
        .await
        .unwrap();
    let repeated = system
        .button_immediate_upgrade(&env, std::slice::from_ref(&base))
        .await
        .unwrap();
    assert!(repeated.post_commit.is_clean());
    drop(repeated);
    assert_eq!(label(&client, child).await, EXPECTED_LABEL);
    assert_eq!(child_identity(&client, child).await, before_identity);
    assert_eq!(contact_count(&client).await, before_count);
    assert_eq!(window_ids(&client).await, before_window_ids);
    assert_eq!(child_identity(&client, tail).await, before_tail_identity);
    assert_window_labels(&client, true).await;
    assert!(
        !client
            .query_one("SELECT reached FROM labels_upgrade_fixture", &[])
            .await
            .unwrap()
            .get::<_, bool>(0),
        "same-version repeat must not rerun the versioned migration"
    );
    drop(client);
    drop(initialized);
    drop(system);
    db.cleanup().await.unwrap();
}

fn private(path: &Path, value: &str) {
    fs::write(path, value).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
}

fn encoded(bytes: &[u8]) -> String {
    let mut result = String::new();
    for byte in bytes {
        if byte.is_ascii_alphanumeric() || b"-._~".contains(byte) {
            result.push(char::from(*byte));
        } else {
            result.push_str(&format!("%{byte:02X}"));
        }
    }
    result
}

fn url(config: &erp_testkit::tokio_postgres::Config, role: Option<&str>) -> String {
    let Host::Tcp(host) = &config.get_hosts()[0] else {
        panic!("native fixture requires configured TCP test cluster")
    };
    let user = role.unwrap_or(config.get_user().unwrap());
    let password = if role.is_some() {
        b"fixture-runtime-password".as_slice()
    } else {
        config.get_password().unwrap()
    };
    format!(
        "postgresql://{}:{}@{}:{}/{}",
        encoded(user.as_bytes()),
        encoded(password),
        host,
        config.get_ports()[0],
        config.get_dbname().unwrap()
    )
}

fn config(directory: &Path, db: &TestDb, role: &str, runtime_addons: &Path) -> PathBuf {
    private(&directory.join("owner-url"), &url(&db.config(), None));
    private(
        &directory.join("runtime-url"),
        &url(&db.config(), Some(role)),
    );
    let text = format!(
        "database='{}'\ndatabase_url_file='runtime-url'\nowner_database_url_file='owner-url'\nbind='127.0.0.1:18093'\npublic_origin='http://127.0.0.1:18093'\nproxy_mode='direct'\naddons_path=['{}']\nassets_manifest='manifest.json'\n",
        db.name(),
        runtime_addons.display()
    );
    fs::write(directory.join("server.toml"), text).unwrap();
    let digest = erp_modules::package::sha256_hex(b"");
    let assets: Vec<_> = [("js", "text/javascript; charset=utf-8"), ("css", "text/css; charset=utf-8")].into_iter().map(|(extension, content_type)| {
        let path = format!("client.{digest}.{extension}");
        fs::write(directory.join(&path), b"").unwrap();
        serde_json::json!({"name": format!("client.{extension}"), "path": path, "sha256": digest, "content_type": content_type})
    }).collect();
    fs::write(
        directory.join("manifest.json"),
        serde_json::to_vec(&serde_json::json!({"schema": 1, "assets": assets})).unwrap(),
    )
    .unwrap();
    directory.join("server.toml")
}

fn fixture_addon(search_path: &Path) {
    let fixture = search_path.join("test_labels_upgrade");
    fs::create_dir(&fixture).unwrap();
    fs::write(fixture.join("manifest.toml"), "name='Partner label upgrade fixture'\nversion='1.0'\ndepends=['base']\ndata=['partners.xml']\n").unwrap();
    fs::write(
        fixture.join("partners.xml"),
        r#"<erp noupdate="1">
  <record id="empty_root" model="res.partner">
    <field name="name" eval="''"/>
    <field name="type">other</field>
    <field name="active" eval="False"/>
  </record>
  <record id="direct_parent" model="res.partner">
    <field name="name">Tenant Parent</field>
    <field name="parent_id" ref="empty_root"/>
  </record>
  <record id="tenant_child" model="res.partner">
    <field name="name">Tenant Child</field>
    <field name="parent_id" ref="direct_parent"/>
    <field name="active" eval="False"/>
    <field name="email">original@example.com</field>
    <field name="phone">original phone</field>
    <field name="street">Original street</field>
  </record>
</erp>
"#,
    )
    .unwrap();
}

fn run(binary: &Path, arguments: &[&str], db: &TestDb) -> Output {
    Command::new(binary)
        .args(arguments)
        .env("DATABASE_URL", url(&db.config(), None))
        .output()
        .unwrap()
}

fn succeeded(output: Output, step: &str) {
    eprintln!(
        "step={step} status={} stdout={} stderr={}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.status.success(), "{step} failed");
}

async fn release_inventory(root: &Path, source: &str, installed_only: Option<&Client>) {
    let mut identities = BTreeMap::new();
    let mut hashes = BTreeMap::new();
    let mut index = AddonIndex::new(true);
    index.add_dev_path(root.join("addons"));
    for (name, entry) in index.discover_entries().unwrap() {
        let snapshot = erp_modules::artifact_identity::ArtifactSnapshot::capture(
            name.as_str(),
            Path::new(entry.source_location()),
        )
        .unwrap();
        if let Some(client) = installed_only {
            let installed = client.query_opt("SELECT content_hash FROM installed_addon_identity WHERE name=$1 AND source_kind <> 'package'", &[&name.as_str()]).await.unwrap();
            let Some(installed) = installed else { continue };
            assert_eq!(
                snapshot.digest(),
                installed.get::<_, String>(0),
                "previous artifact {name} must match the actual installed identity"
            );
        }
        for (path, bytes) in snapshot.files() {
            hashes.insert(
                format!("addons/{name}/{path}"),
                erp_modules::package::sha256_hex(bytes),
            );
        }
        identities.insert(name.to_string(), snapshot.digest().to_owned());
    }
    fs::write(root.join("REVISION"), source).unwrap();
    fs::write(root.join("PACKAGE.json"), serde_json::to_vec(&serde_json::json!({"source": source, "sha256": hashes, "addon_identities": identities})).unwrap()).unwrap();
    erp_cli::identity::release_identities(root).unwrap();
}

#[tokio::test]
#[ignore = "requires pinned previous native CLI/source on an approved build host"]
async fn genuine_previous_cli_partner_label_upgrade_preserves_identity_repeats_and_rolls_back() {
    let old = PathBuf::from(
        std::env::var("ERP_PARTNER_LABELS_OLD_SOURCE")
            .expect("ERP_PARTNER_LABELS_OLD_SOURCE required"),
    );
    let old_binary = std::env::var_os("ERP_PARTNER_LABELS_OLD_BIN")
        .map(PathBuf::from)
        .unwrap_or_else(|| old.join("target/debug/erp"));
    assert_eq!(
        fs::read_to_string(old.join("REVISION")).unwrap().trim(),
        OLD_SOURCE
    );
    // The parent host runner records the locked previous-source build command,
    // tool versions and output. Digests here are tool-derived provenance, never
    // a hand-entered claim that a rewound current binary is historical.
    eprintln!(
        "previous_source={OLD_SOURCE} previous_lock_sha256={} previous_binary_sha256={}",
        erp_modules::package::sha256_hex(&fs::read(old.join("Cargo.lock")).unwrap()),
        erp_modules::package::sha256_hex(&fs::read(&old_binary).unwrap())
    );
    let old_manifest = fs::read_to_string(old.join("addons/base/manifest.toml")).unwrap();
    assert!(
        old_manifest
            .lines()
            .any(|line| line.trim() == "version = \"1.1\""),
        "pinned previous source must be base 1.1"
    );
    let db = TestDb::create().await.unwrap();
    let client = db.client().await.unwrap();
    let role = format!("{}_labels", db.name());
    client.batch_execute(&format!("CREATE ROLE {} LOGIN PASSWORD 'fixture-runtime-password' NOSUPERUSER NOCREATEDB NOCREATEROLE NOREPLICATION", erp_sql::quote(&role))).await.unwrap();
    let directory = tempfile::tempdir().unwrap();
    let directory = directory.path().canonicalize().unwrap();
    let old_runtime = directory.join("old-addons");
    runtime_addons::stage_runtime_addons(&old.join("addons"), &old_runtime).unwrap();
    fixture_addon(&old_runtime);
    let configuration = config(&directory, &db, &role, &old_runtime);
    private(
        &directory.join("password"),
        "fixture-administrator-password",
    );
    succeeded(
        run(
            &old_binary,
            &[
                "db",
                "init",
                "--config",
                configuration.to_str().unwrap(),
                "--admin-login",
                "labels-admin",
                "--admin-password-file",
                directory.join("password").to_str().unwrap(),
            ],
            &db,
        ),
        "previous-db-init",
    );
    succeeded(
        run(
            &old_binary,
            &[
                "module",
                "install",
                "test_labels_upgrade",
                "-d",
                db.name(),
                "--addons-path",
                old_runtime.to_str().unwrap(),
            ],
            &db,
        ),
        "previous-fixture-install",
    );
    let old_version = module_version(&client).await;
    assert!(
        old_version.starts_with("1.1"),
        "previous version {old_version}"
    );
    let row = client.query_one("SELECT p.id, p.complete_name, p.commercial_company_name, parent.name FROM ir_model_data d JOIN res_partner p ON p.id=d.res_id JOIN res_partner parent ON parent.id=p.parent_id WHERE d.module='test_labels_upgrade' AND d.name='tenant_child' AND d.model='res.partner'", &[]).await.unwrap();
    let child: i64 = row.get(0);
    assert_eq!(
        row.get::<_, String>(1),
        OLD_LABEL,
        "old binary must actually compute the stale historical label"
    );
    assert_eq!(row.get::<_, String>(2), "");
    assert_eq!(row.get::<_, String>(3), "Tenant Parent");
    client.execute("UPDATE res_partner SET email='tenant@example.com', phone='tenant phone', street='Tenant street' WHERE id=$1", &[&child]).await.unwrap();
    let before_identity = child_identity(&client, child).await;
    let before_count = contact_count(&client).await;
    let previous = directory.join("previous");
    fs::create_dir(&previous).unwrap();
    runtime_addons::stage_runtime_addons(&old_runtime, &previous.join("addons")).unwrap();
    release_inventory(&previous, OLD_SOURCE, Some(&client)).await;
    let releases = directory.join("releases");
    fs::create_dir(&releases).unwrap();
    let candidate = releases.join("candidate");
    fs::create_dir(&candidate).unwrap();
    runtime_addons::stage_runtime_addons(&addons(), &candidate.join("addons")).unwrap();
    fixture_addon(&candidate.join("addons"));
    let source = fs::read_to_string(addons().parent().unwrap().join("REVISION"))
        .unwrap()
        .trim()
        .to_owned();
    assert_ne!(source, OLD_SOURCE);
    release_inventory(&candidate, &source, None).await;
    for (name, expected) in erp_cli::identity::native_artifact_identities().unwrap() {
        let snapshot = erp_modules::artifact_identity::ArtifactSnapshot::capture(
            name.as_str(),
            &candidate.join("addons").join(name.as_str()),
        )
        .unwrap();
        assert_eq!(
            snapshot.digest(),
            expected,
            "candidate fixture must match its compiled native {name}"
        );
    }
    let current_binary = Path::new(env!("CARGO_BIN_EXE_erp"));
    // The version write is later than the data migration. Its trigger verifies
    // the repaired value before refusing publication, so failure cannot be
    // mistaken for a pre-migration setup refusal.
    client.batch_execute("CREATE FUNCTION fixture_labels_refuse_publication() RETURNS trigger LANGUAGE plpgsql AS $$ DECLARE actual text; BEGIN IF NEW.name='base' AND NEW.latest_version LIKE '1.%' THEN SELECT complete_name INTO actual FROM res_partner WHERE id=TG_ARGV[0]::bigint; IF actual IS DISTINCT FROM 'Tenant Parent, Tenant Child' THEN RAISE EXCEPTION 'fixture observed stale label before version publication' USING ERRCODE='23514'; END IF; RAISE EXCEPTION 'fixture blocked label version publication' USING ERRCODE='23514'; END IF; RETURN NEW; END $$").await.unwrap();
    client.batch_execute(&format!("CREATE TRIGGER fixture_labels_refuse_publication BEFORE UPDATE OF latest_version ON ir_module_module FOR EACH ROW EXECUTE FUNCTION fixture_labels_refuse_publication('{child}')")).await.unwrap();
    let refused = run(
        current_binary,
        &[
            "module",
            "upgrade",
            "base",
            "-d",
            db.name(),
            "--addons-path",
            candidate.join("addons").to_str().unwrap(),
            "--identity-previous",
            previous.to_str().unwrap(),
        ],
        &db,
    );
    eprintln!(
        "step=late-publication-refusal status={} stderr={}",
        refused.status,
        String::from_utf8_lossy(&refused.stderr)
    );
    assert_eq!(refused.status.code(), Some(1));
    assert_eq!(
        std::str::from_utf8(&refused.stderr).unwrap().trim(),
        "Error: The database refused the change: fixture blocked label version publication: db error: ERROR: fixture blocked label version publication"
    );
    assert_eq!(module_version(&client).await, old_version);
    assert_eq!(label(&client, child).await, OLD_LABEL);
    assert_eq!(child_identity(&client, child).await, before_identity);
    assert_eq!(contact_count(&client).await, before_count);
    client.batch_execute("DROP TRIGGER fixture_labels_refuse_publication ON ir_module_module; DROP FUNCTION fixture_labels_refuse_publication()").await.unwrap();
    succeeded(
        run(
            current_binary,
            &[
                "module",
                "upgrade",
                "base",
                "-d",
                db.name(),
                "--addons-path",
                candidate.join("addons").to_str().unwrap(),
                "--identity-previous",
                previous.to_str().unwrap(),
            ],
            &db,
        ),
        "candidate-forward-upgrade",
    );
    assert_eq!(label(&client, child).await, EXPECTED_LABEL);
    assert_eq!(module_version(&client).await, current_base_version());
    assert_eq!(child_identity(&client, child).await, before_identity);
    assert_eq!(contact_count(&client).await, before_count);
    let proof = directory.join("forward-upgrade.json");
    private(&proof, &serde_json::to_string(&serde_json::json!({"source": source, "previous": OLD_SOURCE, "exit": 0, "upgrade_before_activation": true})).unwrap());
    std::os::unix::fs::symlink(&candidate, directory.join("current")).unwrap();
    succeeded(
        run(
            current_binary,
            &[
                "module",
                "identity-activate",
                "--config",
                configuration.to_str().unwrap(),
                "--release",
                candidate.to_str().unwrap(),
                "--previous",
                previous.to_str().unwrap(),
                "--upgrade-receipt",
                proof.to_str().unwrap(),
            ],
            &db,
        ),
        "candidate-identity-activation",
    );
    succeeded(
        run(
            current_binary,
            &[
                "module",
                "upgrade",
                "base",
                "-d",
                db.name(),
                "--addons-path",
                candidate.join("addons").to_str().unwrap(),
            ],
            &db,
        ),
        "ordinary-repeat-upgrade",
    );
    assert_eq!(label(&client, child).await, EXPECTED_LABEL);
    assert_eq!(child_identity(&client, child).await, before_identity);
    assert_eq!(contact_count(&client).await, before_count);
    assert_eq!(client.query_one("SELECT res_id FROM ir_model_data WHERE module='test_labels_upgrade' AND name='tenant_child' AND model='res.partner'", &[]).await.unwrap().get::<_, i64>(0), child);
    drop(client);
    let mut admin = db.config();
    admin.dbname("postgres");
    db.cleanup().await.unwrap();
    let (client, connection) = admin
        .connect(erp_testkit::tokio_postgres::NoTls)
        .await
        .unwrap();
    tokio::spawn(async move { connection.await.unwrap() });
    client
        .batch_execute(&format!("DROP ROLE {}", erp_sql::quote(&role)))
        .await
        .unwrap();
}
