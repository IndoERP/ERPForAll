//! R1: the request replaces all active-record context, including an absent active_id.
use super::*;

static CONTEXT_ADDON: AddonDescriptor = AddonDescriptor {
    name: "test_active_context",
    depends: &["base"],
    exact_only: false,
};
const CONTEXT_STATE: &[FieldDecl] = &[FieldDecl::selection_add_ondelete(
    "state",
    &[("x_check_active_context", Some("Check active context"))],
    &[(
        "x_check_active_context",
        erp_registry::SelectionOnDeleteDecl::Cascade,
    )],
)];

fn check_active_context(
    ctx: RunnerCtx<'_>,
) -> BoxFuture<'_, erp_fields::Result<Option<ActionResult>>> {
    Box::pin(async move {
        let ids = ctx.records().ids();
        let context = ctx.env().context();
        assert_eq!(context.get("active_model"), Some(Value::str("x.partner")));
        assert_eq!(
            context.get("active_ids"),
            Some(Value::List(ids.iter().copied().map(Value::Int).collect()))
        );
        assert_eq!(
            context.get("active_id"),
            ids.first().copied().map(Value::Int)
        );
        assert_eq!(
            context.get("unrelated_marker"),
            Some(Value::str("preserved"))
        );
        assert_eq!(
            ctx.records().env().context().get("active_id"),
            context.get("active_id")
        );
        Ok(Some(ActionResult::new(
            json!({"type": "ir.actions.act_window"}),
        )?))
    })
}

erp_addon_base::server_action_runner!(
    CONTEXT_RUNNER = { addon: CONTEXT_ADDON, state: "x_check_active_context", multi: true, run: check_active_context }
);

#[tokio::test]
async fn request_replaces_stale_active_context() {
    let mut models = decls();
    models.push(
        ModelDecl::extension(
            "test_active_context",
            "ir.actions.server",
            &["ir.actions.server"],
        )
        .fields(CONTEXT_STATE),
    );
    let h = Harness::builder([
        &erp_addon_base::ADDON,
        &erp_data::model_data::ADDON,
        &PARTNER,
        &EXT_A,
        &EXT_B,
        &EXT_NORUNNER,
        &CONTEXT_ADDON,
    ])
    .with_decls(models)
    .build()
    .await
    .unwrap();
    let client = connect(&h).await;
    let seeded = seed(&client).await;
    let orm = policy_orm(&h).await;
    let id = action(
        &h,
        "Context",
        seeded.partner_model,
        "x_check_active_context",
        RawVals::new(),
    )
    .await;
    let first = record(&h, "x.partner", vals(&[("name", s("First"))])).await;
    let second = record(&h, "x.partner", vals(&[("name", s("Second"))])).await;
    for requested in [vec![], vec![first], vec![first, second], vec![]] {
        let mut tx = orm.begin(UserId::new(seeded.demo).unwrap()).await.unwrap();
        let env = tx.env().with_context([
            ("active_model", Value::str("x.country")),
            ("active_ids", Value::List(Arc::from([Value::Int(987)]))),
            ("active_id", Value::Int(987)),
            ("unrelated_marker", Value::str("preserved")),
        ]);
        let ids = real(&requested);
        let request = RunRequest::new(ActionId::new(id).unwrap(), "x.partner", &ids);
        assert!(
            run_server_action(&mut tx, &env, &request)
                .await
                .unwrap()
                .is_some()
        );
        // The derived runner context must not mutate the caller's environment.
        assert_eq!(env.context().get("active_id"), Some(Value::Int(987)));
        tx.rollback().await.unwrap();
    }
    h.cleanup().await.unwrap();
}
