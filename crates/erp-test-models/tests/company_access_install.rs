//! Emission of the generated company restrictions in module operations (A1, design
//! , §11.4) over the fixture "company access": one generated
//! row and one ledger entry per declaring model, rows changed since they were generated, the
//! explicit restoration and the read-back that refuses a wrong end state.
//!
//! Not covered here (open): the orphan
//! owner probe against real orphan rows, the statement-count ceiling, the invalid-declaration
//! installation and the multi-node case.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

use std::fs;
use std::sync::Arc;

use erp_modules::AddonIndex;
use erp_modules::company_access::TamperProblem;
use erp_modules::error::Error;
use erp_modules::manifest::ModuleName;
use erp_modules::system::{InitOptions, ModuleSystem};
use erp_orm::{Context, Orm, PolicyProvider, SUPERUSER_ID, UserId};
use erp_sdk::{Domain, ErrorKind, Value};
use erp_security::company_access::generate;
use erp_security::resolver::Resolver;
use erp_security::source::PolicySource;
use erp_test_models::test_company_emit;
use erp_testkit::TestDb;
use tempfile::TempDir;

const ADDON: &str = "test_company_emit";

fn n(name: &str) -> ModuleName {
    ModuleName::new(name).unwrap()
}

struct Fixture {
    db: TestDb,
    _dir: TempDir,
    system: ModuleSystem,
    orm: Orm,
}

impl Fixture {
    fn env(&self) -> erp_orm::Env {
        self.orm.env(SUPERUSER_ID, Context::new())
    }

    async fn install(&mut self) -> erp_modules::error::Result<()> {
        let outcome = self
            .system
            .button_immediate_install(&self.env(), &[n(ADDON)])
            .await?;
        self.orm = outcome.orm;
        Ok(())
    }

    async fn finish(self) {
        drop(self.orm);
        drop(self.system);
        self.db.cleanup().await.unwrap();
    }
}

/// An initialized database with the module system; the fixture addon is not installed yet.
async fn fixture() -> Fixture {
    let db = TestDb::create().await.unwrap();
    let dir = tempfile::tempdir_in(std::env::temp_dir().canonicalize().unwrap()).unwrap();
    for (name, manifest) in [
        (
            "base",
            "name = 'Base'\nauto_install = true\ncategory = 'Hidden'\n",
        ),
        (ADDON, "name = 'Company access'\ndepends = ['base']\n"),
    ] {
        let module = dir.path().join(name);
        fs::create_dir_all(&module).unwrap();
        fs::write(module.join("manifest.toml"), manifest).unwrap();
    }
    let mut index = AddonIndex::new(true);
    index.add_dev_path(dir.path());
    // This binary links code for these private fixture artifacts. Bind its provenance to
    // the explicit physical fixture bytes rather than exempting tests from admission.
    let native_artifact_identities = index
        .discover_entries()
        .unwrap()
        .into_iter()
        .map(|(name, entry)| {
            let snapshot = erp_modules::artifact_identity::ArtifactSnapshot::capture(
                name.as_str(),
                std::path::Path::new(entry.source_location()),
            )
            .unwrap();
            (name, snapshot.digest().to_owned())
        })
        .collect();
    let system = ModuleSystem::builder(db.pool().clone(), index)
        .native_artifact_identities(native_artifact_identities)
        .linked_hooks(false)
        .code([&erp_addon_base::ADDON, &test_company_emit::ADDON])
        .build()
        .unwrap();
    let init = system
        .initialize(InitOptions::new().with_skip_auto_install(true))
        .await
        .unwrap();
    Fixture {
        db,
        _dir: dir,
        system,
        orm: init.orm,
    }
}

async fn q(orm: &Orm, sql: &str) -> Vec<Vec<Option<String>>> {
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    let rows = tx.query(sql, &[]).await.unwrap();
    tx.rollback().await.unwrap();
    rows.iter()
        .map(|r| {
            (0..r.len())
                .map(|i| r.get::<_, Option<String>>(i))
                .collect()
        })
        .collect()
}

async fn exec(orm: &Orm, sql: &str) {
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    tx.batch(sql).await.unwrap();
    tx.commit().await.unwrap();
}

fn vals(pairs: &[(&str, Value)]) -> std::collections::BTreeMap<String, Value> {
    pairs
        .iter()
        .map(|(k, v)| ((*k).to_owned(), v.clone()))
        .collect()
}

async fn company(
    tx: &mut erp_orm::Transaction,
    currency: i64,
    name: &str,
    parent: Option<i64>,
) -> i64 {
    tx.query(
        "INSERT INTO res_company (name, currency_id, parent_id) VALUES ($1, $2, $3) \
         RETURNING id::int8",
        &[&name, &currency, &parent],
    )
    .await
    .unwrap()[0]
        .get(0)
}

/// exact-company document across own, ancestor, sibling and foreign companies for CRUD).
#[tokio::test]
async fn installed_company_restriction_enforces_user_crud() {
    let mut f = fixture().await;
    f.install().await.unwrap();
    let mut setup = f.orm.begin(SUPERUSER_ID).await.unwrap();
    let currency = erp_testkit::fixtures::seed_currency_in(&mut setup)
        .await
        .unwrap();
    let root = company(&mut setup, currency, "Root", None).await;
    let own = company(&mut setup, currency, "Own", Some(root)).await;
    let sibling = company(&mut setup, currency, "Sibling", Some(root)).await;
    let foreign = company(&mut setup, currency, "Foreign", None).await;
    let group: i64 = setup
        .query(
            "INSERT INTO res_groups (name) VALUES ('Company user') RETURNING id::int8",
            &[],
        )
        .await
        .unwrap()[0]
        .get(0);
    setup
        .execute(
            "INSERT INTO ir_model_data (module, name, model, res_id) \
         VALUES ('base', 'group_user', 'res.groups', $1)",
            &[&group],
        )
        .await
        .unwrap();
    let uid: i64 = setup
        .query(
            "INSERT INTO res_users (name, login, active, share, company_id) \
         VALUES ('Company user', 'company_user', true, false, $1) RETURNING id::int8",
            &[&own],
        )
        .await
        .unwrap()[0]
        .get(0);
    setup
        .execute(
            "INSERT INTO res_groups_users_rel (uid, gid) VALUES ($1, $2)",
            &[&uid, &group],
        )
        .await
        .unwrap();
    setup
        .execute(
            "INSERT INTO res_company_users_rel (user_id, cid) VALUES ($1, $2)",
            &[&uid, &own],
        )
        .await
        .unwrap();
    let model_id: i64 = setup
        .query("SELECT id::int8 FROM ir_model WHERE model = $1", &[&M1])
        .await
        .unwrap()[0]
        .get(0);
    setup
        .execute(
            "INSERT INTO ir_access (name, active, model_id, group_id, operation) \
         VALUES ('company user document', true, $1, $2, 'crud')",
            &[&model_id, &group],
        )
        .await
        .unwrap();
    let mut ids = Vec::new();
    for company_id in [own, root, sibling, foreign] {
        let id: i64 = setup.query(
            "INSERT INTO test_company_emit_doc (name, company_id) VALUES ('seed', $1) RETURNING id::int8",
            &[&company_id],
        ).await.unwrap()[0].get(0);
        ids.push(id);
    }
    setup.commit().await.unwrap();

    let registry = Arc::clone(f.env().registry());
    let source = PolicySource::build(Arc::clone(&registry), f.db.pool().clone())
        .await
        .unwrap();
    let provider: Arc<dyn PolicyProvider> = Arc::new(source);
    let orm = Orm::builder(registry, f.db.pool().clone())
        .policy(provider)
        .resolver(Resolver::id_order())
        .build()
        .unwrap();
    let mut tx = orm.begin(UserId::new(uid).unwrap()).await.unwrap();
    let docs = tx.env().records(M1).unwrap();
    assert_eq!(
        docs.search(&mut tx, &Domain::TRUE).await.unwrap().ids(),
        vec![ids[0]]
    );
    for denied in &ids[1..] {
        let err = docs
            .browse([*denied])
            .unwrap()
            .read(&mut tx, &["name"])
            .await
            .unwrap_err();
        assert_eq!(err.kind(), ErrorKind::Access, "read {denied}: {err}");
    }
    let created = docs
        .create_values(
            &mut tx,
            &[vals(&[
                ("name", Value::str("new")),
                ("company_id", Value::Int(own)),
            ])],
        )
        .await
        .unwrap();
    let err = docs
        .create_values(
            &mut tx,
            &[vals(&[
                ("name", Value::str("bad")),
                ("company_id", Value::Int(foreign)),
            ])],
        )
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Access, "create: {err}");
    created
        .write_values(&mut tx, &vals(&[("name", Value::str("changed"))]))
        .await
        .unwrap();
    let err = created
        .write_values(&mut tx, &vals(&[("company_id", Value::Int(sibling))]))
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Access, "move: {err}");
    drop(tx);
    drop(orm);
    f.finish().await;
}

const M1: &str = "test_company_emit.doc";

/// What the generated rows and the ledger hold now: `(model, ledger, name, active, group,
/// operation, domain)` ordered by model.
async fn emitted(orm: &Orm) -> Vec<Vec<Option<String>>> {
    q(
        orm,
        "SELECT m.model, m.company_restriction, a.name, a.active::text, a.group_id::text, \
         a.operation, a.domain \
         FROM ir_model m FULL JOIN (SELECT * FROM ir_access WHERE generated) a \
           ON a.model_id = m.id \
         WHERE m.company_restriction IS NOT NULL OR a.id IS NOT NULL ORDER BY m.model",
    )
    .await
}

/// ledger entry, written by the installation)
#[tokio::test]
async fn install_emits_one_restriction_per_declaring_model() {
    let mut f = fixture().await;
    f.install().await.unwrap();
    let wanted = generate(f.env().registry()).unwrap();
    assert!(!wanted.is_empty());
    let rows = emitted(&f.orm).await;
    assert_eq!(rows.len(), wanted.len());
    for (row, want) in rows.iter().zip(&wanted) {
        assert_eq!(row[0].as_deref(), Some(want.model_name()));
        assert_eq!(row[1].as_deref(), Some(want.text()), "ledger entry");
        assert_eq!(row[2].as_deref(), Some(want.name()));
        assert_eq!(row[3].as_deref(), Some("true"));
        assert_eq!(row[4], None, "no group");
        assert_eq!(row[5].as_deref(), Some("crud"));
        assert_eq!(row[6].as_deref(), Some(want.text()));
    }
    f.finish().await;
}

/// A tamper the test applies to the generated row of `M1` with SQL.
struct Tamper {
    sql: &'static str,
    problem: TamperProblem,
}

fn tampers() -> Vec<Tamper> {
    let model = "(SELECT id FROM ir_model WHERE model = 'test_company_emit.doc')";
    vec![
        Tamper {
            sql: "DELETE FROM ir_access WHERE generated AND model_id = (SELECT id FROM ir_model \
                  WHERE model = 'test_company_emit.doc')",
            problem: TamperProblem::Deleted,
        },
        Tamper {
            sql: "UPDATE ir_access SET active = false WHERE generated AND model_id = (SELECT id \
                  FROM ir_model WHERE model = 'test_company_emit.doc')",
            problem: TamperProblem::Archived,
        },
        Tamper {
            sql: "UPDATE ir_access SET operation = 'r' WHERE generated AND model_id = (SELECT id \
                  FROM ir_model WHERE model = 'test_company_emit.doc')",
            problem: TamperProblem::Changed {
                what: "operation",
                found: "r".to_owned(),
                expected: "crud".to_owned(),
            },
        },
        Tamper {
            sql: "WITH g AS (INSERT INTO res_groups (name) VALUES ('Tamper') RETURNING id) \
                  UPDATE ir_access SET group_id = (SELECT id FROM g) \
                  WHERE generated AND model_id = (SELECT id FROM ir_model \
                  WHERE model = 'test_company_emit.doc')",
            problem: TamperProblem::Changed {
                what: "group",
                found: String::new(), // "group <id>", filled in by the test
                expected: "no group".to_owned(),
            },
        },
        Tamper {
            sql: "UPDATE ir_access SET domain = '[]' WHERE generated AND model_id = (SELECT id \
                  FROM ir_model WHERE model = 'test_company_emit.doc')",
            problem: TamperProblem::Changed {
                what: "domain",
                found: "[]".to_owned(),
                expected: String::new(), // the ledger text, filled in by the test
            },
        },
    ]
    .into_iter()
    .chain([Tamper {
        // a generated row of a model the ledger does not know: clear the ledger of M1 and keep
        // its row
        sql: Box::leak(
            format!("UPDATE ir_model SET company_restriction = NULL WHERE id = {model}")
                .into_boxed_str(),
        ),
        problem: TamperProblem::CreatedOutside,
    }])
    .collect()
}

/// or created outside a module operation fails naming the model and the declaration, writes
/// nothing, and the explicit option rewrites the row and reports it)
#[tokio::test]
async fn an_update_refuses_rows_changed_since_they_were_generated() {
    for tamper in tampers() {
        let mut f = fixture().await;
        f.install().await.unwrap();
        let want = generate(f.env().registry())
            .unwrap()
            .into_iter()
            .find(|w| w.model_name() == M1)
            .unwrap();
        let before_ok = emitted(&f.orm).await;
        exec(&f.orm, tamper.sql).await;
        let after_tamper = emitted(&f.orm).await;

        let mut problem = tamper.problem.clone();
        if let TamperProblem::Changed {
            what: "group",
            found,
            ..
        } = &mut problem
        {
            let group = q(
                &f.orm,
                "SELECT group_id::text FROM ir_access WHERE generated AND group_id IS NOT NULL",
            )
            .await;
            assert_eq!(group.len(), 1, "the tamper must set a group");
            *found = format!("group {}", group[0][0].as_deref().unwrap());
        }
        if let TamperProblem::Changed {
            what: "domain",
            expected,
            ..
        } = &mut problem
        {
            *expected = want.text().to_owned();
        }

        // refused: the typed error, the full message, and nothing written
        let err = f
            .system
            .button_immediate_upgrade(&f.env(), &[n(ADDON)])
            .await
            .unwrap_err();
        let declaration = format!("{}, declared by {}", want.spelling(), want.source());
        assert!(
            matches!(&err, Error::GeneratedRestrictionTampered { model, problem: p, .. }
                if model == M1 && *p == problem),
            "{err:?}"
        );
        assert_eq!(
            err.to_string(),
            format!(
                "Module operation refused: the generated company restriction of {M1} \
                 ({declaration}) {problem} since it was generated; run the update with \
                 --restore-generated-restrictions to rewrite it"
            )
        );
        assert_eq!(emitted(&f.orm).await, after_tamper, "nothing was written");

        // restored: rewritten and reported as a notice
        let outcome = f
            .system
            .button_immediate_upgrade_restoring(&f.env(), &[n(ADDON)])
            .await
            .unwrap();
        assert_eq!(
            outcome
                .restored_restrictions
                .iter()
                .map(|r| (r.model.as_str(), r.what.clone()))
                .collect::<Vec<_>>(),
            vec![(M1, problem)]
        );
        f.orm = outcome.orm;
        assert_eq!(emitted(&f.orm).await, before_ok);
        f.finish().await;
    }
}

/// An unchanged update leaves the rows alone and reports nothing restored.
#[tokio::test]
async fn an_unchanged_update_rewrites_nothing() {
    let mut f = fixture().await;
    f.install().await.unwrap();
    let before = emitted(&f.orm).await;
    let ids = q(
        &f.orm,
        "SELECT id::text FROM ir_access WHERE generated ORDER BY id",
    )
    .await;
    let outcome = f
        .system
        .button_immediate_upgrade(&f.env(), &[n(ADDON)])
        .await
        .unwrap();
    assert!(outcome.restored_restrictions.is_empty());
    f.orm = outcome.orm;
    assert_eq!(emitted(&f.orm).await, before);
    assert_eq!(
        q(
            &f.orm,
            "SELECT id::text FROM ir_access WHERE generated ORDER BY id"
        )
        .await,
        ids
    );
    f.finish().await;
}

/// its generated row and clears its ledger entry; stood in for by a consistent row and ledger
/// entry on a model that declares nothing)
#[tokio::test]
async fn update_deletes_restrictions_of_dropped_declarations() {
    let mut f = fixture().await;
    f.install().await.unwrap();
    let before = emitted(&f.orm).await;
    exec(
        &f.orm,
        "UPDATE ir_model SET company_restriction = '[(1, \"=\", 1)]' WHERE model = 'res.company'; \
         INSERT INTO ir_access (name, active, model_id, operation, domain, generated) \
         SELECT 'dropped', true, id, 'crud', '[(1, \"=\", 1)]', true FROM ir_model \
         WHERE model = 'res.company'",
    )
    .await;
    assert_eq!(emitted(&f.orm).await.len(), before.len() + 1);
    let outcome = f
        .system
        .button_immediate_upgrade(&f.env(), &[n(ADDON)])
        .await
        .unwrap();
    assert!(outcome.restored_restrictions.is_empty());
    f.orm = outcome.orm;
    assert_eq!(emitted(&f.orm).await, before);
    f.finish().await;
}

/// make the operation fail with the model, the edge and the ids, and publish nothing)
#[tokio::test]
async fn orphan_owner_rows_fail_the_emission() {
    let mut f = fixture().await;
    let before = emitted(&f.orm).await;
    // The owner edge is required, so an owner-less row cannot exist when the schema is synced.
    // A trigger on the generated rows creates one during the emission, as a concurrent
    // writer that relaxed the column would, so that the owner probe finds it.
    exec(
        &f.orm,
        "CREATE FUNCTION add_orphan() RETURNS trigger LANGUAGE plpgsql AS $$ \
         BEGIN \
           ALTER TABLE test_company_emit_line ALTER COLUMN doc_id DROP NOT NULL; \
           IF NOT EXISTS (SELECT 1 FROM test_company_emit_line) THEN \
             INSERT INTO test_company_emit_line (name, doc_id) VALUES ('orphan', NULL); \
           END IF; \
           RETURN NULL; \
         END $$; \
         CREATE TRIGGER add_orphan AFTER INSERT ON ir_access \
           FOR EACH ROW WHEN (NEW.generated) EXECUTE FUNCTION add_orphan()",
    )
    .await;
    let id = "1".to_owned(); // the first row of a table that was empty
    let err = f.install().await.unwrap_err();
    assert!(
        matches!(&err, Error::OrphanOwnerRows { model, edge, ids, more }
            if model == "test_company_emit.line" && edge == "doc_id"
                && ids.iter().map(ToString::to_string).collect::<Vec<_>>() == [id.clone()]
                && !more),
        "{err:?}"
    );
    assert_eq!(
        err.to_string(),
        format!(
            "Module operation refused: test_company_emit.line restricts by its owner 'doc_id', \
             but 1 record(s) have no owner: ids {id}; give them an owner, then update again"
        )
    );
    assert_eq!(emitted(&f.orm).await, before, "nothing was committed");
    f.finish().await;
}

/// ledger entries)
#[tokio::test]
async fn uninstall_removes_restrictions_of_removed_declarations() {
    let mut f = fixture().await;
    let before = emitted(&f.orm).await;
    f.install().await.unwrap();
    assert!(!emitted(&f.orm).await.is_empty());
    let outcome = f
        .system
        .button_immediate_uninstall(&f.env(), &[n(ADDON)])
        .await
        .unwrap();
    assert_eq!(emitted(&outcome.orm).await, before);
    f.orm = outcome.orm;
    f.finish().await;
}

/// A trigger that alters the inserted row makes the read-back refuse; the installation rolls back.
#[tokio::test]
async fn a_wrong_end_state_fails_the_operation() {
    let mut f = fixture().await;
    let before = emitted(&f.orm).await;
    exec(
        &f.orm,
        "CREATE FUNCTION weaken_generated() RETURNS trigger LANGUAGE plpgsql AS $$ \
         BEGIN IF NEW.generated THEN NEW.operation := 'r'; END IF; RETURN NEW; END $$; \
         CREATE TRIGGER weaken_generated BEFORE INSERT ON ir_access \
           FOR EACH ROW EXECUTE FUNCTION weaken_generated()",
    )
    .await;
    let err = f.install().await.unwrap_err();
    let Error::GeneratedRestrictionInvalid { problem, .. } = &err else {
        panic!("unexpected error {err:?}");
    };
    assert!(
        problem.starts_with("was changed: its operation is r, the declaration gives crud"),
        "{problem}"
    );
    assert!(
        err.to_string()
            .starts_with("Module operation refused: the generated company restriction of ")
    );
    assert_eq!(emitted(&f.orm).await, before, "nothing was committed");
    assert_eq!(
        q(
            &f.orm,
            "SELECT state FROM ir_module_module WHERE name = 'test_company_emit'"
        )
        .await[0][0]
            .as_deref(),
        Some("uninstalled")
    );
    f.finish().await;
}
