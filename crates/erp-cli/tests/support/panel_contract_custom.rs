//! Adversarial generation callbacks; no XML lowering can vouch for their final sections.
use super::*;
use erp_sdk::views::*;
// Linked only into this test binary: real stored, group-protected fields for final-host checks.
mod fields {
    use erp_addon_acceptance_notes::models::Note;
    use erp_sdk::prelude::*;
    #[extend(Note, addon = "acceptance_notes")]
    pub struct PrivateCategory {
        #[field(many2one, comodel = "acceptance_notes.panel_private")]
        pub panel_private_id: Option<RecId>,
        #[field(many2many, comodel = "acceptance_notes.panel_private")]
        pub panel_private_ids: Vec<RecId>,
        #[field(selection, selection = [("secret", "Secret")], groups = "base.group_system")]
        pub panel_secret: Option<Arc<str>>,
    }
    // No ACL is installed for this fixture model: ordinary callers cannot read it.
    #[model(name = "acceptance_notes.panel_private", addon = "acceptance_notes")]
    pub struct PrivateModel {
        #[field(char)]
        pub name: Option<Arc<str>>,
    }
    #[extend(ResUsers, addon = "acceptance_notes")]
    pub struct PrivateCriterion {
        #[field(selection, selection = [("secret", "Secret")], groups = "base.group_system")]
        pub panel_secret: Option<Arc<str>>,
    }
}
fn declaration<const MODE: u8>() -> ViewResult<ViewTypeDecl> {
    fn validate(_: &CheckedViewInput<'_>, budget: &DescriptionBudget) -> ViewResult<()> {
        budget.check()
    }
    fn build<const MODE: u8>(
        input: &CheckedViewInput<'_>,
        b: DescriptionValueBuilder<'_>,
    ) -> ViewResult<DescriptionValue> {
        b.object(|root| {
            root.set("root",|b|b.integer(0))?;
            root.set("nodes",|b|b.array(1,|_,b|b.variant("search",|b|b.object(|node| {
                node.set("children",|b|b.array(0,|_,b|b.integer(0)))?;
                node.set("search_panel",|b|b.variant("sections",|b|b.object(|panel| {
                    panel.set("version",|b|b.integer(1))?;
                    panel.set("view_types",|b|b.array(1,|_,b|b.text("list")))?;
                    panel.set("sections",|b|b.array(if MODE==18 {4}else if MODE==7 || matches!(MODE,13..=15) {3}else{2},|i,b| {
                        let category=if MODE==18 {i==0 || i==2}else if MODE==19 {i==0}else if MODE==16 {true} else if MODE>=14 {i==0} else {i!=1};
                        b.variant(if category {"category"} else {"filter"},|b|b.object(|section| {
                            section.set("id",|b|b.integer(i as i64+1))?;
                            section.set("field",|b|b.field_handle(if MODE==1 && i==1 {"res.company"} else {input.root_model()}, if category {if MODE==16 && i==1 {"panel_private_id"}else if MODE==6 {"name"} else if matches!(MODE,13|18|19) && i==0 {"panel_secret"} else {"priority"}} else {if MODE==17 {"panel_private_ids"}else{"owner_id"}}))?;
                            section.set("label",|b|b.text(if matches!(MODE,13|18|19) && i==0 {"Denied category label"}else if matches!(MODE,13|18|19) && i==1 {"Dependent filter label"}else{"Private callback label"}))?;
                            section.set("enable_counters",|b|b.boolean(true))?;
                            section.set("expand",|b|b.boolean(false))?;
                            section.set("limit",|b|b.integer(200))?;
                            if category { section.set("hierarchize",|b|b.boolean(true))?; }
                            else {
                                let source=if i==3 {"[('login','=','kept')]"}else{match MODE {13|18|19=>"[('login','=',panel_secret)]",14=>"[('panel_secret','=',priority)]",4=>"[('login','=',unknown_name)]",5=>"[('login','=',priority+1)]",8=>"[('missing','=',priority)]",_=>"[('login','=',priority)]"}};
                                let expr=erp_sdk::expr::parse(source).unwrap();
                                let ast=erp_sdk::expr::wire::ValidatedAst::from_expr(&expr).unwrap();
                                if (i==1 && MODE<16) || matches!(MODE,18|19) { section.set("domain",|b|b.scoped_domain(if MODE==2 {"res.company"}else{"res.users"},&ast))?; }
                                if i==1 && matches!(MODE,3|9|12|15) {
                                    section.set("group_by",|b|b.field_handle(if MODE==3 {"res.company"}else{"res.users"},if MODE==9 {"missing"}else if MODE==15 {"panel_secret"}else if MODE==12 {"group_ids"}else{"name"}))?;
                                }
                            }
                            Ok(())
                        }))
                    }))?;
                    Ok(())
                })))?;
                Ok(())
            }))))
        })
    }
    ViewTypeDecl::builder("web", "search")
        .wire_major(1)
        .renderer("web.search_sections_v1")
        .schema(&search_schema()?)
        .validator(validate)
        .builder(build::<MODE>)
        .metadata_fields(MetadataFieldSet::builder().arch_references().finish()?)
        .methods(&[])
        .finish()
}
fn list() -> ViewResult<ViewTypeDecl> {
    (erp_addon_web::LIST_VIEW_TYPE)()
}
fn form() -> ViewResult<ViewTypeDecl> {
    (erp_addon_web::FORM_VIEW_TYPE)()
}
async fn call<const MODE: u8>(
    orm: &Orm,
    db: &TestDb,
    principal: erp_orm::Principal,
    view: i64,
) -> erp_rpc::RpcResponse {
    let types = Box::leak(Box::new([
        list as ViewTypeDeclFn,
        form as ViewTypeDeclFn,
        declaration::<MODE> as ViewTypeDeclFn,
    ]));
    get_views_with_types(orm, db, principal, view, "search", types).await
}
async fn validation(orm: &Orm, db: &TestDb, principal: erp_orm::Principal, view: i64) {
    let good = call::<0>(orm, db, principal.clone(), view).await;
    assert_eq!(
        good.status(),
        200,
        "{}",
        String::from_utf8_lossy(good.body())
    );
    assert_eq!(
        result(&good)["views"][0]["nodes"][0]["search_panel"]["sections"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    let cases = [
        (
            call::<1>(orm, db, principal.clone(), view).await,
            "Search panel section 2 has an invalid root field scope.",
        ),
        (
            call::<2>(orm, db, principal.clone(), view).await,
            "Search panel section 2 has an invalid domain model.",
        ),
        (
            call::<3>(orm, db, principal.clone(), view).await,
            "Search panel section 2 has an invalid grouping scope.",
        ),
        (
            call::<4>(orm, db, principal.clone(), view).await,
            "Search panel section 2 has an undeclared category dependency.",
        ),
        (
            call::<5>(orm, db, principal.clone(), view).await,
            "Search panel section 2 has an unsupported domain syntax; additional syntax requires a later release.",
        ),
        (
            call::<6>(orm, db, principal.clone(), view).await,
            "Search panel section 1 has an unsupported field type; additional syntax requires a later release.",
        ),
        (
            call::<7>(orm, db, principal.clone(), view).await,
            "Search panel section 3 has an ambiguous category binding.",
        ),
        (
            call::<8>(orm, db, principal.clone(), view).await,
            "Search panel section 2 references an invalid domain field.",
        ),
        (
            call::<9>(orm, db, principal.clone(), view).await,
            "Search panel section 2 references an invalid grouping field.",
        ),
        (
            call::<12>(orm, db, principal.clone(), view).await,
            "Search panel section 2 has an unsupported grouping field; additional syntax requires a later release.",
        ),
    ];
    for (response, message) in cases {
        assert_eq!(
            response.status(),
            422,
            "{}",
            String::from_utf8_lossy(response.body())
        );
        assert_eq!(result(&response)["name"], "erp.exceptions.ValidationError");
        assert_eq!(result(&response)["message"], message);
    }
}

pub(super) async fn pruning(
    orm: &Orm,
    db: &TestDb,
    principal: erp_orm::Principal,
    admin: erp_orm::Principal,
    view: i64,
) {
    for (response, ids) in [
        (call::<13>(orm, db, principal.clone(), view).await, vec![3]),
        (
            call::<14>(orm, db, principal.clone(), view).await,
            vec![1, 3],
        ),
        (
            call::<15>(orm, db, principal.clone(), view).await,
            vec![1, 3],
        ),
        (call::<16>(orm, db, principal.clone(), view).await, vec![1]),
        (call::<17>(orm, db, principal.clone(), view).await, vec![1]),
    ] {
        assert_eq!(
            response.status(),
            200,
            "{}",
            String::from_utf8_lossy(response.body())
        );
        let body = result(&response);
        let panel = &body["views"][0]["nodes"][0]["search_panel"];
        let sections = panel["sections"].as_array().unwrap();
        assert_eq!(
            sections
                .iter()
                .map(|s| s["id"].as_u64().unwrap())
                .collect::<Vec<_>>(),
            ids
        );
        assert!(panel["notice"].is_null(), "{body}");
        for section in sections {
            assert_eq!(section["enable_counters"], true);
            assert_eq!(section["default_values"], serde_json::json!([]));
            if section["kind"] == "filter" {
                assert!(section["domain"].is_null());
            }
        }
        assert!(!panel.to_string().contains("panel_secret"));
        assert!(!panel.to_string().contains("Denied category label"));
        assert!(!panel.to_string().contains("Dependent filter label"));
        assert!(body["fields"]["acceptance_notes.note"]["panel_secret"].is_null());
    }
    let response = call::<18>(orm, db, principal.clone(), view).await;
    assert_eq!(
        response.status(),
        200,
        "{}",
        String::from_utf8_lossy(response.body())
    );
    let body = result(&response);
    let panel = &body["views"][0]["nodes"][0]["search_panel"];
    assert_eq!(panel["sections"][0]["id"], 3);
    assert_eq!(panel["sections"][1]["id"], 4);
    assert_eq!(panel["sections"].as_array().unwrap().len(), 2);
    let expr = erp_sdk::expr::parse("[('login','=','kept')]").unwrap();
    let document = erp_sdk::expr::wire::WireDocument::from_expr(&expr).unwrap();
    assert_eq!(
        panel["sections"][1]["domain"]["ast"],
        serde_json::to_value(document).unwrap()
    );
    assert_eq!(panel["sections"][0]["enable_counters"], false);
    assert_eq!(panel["sections"][1]["enable_counters"], true);
    assert_eq!(
        panel["notice"],
        "Category counters are unavailable while a filter domain is active."
    );
    assert!(!panel.to_string().contains("panel_secret"));
    let empty = call::<19>(orm, db, principal, view).await;
    assert_eq!(
        empty.status(),
        200,
        "{}",
        String::from_utf8_lossy(empty.body())
    );
    assert!(result(&empty)["views"][0]["nodes"][0]["search_panel"].is_null());
    let readable = call::<13>(orm, db, admin, view).await;
    assert_eq!(
        readable.status(),
        200,
        "{}",
        String::from_utf8_lossy(readable.body())
    );
    assert_eq!(
        result(&readable)["views"][0]["nodes"][0]["search_panel"]["sections"]
            .as_array()
            .unwrap()
            .len(),
        3
    );
}

// The installed base grants no ordinary res.users model read. Explicit fixture authority
// isolates protected criterion/group fields from a model-level denial, then is revoked.
pub(super) async fn run(
    orm: &Orm,
    db: &TestDb,
    principal: erp_orm::Principal,
    admin: erp_orm::Principal,
    view: i64,
) {
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    let model: i64 = tx
        .query("SELECT id FROM ir_model WHERE model=$1", &[&"res.users"])
        .await
        .unwrap()[0]
        .get(0);
    let group: i64 = tx
        .query(
            "SELECT res_id FROM ir_model_data WHERE module='base' AND name='group_user'",
            &[],
        )
        .await
        .unwrap()[0]
        .get(0);
    let grant = tx
        .env()
        .records("ir.access")
        .unwrap()
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell(
                        "name",
                        Cell::Str("Panel callback fixture comodel read".into()),
                    )
                    .set_cell("model_id", Cell::Id(model))
                    .set_cell("group_id", Cell::Id(group))
                    .set_cell("operation", Cell::Str("r".into()))
                    .set_cell("domain", Cell::Str("[]".into())),
            ],
        )
        .await
        .unwrap()
        .ids()[0];
    tx.commit().await.unwrap();
    pruning(orm, db, principal.clone(), admin, view).await;
    validation(orm, db, principal, view).await;
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    tx.env()
        .records("ir.access")
        .unwrap()
        .browse([grant])
        .unwrap()
        .unlink(&mut tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
}
