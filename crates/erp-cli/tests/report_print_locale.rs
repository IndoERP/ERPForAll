// SPDX-License-Identifier: MIT OR Apache-2.0
//! The print service prints in the printing user's language formats and timezone: a test report
//! over partners is saved and printed through the module system on a real database, and the PDF
//! is read back.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "integration assertions"
)]

#[path = "support/report_dpi.rs"]
mod report_dpi;
#[path = "support/report_frame.rs"]
mod report_frame;

use std::{
    path::{Path, PathBuf},
    sync::Arc,
    thread,
};

use erp_fields::Cell;
use erp_modules::{
    AddonIndex,
    system::{InitOptions, ModuleSystem},
};
use erp_orm::{RawVals, SUPERUSER_ID};
use erp_report::LayoutEngine;
use erp_testkit::TestDb;

fn repo_addons() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("addons")
}

fn system(db: &TestDb) -> ModuleSystem {
    let mut index = AddonIndex::new(true);
    index.add_dev_path(repo_addons());
    let native_artifact_identities = index
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
        .view_types(erp_addon_web::view_type_contributions())
        .native_artifact_identities(native_artifact_identities)
        .code([
            &erp_addon_uom::ADDON,
            &erp_addon_base::ADDON,
            &erp_addon_web::ADDON,
        ])
        .require_core_tables(erp_security::auth::core_tables())
        .build()
        .unwrap()
}

fn big_stack(body: impl std::future::Future<Output = ()> + Send + 'static) {
    thread::Builder::new()
        .stack_size(16 * 1024 * 1024)
        .spawn(move || {
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .unwrap()
                .block_on(body);
        })
        .unwrap()
        .join()
        .unwrap();
}

/// Text of every page of a PDF without white space.
fn page_texts(bytes: &[u8]) -> Vec<String> {
    let pdf = lopdf::Document::load_mem(bytes).unwrap();
    pdf.get_pages()
        .keys()
        .map(|page| {
            let text = pdf.extract_text(&[*page]).unwrap();
            text.chars().filter(|c| !c.is_whitespace()).collect()
        })
        .collect()
}

const REPORT: &str = r#"<t t-name="test_locale.report_partner">
<t t-call="web.html_container"><t t-call="web.external_layout">
<t t-foreach="docs" t-as="o"><div t-att-class="None if o_first else 'page-break-before'">
    <p>Name <span t-field="o.name"/></p>
    <p>Partner <b t-field="o.name"/> ready</p>
    <p groups="base.group_system">Administrator heading</p>
    <p><span t-field="o.parent_id" t-options="{'widget':'contact','fields':['address','email']}"/></p>
    <p><img src="data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAQAAAADCAIAAAA7ljmRAAAAEElEQVR4nGMwSJ0ARww4OQAHiA29HVf/iAAAAABJRU5ErkJggg==" alt="Mark"/><img src="data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAQAAAADCAIAAAA7ljmRAAAAEElEQVR4nGMwSJ0ARww4OQAHiA29HVf/iAAAAABJRU5ErkJggg==" alt="Mark"/></p>
    <p><span t-field="o.name" t-options="{'widget':'barcode','width':360,'height':60}"/></p>
    <p>Stamp <span t-field="o.create_date"/></p>
    <p>Calendar <span t-field="o.create_date" t-options="{'format':'EEEE d MMMM yyyy'}"/></p>
    <p>Duration <span t-out="1.75" t-options="{'widget':'duration','unit':'hour','round':'minute'}"/></p>
    <p>Amount <span t-out="12500000" t-options="{'widget': 'float', 'precision': 2}"/></p>
</div></t>
</t></t>
</t>"#;

/// A user in Jakarta whose language writes `.` between thousands and `,` before decimals prints a
/// timestamp in local time (here the next day) and amounts in that language's format; a print of
/// two records of one company is one document.
#[test]
fn a_print_uses_the_users_language_formats_and_timezone() {
    big_stack(async {
        let db = TestDb::create().await.unwrap();
        let system = system(&db);
        let orm = system.initialize(InitOptions::new()).await.unwrap().orm;
        let mut tx = orm
            .begin_with(SUPERUSER_ID, erp_orm::Context::new().with_company_id(1))
            .await
            .unwrap();
        tx.execute(
            "UPDATE res_lang SET active = true WHERE code = 'id_ID'",
            &[],
        )
        .await
        .unwrap();
        let group: i64 = tx.query(
            "SELECT res_id::bigint FROM ir_model_data WHERE module='base' AND name='group_system'",
            &[],
        ).await.unwrap()[0].get(0);
        let company = erp_sdk::RecId::from_real(1).unwrap();
        let administrator = tx
            .env()
            .records("res.users")
            .unwrap()
            .create(
                &mut tx,
                vec![
                    RawVals::new()
                        .set_cell("name", Cell::Str("Report administrator".into()))
                        .set_cell("login", Cell::Str("report-locale-admin".into()))
                        .set_cell("company_id", Cell::Id(company.get()))
                        .set_cell("lang", Cell::Str("id_ID".into()))
                        .set_cell("tz", Cell::Str("Asia/Jakarta".into()))
                        .set_commands("company_ids", [erp_sdk::Command::set([company])])
                        .set_commands(
                            "group_ids",
                            [erp_sdk::Command::set([
                                erp_sdk::RecId::from_real(group).unwrap()
                            ])],
                        ),
                ],
            )
            .await
            .unwrap()
            .ids()[0];
        let key = "test_locale.report_partner";
        let view = tx
            .env()
            .records("ir.ui.view")
            .unwrap()
            .create(
                &mut tx,
                vec![
                    RawVals::new()
                        .set_cell("name", Cell::Str(key.into()))
                        .set_cell("type", Cell::Str("qweb".into()))
                        .set_cell("key", Cell::Str(key.into()))
                        .set_cell("arch_db", Cell::Str(REPORT.into())),
                ],
            )
            .await
            .unwrap();
        tx.flush().await.unwrap();
        let env = tx.env();
        erp_data::model_data::update_xmlids(
            &mut tx,
            &env,
            &[erp_data::model_data::XidBinding::new(
                erp_data::ast::XmlId::parse(key).unwrap(),
                "ir.ui.view",
                view.rec_ids()[0].get(),
                false,
            )],
            false,
        )
        .await
        .unwrap();
        let mut terms = erp_data::TranslationImporter::new();
        terms.load_po("#: model_terms:ir.ui.view,arch_db:test_locale.report_partner\nmsgid \"Partner {0} ready\"\nmsgstr \"Siap {0} sekarang\"\n", "id.po", "id_ID").unwrap();
        terms
            .save(&mut tx, erp_data::OverwriteMode::Overwrite)
            .await
            .unwrap();
        tx.env()
            .records("ir.actions.report")
            .unwrap()
            .create(
                &mut tx,
                vec![
                    RawVals::new()
                        .set_cell("name", Cell::Str("Partner Sheet".into()))
                        .set_cell("model", Cell::Str("res.partner".into()))
                        .set_cell("report_type", Cell::Str("qweb-pdf".into()))
                        .set_cell("report_name", Cell::Str(key.into())),
                ],
            )
            .await
            .unwrap();
        let partner_model = tx.env().records("res.partner").unwrap();
        let office = partner_model
            .create(
                &mut tx,
                vec![
                    RawVals::new()
                        .set_cell("name", Cell::Str("Kantor Pusat".into()))
                        .set_cell("is_company", Cell::Bool(true))
                        .set_cell("street", Cell::Str("Jalan Melati 8".into()))
                        .set_cell("city", Cell::Str("Jakarta".into()))
                        .set_cell("email", Cell::Str("office@example.test".into())),
                ],
            )
            .await
            .unwrap()
            .ids()[0];
        let partners = partner_model
            .create(
                &mut tx,
                ["Sinar Abadi", "Maju Jaya"]
                    .into_iter()
                    .map(|name| {
                        RawVals::new()
                            .set_cell("name", Cell::Str(name.into()))
                            .set_cell("parent_id", Cell::Id(office))
                    })
                    .collect(),
            )
            .await
            .unwrap()
            .ids();
        tx.flush().await.unwrap();
        // 20:30 UTC on the 16th is 03:30 on the 17th in Jakarta (UTC+7).
        tx.execute(
            "UPDATE res_partner SET create_date = '2026-11-16 20:30:00' WHERE id = ANY($1)",
            &[&partners],
        )
        .await
        .unwrap();
        tx.commit().await.unwrap();

        // Match the served print path, including group evaluation and the caller's companies.
        let registry = Arc::clone(orm.registry());
        let policy =
            erp_security::source::PolicySource::build(Arc::clone(&registry), db.pool().clone())
                .await
                .unwrap();
        let orm = erp_orm::Orm::builder(Arc::clone(&registry), db.pool().clone())
            .resolver(erp_security::resolver::Resolver::for_registry(&registry).unwrap())
            .policy(Arc::new(policy) as Arc<dyn erp_orm::PolicyProvider>)
            .require_core_tables(erp_security::auth::core_tables())
            .require_core_tables(erp_addon_base::partner_hierarchy::core_tables())
            .build()
            .unwrap();
        let mut tx = orm
            .begin_caller(erp_orm::UserId::new(administrator).unwrap())
            .await
            .unwrap();
        let report = erp_addon_base::report_print::find(&mut tx, key)
            .await
            .unwrap();
        let documents = erp_addon_base::report_print::prepare_runs(&mut tx, &report, &partners)
            .await
            .unwrap();
        tx.rollback().await.unwrap();
        assert_eq!(documents.len(), 1);

        let html = erp_report::render_html(&documents[0], &[]).unwrap();
        assert!(html.contains("Selasa 17 November 2026"), "{html}");
        assert!(html.contains("Siap") && html.contains("sekarang"), "{html}");
        assert!(html.contains("Administrator heading"), "{html}");
        for contact_line in ["Jalan Melati 8", "Jakarta", "office@example.test"] {
            assert!(html.contains(contact_line), "{html}");
        }
        assert!(html.contains("1 jam 45 menit"), "{html}");
        assert_eq!(html.matches("data:image/png;base64,").count(), 1, "{html}");
        assert_eq!(html.matches("aria-label=\"Mark\"").count(), 4, "{html}");
        assert_eq!(html.matches("<svg").count(), 2, "{html}");
        let fonts = repo_addons().join("report_fonts");
        let pool = erp_report_worker::WorkerPool::new(erp_report_worker::PoolSettings::new(
            erp_report_worker::WorkerCommand::new(env!("CARGO_BIN_EXE_erp"), ["report-worker"]),
            fonts,
        ))
        .unwrap();
        let mut samples = Vec::new();
        let mut pages = Vec::new();
        for _ in 0..4 {
            let start = std::time::Instant::now();
            let result = pool.render(&documents[0]).unwrap();
            samples.push(start.elapsed().as_millis());
            let pdf = lopdf::Document::load_mem(result.bytes()).unwrap();
            let marks = pdf
                .objects
                .values()
                .filter_map(|obj| obj.as_stream().ok())
                .filter(|stream| {
                    stream.dict.get(b"Subtype").and_then(|o| o.as_name()).ok() == Some(b"Image")
                        && stream.dict.get(b"Width").and_then(|o| o.as_i64()).ok() == Some(4)
                })
                .count();
            assert_eq!(marks, 1, "four placements share one raster object");
            // Typst fills all bars in one compound path. Count the closed shapes within a fill
            // on each page, independently of the simple rules above and below the report.
            for page in pdf.get_pages().into_values() {
                let content = lopdf::content::Content::decode(&pdf.get_page_content(page)).unwrap();
                let mut closed = 0;
                let mut barcode_bars = 0;
                for op in content.operations {
                    match op.operator.as_str() {
                        "h" | "re" => closed += 1,
                        "f" | "f*" => {
                            barcode_bars = barcode_bars.max(closed);
                            closed = 0;
                        }
                        "n" | "S" | "s" | "B" | "B*" | "b" | "b*" => closed = 0,
                        _ => {}
                    }
                }
                assert!(
                    barcode_bars > 20,
                    "expected a filled vector barcode on each page, got {barcode_bars} bars"
                );
            }
            pages = page_texts(result.bytes());
            assert_eq!(pages.len(), 2);
        }
        eprintln!(
            "two-page localized partner report, cold then three warm worker renders (ms): {samples:?}"
        );
        assert_eq!(pages.len(), 2, "{pages:?}");
        for (page, name) in pages.iter().zip(["SinarAbadi", "MajuJaya"]) {
            assert!(page.contains(&format!("Name{name}")), "{page}");
            assert!(page.contains("Stamp17/11/202603:30:00"), "{page}");
            assert!(page.contains("CalendarSelasa17November2026"), "{page}");
            assert!(page.contains(&format!("Siap{name}sekarang")), "{page}");
            for value in [
                "Administratorheading",
                "JalanMelati8",
                "Jakarta",
                "office@example.test",
            ] {
                assert!(page.contains(value), "{page}");
            }
            assert!(page.contains("Duration1jam45menit"), "{page}");
            assert!(page.contains("Amount12.500.000,00"), "{page}");
        }
    });
}
