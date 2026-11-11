//! `#[erp_sdk::migration]` declared the way an addon declares it (only `erp_sdk` paths), linked
//! into `MIGRATIONS` and run by an upgrade (review r2 MUST 1-3).
//!
//! A separate test binary: the migrations below are linked into every module system built here
//! with the linked migrations on (the default).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

mod common;

use common::*;
use erp_modules::MIGRATIONS;
use erp_modules::system::InitOptions;
use erp_testkit::TestDb;

/// The addon's migration code: it names `erp_sdk` items only.
mod addon {
    use erp_sdk::migrations::SemverVersion;
    use erp_sdk::{BoxFuture, Result, Transaction};

    /// The async form: the macro generates the `MigrationFn` shim.
    #[erp_sdk::migration(addon = "macro_mod", version = "1.1", stage = post)]
    pub async fn fill_post(tx: &mut Transaction, from: &SemverVersion) -> Result<()> {
        tx.execute(
            "INSERT INTO macro_log (stage, name, installed) VALUES ('post', 'fill_post', $1)",
            &[&from.to_string()],
        )
        .await?;
        Ok(())
    }

    /// The plain form with a string stage and an explicit name.
    #[erp_sdk::migration(
        addon = "macro_mod",
        version = "20.0.1.1",
        stage = "pre",
        name = "pre-a"
    )]
    pub fn prepare<'a>(
        tx: &'a mut Transaction,
        from: &'a SemverVersion,
    ) -> BoxFuture<'a, Result<()>> {
        let installed = from.to_string();
        Box::pin(async move {
            tx.execute(
                "INSERT INTO macro_log (stage, name, installed) VALUES ('pre', 'pre-a', $1)",
                &[&installed],
            )
            .await?;
            Ok(())
        })
    }

    /// The end stage, for a version the upgrade below (1.0 to 1.1) does not select.
    #[erp_sdk::migration(addon = "macro_mod", version = "1.2", stage = end)]
    pub async fn later(tx: &mut Transaction, _from: &SemverVersion) -> Result<()> {
        tx.execute(
            "INSERT INTO macro_log (stage, name, installed) VALUES ('end', 'later', '')",
            &[],
        )
        .await?;
        Ok(())
    }
}

/// The macro registers each function in `MIGRATIONS` with its addon, version, stage, name and
/// source location (the line of the attribute, followed by the function).
#[test]
fn the_macro_registers_in_the_linked_slice() {
    let mut linked: Vec<(&str, &str, &str, &str, String)> = MIGRATIONS
        .iter()
        .filter(|m| m.addon() == "macro_mod")
        .map(|m| {
            (
                m.addon(),
                m.version(),
                m.stage().as_str(),
                m.name(),
                m.location(),
            )
        })
        .collect();
    linked.sort();
    let names: Vec<(&str, &str, &str, &str)> =
        linked.iter().map(|l| (l.0, l.1, l.2, l.3)).collect();
    assert_eq!(
        names,
        vec![
            ("macro_mod", "1.1", "post", "fill_post"),
            ("macro_mod", "1.2", "end", "later"),
            ("macro_mod", "20.0.1.1", "pre", "pre-a"),
        ]
    );
    let source: Vec<&str> = include_str!("migration_macro.rs").lines().collect();
    for ((_, _, _, name, location), function) in
        linked.iter().zip(["fill_post", "later", "prepare"])
    {
        let line = location
            .strip_prefix(concat!(file!(), ":"))
            .unwrap_or_else(|| panic!("{name}: location {location} is not in {}", file!()));
        let line: usize = line.parse().unwrap();
        assert!(
            source[line - 1]
                .trim_start()
                .starts_with("#[erp_sdk::migration("),
            "{name}: line {line} is not the attribute: {}",
            source[line - 1]
        );
        let rest = source[line..].join("\n");
        let item = rest
            .split_once(&format!("fn {function}"))
            .map(|(before, _)| before);
        assert!(
            item.is_some_and(|before| !before.contains("#[erp_sdk::migration(")),
            "{name}: the attribute at line {line} is not on fn {function}"
        );
    }
}

/// An upgrade runs the selected linked migrations, each with the previous version.
#[tokio::test]
async fn linked_macro_migrations_run_in_an_upgrade() {
    let addons = Addons::new();
    addons.add("macro_mod", "name = 'Macro'\nversion = '1.0'\n");
    let db = TestDb::create().await.unwrap();
    let system = builder(&db, &addons).build().unwrap();
    let outcome = system
        .initialize(InitOptions::new().with_skip_auto_install(true))
        .await
        .unwrap();
    let admin = su(&outcome.orm);
    let outcome = system
        .button_immediate_install(&admin, &[n("macro_mod")])
        .await
        .unwrap();
    assert_eq!(outcome.installed, vec![n("macro_mod")]);
    db.pool()
        .get()
        .await
        .unwrap()
        .batch_execute(
            "CREATE TABLE macro_log (seq serial PRIMARY KEY, stage text, name text, installed text);",
        )
        .await
        .unwrap();

    addons.add("macro_mod", "name = 'Macro'\nversion = '1.1'\n");
    enroll_fixture_transition(&system, &["macro_mod"]).await;
    let admin = su(&outcome.orm);
    let outcome = system
        .button_immediate_upgrade(&admin, &[n("macro_mod")])
        .await
        .unwrap();
    assert_eq!(outcome.upgraded, vec![n("macro_mod")]);
    assert_eq!(
        rows(
            &outcome.orm,
            "SELECT stage, name, installed FROM macro_log ORDER BY seq"
        )
        .await,
        vec![
            vec![
                Some("pre".to_owned()),
                Some("pre-a".to_owned()),
                Some("1.0.0".to_owned())
            ],
            vec![
                Some("post".to_owned()),
                Some("fill_post".to_owned()),
                Some("1.0.0".to_owned())
            ],
        ]
    );
    db.cleanup().await.unwrap();
}
