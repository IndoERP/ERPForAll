//! D2: real code/equation runners; the compiled caller policy governs every host call.
use super::*;

#[tokio::test]
async fn luau_host_calls_use_caller_transaction() {
    let h = harness().await;
    let client = connect(&h).await;
    let seeded = seed(&client).await;
    let orm = policy_orm(&h).await;
    let partner = record(&h, "x.partner", vals(&[("name", s("Initial"))])).await;
    client.execute("INSERT INTO ir_model_data(module,name,model,res_id) VALUES('x_partner','script_target','x.partner',$1)", &[&partner]).await.unwrap();
    let code = r#"
        assert(env.uid > 0 and record.id == records[1].id)
        local ref = env.ref("x_partner.script_target")
        assert(ref.model == "x.partner" and ref.id == record.id)
        local before = env.read("x.partner", {record.id}, {"name"})
        assert(before[1].name == "Initial")
        env.write("x.partner", {record.id}, {city = "Code city"})
        local created = env.create("x.partner", {name = "From Luau"})
        local found = env.search("x.partner", {{"name", "=", "From Luau"}}, 1)
        assert(#found == 1 and found[1].id == created.id)
        env.unlink("x.partner", {created.id})
        log("done")
        action = {type = "ir.actions.act_url", url = "/code/" .. record.id}
    "#;
    let id = action(
        &h,
        "Code",
        seeded.partner_model,
        "code",
        vals(&[("code", s(code))]),
    )
    .await;
    let out = run(&orm, seeded.demo, id, "x.partner", &[partner])
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        out.into_value(),
        json!({"type":"ir.actions.act_url","url":format!("/code/{partner}")})
    );
    assert_eq!(
        read(&h, "x.partner", partner, "city").await,
        Value::str("Code city")
    );
    assert!(named(&client, "x_partner", "From Luau").await.is_empty());
    let writer: i64 = client
        .query_one(
            "SELECT write_uid::int8 FROM x_partner WHERE id=$1",
            &[&partner],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(writer, seeded.demo);
}

#[tokio::test]
async fn luau_host_failure_cannot_be_swallowed() {
    let h = harness().await;
    let client = connect(&h).await;
    let seeded = seed(&client).await;
    let orm = policy_orm(&h).await;
    let partner = record(
        &h,
        "x.partner",
        vals(&[("name", s("P")), ("city", s("Before"))]),
    )
    .await;
    let country = record(&h, "x.country", vals(&[("name", s("Country"))])).await;
    let code = format!(
        r#"
      env.write("x.partner", {{record.id}}, {{city="must roll back"}})
      local ok = pcall(function() env.write("x.country", {{{country}}}, {{name="Denied"}}) end)
      env.write("x.partner", {{record.id}}, {{city="swallowed"}})
      return {{type="ir.actions.act_url", url="/bad"}}
    "#
    );
    let id = action(
        &h,
        "Code failure",
        seeded.partner_model,
        "code",
        vals(&[("code", s(&code))]),
    )
    .await;
    let err = run(&orm, seeded.demo, id, "x.partner", &[partner])
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Access);
    let mut direct_tx = orm.begin(UserId::new(seeded.demo).unwrap()).await.unwrap();
    let direct = direct_tx
        .env()
        .records("x.country")
        .unwrap()
        .browse([country])
        .unwrap()
        .write(&mut direct_tx, vals(&[("name", s("Denied"))]))
        .await
        .unwrap_err();
    assert_eq!(err.message(), direct.message());
    assert_eq!(err.sqlstate(), direct.sqlstate());
    assert_eq!(
        std::error::Error::source(&err).is_some(),
        std::error::Error::source(&direct).is_some()
    );
    direct_tx.rollback().await.unwrap();
    assert_eq!(
        read(&h, "x.partner", partner, "city").await,
        Value::str("Before")
    );
    assert_eq!(
        read(&h, "x.country", country, "name").await,
        Value::str("Country")
    );
}

#[tokio::test]
async fn luau_results_are_explicit_and_validated() {
    let h = harness().await;
    let client = connect(&h).await;
    let seeded = seed(&client).await;
    let orm = policy_orm(&h).await;
    for code in [
        "return {type='ir.actions.act_url', url='/return'}",
        "action={type='ir.actions.act_url', url='/global'}",
        "assert(record == nil and #records == 0); return nil",
    ] {
        let id = action(
            &h,
            "Result",
            seeded.partner_model,
            "code",
            vals(&[("code", s(code))]),
        )
        .await;
        let out = run(&orm, seeded.demo, id, "x.partner", &[]).await.unwrap();
        assert_eq!(out.is_some(), !code.ends_with("return nil"));
    }
    for code in [
        "action={type='x'}; return {type='y'}",
        "return 4",
        "return {}",
        "return {type=''}",
    ] {
        let id = action(
            &h,
            "Invalid result",
            seeded.partner_model,
            "code",
            vals(&[("code", s(code))]),
        )
        .await;
        let error = run(&orm, seeded.demo, id, "x.partner", &[])
            .await
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Programming);
        assert!(error.message().contains("action"));
    }
}

#[tokio::test]
async fn luau_equation_has_per_record_context() {
    let h = harness().await;
    let client = connect(&h).await;
    let seeded = seed(&client).await;
    let orm = policy_orm(&h).await;
    let a = record(&h, "x.partner", vals(&[("name", s("A"))])).await;
    let b = record(&h, "x.partner", vals(&[("name", s("B"))])).await;
    let id = action(
        &h,
        "Equation",
        seeded.partner_model,
        "object_write",
        vals(&[
            ("update_path", s("city")),
            ("evaluation_type", s("equation")),
            (
                "value",
                s("env.read('x.partner', {record.id}, {'name'})[1].name .. '-equation'"),
            ),
        ]),
    )
    .await;
    assert!(
        run(&orm, seeded.demo, id, "x.partner", &[a, b])
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(
        read(&h, "x.partner", a, "city").await,
        Value::str("A-equation")
    );
    assert_eq!(
        read(&h, "x.partner", b, "city").await,
        Value::str("B-equation")
    );
}

#[tokio::test]
async fn luau_host_input_is_bounded_and_strict() {
    let h = harness().await;
    let client = connect(&h).await;
    let seeded = seed(&client).await;
    let orm = policy_orm(&h).await;
    for (code, expected) in [
        ("env.search('x.partner', {}, 0)", "wrong arguments"),
        ("env.search('x.partner', {}, 257)", "wrong arguments"),
        ("env.read('x.partner', {}, {})", "explicit fields"),
        ("env.write('x.partner', {1,1}, {})", "positive unique"),
        ("env.unlink('x.partner', {-1})", "positive unique"),
        ("env.create('x.partner', {id=1})", "id is not writable"),
        ("env.ref('unqualified')", "module.name"),
        ("log(string.rep('x', 65537))", "log exceeds"),
        ("for i=1,1025 do log('') end", "1024 host calls"),
    ] {
        let id = action(
            &h,
            "Input boundary",
            seeded.partner_model,
            "code",
            vals(&[("code", s(code))]),
        )
        .await;
        let error = run(&orm, seeded.demo, id, "x.partner", &[])
            .await
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Validation, "{code}: {error:?}");
        assert!(error.message().contains(expected), "{code}: {error:?}");
    }
    let id = action(
        &h,
        "Log boundary",
        seeded.partner_model,
        "code",
        vals(&[("code", s("log(string.rep('x',65536)); return nil"))]),
    )
    .await;
    assert!(
        run(&orm, seeded.demo, id, "x.partner", &[])
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn dropping_luau_action_does_not_leave_transaction_borrowed() {
    let h = harness().await;
    let client = connect(&h).await;
    let seeded = seed(&client).await;
    let orm = policy_orm(&h).await;
    let id = action(
        &h,
        "Cancelled",
        seeded.partner_model,
        "code",
        vals(&[("code", s("while true do end"))]),
    )
    .await;
    let mut tx = orm.begin(UserId::new(seeded.demo).unwrap()).await.unwrap();
    let env = tx.env();
    let req = RunRequest::new(ActionId::new(id).unwrap(), "x.partner", &[]);
    let result = tokio::time::timeout(
        std::time::Duration::from_millis(10),
        run_server_action(&mut tx, &env, &req),
    )
    .await;
    assert!(
        result.is_err(),
        "the action must not finish before cancellation"
    );
    tx.rollback().await.unwrap();
}

#[tokio::test]
async fn luau_host_reads_and_writes_enforce_target_policy() {
    let h = harness().await;
    let client = connect(&h).await;
    let seeded = seed(&client).await;
    let orm = policy_orm(&h).await;
    let partner = record(
        &h,
        "x.partner",
        vals(&[("name", s("P")), ("script_secret", s("Secret"))]),
    )
    .await;
    let locked = record(&h, "x.partner", vals(&[("name", s("Locked"))])).await;
    client.execute("INSERT INTO ir_model_data(module,name,model,res_id) VALUES('x_partner','locked_target','x.partner',$1)", &[&locked]).await.unwrap();
    client.execute("INSERT INTO ir_model_data(module,name,model,res_id) VALUES('x_partner','hidden_user','res.users',$1)",&[&seeded.admin]).await.unwrap();
    for code in [
        "env.ref('x_partner.hidden_user')".to_owned(),
        format!("return env.read('x.partner', {{{partner}}}, {{'script_secret'}})"),
        format!("env.write('x.partner', {{{partner}}}, {{script_secret='Changed'}})"),
        format!("env.write('x.partner', {{{locked}}}, {{name='Bypassed'}})"),
    ] {
        let id = action(
            &h,
            "Target policy",
            seeded.partner_model,
            "code",
            vals(&[("code", s(&code))]),
        )
        .await;
        let error = run(&orm, seeded.demo, id, "x.partner", &[])
            .await
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Access, "{code}: {error:?}");
    }
    assert_eq!(
        read(&h, "x.partner", partner, "script_secret").await,
        Value::str("Secret")
    );
    assert_eq!(
        read(&h, "x.partner", locked, "name").await,
        Value::str("Locked")
    );
    let oversized = "x".repeat(1_048_577);
    client
        .execute(
            "UPDATE x_partner SET note=$2 WHERE id=$1",
            &[&partner, &oversized],
        )
        .await
        .unwrap();
    let code = format!("env.read('x.partner', {{{partner}}}, {{'note'}})");
    let id = action(
        &h,
        "Read bounds",
        seeded.partner_model,
        "code",
        vals(&[("code", s(&code))]),
    )
    .await;
    let error = run(&orm, seeded.demo, id, "x.partner", &[])
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Validation);
    assert!(error.message().contains("byte, node or depth limit"));
}

#[tokio::test]
async fn two_addon_extension_runners_still_execute() {
    let h = harness().await;
    let client = connect(&h).await;
    let seeded = seed(&client).await;
    let orm = policy_orm(&h).await;
    let partner = record(&h, "x.partner", vals(&[("name", s("P"))])).await;
    for (state, kind) in [
        ("x_open_window", "ir.actions.act_window"),
        ("x_open_url", "ir.actions.act_url"),
    ] {
        let id = action(&h, state, seeded.partner_model, state, RawVals::new()).await;
        assert_eq!(
            run(&orm, seeded.demo, id, "x.partner", &[partner])
                .await
                .unwrap()
                .unwrap()
                .action_type(),
            kind
        );
    }
}

#[tokio::test]
async fn luau_context_and_active_record_admission() {
    let h = harness().await;
    let client = connect(&h).await;
    let seeded = seed(&client).await;
    let orm = policy_orm(&h).await;
    let code = "assert(env.context.marker == 'kept' and context.marker == 'kept'); assert(context.active_id == nil); assert(#context.active_ids == 0); return nil";
    let id = action(
        &h,
        "Context",
        seeded.partner_model,
        "code",
        vals(&[("code", s(code))]),
    )
    .await;
    let mut tx = orm.begin(UserId::new(seeded.demo).unwrap()).await.unwrap();
    let env = tx.env().with_context([
        ("marker", Value::str("kept")),
        ("active_id", Value::Int(999)),
    ]);
    let req = RunRequest::new(ActionId::new(id).unwrap(), "x.partner", &[]);
    assert!(
        run_server_action(&mut tx, &env, &req)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(env.context().get("active_id"), Some(Value::Int(999)));
    tx.rollback().await.unwrap();
    drop(tx);
    let mut tx = orm.begin(UserId::new(seeded.demo).unwrap()).await.unwrap();
    let env = tx
        .env()
        .with_context((0..257).map(|i| (format!("k{i}"), Value::Int(1))));
    let error = run_server_action(&mut tx, &env, &req).await.unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Validation);
    assert_eq!(error.message(), "Context has more than 256 entries");
    tx.rollback().await.unwrap();
}

#[tokio::test]
async fn luau_active_ids_refused_before_context_materialization() {
    let h = harness().await;
    let client = connect(&h).await;
    let seeded = seed(&client).await;
    let orm = policy_orm(&h).await;
    let id = action(
        &h,
        "Admission",
        seeded.partner_model,
        "code",
        vals(&[("code", s("return nil"))])
            .set_commands("group_ids", [Command::link(rid(seeded.group_runner))]),
    )
    .await;
    let input: Vec<i64> = (1..=257).collect();
    let error = run(&orm, seeded.runner, id, "x.partner", &input)
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Validation);
    assert_eq!(
        error.message(),
        "Server action script accepts at most 256 active records"
    );
}

#[tokio::test]
async fn luau_preserves_html_and_system_field_refusals() {
    let h = harness().await;
    let client = connect(&h).await;
    let seeded = seed(&client).await;
    let orm = policy_orm(&h).await;
    let partner = record(&h, "x.partner", vals(&[("name", s("P"))])).await;
    for (field, kind) in [
        ("id", ErrorKind::Validation),
        ("body", ErrorKind::UserError),
    ] {
        let id = action(
            &h,
            "Equation guard",
            seeded.partner_model,
            "object_write",
            vals(&[
                ("update_path", s(field)),
                ("evaluation_type", s("equation")),
                ("value", s("1")),
            ]),
        )
        .await;
        let error = run(&orm, seeded.demo, id, "x.partner", &[partner])
            .await
            .unwrap_err();
        assert_eq!(error.kind(), kind);
        if field == "body" {
            assert!(error.message().contains("a later release"));
        }
    }
    let id = action(
        &h,
        "Host HTML guard",
        seeded.partner_model,
        "code",
        vals(&[(
            "code",
            s("env.write('x.partner', {record.id}, {body='<p>unsafe</p>'})"),
        )]),
    )
    .await;
    let error = run(&orm, seeded.demo, id, "x.partner", &[partner])
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::UserError);
    assert!(error.message().contains("a later release"));
    assert_eq!(read(&h, "x.partner", partner, "body").await, Value::False);
}

#[tokio::test]
async fn dropping_blocked_host_call_cannot_run_another_callback() {
    let h = harness().await;
    let observer = connect(&h).await;
    let locker = connect(&h).await;
    let seeded = seed(&observer).await;
    let orm = policy_orm(&h).await;
    let partner = record(&h, "x.partner", vals(&[("name", s("Preserved"))])).await;
    let id = action(&h, "Cancel host", seeded.partner_model, "code", vals(&[("code", s("env.unlink('x.partner', {record.id}); env.create('x.partner', {name='After cancellation'})"))])).await;
    locker.batch_execute("BEGIN").await.unwrap();
    locker
        .query_one(
            "SELECT id FROM x_partner WHERE id=$1 FOR UPDATE",
            &[&partner],
        )
        .await
        .unwrap();
    let mut tx = orm.begin(UserId::new(seeded.demo).unwrap()).await.unwrap();
    let env = tx.env();
    let ids = real(&[partner]);
    let req = RunRequest::new(ActionId::new(id).unwrap(), "x.partner", &ids);
    let mut future = Box::pin(run_server_action(&mut tx, &env, &req));
    let mut blocked = false;
    for _ in 0..100 {
        tokio::select! {
            result=&mut future=>panic!("host call finished before cancellation: {result:?}"),
            ()=tokio::time::sleep(std::time::Duration::from_millis(5))=>{},
        }
        let count: i64 = observer.query_one("SELECT count(*) FROM pg_stat_activity WHERE datname=current_database() AND wait_event_type='Lock'", &[]).await.unwrap().get(0);
        if count > 0 {
            blocked = true;
            break;
        }
    }
    assert!(
        blocked,
        "the real host mutation must be waiting before cancellation"
    );
    drop(future);
    locker.batch_execute("ROLLBACK").await.unwrap();
    tx.rollback().await.unwrap();
    assert_eq!(
        read(&h, "x.partner", partner, "name").await,
        Value::str("Preserved")
    );
    assert!(
        named(&observer, "x_partner", "After cancellation")
            .await
            .is_empty()
    );
}

async fn nested_html_refusal(create_child: bool) {
    let h = harness().await;
    let client = connect(&h).await;
    let seeded = seed(&client).await;
    let orm = policy_orm(&h).await;
    let parent = record(&h, "x.partner", vals(&[("name", s("Parent"))])).await;
    let child = record(
        &h,
        "x.partner",
        vals(&[("name", s("Child")), ("parent_id", Cell::Id(parent))]),
    )
    .await;
    let source = if create_child {
        "env.write('x.partner', {record.id}, {child_ids={{0,0,{name='NestedHTML',body='<p>raw</p>'}}}})".to_owned()
    } else {
        "env.write('x.partner', {record.id}, {child_ids={{1,CHILDID,{body='<p>raw</p>'}}}})"
            .replace("CHILDID", &child.to_string())
    };
    let id = action(
        &h,
        "Nested guard",
        seeded.partner_model,
        "code",
        vals(&[("code", s(&source))]),
    )
    .await;
    let error = run(&orm, seeded.demo, id, "x.partner", &[parent])
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::UserError);
    assert!(error.message().contains("a later release"));
    assert_eq!(read(&h, "x.partner", child, "body").await, Value::False);
    assert!(named(&client, "x_partner", "NestedHTML").await.is_empty());
}
#[tokio::test]
async fn nested_html_create_is_refused() {
    nested_html_refusal(true).await;
}
#[tokio::test]
async fn nested_html_update_is_refused() {
    nested_html_refusal(false).await;
}

#[tokio::test]
async fn nested_command_work_has_explicit_boundary() {
    let h = harness().await;
    let client = connect(&h).await;
    let seeded = seed(&client).await;
    let orm = policy_orm(&h).await;
    let partner = record(&h, "x.partner", vals(&[("name", s("P"))])).await;
    let tag = record(&h, "x.tag", vals(&[("name", s("T"))])).await;
    let plain = "env.write('x.partner', {record.id}, {child_ids={{0,0,{name='NestedPlain'}}}})";
    let id = action(
        &h,
        "Plain nested",
        seeded.partner_model,
        "code",
        vals(&[("code", s(plain))]),
    )
    .await;
    assert!(
        run(&orm, seeded.demo, id, "x.partner", &[partner])
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(named(&client, "x_partner", "NestedPlain").await.len(), 1);
    for count in [256, 257] {
        let code = format!(
            "local commands={{}}; for i=1,{count} do commands[i]={{4,{tag},0}} end; env.write('x.partner', {{record.id}}, {{tag_ids=commands}})"
        );
        let id = action(
            &h,
            "Command boundary",
            seeded.partner_model,
            "code",
            vals(&[("code", s(&code))]),
        )
        .await;
        let result = run(&orm, seeded.demo, id, "x.partner", &[partner]).await;
        if count == 256 {
            assert!(result.unwrap().is_none());
        } else {
            let error = result.unwrap_err();
            assert_eq!(error.kind(), ErrorKind::Validation);
            assert!(error.message().contains("256 command records"));
        }
    }
}

#[tokio::test]
async fn nested_html_equation_is_refused() {
    let h = harness().await;
    let client = connect(&h).await;
    let seeded = seed(&client).await;
    let orm = policy_orm(&h).await;
    let parent = record(&h, "x.partner", vals(&[("name", s("P"))])).await;
    let id = action(
        &h,
        "Equation nested guard",
        seeded.partner_model,
        "object_write",
        vals(&[
            ("update_path", s("child_ids")),
            ("evaluation_type", s("equation")),
            (
                "value",
                s("{{0,0,{name='NestedEquation',body='<p>raw</p>'}}}"),
            ),
        ]),
    )
    .await;
    let error = run(&orm, seeded.demo, id, "x.partner", &[parent])
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::UserError);
    assert!(error.message().contains("a later release"));
    assert!(
        named(&client, "x_partner", "NestedEquation")
            .await
            .is_empty()
    );
}
