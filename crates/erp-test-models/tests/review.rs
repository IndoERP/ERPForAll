//! Cases from the S1 review round (orchestrator decisions–D6 and review items).
#![allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]

use support::currency_fixture;

mod support;

use erp_registry::{AddonDescriptor, FieldDecl, ModelDecl, RegistryBuilder};
use erp_sdk::{ErrorKind, Orm, SUPERUSER_ID, StaticPolicy, Vals, Value};
use erp_test_models::test_orm::{Mixed, NoName, Partner, Schema, Selection};
use erp_testkit::Harness;
use support::{one, rows};

/// writing to a record deleted behind the ORM's back is `Missing` at flush.
#[tokio::test]
async fn writing_a_deleted_record_is_missing() {
    let h = support::orm().await;
    let mut tx = h.begin().await.unwrap();
    let partners = tx.env().model::<Partner>().unwrap();
    let recs = partners
        .create(&mut tx, vec![Vals::new(), Vals::new()])
        .await
        .unwrap();
    let gone = recs.ids()[1];
    tx.execute(
        &format!("DELETE FROM test_orm_partner WHERE id = {gone}"),
        &[],
    )
    .await
    .unwrap();
    recs.set(&mut tx, Partner::NAME, "x").await.unwrap();
    let err = tx.flush().await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Missing);
    assert_eq!(
        err.message(),
        format!(
            "Record does not exist or has been deleted.\n(Record: test_orm.partner({gone},), User: 1)"
        )
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

/// `Char(size)` is validated by the converter, typed and dynamic alike.
#[tokio::test]
async fn char_size_is_validated_before_the_database() {
    let h = support::orm().await;
    let mut tx = h.begin().await.unwrap();
    let schema = tx.env().model::<Schema>().unwrap();
    let rec = schema
        .create(
            &mut tx,
            vec![
                schema
                    .vals()
                    .set(Schema::REQUIRED, "r")
                    .set(Schema::SIZE, "abc"),
            ],
        )
        .await
        .unwrap();
    let err = rec.set(&mut tx, Schema::SIZE, "abcd").await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Validation);
    assert!(
        err.message().contains("test_orm.schema.size") && err.message().contains("at most 3"),
        "{err}"
    );
    let err = rec
        .set_value(&mut tx, "size", Value::str("abcd"))
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Validation);
    // nothing reached the database, but a validation error poisons the transaction until it is
    // rolled back (S4a review decision E1)
    assert_eq!(tx.commit().await.unwrap_err().kind(), ErrorKind::Validation);
    tx.rollback().await.unwrap();
    tx.commit().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

/// (S1's refusal) is replaced in S4a: non-stored computed fields are computed on read
/// and recomputed when their dependencies change.
#[tokio::test]
async fn non_stored_computed_fields_are_computed_on_read() {
    let h = support::orm().await;
    let mut tx = h.begin().await.unwrap();
    let model = tx.env().model::<NoName>().unwrap();
    let rec = model
        .create(&mut tx, vec![model.vals().set(NoName::CODE, "abc")])
        .await
        .unwrap();
    assert_eq!(
        rec.get(&mut tx, NoName::UPPER).await.unwrap().as_deref(),
        Some("ABC")
    );
    rec.set(&mut tx, NoName::CODE, "xyz").await.unwrap();
    let row = rec.read(&mut tx, &["upper"]).await.unwrap();
    assert_eq!(row[0]["upper"], Value::str("XYZ"));
    drop(tx);
    h.cleanup().await.unwrap();
}

/// Typed writes are validated like dynamic ones.
#[tokio::test]
async fn typed_writes_are_validated() {
    let h = support::orm().await;
    let mut tx = h.begin().await.unwrap();
    let selection = tx.env().model::<Selection>().unwrap();
    let err = selection
        .create(
            &mut tx,
            vec![selection.vals().set(Selection::STATE, "zz_ZZ")],
        )
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Validation);
    assert_eq!(
        err.message(),
        "Wrong value for test_orm.selection.state: 'zz_ZZ'"
    );
    let mixed = tx.env().model::<Mixed>().unwrap();
    let amount: erp_sdk::Decimal = "2.675".parse().unwrap();
    let rec = mixed
        .create(&mut tx, vec![mixed.vals().set(Mixed::NUMBER, amount)])
        .await
        .unwrap();
    tx.invalidate_all().await.unwrap();
    let expected: erp_sdk::Decimal = "2.68".parse().unwrap();
    assert_eq!(rec.get(&mut tx, Mixed::NUMBER).await.unwrap(), expected);
    let err = rec.set(&mut tx, Mixed::FLOAT, f64::NAN).await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Validation);
    drop(tx);
    h.cleanup().await.unwrap();
}

/// every many2one column is indexed, except the audit fields.
#[tokio::test]
async fn many2one_columns_are_indexed() {
    let h = support::orm().await;
    let mut tx = h.begin().await.unwrap();
    let indexes = rows(
        &mut tx,
        "SELECT indexname::text FROM pg_indexes WHERE tablename = 'test_orm_mixed' ORDER BY 1",
    )
    .await;
    let names: Vec<_> = indexes.into_iter().filter_map(|r| r[0].clone()).collect();
    assert!(
        names.contains(&"test_orm_mixed__currency_id_index".to_owned()),
        "{names:?}"
    );
    assert!(
        !names
            .iter()
            .any(|n| n.contains("create_uid") || n.contains("write_uid")),
        "{names:?}"
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

static FKS: AddonDescriptor = AddonDescriptor {
    name: "test_fks",
    depends: &["base"],
    exact_only: false,
};

/// Foreign keys follow `ondelete` changes (compared with the declaration, not skipped by column),
/// and are added `NOT VALID` then validated on tables that already existed.
#[tokio::test]
async fn foreign_keys_follow_ondelete_changes() {
    let v1 = ModelDecl::new("test_fks", "x.fk")
        .fields(const { &[FieldDecl::many2one("user_id", "res.users")] });
    let h = Harness::builder([&FKS])
        .with_decls([v1])
        .build()
        .await
        .unwrap();
    let def = "SELECT pg_get_constraintdef(k.oid) FROM pg_constraint k \
               JOIN pg_attribute a ON a.attrelid = k.conrelid AND a.attnum = k.conkey[1] \
               WHERE k.conrelid = 'x_fk'::regclass AND k.contype = 'f' AND a.attname = 'user_id'";
    let mut tx = h.begin().await.unwrap();
    assert!(
        one(&mut tx, def)
            .await
            .unwrap()
            .ends_with("ON DELETE SET NULL")
    );
    tx.commit().await.unwrap();
    let v2: &'static ModelDecl =
        Box::leak(Box::new(ModelDecl::new("test_fks", "x.fk").fields(
            const {
                &[
                    FieldDecl::many2one("user_id", "res.users")
                        .ondelete(erp_sdk::OnDelete::Cascade),
                ]
            },
        )));
    let registry = std::sync::Arc::new(
        RegistryBuilder::new([&FKS])
            .with_decls([v2])
            .build()
            .unwrap(),
    );
    let orm = Orm::builder(registry, h.db().pool().clone())
        .policy(StaticPolicy::AllowAll)
        .build()
        .unwrap();
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    tx.capture_statements(true);
    tx.init_models(&[]).await.unwrap();
    let ddl = tx.captured().join("\n");
    assert!(
        ddl.contains("NOT VALID") && ddl.contains("VALIDATE CONSTRAINT"),
        "{ddl}"
    );
    assert!(
        one(&mut tx, def)
            .await
            .unwrap()
            .ends_with("ON DELETE CASCADE")
    );
    tx.commit().await.unwrap();
    drop(tx);
    drop(orm);
    h.cleanup().await.unwrap();
}

mod dispatch_errors {
    //! misdeclared methods fail the dispatch build with file:line.
    use std::any::Any;

    use erp_sdk::__private::{
        BoxFuture, ImplKind, MethodFn, MethodImpl, Records, Super, Transaction,
    };
    use erp_sdk::{Orm, StaticPolicy};

    type Ping = MethodFn<(), i64>;

    fn ping<'a>(
        _tx: &'a mut Transaction,
        _records: &'a Records,
        _args: (),
        _sup: Super<(), i64>,
    ) -> BoxFuture<'a, erp_sdk::Result<i64>> {
        Box::pin(async { Ok(1) })
    }

    fn entry() -> &'static (dyn Any + Send + Sync) {
        static F: Ping = ping;
        &F
    }

    /// An implementation in an addon that has no declaration of the model.
    static ORPHAN: MethodImpl = MethodImpl {
        addon: "test_base_ext",
        model: "test_orm.partner",
        method: "ping",
        kind: ImplKind::Define,
        sig_name: std::any::type_name::<Ping>,
        entry,
        file: file!(),
        line: line!(),
    };

    /// A second plain definition of `hello` (test_base_ext defines it on `base`).
    static TWICE: MethodImpl = MethodImpl {
        addon: "test_orm",
        model: "test_orm.partner",
        method: "hello",
        kind: ImplKind::Define,
        sig_name: std::any::type_name::<Ping>,
        entry,
        file: file!(),
        line: line!(),
    };

    #[tokio::test]
    async fn misdeclared_methods_fail_the_build() {
        let h = erp_testkit::Harness::builder([
            &erp_test_models::test_orm::ADDON,
            &erp_test_models::test_base_ext::ADDON,
        ])
        .with_static_decls(crate::currency_fixture::decls())
        .without_schema()
        .build()
        .await
        .unwrap();
        let build = |imp: &'static MethodImpl| {
            Orm::builder(h.registry().clone(), h.db().pool().clone())
                .policy(StaticPolicy::AllowAll)
                .impls([imp])
                .build()
                .err()
                .map(|e| (e.kind(), e.message().to_owned()))
        };
        let (kind, message) = build(&ORPHAN).unwrap();
        assert_eq!(kind, erp_sdk::ErrorKind::Programming);
        assert!(
            message.contains(
                "has no declaration of model 'test_orm.partner' in addon 'test_base_ext'"
            ),
            "{message}"
        );
        assert!(message.contains("review.rs:"), "{message}");
        let (_, message) = build(&TWICE).unwrap();
        assert!(message.contains("must be #[method(override)]"), "{message}");
        h.cleanup().await.unwrap();
    }
}
