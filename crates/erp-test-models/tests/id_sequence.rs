//! `id_sequence`, one named id sequence shared by several models. Registry checks,
//! schema sync (the sequence created once, owned by nobody, used as the `id` default; an existing
//! table adopted or refused) and `create` drawing from it.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

mod support;

use std::sync::Arc;

use erp_registry::{AddonDescriptor, FieldDecl, ModelDecl, ModelKind, RegistryBuilder};
use erp_sdk::{Cell, ErrorKind, Orm, RawVals, SUPERUSER_ID, StaticPolicy};
use erp_testkit::Harness;
use support::rows;

static SEQ: AddonDescriptor = AddonDescriptor {
    name: "test_seq",
    depends: &["base"],
    exact_only: false,
};

static SEQ_EXT: AddonDescriptor = AddonDescriptor {
    name: "test_seq_ext",
    depends: &["test_seq"],
    exact_only: false,
};

const N: &[FieldDecl] = &[FieldDecl::integer("n")];

fn decl(name: &'static str, seq: Option<&'static str>) -> ModelDecl {
    let decl = ModelDecl::new("test_seq", name).fields(N);
    match seq {
        Some(seq) => decl.id_sequence(seq),
        None => decl,
    }
}

fn model(name: &'static str, seq: Option<&'static str>) -> &'static ModelDecl {
    Box::leak(Box::new(decl(name, seq)))
}

fn build(
    addons: &[&'static AddonDescriptor],
    decls: Vec<&'static ModelDecl>,
) -> erp_sdk::Result<erp_registry::Registry> {
    RegistryBuilder::new(addons.iter().copied())
        .with_decls(decls)
        .build()
}

fn refused(addons: &[&'static AddonDescriptor], decls: Vec<&'static ModelDecl>) -> String {
    let err = build(addons, decls).unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Programming, "{err}");
    err.message().to_owned()
}

/// §2: the value comes from the model's own declarations (extensions may repeat
/// it, not change it), is not carried by `_inherit`, and every name check refuses with its
/// reason.
#[test]
fn registry_resolves_and_checks_the_sequence_name() {
    let registry = build(
        &[&SEQ],
        vec![
            model("x.a", Some("x_shared_seq")),
            model("x.b", Some("x_shared_seq")),
            model("x.c", None),
        ],
    )
    .unwrap();
    assert_eq!(
        registry.model("x.a").unwrap().id_sequence,
        Some("x_shared_seq")
    );
    assert_eq!(
        registry.model("x.b").unwrap().id_sequence,
        Some("x_shared_seq")
    );
    assert_eq!(registry.model("x.c").unwrap().id_sequence, None);

    // an extension repeating the value is fine, a different one is refused
    let same: &'static ModelDecl = Box::leak(Box::new(
        ModelDecl::extension("test_seq_ext", "x.a", &["x.a"]).id_sequence("x_shared_seq"),
    ));
    let registry = build(
        &[&SEQ, &SEQ_EXT],
        vec![model("x.a", Some("x_shared_seq")), same],
    )
    .unwrap();
    assert_eq!(
        registry.model("x.a").unwrap().id_sequence,
        Some("x_shared_seq")
    );
    let other: &'static ModelDecl = Box::leak(Box::new(
        ModelDecl::extension("test_seq_ext", "x.a", &["x.a"]).id_sequence("x_other_seq"),
    ));
    assert_eq!(
        refused(
            &[&SEQ, &SEQ_EXT],
            vec![model("x.a", Some("x_shared_seq")), other]
        ),
        "model definition 'x.a' of addon 'test_seq' sets id_sequence 'x_shared_seq' on model \
         'x.a', but model definition 'x.a' of addon 'test_seq_ext' sets 'x_other_seq': the \
         declarations of a model must name the same id sequence"
    );

    // `_inherit` from another model does not carry it
    let mixin: &'static ModelDecl = Box::leak(Box::new(
        ModelDecl::new("test_seq", "x.mixin")
            .kind(ModelKind::Abstract)
            .fields(N),
    ));
    let child: &'static ModelDecl = Box::leak(Box::new(
        ModelDecl::new("test_seq", "x.child").inherit(&["x.mixin"]),
    ));
    let registry = build(&[&SEQ], vec![mixin, child]).unwrap();
    assert_eq!(registry.model("x.child").unwrap().id_sequence, None);
    let abstract_seq: &'static ModelDecl = Box::leak(Box::new(
        ModelDecl::new("test_seq", "x.mixin")
            .kind(ModelKind::Abstract)
            .id_sequence("x_shared_seq"),
    ));
    assert_eq!(
        refused(&[&SEQ], vec![abstract_seq]),
        "model definition 'x.mixin' of addon 'test_seq' sets id_sequence 'x_shared_seq' on the \
         abstract model 'x.mixin': an abstract model has no table; set it on each model with a \
         table"
    );

    // names
    for bad in ["1seq", "bad-name", ""] {
        let leaked: &'static str = Box::leak(bad.to_owned().into_boxed_str());
        assert_eq!(
            refused(&[&SEQ], vec![model("x.a", Some(leaked))]),
            format!(
                "model definition 'x.a' of addon 'test_seq' sets id_sequence '{bad}' on model \
                 'x.a': a sequence name is a PostgreSQL identifier of letters, digits, '_' and \
                 '$' (not starting with a digit), at most 63 bytes"
            )
        );
    }
    let long: &'static str = Box::leak("s".repeat(64).into_boxed_str());
    assert!(
        refused(&[&SEQ], vec![model("x.a", Some(long))]).contains("at most 63 bytes"),
        "64 bytes"
    );
    let fits: &'static str = Box::leak("s".repeat(63).into_boxed_str());
    build(&[&SEQ], vec![model("x.a", Some(fits))]).unwrap();

    // a model table, a table's own identity sequence, a many2many relation table
    assert_eq!(
        refused(&[&SEQ], vec![model("x.a", Some("x_c")), model("x.c", None)]),
        "model 'x.a' sets id_sequence 'x_c', which is the table of model 'x.c': a shared id \
         sequence needs a name of its own"
    );
    assert_eq!(
        refused(
            &[&SEQ],
            vec![model("x.a", Some("x_c_id_seq")), model("x.c", None)]
        ),
        "model 'x.a' sets id_sequence 'x_c_id_seq', which is the identity sequence of the table \
         of model 'x.c': a shared id sequence needs a name of its own"
    );
    // a model that draws from a shared sequence has no identity sequence to clash with
    build(
        &[&SEQ],
        vec![
            model("x.a", Some("x_c_id_seq")),
            model("x.c", Some("x_c_id_seq")),
        ],
    )
    .unwrap();
    const LINKED: &[FieldDecl] =
        &[FieldDecl::many2many("tag_ids", "x.c").relation("x_link_rel", "a_id", "c_id")];
    let linked: &'static ModelDecl =
        Box::leak(Box::new(ModelDecl::new("test_seq", "x.l").fields(LINKED)));
    assert_eq!(
        refused(
            &[&SEQ],
            vec![model("x.a", Some("x_link_rel")), model("x.c", None), linked]
        ),
        "model 'x.a' sets id_sequence 'x_link_rel', which is a many2many relation table of model \
         'x.l': a shared id sequence needs a name of its own"
    );
}

async fn sync(h: &Harness, decls: Vec<&'static ModelDecl>) -> erp_sdk::Result<String> {
    let registry = Arc::new(build(&[&SEQ], decls)?);
    let orm = Orm::builder(registry, h.db().pool().clone())
        .policy(StaticPolicy::AllowAll)
        .build()?;
    let mut tx = orm.begin(SUPERUSER_ID).await?;
    tx.capture_statements(true);
    tx.init_models(&[]).await?;
    let ddl = tx.captured().join("\n");
    tx.commit().await?;
    Ok(ddl)
}

/// §4: schema sync creates the shared sequence once (owned by no column) and makes
/// it the `id` default of both tables; `create` on either model draws from it, so the ids never
/// collide and follow the creation order; a second sync adopts the tables; an existing identity
/// table is refused, and so is a relation of another kind under the sequence's name.
#[tokio::test]
async fn tables_share_the_sequence() {
    let h = Harness::builder([&SEQ])
        .with_decls([
            decl("x.a", Some("x_shared_seq")),
            decl("x.b", Some("x_shared_seq")),
            decl("x.c", None),
        ])
        .build()
        .await
        .unwrap();
    let mut tx = h.begin().await.unwrap();
    let owned = rows(
        &mut tx,
        "SELECT c.relkind::text, (SELECT count(*) FROM pg_depend d WHERE d.objid = c.oid \
         AND d.deptype IN ('a', 'i'))::text FROM pg_class c WHERE c.relname = 'x_shared_seq'",
    )
    .await;
    assert_eq!(
        owned,
        vec![vec![Some("S".to_owned()), Some("0".to_owned())]]
    );
    let defaults = rows(
        &mut tx,
        "SELECT c.relname::text, pg_get_expr(d.adbin, d.adrelid) FROM pg_attrdef d \
         JOIN pg_class c ON c.oid = d.adrelid JOIN pg_attribute a ON a.attrelid = d.adrelid \
         AND a.attnum = d.adnum WHERE a.attname = 'id' AND c.relname IN ('x_a', 'x_b') \
         ORDER BY 1",
    )
    .await;
    let expr = Some("nextval('x_shared_seq'::regclass)".to_owned());
    assert_eq!(
        defaults,
        vec![
            vec![Some("x_a".to_owned()), expr.clone()],
            vec![Some("x_b".to_owned()), expr]
        ]
    );

    let row = |n: i32| RawVals::new().set_cell("n", Cell::Int(n));
    let env = tx.env();
    let a1 = env
        .records("x.a")
        .unwrap()
        .create(&mut tx, vec![row(1), row(2)])
        .await
        .unwrap()
        .ids();
    let b1 = env
        .records("x.b")
        .unwrap()
        .create(&mut tx, vec![row(3)])
        .await
        .unwrap()
        .ids();
    let a2 = env
        .records("x.a")
        .unwrap()
        .create(&mut tx, vec![row(4)])
        .await
        .unwrap()
        .ids();
    let mut all: Vec<i64> = a1.iter().chain(&b1).chain(&a2).copied().collect();
    assert_eq!(all.len(), 4);
    assert!(all.windows(2).all(|w| w[0] < w[1]), "{all:?}");
    all.dedup();
    assert_eq!(all.len(), 4, "ids are unique across the two tables");
    // a model without the sequence keeps its own identity
    let c = env
        .records("x.c")
        .unwrap()
        .create(&mut tx, vec![row(5)])
        .await
        .unwrap()
        .ids();
    assert_eq!(c, vec![1]);
    tx.commit().await.unwrap();

    // idempotent: the sequence exists, the tables are adopted
    let ddl = sync(
        &h,
        vec![
            model("x.a", Some("x_shared_seq")),
            model("x.b", Some("x_shared_seq")),
            model("x.c", None),
        ],
    )
    .await
    .unwrap();
    assert!(!ddl.contains("SEQUENCE"), "{ddl}");
    assert!(!ddl.contains("CREATE TABLE"), "{ddl}");

    // an identity table cannot be moved onto the sequence by schema sync
    let err = sync(
        &h,
        vec![
            model("x.a", Some("x_shared_seq")),
            model("x.b", Some("x_shared_seq")),
            model("x.c", Some("x_shared_seq")),
        ],
    )
    .await
    .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Programming);
    assert_eq!(
        err.message(),
        "model 'x.c' sets id_sequence 'x_shared_seq', but its existing table \"x_c\" has an \
         identity id (or no id default); converting a table's ids to a shared sequence is not \
         done by schema sync: migrate the ids and the column default by hand"
    );
    // nor onto another sequence
    let err = sync(
        &h,
        vec![
            model("x.a", Some("x_other_seq")),
            model("x.b", Some("x_shared_seq")),
        ],
    )
    .await
    .unwrap_err();
    assert_eq!(
        err.message(),
        "model 'x.a' sets id_sequence 'x_other_seq', but its existing table \"x_a\" has an id \
         drawn from sequence 'x_shared_seq'; converting a table's ids to a shared sequence is \
         not done by schema sync: migrate the ids and the column default by hand"
    );
    // dropping the declaration while the table still draws from the sequence: refused
    let err = sync(
        &h,
        vec![model("x.a", None), model("x.b", Some("x_shared_seq"))],
    )
    .await
    .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Programming);
    assert_eq!(
        err.message(),
        "model 'x.a' sets no id_sequence, but its existing table \"x_a\" draws its id from \
         sequence 'x_shared_seq': declare id_sequence = \"x_shared_seq\" again, or \
         migrate the ids and the column default by hand"
    );
    // a relation of another kind under the sequence's name
    let client = h.db().client().await.unwrap();
    client
        .batch_execute(
            "CREATE VIEW x_view_seq AS SELECT 1 AS one; CREATE INDEX x_idx_seq ON x_c (n)",
        )
        .await
        .unwrap();
    for (seq, kind) in [("x_idx_seq", "i"), ("x_view_seq", "v")] {
        let err = sync(&h, vec![model("x.d", Some(seq))]).await.unwrap_err();
        assert_eq!(err.kind(), ErrorKind::Programming);
        assert_eq!(
            err.message(),
            format!(
                "the shared id sequence '{seq}' is taken by a relation of kind \
                 '{kind}' in the schema; rename that relation or the sequence"
            )
        );
    }
    // review C-S2: an existing sequence is shared only with the parameters schema sync creates
    client
        .batch_execute(
            "CREATE SEQUENCE x_int4_seq AS int4; CREATE SEQUENCE x_step_seq INCREMENT BY 2; \
             CREATE SEQUENCE x_cycle_seq CYCLE; CREATE SEQUENCE x_owned_seq OWNED BY x_c.n; \
             CREATE SEQUENCE x_small_seq MAXVALUE 1000; CREATE SEQUENCE x_neg_seq MINVALUE -5; \
             CREATE SEQUENCE x_fine_seq",
        )
        .await
        .unwrap();
    for (seq, why) in [
        ("x_int4_seq", "its type is integer, not bigint"),
        ("x_step_seq", "its increment is 2, not 1"),
        (
            "x_small_seq",
            "its range is 1 to 1000, not 1 to 9223372036854775807",
        ),
        (
            "x_neg_seq",
            "its range is -5 to 9223372036854775807, not 1 to 9223372036854775807",
        ),
        ("x_cycle_seq", "it cycles (CYCLE), so ids could repeat"),
        (
            "x_owned_seq",
            "it is owned by a column (OWNED BY, or an identity), so dropping that column or \
             table would drop it",
        ),
    ] {
        let err = sync(&h, vec![model("x.d", Some(seq))]).await.unwrap_err();
        assert_eq!(err.kind(), ErrorKind::Programming, "{seq}");
        assert_eq!(
            err.message(),
            format!(
                "the shared id sequence '{seq}' exists, but {why}: a shared id \
                 sequence is bigint, increment 1, range 1 to the bigint maximum, no cycle, owned \
                 by no column; fix or rename the existing sequence"
            )
        );
    }
    // a sequence of the right shape made by hand is adopted, not created again
    let ddl = sync(&h, vec![model("x.d", Some("x_fine_seq"))])
        .await
        .unwrap();
    assert!(!ddl.contains("CREATE SEQUENCE"), "{ddl}");
    assert!(ddl.contains("nextval('\"x_fine_seq\"'::regclass)"), "{ddl}");
    h.cleanup().await.unwrap();
}
