// SPDX-License-Identifier: MIT OR Apache-2.0
//! Installing `project` without demo data: the four template stages exist and belong to no
//! project, no project exists, the access rows are the declared ones, the root menu sits at
//! sequence 55 with Overview as its first entry, and a new project gets its own copies of the
//! template stages in board order.

use erp_fields::Cell;
use erp_modules::InitOptions;
use erp_orm::{Context, SUPERUSER_ID};
use erp_testkit::TestDb;

use crate::support::{
    TEMPLATE_STAGES, cell, ids, named, project_module, run, stages_of, system_for_pool,
    template_stages, xid,
};

/// The restriction row of the stages: a stage is reachable when it is a template or its project is.
const STAGE_SCOPE_DOMAIN: &str =
    "['|', ('project_id', '=', False), ('project_id', 'access', 'read')]";

fn manifest_version() -> &'static str {
    include_str!("../../../../addons/project/manifest.toml")
        .lines()
        .find_map(|line| line.strip_prefix("version = \""))
        .and_then(|rest| rest.strip_suffix('"'))
        .unwrap()
}

/// The declared access rows of the addon as `(external id, group, operation, domain)`.
async fn declared_access(client: &tokio_postgres::Client) -> Vec<(String, String, String, String)> {
    client
        .query(
            "SELECT x.name::text, coalesce(g.module || '.' || g.name, '-')::text, \
             a.operation::text, coalesce(a.domain, '')::text \
             FROM ir_access a \
             JOIN ir_model_data x ON x.model = 'ir.access' AND x.res_id = a.id AND x.module = 'project' \
             LEFT JOIN ir_model_data g ON g.model = 'res.groups' AND g.res_id = a.group_id \
             ORDER BY x.name",
            &[],
        )
        .await
        .unwrap()
        .iter()
        .map(|row| (row.get(0), row.get(1), row.get(2), row.get(3)))
        .collect()
}

#[test]
fn project_installs_template_stages_access_and_menus() {
    run(|| async {
        let db = TestDb::create().await.unwrap();
        let system = system_for_pool(db.pool().clone());
        let init = Box::pin(system.initialize(InitOptions::new().with_skip_auto_install(true)))
            .await
            .unwrap();
        let env = init.orm.env(SUPERUSER_ID, Context::new());
        let outcome = Box::pin(system.button_immediate_install(&env, &[project_module()]))
            .await
            .unwrap();
        assert!(outcome.post_commit.is_clean(), "{:?}", outcome.post_commit);
        assert!(
            outcome.data_notices.is_empty(),
            "install notices: {:?}",
            outcome.data_notices
        );
        let client = db.client().await.unwrap();

        // The installed version is the manifest's.
        let installed: String = client
            .query_one(
                "SELECT latest_version FROM ir_module_module WHERE name = 'project'",
                &[],
            )
            .await
            .unwrap()
            .get(0);
        assert_eq!(installed, manifest_version());

        // The template stages: the four external ids, in board order, with no project; and no
        // project (hence no copied stage) exists after a plain install.
        let templates: Vec<(String, String, i32, bool, bool)> = client
            .query(
                "SELECT coalesce(x.name, '-')::text, t.name::text, t.sequence, \
                 coalesce(t.fold, false), t.project_id IS NULL \
                 FROM project_task_type t \
                 LEFT JOIN ir_model_data x ON x.model = 'project.task.type' AND x.res_id = t.id \
                 AND x.module = 'project' \
                 ORDER BY t.sequence, t.id",
                &[],
            )
            .await
            .unwrap()
            .iter()
            .map(|row| (row.get(0), row.get(1), row.get(2), row.get(3), row.get(4)))
            .collect();
        let expected: Vec<(String, String, i32, bool, bool)> = TEMPLATE_STAGES
            .iter()
            .map(|(xmlid, name, sequence, fold)| {
                (
                    (*xmlid).to_owned(),
                    (*name).to_owned(),
                    *sequence,
                    *fold,
                    true,
                )
            })
            .collect();
        assert_eq!(templates, expected, "template stages");
        let projects: i64 = client
            .query_one("SELECT count(*) FROM project_project", &[])
            .await
            .unwrap()
            .get(0);
        assert_eq!(projects, 0, "a plain install creates no project");

        // The declared access rows: users read and update projects, work on tasks without
        // deleting them, read stages and read and create tags (D7); managers do everything; the
        // stages of a project follow the project's own readability.
        let row = |xmlid: &str, group: &str, operation: &str, domain: &str| {
            (
                xmlid.to_owned(),
                group.to_owned(),
                operation.to_owned(),
                domain.to_owned(),
            )
        };
        assert_eq!(
            declared_access(&client).await,
            [
                row(
                    "access_project_project_manager",
                    "project.group_project_manager",
                    "crud",
                    ""
                ),
                row(
                    "access_project_project_user",
                    "project.group_project_user",
                    "ru",
                    ""
                ),
                row(
                    "access_project_tags_manager",
                    "project.group_project_manager",
                    "crud",
                    ""
                ),
                row(
                    "access_project_tags_user",
                    "project.group_project_user",
                    "cr",
                    ""
                ),
                row(
                    "access_project_task_manager",
                    "project.group_project_manager",
                    "crud",
                    ""
                ),
                row(
                    "access_project_task_type_manager",
                    "project.group_project_manager",
                    "crud",
                    ""
                ),
                row(
                    "access_project_task_type_scope",
                    "-",
                    "crud",
                    STAGE_SCOPE_DOMAIN
                ),
                row(
                    "access_project_task_type_user",
                    "project.group_project_user",
                    "r",
                    ""
                ),
                row(
                    "access_project_task_user",
                    "project.group_project_user",
                    "cru",
                    ""
                ),
            ]
        );

        // The root menu: sequence 55, the app icon, and Overview as its first entry.
        let root = xid(&client, "project", "menu_project_root").await;
        let (sequence, icon): (i32, Option<String>) = client
            .query_one(
                "SELECT sequence, web_icon::text FROM ir_ui_menu WHERE id = $1::int8",
                &[&root],
            )
            .await
            .map(|row| (row.get(0), row.get(1)))
            .unwrap();
        assert_eq!(sequence, 55, "root menu sequence (D10)");
        assert_eq!(icon.as_deref(), Some("lucide:folder-kanban"));
        let children: Vec<String> = client
            .query(
                "SELECT coalesce(x.name, '-')::text FROM ir_ui_menu m \
                 LEFT JOIN ir_model_data x ON x.model = 'ir.ui.menu' AND x.res_id = m.id \
                 WHERE m.parent_id = $1::int8 ORDER BY m.sequence, m.id",
                &[&root],
            )
            .await
            .unwrap()
            .iter()
            .map(|row| row.get(0))
            .collect();
        assert_eq!(
            children.first().map(String::as_str),
            Some("menu_project_overview"),
            "Overview opens the app: {children:?}"
        );

        // A new project gets its own copies of the four template stages, in order; a batch of
        // two gets two separate sets; the templates stay as they were.
        let mut tx = outcome.orm.begin(SUPERUSER_ID).await.unwrap();
        let created = tx
            .env()
            .records("project.project")
            .unwrap()
            .create(&mut tx, vec![named("First"), named("Second")])
            .await
            .unwrap()
            .ids();
        assert_eq!(created.len(), 2);
        let mut copied = Vec::new();
        for project in &created {
            let stages = ids(&cell(&mut tx, "project.project", *project, "type_ids").await);
            assert_eq!(stages.len(), 4, "project {project} stages: {stages:?}");
            for stage in &stages {
                assert_eq!(
                    cell(&mut tx, "project.task.type", *stage, "project_id").await,
                    Cell::Id(*project),
                    "stage {stage} belongs to its project"
                );
            }
            copied.extend(stages);
        }
        tx.commit().await.unwrap();
        drop(tx);
        let mut unique = copied.clone();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(unique.len(), 8, "every project has its own stages");
        for project in &created {
            assert_eq!(
                stages_of(&client, *project).await,
                template_stages(),
                "project {project} stages copy the templates"
            );
        }
        let templates_left: i64 = client
            .query_one(
                "SELECT count(*) FROM project_task_type WHERE project_id IS NULL",
                &[],
            )
            .await
            .unwrap()
            .get(0);
        assert_eq!(templates_left, 4, "the templates are copied, not moved");

        drop(outcome);
        drop(init);
        drop(system);
        drop(client);
        db.cleanup().await.unwrap();
    });
}
