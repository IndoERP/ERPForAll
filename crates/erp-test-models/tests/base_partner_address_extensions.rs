//! Independently authored addon dispatch and field-list extension probes.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]
use erp_addon_base::ResPartner;
use erp_sdk::prelude::*;
use erp_test_models::base_users;
use erp_testkit::Harness;

erp_sdk::addon!(name = "test_partner_address_extensions", depends = ["base"]);

#[derive(Default)]
struct CancelHook(std::sync::Arc<tokio::sync::Notify>);

#[derive(Default)]
struct AssignmentStress(bool);

#[derive(Default)]
struct AfterSuperObservations(Vec<(i64, Value, Value)>);
#[extend(ResPartner, addon = "test_partner_address_extensions")]
pub struct AddressExtension {
    #[field(float)]
    pub address_coordinate: f64,
    #[field(char)]
    pub address_note: Option<Arc<str>>,
    #[field(boolean)]
    pub address_flag: bool,
    #[field(integer)]
    pub address_floor: i32,
    #[field(selection, selection = [("east", "East"), ("west", "West")])]
    pub address_zone: Option<Arc<str>>,
    #[field(many2one, comodel = "res.partner")]
    pub address_contact_id: Option<RecId>,
    #[field(
        many2many,
        comodel = "res.partner",
        relation = "test_partner_address_peer_rel",
        column1 = "owner_id",
        column2 = "peer_id"
    )]
    pub address_peer_ids: Vec<RecId>,
}
#[methods(ResPartner, addon = "test_partner_address_extensions")]
impl AddressExtension {
    #[method(override)]
    async fn _address_fields(
        this: &RecordSet<ResPartner>,
        tx: &mut Transaction,
        sup: Super,
    ) -> Result<Vec<String>> {
        let mut fields: Vec<String> = sup.call(tx, this.as_records(), ()).await?;
        if this.env().context().get("wide_address_hook") == Some(Value::Bool(true)) {
            return Ok([
                "street",
                "street2",
                "zip",
                "city",
                "name",
                "ref",
                "vat",
                "website",
                "active",
                "type",
                "email",
                "phone",
                "color",
                "tz",
                "lang",
                "industry_id",
            ]
            .into_iter()
            .map(str::to_owned)
            .collect());
        }
        fields.extend(["address_coordinate".to_owned(), "address_note".to_owned()]);
        if this.env().context().get("relational_address_hook") == Some(Value::Bool(true)) {
            fields.extend([
                "address_flag".to_owned(),
                "address_floor".to_owned(),
                "address_zone".to_owned(),
                "address_contact_id".to_owned(),
                "address_peer_ids".to_owned(),
            ]);
        }
        if let Some(Value::Str(field)) = this.env().context().get("invalid_address_hook") {
            fields.push(field.to_string());
        }
        Ok(fields)
    }
    #[method(override)]
    async fn _formatting_address_fields(
        this: &RecordSet<ResPartner>,
        tx: &mut Transaction,
        sup: Super,
    ) -> Result<Vec<String>> {
        let mut names: Vec<String> = sup.call(tx, this.as_records(), ()).await?;
        names.retain(|name| name != "address_coordinate");
        Ok(names)
    }

    #[method(override)]
    async fn _fields_sync(
        this: &RecordSet<ResPartner>,
        tx: &mut Transaction,
        changes: Vec<RawVals>,
        sup: Super,
    ) -> Result<()> {
        let count = match this.env().context().get("address_repeat_writes") {
            Some(Value::Int(count)) if (1..=65).contains(&count) => count,
            None => 0,
            Some(_) => {
                return Err(Error::programming(
                    "Address fixture repeat count is invalid",
                ));
            }
        };
        let driver = count > 0 && !tx.extension::<AssignmentStress>().0;
        if driver {
            tx.extension::<AssignmentStress>().0 = true;
        }
        let result = async {
            sup.call(tx, this.as_records(), (changes,)).await?;
            if driver {
                for index in 1..count {
                    let value = format!("Stress {index}");
                    this.as_records()
                        .write(
                            tx,
                            RawVals::new()
                                .set_cell("street", Cell::Str(value.clone().into()))
                                .set_cell("city", Cell::Str(value.into())),
                        )
                        .await?;
                }
            }
            Ok(())
        }
        .await;
        if driver {
            tx.extension::<AssignmentStress>().0 = false;
        }
        result
    }

    #[method(override)]
    async fn write(
        this: &RecordSet<ResPartner>,
        tx: &mut Transaction,
        vals: RawVals,
        sup: Super,
    ) -> Result<()> {
        if let Some(Value::Int(target)) = this.env().context().get("address_wait_target")
            && this.ids().contains(&target)
            && vals.contains("street")
        {
            tx.extension::<CancelHook>().0.notify_one();
            std::future::pending::<()>().await;
        }
        if let Some(Value::Int(target)) = this.env().context().get("address_guard_target")
            && this.ids().contains(&target)
            && vals.contains("street")
        {
            return Err(Error::access("Address fixture write guard refused"));
        }
        let edit_after_super = vals.contains("street")
            && matches!(
                this.env().context().get("address_after_super_target"),
                Some(Value::Int(target)) if this.ids().contains(&target)
            );
        let observe_after_super = vals.contains("street")
            && matches!(
                this.env().context().get("address_observe_after_super_target"),
                Some(Value::Int(target)) if this.ids().contains(&target)
            );
        sup.call(tx, this.as_records(), (vals,)).await?;
        if observe_after_super {
            let ancestor = this.as_records().related(tx, "parent_id").await?;
            let own_street = this.as_records().value(tx, "street").await?;
            let ancestor_street = ancestor.value(tx, "street").await?;
            tx.extension::<AfterSuperObservations>().0.push((
                this.ids()[0],
                own_street,
                ancestor_street,
            ));
        }
        if edit_after_super {
            this.as_records()
                .write(
                    tx,
                    RawVals::new().set_cell("city", Cell::Str("Addon city".into())),
                )
                .await?;
        }
        Ok(())
    }
}
async fn harness() -> Harness {
    Harness::builder([&base_users::ADDON, &erp_addon_base::ADDON, &ADDON])
        .build()
        .await
        .unwrap()
}
async fn create(tx: &mut Transaction, name: &str, parent: Option<i64>) -> Records {
    let model = tx.env().records("res.partner").unwrap();
    let mut vals = RawVals::new().set_cell("name", Cell::Str(name.into()));
    if let Some(id) = parent {
        vals.insert("parent_id".into(), Cell::Id(id));
    }
    model.create(tx, vec![vals]).await.unwrap()
}
#[tokio::test]
async fn downstream_bypasses_addon_guard_upstream_keeps_it_and_failure_cannot_commit() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let parent = create(&mut tx, "Parent", None).await;
    let child = create(&mut tx, "Child", Some(parent.ids()[0])).await;
    parent
        .with_context([("address_guard_target", Value::Int(child.ids()[0]))])
        .write(
            &mut tx,
            RawVals::new().set_cell("street", Cell::Str("Copied".into())),
        )
        .await
        .unwrap();
    assert_eq!(
        child.value(&mut tx, "street").await.unwrap(),
        Value::Str("Copied".into())
    );
    let guarded = child.with_context([("address_guard_target", Value::Int(parent.ids()[0]))]);
    let error = guarded
        .write(
            &mut tx,
            RawVals::new().set_cell("street", Cell::Str("Refused".into())),
        )
        .await
        .unwrap_err();
    assert_eq!(error.kind(), erp_sdk::ErrorKind::Access);
    assert_eq!(error.message(), "Address fixture write guard refused");
    let error = tx.commit().await.unwrap_err();
    assert_eq!(error.kind(), erp_sdk::ErrorKind::Access);
    assert_eq!(error.message(), "Address fixture write guard refused");
    h.cleanup().await.unwrap();
}
#[tokio::test]
async fn address_extension_float_and_text_sync_are_distinct_from_formatting() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let parent = create(&mut tx, "Parent", None).await;
    parent
        .write(
            &mut tx,
            RawVals::new()
                .set_cell("address_coordinate", Cell::Float(42.0))
                .set_cell("address_note", Cell::Str("Note".into())),
        )
        .await
        .unwrap();
    let child = create(&mut tx, "Child", Some(parent.ids()[0])).await;
    assert_eq!(
        child.value(&mut tx, "address_coordinate").await.unwrap(),
        Value::Float(42.0)
    );
    assert_eq!(
        child.value(&mut tx, "address_note").await.unwrap(),
        Value::Str("Note".into())
    );
    let address_fields: Vec<String> = erp_orm::call_method(&mut tx, &parent, "_address_fields", ())
        .await
        .unwrap();
    assert_eq!(
        address_fields,
        [
            "street",
            "street2",
            "zip",
            "city",
            "state_id",
            "country_id",
            "address_coordinate",
            "address_note"
        ]
    );
    let fields: Vec<String> =
        erp_orm::call_method(&mut tx, &parent, "_formatting_address_fields", ())
            .await
            .unwrap();
    assert!(!fields.contains(&"address_coordinate".to_owned()));
    assert!(
        fields.contains(&"address_note".to_owned()),
        "ordinary address additions enter formatting unless its own hook removes them"
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn address_hook_syncs_scalar_selection_many2one_and_many2many_values() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let target = create(&mut tx, "Referenced contact", None).await;
    let parent = create(&mut tx, "Extended parent", None)
        .await
        .with_context([("relational_address_hook", Value::Bool(true))]);
    let child = create(&mut tx, "Extended child", Some(parent.ids()[0])).await;
    parent
        .write(
            &mut tx,
            RawVals::new()
                .set_cell("address_flag", Cell::Bool(true))
                .set_cell("address_floor", Cell::Int(7))
                .set_cell("address_zone", Cell::Str("east".into()))
                .set_cell("address_contact_id", Cell::Id(target.ids()[0]))
                .set_commands(
                    "address_peer_ids",
                    [Command::set([RecId::from_real(target.ids()[0]).unwrap()])],
                ),
        )
        .await
        .unwrap();
    for field in [
        "address_flag",
        "address_floor",
        "address_zone",
        "address_contact_id",
        "address_peer_ids",
    ] {
        assert_eq!(
            child.cells(&mut tx, field).await.unwrap(),
            parent.cells(&mut tx, field).await.unwrap(),
            "{field}"
        );
    }
    parent
        .write(
            &mut tx,
            RawVals::new()
                .set_cell("address_flag", Cell::Bool(false))
                .set_cell("address_floor", Cell::Int(0))
                .set_cell("address_zone", Cell::Null)
                .set_cell("address_contact_id", Cell::Null)
                .set_commands("address_peer_ids", [Command::set([])]),
        )
        .await
        .unwrap();
    for field in [
        "address_flag",
        "address_floor",
        "address_zone",
        "address_contact_id",
        "address_peer_ids",
    ] {
        assert_eq!(
            child.cells(&mut tx, field).await.unwrap(),
            parent.cells(&mut tx, field).await.unwrap(),
            "cleared {field}"
        );
    }
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn zero_float_is_empty_and_does_not_replace_a_childs_own_address() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let parent = create(&mut tx, "Zero coordinate parent", None).await;
    parent
        .write(
            &mut tx,
            RawVals::new().set_cell("address_coordinate", Cell::Float(0.0)),
        )
        .await
        .unwrap();
    let model = tx.env().records("res.partner").unwrap();
    let child = model
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("name", Cell::Str("Own address child".into()))
                    .set_cell("parent_id", Cell::Id(parent.ids()[0]))
                    .set_cell("street", Cell::Str("Child street".into())),
            ],
        )
        .await
        .unwrap();
    assert_eq!(
        child.value(&mut tx, "type").await.unwrap(),
        Value::Str("contact".into())
    );
    assert_eq!(
        parent.value(&mut tx, "street").await.unwrap(),
        Value::Str("Child street".into())
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn invalid_address_hook_fields_are_programming_errors() {
    let h = harness().await;
    for (field, expected) in [
        (
            "absent_address_field",
            "res.partner._address_fields: unknown field 'absent_address_field'",
        ),
        (
            "child_ids",
            "One2Many fields cannot be synchronized as part of `commercial_fields` or `address fields`",
        ),
        (
            "display_name",
            "res.partner._address_fields: unsupported synchronization field 'display_name'",
        ),
    ] {
        let mut tx = h.begin().await.unwrap();
        let record = create(&mut tx, "Invalid hook parent", None).await;
        let error = record
            .with_context([("invalid_address_hook", Value::Str(field.into()))])
            .write(
                &mut tx,
                RawVals::new().set_cell("street", Cell::Str("Changed".into())),
            )
            .await
            .unwrap_err();
        assert_eq!(error.kind(), erp_sdk::ErrorKind::Programming);
        assert_eq!(error.message(), expected);
    }
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn fields_sync_refuses_mismatched_change_map_cardinality() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let record = create(&mut tx, "Cardinality", None).await;
    for changes in [vec![], vec![RawVals::new(), RawVals::new()]] {
        let count = changes.len();
        let error = erp_orm::call_method::<_, ()>(&mut tx, &record, "_fields_sync", (changes,))
            .await
            .unwrap_err();
        assert_eq!(error.kind(), erp_sdk::ErrorKind::Programming);
        assert_eq!(
            error.message(),
            format!("res.partner._fields_sync: expected 1 change maps, got {count}")
        );
    }
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn savepoint_failure_does_not_suppress_a_later_synchronization() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let parent = create(&mut tx, "Parent", None).await;
    let child = create(&mut tx, "Child", Some(parent.ids()[0])).await;
    let sibling = create(&mut tx, "Sibling", Some(parent.ids()[0])).await;
    let guarded = child.with_context([("address_guard_target", Value::Int(parent.ids()[0]))]);
    let error = tx
        .savepoint(async |tx: &mut Transaction| {
            guarded
                .write(
                    tx,
                    RawVals::new().set_cell("street", Cell::Str("Refused".into())),
                )
                .await
        })
        .await
        .unwrap_err();
    assert_eq!(error.kind(), erp_sdk::ErrorKind::Access);
    assert_eq!(error.message(), "Address fixture write guard refused");
    child
        .write(
            &mut tx,
            RawVals::new().set_cell("street", Cell::Str("Recovered".into())),
        )
        .await
        .unwrap();
    for records in [&parent, &child, &sibling] {
        assert_eq!(
            records.value(&mut tx, "street").await.unwrap(),
            Value::str("Recovered")
        );
    }
    tx.commit().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn cancelled_upstream_write_does_not_leave_operation_suppression_after_rollback() {
    let h = harness().await;
    let mut setup = h.begin().await.unwrap();
    let parent = create(&mut setup, "Parent", None).await;
    let child = create(&mut setup, "Child", Some(parent.ids()[0])).await;
    let ids = [parent.ids()[0], child.ids()[0]];
    setup.commit().await.unwrap();
    let mut tx = h.begin().await.unwrap();
    let child = tx
        .env()
        .records("res.partner")
        .unwrap()
        .browse([ids[1]])
        .unwrap();
    let notify = std::sync::Arc::clone(&tx.extension::<CancelHook>().0);
    let waiting = child.with_context([("address_wait_target", Value::Int(ids[0]))]);
    {
        let write = waiting.write(
            &mut tx,
            RawVals::new().set_cell("street", Cell::Str("Cancelled".into())),
        );
        tokio::pin!(write);
        tokio::select! {
            () = notify.notified() => {},
            result = &mut write => panic!("upstream write did not reach cancellation fixture: {result:?}"),
        }
    }
    // Explicit whole-transaction recovery preserves this Transaction's extension slot.
    tx.rollback().await.unwrap();
    child
        .write(
            &mut tx,
            RawVals::new().set_cell("street", Cell::Str("After cancellation".into())),
        )
        .await
        .unwrap();
    let all = tx
        .env()
        .records("res.partner")
        .unwrap()
        .browse(ids)
        .unwrap();
    assert_eq!(
        all.values(&mut tx, "street").await.unwrap(),
        vec![Value::str("After cancellation"); 2]
    );
    tx.commit().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn direct_sync_hook_failure_after_an_earlier_group_cannot_commit() {
    let policy = erp_testkit::DomainPolicy::builder(erp_testkit::Otherwise::Allow)
        .rule(
            "res.partner",
            erp_sdk::Operation::Write,
            Domain::condition("name", Operator::Ne, "Z denied").unwrap(),
        )
        .build()
        .unwrap();
    let h = Harness::builder([&base_users::ADDON, &erp_addon_base::ADDON, &ADDON])
        .policy(policy)
        .build()
        .await
        .unwrap();
    let mut tx = h.begin().await.unwrap();
    tx.batch("INSERT INTO res_users (id, name, login, active, share) VALUES (2, 'Address writer', 'address-writer', true, false)").await.unwrap();
    let parent = create(&mut tx, "Root", None).await;
    parent
        .write(
            &mut tx,
            RawVals::new()
                .set_cell("street", Cell::Str("Desired".into()))
                .set_cell("city", Cell::Str("Desired city".into())),
        )
        .await
        .unwrap();
    let first = create(&mut tx, "A first", Some(parent.ids()[0])).await;
    let denied = create(&mut tx, "Z denied", Some(parent.ids()[0])).await;
    // Original native fixture values deliberately make two different changed-field groups.
    first
        .write_base(
            &mut tx,
            RawVals::new().set_cell("street", Cell::Str("Old street".into())),
        )
        .await
        .unwrap();
    denied
        .write_base(
            &mut tx,
            RawVals::new().set_cell("city", Cell::Str("Old city".into())),
        )
        .await
        .unwrap();
    tx.flush().await.unwrap();
    let caller = parent.with_user(2);
    let error = caller
        .call::<_, ()>(
            &mut tx,
            "_fields_sync",
            (vec![
                RawVals::new()
                    .set_cell("street", Cell::Str("Desired".into()))
                    .set_cell("city", Cell::Str("Desired city".into())),
            ],),
        )
        .await
        .unwrap_err();
    let expected = format!(
        "You are not allowed to modify 'Contact' (res.partner) records.\n\nRecords: res.partner({},), User: 2",
        denied.ids()[0]
    );
    assert_eq!(error.kind(), erp_sdk::ErrorKind::Access);
    assert_eq!(error.message(), expected);
    assert_eq!(
        first.value(&mut tx, "street").await.unwrap(),
        Value::str("Desired"),
        "fixture must reach a prior changed group"
    );
    let commit_error = tx.commit().await.unwrap_err();
    assert_eq!(commit_error.kind(), erp_sdk::ErrorKind::Access);
    assert_eq!(commit_error.message(), expected);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn empty_mutations_do_not_start_address_synchronization() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let records = tx
        .env()
        .records("res.partner")
        .unwrap()
        .with_context([("invalid_address_hook", Value::str("absent_address_field"))]);
    let before = tx.change_count();
    let result = records.create(&mut tx, vec![]).await.unwrap();
    assert!(result.is_empty());
    records.write(&mut tx, RawVals::new()).await.unwrap();
    assert_eq!(tx.change_count(), before);
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn synchronization_charges_real_reentrant_assignments_at_the_shared_limit() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let parent = create(&mut tx, "Stress root", None).await;
    parent
        .write(
            &mut tx,
            RawVals::new()
                .set_cell("street", Cell::Str("Initial".into()))
                .set_cell("city", Cell::Str("Initial".into())),
        )
        .await
        .unwrap();
    let model = tx.env().records("res.partner").unwrap();
    let children = model
        .create(
            &mut tx,
            (0..512)
                .map(|index| {
                    RawVals::new()
                        .set_cell("name", Cell::Str(format!("Child {index}").into()))
                        .set_cell("parent_id", Cell::Id(parent.ids()[0]))
                })
                .collect(),
        )
        .await
        .unwrap();
    // 64 real rounds * 512 direct children * 2 fields = the accepted65536 assignments.
    parent
        .with_context([("address_repeat_writes", Value::Int(64))])
        .write(
            &mut tx,
            RawVals::new()
                .set_cell("street", Cell::Str("Start".into()))
                .set_cell("city", Cell::Str("Start".into())),
        )
        .await
        .unwrap();
    assert_eq!(
        children.values(&mut tx, "street").await.unwrap(),
        vec![Value::str("Stress 63"); 512]
    );
    assert_eq!(
        children.values(&mut tx, "city").await.unwrap(),
        vec![Value::str("Stress 63"); 512]
    );
    let error = parent
        .with_context([("address_repeat_writes", Value::Int(65))])
        .write(
            &mut tx,
            RawVals::new()
                .set_cell("street", Cell::Str("Again".into()))
                .set_cell("city", Cell::Str("Again".into())),
        )
        .await
        .unwrap_err();
    assert_eq!(error.kind(), erp_sdk::ErrorKind::Validation);
    assert_eq!(
        error.message(),
        "Partner address synchronization maximum field assignments of 65536 exceeded."
    );
    let commit_error = tx.commit().await.unwrap_err();
    assert_eq!(commit_error.kind(), error.kind());
    assert_eq!(commit_error.message(), error.message());
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn inherited_parent_address_bypasses_the_receivers_write_override() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let parent = create(&mut tx, "Parent", None).await;
    parent
        .write(
            &mut tx,
            RawVals::new().set_cell("street", Cell::Str("Parent address".into())),
        )
        .await
        .unwrap();
    let model = tx.env().records("res.partner").unwrap();
    let child = model
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("name", Cell::Str("Invoice child".into()))
                    .set_cell("type", Cell::Str("invoice".into()))
                    .set_cell("parent_id", Cell::Id(parent.ids()[0]))
                    .set_cell("street", Cell::Str("Own address".into())),
            ],
        )
        .await
        .unwrap();
    let grandchild = create(&mut tx, "Grandchild", Some(child.ids()[0])).await;
    child
        .with_context([("address_guard_target", Value::Int(child.ids()[0]))])
        .write(
            &mut tx,
            RawVals::new().set_cell("type", Cell::Str("contact".into())),
        )
        .await
        .unwrap();
    assert_eq!(
        child.value(&mut tx, "street").await.unwrap(),
        Value::str("Parent address")
    );
    assert_eq!(
        grandchild.value(&mut tx, "street").await.unwrap(),
        Value::str("Own address")
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn native_sync_hook_rejects_arguments_with_the_wrong_signature() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let record = create(&mut tx, "Signature", None).await;
    let implementations = h
        .orm()
        .dispatch()
        .implementations(record.model_id(), "_fields_sync");
    let first = implementations.first().unwrap();
    let expected = format!(
        "method '_fields_sync' called with signature {} but {}:{} (addon '{}') implements {}",
        std::any::type_name::<erp_orm::MethodFn<(Vec<String>,), ()>>(),
        first.file,
        first.line,
        first.addon,
        (first.sig_name)()
    );
    let error = erp_orm::call_method::<_, ()>(
        &mut tx,
        &record,
        "_fields_sync",
        (vec!["invalid change map".to_owned()],),
    )
    .await
    .unwrap_err();
    assert_eq!(error.kind(), erp_sdk::ErrorKind::Programming);
    assert_eq!(error.message(), expected);
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn addon_edit_after_upstream_super_is_not_suppressed_by_the_active_driver() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let parent = create(&mut tx, "Addon root", None).await;
    parent
        .write(
            &mut tx,
            RawVals::new()
                .set_cell("street", Cell::Str("Initial street".into()))
                .set_cell("city", Cell::Str("Initial city".into())),
        )
        .await
        .unwrap();
    let child = create(&mut tx, "Source child", Some(parent.ids()[0])).await;
    let sibling = create(&mut tx, "Sibling", Some(parent.ids()[0])).await;
    let grandchild = create(&mut tx, "Grandchild", Some(child.ids()[0])).await;
    child
        .with_context([("address_after_super_target", Value::Int(parent.ids()[0]))])
        .write(
            &mut tx,
            RawVals::new().set_cell("street", Cell::Str("Edited street".into())),
        )
        .await
        .unwrap();
    for record in [&parent, &child, &sibling] {
        assert_eq!(
            record.value(&mut tx, "street").await.unwrap(),
            Value::str("Edited street")
        );
        assert_eq!(
            record.value(&mut tx, "city").await.unwrap(),
            Value::str("Addon city")
        );
    }
    assert_eq!(
        grandchild.value(&mut tx, "street").await.unwrap(),
        Value::str("Edited street")
    );
    assert_eq!(
        grandchild.value(&mut tx, "city").await.unwrap(),
        Value::str("Initial city"),
        "the parent's inherited city copy does not recurse into grandchildren"
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn independent_roots_accept_exact_sixteen_field_input_without_copy_assignments() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let partners = tx.env().records("res.partner").unwrap();
    // No installed language exists yet, so the roots' stored language is empty.
    // Activating English afterward makes all sixteen submitted values real changes.
    let roots = partners
        .create(
            &mut tx,
            (0..4096)
                .map(|index| {
                    RawVals::new().set_cell("name", Cell::Str(format!("Wide root {index}").into()))
                })
                .collect(),
        )
        .await
        .unwrap();
    assert_eq!(roots.ids().len(), 4096);
    assert!(
        roots
            .related(&mut tx, "parent_id")
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        roots
            .related(&mut tx, "child_ids")
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        roots.cells(&mut tx, "lang").await.unwrap(),
        vec![Cell::Null; 4096],
        "compute the initial language before installing English"
    );
    let languages = tx.env().records("res.lang").unwrap();
    languages
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("name", Cell::Str("English".into()))
                    .set_cell("code", Cell::Str("en_US".into()))
                    .set_cell("url_code", Cell::Str("en".into()))
                    .set_cell("active", Cell::Bool(true)),
            ],
        )
        .await
        .unwrap();
    let industries = tx.env().records("res.partner.industry").unwrap();
    let industry = industries
        .create(
            &mut tx,
            vec![RawVals::new().set_cell("name", Cell::Str("Wide industry".into()))],
        )
        .await
        .unwrap();
    let expected = [
        ("street", Cell::Str("Changed street".into())),
        ("street2", Cell::Str("Changed suite".into())),
        ("zip", Cell::Str("12345".into())),
        ("city", Cell::Str("Changed city".into())),
        ("name", Cell::Str("Changed name".into())),
        ("ref", Cell::Str("Changed reference".into())),
        ("vat", Cell::Str("Changed VAT".into())),
        ("website", Cell::Str("https://example.org".into())),
        ("active", Cell::Bool(false)),
        ("type", Cell::Str("other".into())),
        ("email", Cell::Str("wide@example.org".into())),
        ("phone", Cell::Str("123".into())),
        ("color", Cell::Int(1)),
        ("tz", Cell::Str("UTC".into())),
        ("lang", Cell::Str("en_US".into())),
        ("industry_id", Cell::Id(industry.ids()[0])),
    ];
    let mut values = RawVals::new();
    for (field, value) in &expected {
        let before = roots.cells(&mut tx, field).await.unwrap();
        assert_eq!(before.len(), 4096);
        assert!(
            before.iter().all(|old| old != value),
            "{field} must change on every root"
        );
        values.insert((*field).into(), value.clone());
    }
    // No parents or children means zero scheduled synchronization copies, even
    // though admitted input is 4096 records * 16 changed fields = 65536 cells.
    roots
        .with_context([("wide_address_hook", Value::Bool(true))])
        .write(&mut tx, values)
        .await
        .unwrap();
    for (field, value) in &expected {
        assert_eq!(
            roots.cells(&mut tx, field).await.unwrap(),
            vec![value.clone(); 4096],
            "returned {field}"
        );
    }
    tx.flush().await.unwrap();
    tx.invalidate_all().await.unwrap();
    for (field, value) in &expected {
        assert_eq!(
            roots.cells(&mut tx, field).await.unwrap(),
            vec![value.clone(); 4096],
            "stored {field}"
        );
    }
    drop(tx);
    h.cleanup().await.unwrap();
}

// Characterization of the current iterative driver, pending designated lead
// disposition. This is not a parity assertion or acceptance of its timing.
#[tokio::test]
async fn upstream_after_super_observes_own_mutation_before_queued_ancestor_propagation() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let root = create(&mut tx, "Observed root", None).await;
    root.write(
        &mut tx,
        RawVals::new().set_cell("street", Cell::Str("Before".into())),
    )
    .await
    .unwrap();
    let parent = create(&mut tx, "Observed parent", Some(root.ids()[0])).await;
    let child = create(&mut tx, "Observed child", Some(parent.ids()[0])).await;
    child
        .with_context([(
            "address_observe_after_super_target",
            Value::Int(parent.ids()[0]),
        )])
        .write(
            &mut tx,
            RawVals::new().set_cell("street", Cell::Str("After".into())),
        )
        .await
        .unwrap();
    assert_eq!(
        tx.extension::<AfterSuperObservations>().0,
        vec![(parent.ids()[0], Value::str("After"), Value::str("Before"))],
        "the parent override returns from super while root propagation is still queued"
    );
    let ids = [root.ids()[0], parent.ids()[0], child.ids()[0]];
    for record in [&root, &parent, &child] {
        assert_eq!(
            record.value(&mut tx, "street").await.unwrap(),
            Value::str("After")
        );
    }
    tx.commit().await.unwrap();
    drop(tx);
    let mut verify = h.begin().await.unwrap();
    let stored = verify
        .env()
        .records("res.partner")
        .unwrap()
        .browse(ids)
        .unwrap();
    assert_eq!(
        stored.values(&mut verify, "street").await.unwrap(),
        vec![Value::str("After"); 3]
    );
    drop(verify);
    h.cleanup().await.unwrap();
}
