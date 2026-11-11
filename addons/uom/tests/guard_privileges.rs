//! Native and raw-SQL guard boundaries. Runtime-role grants are tested in erp-cli.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "integration assertions"
)]
use erp_sdk::ErrorKind;
use erp_sdk::prelude::*;
use erp_testkit::Harness;
#[path = "support/guard_fixture.rs"]
mod guard_fixture;

async fn fixture() -> (Harness, i64, i64) {
    let h = Harness::builder([&erp_addon_base::ADDON, &erp_addon_uom::ADDON])
        .build()
        .await
        .unwrap();
    guard_fixture::install(&h).await;
    let mut tx = h.begin().await.unwrap();
    let units = tx.env().records("uom.uom").unwrap();
    let root = units
        .create(
            &mut tx,
            vec![RawVals::new().set_cell("name", Cell::Str("Root".into()))],
        )
        .await
        .unwrap()
        .ids()[0];
    let child = units
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("name", Cell::Str("Child".into()))
                    .set_cell("relative_uom_id", Cell::Id(root))
                    .set_cell("relative_factor", Cell::Decimal(Decimal::from(6))),
            ],
        )
        .await
        .unwrap()
        .ids()[0];
    tx.commit().await.unwrap();
    (h, root, child)
}

#[tokio::test]
async fn invalid_inputs_fail_before_sql_and_empty_input_is_a_noop() {
    let (h, _, child) = fixture().await;
    let mut tx = h.begin().await.unwrap();
    let env = tx.env();
    let before = tx.query_count();
    assert!(
        erp_addon_uom::references::register(&env, &mut tx, &[])
            .await
            .unwrap()
            .is_empty()
    );
    for ids in [vec![0], vec![-1], vec![child; 4097]] {
        let error = erp_addon_uom::references::register(&env, &mut tx, &ids)
            .await
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Validation);
        assert_eq!(
            error.message(),
            "Unit reference guard input exceeds its limits or has invalid identities."
        );
    }
    assert_eq!(tx.query_count(), before);
    let (other, _, _) = fixture().await;
    let mut foreign = other.begin().await.unwrap();
    let error = erp_addon_uom::references::register(&foreign.env(), &mut tx, &[child])
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Programming);
    assert_eq!(
        error.message(),
        "Unit reference environment and transaction belong to different ORM lineages."
    );
    assert_eq!(tx.query_count(), before);
    foreign.rollback().await.unwrap();
    drop(foreign);
    other.cleanup().await.unwrap();
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn sql_registration_refuses_null_input() {
    let (h, _, _) = fixture().await;
    let client = h.db().client().await.unwrap();
    let error = client
        .query("SELECT * FROM public.erp_uom_register(NULL::bigint[])", &[])
        .await
        .unwrap_err();
    assert_eq!(error.code().unwrap().code(), "UOM04");
    assert_eq!(
        error.as_db_error().unwrap().message(),
        "Unit reference guard input exceeds its limits or has invalid identities."
    );
    drop(client);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn raw_sql_unrepresentable_factor_cannot_be_registered() {
    let (h, _, child) = fixture().await;
    let client = h.db().client().await.unwrap();
    // Owner injection emulates a pre-guard corrupt unit; not an allowed runtime mutation.
    client
        .batch_execute("ALTER TABLE uom_uom DISABLE TRIGGER erp_uom_after_update")
        .await
        .unwrap();
    client
        .execute(
            "UPDATE uom_uom SET relative_factor=1e40, factor=1e40 WHERE id=$1",
            &[&child],
        )
        .await
        .unwrap();
    client
        .batch_execute("ALTER TABLE uom_uom ENABLE TRIGGER erp_uom_after_update")
        .await
        .unwrap();
    let error = client
        .query(
            "SELECT * FROM public.erp_uom_register($1::bigint[])",
            &[&vec![child]],
        )
        .await
        .unwrap_err();
    assert_eq!(error.code().unwrap().code(), "UOM07");
    assert_eq!(
        error.as_db_error().unwrap().message(),
        "Unit reference factor is not representable exactly."
    );
    assert_eq!(
        client
            .query_one("SELECT count(*) FROM uom_reference_use", &[])
            .await
            .unwrap()
            .get::<_, i64>(0),
        0
    );
    drop(client);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn raw_sql_and_elevated_crud_refuse_identity_and_history_forgery() {
    let (h, root, child) = fixture().await;
    let client = h.db().client().await.unwrap();
    client
        .query(
            "SELECT * FROM public.erp_uom_register($1::bigint[])",
            &[&vec![child]],
        )
        .await
        .unwrap();
    for (sql, code, message) in [
        (
            "UPDATE uom_uom SET relative_factor=7 WHERE id=$1",
            "UOM01",
            "A referenced unit of measure would change. Create a new unit instead.",
        ),
        (
            "UPDATE uom_uom SET parent_path='forged/' WHERE id=$1",
            "UOM02",
            "Unit of measure hierarchy is invalid.",
        ),
        (
            "UPDATE uom_reference_use SET factor=7 WHERE uom_id=$1",
            "UOM05",
            "Unit reference guard records cannot be modified through ordinary CRUD.",
        ),
    ] {
        let error = client.execute(sql, &[&child]).await.unwrap_err();
        assert_eq!(error.code().unwrap().code(), code);
        assert_eq!(error.as_db_error().unwrap().message(), message);
    }
    enum Attempt {
        Create,
        Write,
        Unlink,
    }
    for model in ["uom.reference.use", "uom.reference.fence"] {
        for operation in [Attempt::Create, Attempt::Write, Attempt::Unlink] {
            let mut tx = h.begin().await.unwrap();
            let records = tx.env().records(model).unwrap().browse([1]).unwrap();
            let error = match operation {
                Attempt::Create => records
                    .create(&mut tx, vec![RawVals::new()])
                    .await
                    .unwrap_err(),
                Attempt::Write => records.write(&mut tx, RawVals::new()).await.unwrap_err(),
                Attempt::Unlink => records.unlink(&mut tx).await.unwrap_err(),
            };
            assert_eq!(error.kind(), ErrorKind::Access);
            assert_eq!(
                error.message(),
                "Unit reference guard records cannot be modified through ordinary CRUD."
            );
            tx.rollback().await.unwrap();
        }
    }
    let error = client
        .execute(
            "UPDATE uom_uom SET relative_uom_id=id WHERE id=$1",
            &[&root],
        )
        .await
        .unwrap_err();
    assert_eq!(error.code().unwrap().code(), "UOM02");
    assert_eq!(
        error.as_db_error().unwrap().message(),
        "Unit of measure hierarchy is invalid."
    );
    let constraint: String = client.query_one("SELECT c.conname::text FROM pg_constraint c JOIN pg_attribute a ON a.attrelid=c.conrelid AND a.attnum=ANY(c.conkey) WHERE c.conrelid='uom_reference_use'::regclass AND c.contype='f' AND a.attname='uom_id'", &[]).await.unwrap().get(0);
    let error = client
        .execute("DELETE FROM uom_uom WHERE id=$1", &[&child])
        .await
        .unwrap_err();
    assert_eq!(error.code().unwrap().code(), "23001");
    assert_eq!(
        error.as_db_error().unwrap().message(),
        format!(
            "update or delete on table \"uom_uom\" violates RESTRICT setting of foreign key constraint \"{constraint}\" on table \"uom_reference_use\""
        )
    );
    drop(client);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn depth_and_graph_overflow_never_truncate_to_success() {
    let (h, root, _) = fixture().await;
    let client = h.db().client().await.unwrap();
    // Owner corruption/large-graph fixtures bypass only the AFTER integrity check.
    client.batch_execute("ALTER TABLE uom_uom DISABLE TRIGGER erp_uom_after_insert; INSERT INTO uom_uom(id,name,relative_factor,factor,active,relative_uom_id) SELECT 10000+n,jsonb_build_object('en_US','Depth fixture'),1,1,true,CASE WHEN n=1 THEN NULL ELSE 9999+n END FROM generate_series(1,257) n; ALTER TABLE uom_uom ENABLE TRIGGER erp_uom_after_insert").await.unwrap();
    let row = client
        .query_one(
            "SELECT uom_id FROM erp_uom_register(ARRAY[10256]::bigint[])",
            &[],
        )
        .await
        .unwrap();
    assert_eq!(row.get::<_, i64>(0), 10256);
    let error = client
        .query(
            "SELECT * FROM erp_uom_register(ARRAY[10257]::bigint[])",
            &[],
        )
        .await
        .unwrap_err();
    assert_eq!(error.code().unwrap().code(), "UOM04");
    assert_eq!(
        error.as_db_error().unwrap().message(),
        "Unit of measure hierarchy exceeds 256 levels."
    );
    client
        .batch_execute("ALTER TABLE uom_uom DISABLE TRIGGER erp_uom_after_insert")
        .await
        .unwrap();
    client.execute("INSERT INTO uom_uom(id,name,relative_factor,factor,active,relative_uom_id) SELECT 20000+n,jsonb_build_object('en_US','Graph fixture'),1,1,true,$1 FROM generate_series(1,4096) n",&[&root]).await.unwrap();
    client
        .batch_execute("ALTER TABLE uom_uom ENABLE TRIGGER erp_uom_after_insert")
        .await
        .unwrap();
    let error = client
        .query(
            "SELECT * FROM erp_uom_register(ARRAY(SELECT generate_series(20001,24096)::bigint))",
            &[],
        )
        .await
        .unwrap_err();
    assert_eq!(error.code().unwrap().code(), "UOM04");
    assert_eq!(
        error.as_db_error().unwrap().message(),
        "Unit reference guard graph exceeds 4096 identities."
    );
    assert_eq!(client.query("SELECT * FROM erp_uom_register(ARRAY(SELECT generate_series(20001,24095)::bigint))",&[]).await.unwrap().len(),4095);
    let mut tx = h.begin().await.unwrap();
    let error = tx
        .env()
        .records("uom.uom")
        .unwrap()
        .browse([root])
        .unwrap()
        .write(
            &mut tx,
            RawVals::new().set_cell("relative_factor", Cell::Decimal(Decimal::ONE)),
        )
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Validation);
    assert_eq!(
        error.message(),
        "Unit reference guard graph exceeds 4096 identities."
    );
    tx.rollback().await.unwrap();
    drop(client);
    h.cleanup().await.unwrap();
}
