// SPDX-License-Identifier: MIT OR Apache-2.0
//! Addon-owned SQL objects in the registry, slice B1 (contract
//! ): declarations, validation, admission bounds and the
//! resolved dependency graph. Pure: no database, so no DDL can be sent.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

use erp_fields::ErrorKind;
use erp_registry::ident::MAX_IDENTIFIER_LEN;
use erp_registry::{
    AddonDescriptor, FieldDecl, ModelDecl, ModelKind, PartitionDecl, PartitionInterval,
    PartitionRetain, Registry, RegistryBuilder, SqlConditionDecl, SqlDeferral, SqlDep, SqlEvent,
    SqlFunctionDecl, SqlLanguage, SqlLevel, SqlObjectDecl, SqlObjectKind, SqlObjectLimits,
    SqlObjectManifest, SqlReference, SqlResult, SqlScalar, SqlTiming, SqlTransition,
    SqlTriggerDecl, SqlTriggerShape, SqlType,
};
use erp_registry::{SqlBinding, SqlSecurity, SqlTableGrantDecl, SqlTableOp};

static SQA: AddonDescriptor = AddonDescriptor {
    name: "test_sqa",
    depends: &["base"],
    exact_only: false,
};
static SQB: AddonDescriptor = AddonDescriptor {
    name: "test_sqb",
    depends: &["test_sqa"],
    exact_only: false,
};
static SQC: AddonDescriptor = AddonDescriptor {
    name: "test_sqc",
    depends: &["base"],
    exact_only: false,
};
/// Depends on `test_sqb` only: reaches `test_sqa` transitively.
static SQD: AddonDescriptor = AddonDescriptor {
    name: "test_sqd",
    depends: &["test_sqb"],
    exact_only: false,
};
static LINKED: AddonDescriptor = AddonDescriptor {
    name: "test_sq_linked",
    depends: &["base"],
    exact_only: false,
};

// Linked into this test binary: one addon that some tests install, one never installed. The
// expected set is not linked: the build site requires it.
static LINKED_MANIFEST: SqlObjectManifest = SqlObjectManifest::new("test_sq_linked", &["cond"]);
#[erp_registry::linkme::distributed_slice(erp_registry::SQL_OBJECT_DECLS)]
#[linkme(crate = erp_registry::linkme)]
static LINKED_COND: SqlObjectDecl = SqlObjectDecl::condition(
    "test_sq_linked",
    "cond",
    SqlConditionDecl::new("ERP09", "linked_cond", "Linked condition.")
        .kind(erp_registry::SqlConditionKind::Validation),
);
#[erp_registry::linkme::distributed_slice(erp_registry::SQL_OBJECT_DECLS)]
#[linkme(crate = erp_registry::linkme)]
static LINKED_ABSENT: SqlObjectDecl = SqlObjectDecl::condition(
    "test_sq_never_installed",
    "cond",
    SqlConditionDecl::new("ERP08", "absent_cond", "Never installed.")
        .kind(erp_registry::SqlConditionKind::Validation),
);

const MOVE: &str = "sqa.move";

fn move_model() -> ModelDecl {
    ModelDecl::new("test_sqa", MOVE).fields(const { &[FieldDecl::char("name")] })
}

/// A model of the closed event namespace (core declaration path only).
fn event_model() -> ModelDecl {
    ModelDecl::new("test_sqa", "sqa.event")
        .log_access(false)
        .fields(
            const {
                &[
                    FieldDecl::timestamptz("recorded_at")
                        .required(true)
                        .server_set(true),
                    FieldDecl::char("name"),
                ]
            },
        )
        .partition(
            PartitionDecl::new(PartitionInterval::Day, 7, PartitionRetain::Intervals(30))
                .by("recorded_at"),
        )
}

fn leak<T>(value: T) -> &'static T {
    Box::leak(Box::new(value))
}

fn leak_str(value: String) -> &'static str {
    Box::leak(value.into_boxed_str())
}

fn leak_slice<T>(value: Vec<T>) -> &'static [T] {
    Box::leak(value.into_boxed_slice())
}

fn helper() -> SqlObjectDecl {
    SqlObjectDecl::function(
        "test_sqa",
        "fn_helper",
        SqlFunctionDecl::new(
            "sqa_helper",
            SqlLanguage::Sql,
            SqlResult::Scalar(SqlScalar::Integer),
            "SELECT 1",
        )
        .args(&[SqlType::Scalar(SqlScalar::Integer)]),
    )
}

fn guard() -> SqlObjectDecl {
    SqlObjectDecl::function(
        "test_sqa",
        "fn_guard",
        SqlFunctionDecl::new(
            "sqa_move_guard",
            SqlLanguage::PlPgSql,
            SqlResult::Trigger,
            "BEGIN RETURN NULL; END",
        )
        .deps(&[SqlDep::Model(MOVE)]),
    )
}

fn insert_trigger(key: &'static str, name: &'static str) -> SqlObjectDecl {
    SqlObjectDecl::trigger(
        "test_sqa",
        key,
        SqlTriggerDecl::new(
            name,
            MOVE,
            ("test_sqa", "fn_guard"),
            SqlTiming::After,
            &[SqlEvent::Insert],
            SqlLevel::Statement,
        )
        .new_table("new_rows"),
    )
}

fn condition() -> SqlObjectDecl {
    SqlObjectDecl::condition(
        "test_sqa",
        "cond_unbalanced",
        SqlConditionDecl::new("ERP01", "sqa_unbalanced", "The move is not balanced.")
            .kind(erp_registry::SqlConditionKind::Validation),
    )
}

fn total() -> SqlObjectDecl {
    SqlObjectDecl::function(
        "test_sqb",
        "fn_total",
        SqlFunctionDecl::new(
            "sqb_total",
            SqlLanguage::Sql,
            SqlResult::Scalar(SqlScalar::Numeric),
            "SELECT 1",
        )
        .deps(&[SqlDep::Object {
            addon: "test_sqa",
            key: "fn_helper",
        }]),
    )
}

/// The manifests that list exactly the keys of `decls`, per addon.
fn manifests_for(decls: &[SqlObjectDecl]) -> Vec<SqlObjectManifest> {
    let mut by_addon: std::collections::BTreeMap<&'static str, Vec<&'static str>> =
        std::collections::BTreeMap::new();
    for decl in decls {
        let (addon, key) = (decl.addon(), decl.key());
        let keys = by_addon.entry(addon).or_default();
        if !keys.contains(&key) {
            keys.push(key);
        }
    }
    by_addon
        .into_iter()
        .map(|(addon, keys)| SqlObjectManifest::new(addon, leak_slice(keys)))
        .collect()
}

struct Build {
    addons: Vec<&'static AddonDescriptor>,
    models: Vec<ModelDecl>,
    events: Vec<ModelDecl>,
    manifests: Vec<SqlObjectManifest>,
    decls: Vec<SqlObjectDecl>,
    limits: SqlObjectLimits,
}

impl Build {
    fn new(decls: Vec<SqlObjectDecl>) -> Self {
        Self {
            addons: vec![&SQA, &SQB, &SQC, &SQD],
            models: vec![move_model()],
            events: vec![],
            manifests: manifests_for(&decls),
            decls,
            limits: SqlObjectLimits::default(),
        }
    }

    fn manifests(mut self, manifests: Vec<SqlObjectManifest>) -> Self {
        self.manifests = manifests;
        self
    }

    fn limits(mut self, limits: SqlObjectLimits) -> Self {
        self.limits = limits;
        self
    }

    fn run(self) -> erp_fields::Result<Registry> {
        let models: Vec<&'static ModelDecl> = self.models.into_iter().map(leak).collect();
        let manifests: Vec<&'static SqlObjectManifest> =
            self.manifests.into_iter().map(leak).collect();
        let decls: Vec<&'static SqlObjectDecl> = self.decls.into_iter().map(leak).collect();
        RegistryBuilder::new(self.addons)
            .with_decls(models)
            .with_core_event_decls(self.events.into_iter().map(leak))
            .require_sql_objects(manifests)
            .with_sql_objects(decls)
            .sql_object_limits(self.limits)
            .build()
    }
}

#[track_caller]
fn refused(build: Build, message: &str) {
    let error = build.run().expect_err("the registry must refuse");
    assert_eq!(error.kind(), ErrorKind::Programming, "{error:?}");
    assert_eq!(error.message(), message);
}

#[track_caller]
fn refused_decls(decls: Vec<SqlObjectDecl>, message: &str) {
    refused(Build::new(decls), message);
}

fn ids(registry: &Registry) -> Vec<String> {
    registry
        .sql_objects()
        .objects()
        .iter()
        .map(|o| o.id().to_string())
        .collect()
}

/// A plain SQL function of `test_sqa` that may depend on another object of the same addon.
fn plain_fn(key: &'static str, name: &'static str, dep: Option<&'static str>) -> SqlObjectDecl {
    let decl = SqlFunctionDecl::new(
        name,
        SqlLanguage::Sql,
        SqlResult::Scalar(SqlScalar::Integer),
        "SELECT 1",
    );
    SqlObjectDecl::function(
        "test_sqa",
        key,
        match dep {
            Some(key) => decl.deps(leak_slice(vec![SqlDep::Object {
                addon: "test_sqa",
                key,
            }])),
            None => decl,
        },
    )
}

/// A function whose key sorts before the function it uses, and a trigger whose key sorts before
/// its function's key, still come after what they reference, whatever the input order.
#[test]
fn spec_ormx_037_dependency_order_beats_identity_order() {
    let decls = vec![
        plain_fn("a_user", "sqa_a_user", Some("z_dep")),
        plain_fn("z_dep", "sqa_z_dep", None),
        insert_trigger("a_trg", "sqa_move_check_a"),
        guard(),
    ];
    let order = ids(&Build::new(decls.clone()).run().unwrap());
    assert_eq!(
        order,
        [
            "test_sqa/fn_guard",
            "test_sqa/a_trg",
            "test_sqa/z_dep",
            "test_sqa/a_user",
        ]
    );
    let mut reversed = decls;
    reversed.reverse();
    assert_eq!(ids(&Build::new(reversed).run().unwrap()), order);
}

/// Addon A declares a function, a trigger on its native model's table and a condition; addon B
/// depends on A and declares a function using A's function. The registry has qualified
/// identities, owners, resolved references and a deterministic dependency order; a key the
/// manifest expects but nobody declares fails naming the identity and the addon.
#[test]
fn spec_ormx_037_native_declarations_resolve_to_an_owned_graph() {
    let decls = vec![
        total(),
        insert_trigger("trg_insert", "sqa_move_check_insert"),
        condition(),
        helper(),
        guard(),
    ];
    let registry = Build::new(decls.clone()).run().unwrap();
    let graph = registry.sql_objects();
    let table = registry.model(MOVE).unwrap().table.clone();

    // each object after what it references, ties by identity (dependencies: trigger -> guard,
    // b/fn_total -> a/fn_helper)
    let order = ids(&registry);
    assert_eq!(
        order,
        [
            "test_sqa/cond_unbalanced",
            "test_sqa/fn_guard",
            "test_sqa/fn_helper",
            "test_sqa/trg_insert",
            "test_sqb/fn_total",
        ]
    );
    for object in graph.objects() {
        let position = |id: &str| order.iter().position(|o| o == id).unwrap();
        for reference in object.references() {
            if let SqlReference::Object(target) = reference {
                assert!(position(&target.to_string()) < position(&object.id().to_string()));
            }
        }
    }
    // another input order gives the same graph
    let mut reversed = decls;
    reversed.reverse();
    assert_eq!(ids(&Build::new(reversed).run().unwrap()), order);

    let fn_guard = graph.get("test_sqa", "fn_guard").unwrap();
    assert_eq!(
        (fn_guard.id().addon(), fn_guard.id().key(), fn_guard.owner()),
        ("test_sqa", "fn_guard", "test_sqa")
    );
    match fn_guard.references() {
        [SqlReference::Model(m)] => assert_eq!((m.model(), m.table()), (MOVE, table.as_str())),
        other => panic!("guard references {other:?}"),
    }
    let SqlObjectKind::Function(f) = fn_guard.kind() else {
        panic!("guard is {:?}", fn_guard.kind())
    };
    assert_eq!(
        (f.schema(), f.name(), f.result(), f.args()),
        ("public", "sqa_move_guard", SqlResult::Trigger, &[][..])
    );

    let trigger = graph.get("test_sqa", "trg_insert").unwrap();
    assert_eq!(trigger.owner(), "test_sqa");
    match trigger.references() {
        [SqlReference::Object(func), SqlReference::Model(m)] => {
            assert_eq!(func.to_string(), "test_sqa/fn_guard");
            assert_eq!((m.model(), m.table()), (MOVE, table.as_str()));
        }
        other => panic!("trigger references {other:?}"),
    }
    let SqlObjectKind::Trigger(t) = trigger.kind() else {
        panic!("trigger is {:?}", trigger.kind())
    };
    assert_eq!(t.name(), "sqa_move_check_insert");
    assert_eq!(t.function().to_string(), "test_sqa/fn_guard");
    assert_eq!(t.model().table(), table);
    assert_eq!(
        t.shape(),
        SqlTriggerShape::AfterStatement(SqlTransition::Insert {
            new_table: "new_rows"
        })
    );

    let cond = graph.get("test_sqa", "cond_unbalanced").unwrap();
    assert!(cond.references().is_empty());
    let SqlObjectKind::Condition(c) = cond.kind() else {
        panic!("condition is {:?}", cond.kind())
    };
    assert_eq!(
        (c.sqlstate(), c.identifier(), c.message()),
        ("ERP01", "sqa_unbalanced", "The move is not balanced.")
    );

    let total = graph.get("test_sqb", "fn_total").unwrap();
    assert_eq!(total.owner(), "test_sqb");
    match total.references() {
        [SqlReference::Object(target)] => {
            assert_eq!((target.addon(), target.key()), ("test_sqa", "fn_helper"));
        }
        other => panic!("total references {other:?}"),
    }
    assert_eq!(graph.objects().len(), 5);
    assert!(graph.get("test_sqa", "absent").is_none());

    // expected-registration check: the manifest expects a key nobody declared
    refused(
        Build::new(vec![helper()]).manifests(vec![SqlObjectManifest::new(
            "test_sqa",
            &["fn_helper", "fn_missing"],
        )]),
        "SQL object 'test_sqa/fn_missing' required by addon 'test_sqa' was not registered",
    );
    // a declaration its addon's manifest does not list, and one without any manifest
    refused(
        Build::new(vec![helper(), guard()])
            .manifests(vec![SqlObjectManifest::new("test_sqa", &["fn_helper"])]),
        "SQL object 'test_sqa/fn_guard': not listed in the SQL object manifest of addon 'test_sqa'",
    );
    refused(
        Build::new(vec![helper()]).manifests(vec![]),
        "SQL object 'test_sqa/fn_helper': not listed in the SQL object manifest of addon 'test_sqa'",
    );
    // a second, different manifest of one addon
    refused(
        Build::new(vec![helper()]).manifests(vec![
            SqlObjectManifest::new("test_sqa", &["fn_helper"]),
            SqlObjectManifest::new("test_sqa", &["fn_helper"]),
        ]),
        "SQL object manifest of addon 'test_sqa' is declared more than once",
    );
    // a manifest listing a key twice
    refused(
        Build::new(vec![helper()]).manifests(vec![SqlObjectManifest::new(
            "test_sqa",
            &["fn_helper", "fn_helper"],
        )]),
        "SQL object 'test_sqa/fn_helper': listed more than once in the manifest of addon 'test_sqa'",
    );
    // explicit input of an addon outside the set is refused (mirrors explicit model decls)
    let mut absent = Build::new(vec![helper()]);
    absent.addons = vec![&SQC];
    absent.models = vec![];
    absent.manifests = vec![];
    refused(
        absent,
        "SQL object 'test_sqa/fn_helper': addon 'test_sqa' is not in the addon set",
    );
    // a required manifest of an addon outside the set is ignored (one binary, many addon sets)
    let mut absent = Build::new(vec![]);
    absent.addons = vec![&SQC];
    absent.models = vec![];
    absent.manifests = vec![SqlObjectManifest::new("test_sqa", &["fn_helper"])];
    assert!(absent.run().unwrap().sql_objects().objects().is_empty());
}

/// A linked declaration resolves when its addon is installed and the build site requires its
/// manifest; it is ignored when its addon is not installed and refused when no required manifest
/// lists it. A required key whose declaring crate is not linked (this binary links none for it)
/// fails naming the object and the addon, instead of building with the object missing.
#[test]
fn spec_ormx_037_linked_objects_follow_the_addon_set() {
    let registry = RegistryBuilder::new([&LINKED])
        .require_sql_objects([&LINKED_MANIFEST])
        .build()
        .unwrap();
    assert_eq!(ids(&registry), ["test_sq_linked/cond"]);
    // the same manifest required by two build helpers counts once
    let registry = RegistryBuilder::new([&LINKED])
        .require_sql_objects([&LINKED_MANIFEST])
        .require_sql_objects([&LINKED_MANIFEST])
        .build()
        .unwrap();
    assert_eq!(ids(&registry), ["test_sq_linked/cond"]);
    // the linked declaration of the never-installed addon is ignored
    let registry = RegistryBuilder::new([&SQA]).build().unwrap();
    assert!(registry.sql_objects().objects().is_empty());
    // a linked declaration no required manifest lists
    let error = RegistryBuilder::new([&LINKED]).build().unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Programming);
    assert_eq!(
        error.message(),
        "SQL object 'test_sq_linked/cond': not listed in the SQL object manifest of addon \
         'test_sq_linked'"
    );
    // the declaring crate of a required key is not linked
    static UNLINKED: SqlObjectManifest =
        SqlObjectManifest::new("test_sq_linked", &["cond", "unlinked"]);
    let error = RegistryBuilder::new([&LINKED])
        .require_sql_objects([&UNLINKED])
        .build()
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Programming);
    assert_eq!(
        error.message(),
        "SQL object 'test_sq_linked/unlinked' required by addon 'test_sq_linked' was not registered"
    );
    let registry = RegistryBuilder::new([&LINKED])
        .linked(false)
        .build()
        .unwrap();
    assert!(registry.sql_objects().objects().is_empty());
}

fn function(addon: &'static str, key: &'static str, f: SqlFunctionDecl) -> SqlObjectDecl {
    SqlObjectDecl::function(addon, key, f)
}

fn sql(name: &'static str, body: &'static str) -> SqlFunctionDecl {
    SqlFunctionDecl::new(
        name,
        SqlLanguage::Sql,
        SqlResult::Scalar(SqlScalar::Integer),
        body,
    )
}

fn obj(addon: &'static str, key: &'static str) -> SqlDep {
    SqlDep::Object { addon, key }
}

/// Duplicate identities, invalid and overlong identifiers, excessive body bytes, unresolved
/// references, dependency cycles, contradictory trigger variants, cross-addon edges without the
/// manifest dependency and physical identity collisions fail Programming with addon, object and
/// reason; boundary-sized valid declarations succeed. The registry is pure: nothing reaches a
/// database.
#[test]
fn spec_ormx_038_malformed_declarations_fail_before_ddl() {
    let limits = SqlObjectLimits::default();
    let max = MAX_IDENTIFIER_LEN;

    // duplicate identity
    refused_decls(
        vec![helper(), helper()],
        "SQL object 'test_sqa/fn_helper': declared more than once",
    );
    // invalid and overlong keys (63 bytes is accepted)
    refused_decls(
        vec![function("test_sqa", "Bad-Key", sql("f", "SELECT 1"))],
        "SQL object 'test_sqa/Bad-Key': key is not of the form [a-z][a-z0-9_]*",
    );
    let long_key = leak_str("k".repeat(max + 1));
    refused_decls(
        vec![function("test_sqa", long_key, sql("f", "SELECT 1"))],
        &format!("SQL object 'test_sqa/{long_key}': key bytes limit {max} exceeded"),
    );
    let key63 = leak_str("k".repeat(max));
    Build::new(vec![function("test_sqa", key63, sql("f", "SELECT 1"))])
        .run()
        .unwrap();
    // invalid and overlong physical names (63 bytes is accepted)
    refused_decls(
        vec![function("test_sqa", "f", sql("9fn", "SELECT 1"))],
        "SQL object 'test_sqa/f': function name '9fn' is not a valid identifier",
    );
    let long_name = leak_str("n".repeat(max + 1));
    refused_decls(
        vec![function("test_sqa", "f", sql(long_name, "SELECT 1"))],
        &format!("SQL object 'test_sqa/f': function name bytes limit {max} exceeded"),
    );
    let name63 = leak_str("n".repeat(max));
    let registry = Build::new(vec![function("test_sqa", "f", sql(name63, "SELECT 1"))])
        .run()
        .unwrap();
    let SqlObjectKind::Function(f) = registry.sql_objects().get("test_sqa", "f").unwrap().kind()
    else {
        panic!("not a function")
    };
    assert_eq!(f.name().len(), max);
    refused_decls(
        vec![function(
            "test_sqa",
            "f",
            sql("f", "SELECT 1").schema("pg_catalog"),
        )],
        "SQL object 'test_sqa/f': schema 'pg_catalog' is reserved",
    );
    refused_decls(
        vec![function(
            "test_sqa",
            "f",
            sql("f", "SELECT 1").schema("information_schema"),
        )],
        "SQL object 'test_sqa/f': schema 'information_schema' is reserved",
    );
    // core object names are reserved
    refused_decls(
        vec![function("test_sqa", "f", sql("erp_unaccent", "SELECT 1"))],
        "SQL object 'test_sqa/f': names starting with 'erp_' are reserved for core objects",
    );
    refused_decls(
        vec![guard(), insert_trigger("trg", "erp_follow_delete")],
        "SQL object 'test_sqa/trg': names starting with 'erp_' are reserved for core objects",
    );
    refused_decls(
        vec![guard(), insert_trigger("trg", "sqa_move__row_version")],
        "SQL object 'test_sqa/trg': trigger names ending in '__row_version' are reserved for core",
    );
    // a trigger on a model of the closed event namespace
    let mut event = Build::new(vec![
        guard(),
        SqlObjectDecl::trigger(
            "test_sqa",
            "trg",
            SqlTriggerDecl::new(
                "trg",
                "sqa.event",
                ("test_sqa", "fn_guard"),
                SqlTiming::Before,
                &[SqlEvent::Insert],
                SqlLevel::Row,
            ),
        ),
    ]);
    event.events = vec![event_model()];
    refused(
        event,
        "SQL object 'test_sqa/trg': model 'sqa.event' has no table in the current schema",
    );
    // body bytes: exactly the limit is accepted, one more is refused
    let body = leak_str(format!("SELECT 1{}", " ".repeat(limits.body_bytes - 8)));
    assert_eq!(body.len(), limits.body_bytes);
    Build::new(vec![function("test_sqa", "f", sql("f", body))])
        .run()
        .unwrap();
    let body = leak_str(format!("{body} "));
    refused_decls(
        vec![function("test_sqa", "f", sql("f", body))],
        &format!(
            "SQL object 'test_sqa/f': body bytes limit {} exceeded",
            limits.body_bytes
        ),
    );
    refused_decls(
        vec![function("test_sqa", "f", sql("f", "  "))],
        "SQL object 'test_sqa/f': function body is empty",
    );
    // arguments: exactly the limit is accepted, one more is refused
    let args = leak_slice(vec![SqlType::Array(SqlScalar::Text); limits.arguments]);
    Build::new(vec![function(
        "test_sqa",
        "f",
        sql("f", "SELECT 1").args(args),
    )])
    .run()
    .unwrap();
    let args = leak_slice(vec![SqlType::Scalar(SqlScalar::Text); limits.arguments + 1]);
    refused_decls(
        vec![function("test_sqa", "f", sql("f", "SELECT 1").args(args))],
        &format!(
            "SQL object 'test_sqa/f': arguments limit {} exceeded",
            limits.arguments
        ),
    );
    // dependencies: exactly the limit (all resolved) is accepted, one more is refused
    let mut decls: Vec<SqlObjectDecl> = (0..=limits.dependencies)
        .map(|i| {
            let key = leak_str(format!("h{i:04}"));
            function("test_sqa", key, sql(key, "SELECT 1"))
        })
        .collect();
    let deps: Vec<SqlDep> = (0..limits.dependencies)
        .map(|i| obj("test_sqa", leak_str(format!("h{i:04}"))))
        .collect();
    decls.push(function(
        "test_sqa",
        "user",
        sql("usr", "SELECT 1").deps(leak_slice(deps.clone())),
    ));
    let registry = Build::new(decls.clone()).run().unwrap();
    let user = registry.sql_objects().get("test_sqa", "user").unwrap();
    assert_eq!(user.references().len(), limits.dependencies);
    let mut more = deps;
    more.push(obj(
        "test_sqa",
        leak_str(format!("h{:04}", limits.dependencies)),
    ));
    *decls.last_mut().unwrap() = function(
        "test_sqa",
        "user",
        sql("usr", "SELECT 1").deps(leak_slice(more)),
    );
    refused_decls(
        decls,
        &format!(
            "SQL object 'test_sqa/user': dependencies limit {} exceeded",
            limits.dependencies
        ),
    );
    refused_decls(
        vec![
            helper(),
            function(
                "test_sqa",
                "f",
                sql("f", "SELECT 1").deps(&[
                    SqlDep::Object {
                        addon: "test_sqa",
                        key: "fn_helper",
                    },
                    SqlDep::Object {
                        addon: "test_sqa",
                        key: "fn_helper",
                    },
                ]),
            ),
        ],
        "SQL object 'test_sqa/f': dependency 'test_sqa/fn_helper' is listed more than once",
    );
    // unresolved references
    refused_decls(
        vec![function(
            "test_sqa",
            "f",
            sql("f", "SELECT 1").deps(&[SqlDep::Object {
                addon: "test_sqa",
                key: "nope",
            }]),
        )],
        "SQL object 'test_sqa/f': dependency 'test_sqa/nope' is not registered",
    );
    refused_decls(
        vec![function(
            "test_sqa",
            "f",
            sql("f", "SELECT 1").deps(&[SqlDep::Model("sqa.nope")]),
        )],
        "SQL object 'test_sqa/f': model 'sqa.nope' is not in the registry",
    );
    let mut abstract_model = Build::new(vec![function(
        "test_sqa",
        "f",
        sql("f", "SELECT 1").deps(&[SqlDep::Model("sqa.mixin")]),
    )]);
    abstract_model
        .models
        .push(ModelDecl::new("test_sqa", "sqa.mixin").kind(ModelKind::Abstract));
    refused(
        abstract_model,
        "SQL object 'test_sqa/f': model 'sqa.mixin' has no table in the current schema",
    );
    // trigger function that does not return trigger, or is not a function
    refused_decls(
        vec![
            helper(),
            SqlObjectDecl::trigger(
                "test_sqa",
                "trg",
                SqlTriggerDecl::new(
                    "trg",
                    MOVE,
                    ("test_sqa", "fn_helper"),
                    SqlTiming::Before,
                    &[SqlEvent::Insert],
                    SqlLevel::Row,
                ),
            ),
        ],
        "SQL object 'test_sqa/trg': trigger function 'test_sqa/fn_helper' does not return trigger",
    );
    refused_decls(
        vec![
            condition(),
            SqlObjectDecl::trigger(
                "test_sqa",
                "trg",
                SqlTriggerDecl::new(
                    "trg",
                    MOVE,
                    ("test_sqa", "cond_unbalanced"),
                    SqlTiming::Before,
                    &[SqlEvent::Insert],
                    SqlLevel::Row,
                ),
            ),
        ],
        "SQL object 'test_sqa/trg': trigger function 'test_sqa/cond_unbalanced' is not a function",
    );
    refused_decls(
        vec![function(
            "test_sqa",
            "f",
            SqlFunctionDecl::new("f", SqlLanguage::Sql, SqlResult::Trigger, "SELECT 1"),
        )],
        "SQL object 'test_sqa/f': a function returning trigger must be written in PL/pgSQL",
    );
    refused_decls(
        vec![function(
            "test_sqa",
            "f",
            SqlFunctionDecl::new("f", SqlLanguage::PlPgSql, SqlResult::Trigger, "BEGIN END")
                .args(&[SqlType::Scalar(SqlScalar::Integer)]),
        )],
        "SQL object 'test_sqa/f': a function returning trigger takes no arguments",
    );
    // cycles: named by their members only (c0 depends on the cycle and is not named)
    refused_decls(
        vec![
            function("test_sqa", "c1", sql("c1", "SELECT 1").deps(&[C2])),
            function("test_sqa", "c2", sql("c2", "SELECT 1").deps(&[C1])),
            function("test_sqa", "c0", sql("c0", "SELECT 1").deps(&[C1])),
        ],
        "SQL object 'test_sqa/c1': dependency cycle among 'test_sqa/c1', 'test_sqa/c2'",
    );
    refused_decls(
        vec![function(
            "test_sqa",
            "c1",
            sql("c1", "SELECT 1").deps(&[C1]),
        )],
        "SQL object 'test_sqa/c1': dependency cycle among 'test_sqa/c1'",
    );
    // contradictory trigger variant (the full set is)
    refused_decls(
        vec![
            guard(),
            SqlObjectDecl::trigger(
                "test_sqa",
                "trg",
                SqlTriggerDecl::new(
                    "trg",
                    MOVE,
                    ("test_sqa", "fn_guard"),
                    SqlTiming::Before,
                    &[SqlEvent::Truncate],
                    SqlLevel::Row,
                ),
            ),
        ],
        "SQL object 'test_sqa/trg': TRUNCATE triggers are supported only as BEFORE TRUNCATE FOR \
         EACH STATEMENT",
    );
    // cross-addon edges need the manifest dependency (transitive dependencies count)
    let uses_helper = |addon| {
        function(
            addon,
            "f",
            sql("f2", "SELECT 1").deps(&[SqlDep::Object {
                addon: "test_sqa",
                key: "fn_helper",
            }]),
        )
    };
    refused_decls(
        vec![helper(), uses_helper("test_sqc")],
        "SQL object 'test_sqc/f': depends on 'test_sqa/fn_helper', but addon 'test_sqc' does not \
         depend on addon 'test_sqa'",
    );
    Build::new(vec![helper(), uses_helper("test_sqd")])
        .run()
        .unwrap();
    refused_decls(
        vec![function(
            "test_sqc",
            "f",
            sql("f", "SELECT 1").deps(&[SqlDep::Model(MOVE)]),
        )],
        "SQL object 'test_sqc/f': depends on model 'sqa.move', but addon 'test_sqc' does not \
         depend on addon 'test_sqa'",
    );
    // physical identity collisions; an overload with other arguments is a different function
    refused_decls(
        vec![
            helper(),
            function(
                "test_sqb",
                "same",
                sql("sqa_helper", "SELECT 2").args(&[SqlType::Scalar(SqlScalar::Integer)]),
            ),
        ],
        "SQL object 'test_sqb/same': function public.sqa_helper with the same argument types is \
         also declared by 'test_sqa/fn_helper'",
    );
    Build::new(vec![
        helper(),
        function(
            "test_sqb",
            "overload",
            sql("sqa_helper", "SELECT 2").args(&[SqlType::Array(SqlScalar::Integer)]),
        ),
    ])
    .run()
    .unwrap();
    refused_decls(
        vec![
            guard(),
            insert_trigger("trg_a", "sqa_same_name"),
            insert_trigger("trg_b", "sqa_same_name"),
        ],
        "SQL object 'test_sqa/trg_b': trigger 'sqa_same_name' on table 'sqa_move' is also declared \
         by 'test_sqa/trg_a'",
    );
    refused_decls(
        vec![
            condition(),
            SqlObjectDecl::condition(
                "test_sqb",
                "again",
                SqlConditionDecl::new("ERP01", "sqa_unbalanced", "Other text.")
                    .kind(erp_registry::SqlConditionKind::Validation),
            ),
        ],
        "SQL object 'test_sqb/again': condition ERP01 'sqa_unbalanced' is also declared by \
         'test_sqa/cond_unbalanced'",
    );
    // an exact duplicate under another key is refused too (one object per condition identity)
    refused_decls(
        vec![
            condition(),
            SqlObjectDecl::condition(
                "test_sqa",
                "same",
                SqlConditionDecl::new("ERP01", "sqa_unbalanced", "The move is not balanced.")
                    .kind(erp_registry::SqlConditionKind::Validation),
            ),
        ],
        "SQL object 'test_sqa/same': condition ERP01 'sqa_unbalanced' is also declared by \
         'test_sqa/cond_unbalanced'",
    );
    // conditions: SQLSTATE namespace, identifier, message bounds (exactly the limit is accepted)
    let cond = |state, message| {
        SqlObjectDecl::condition(
            "test_sqa",
            "c",
            SqlConditionDecl::new(state, "c", message)
                .kind(erp_registry::SqlConditionKind::Validation),
        )
    };
    refused_decls(
        vec![cond("23505", "Duplicate.")],
        "SQL object 'test_sqa/c': SQLSTATE '23505' is outside the ERP namespace (ERP00 to ERPZZ)",
    );
    refused_decls(
        vec![cond("erp01", "Lower.")],
        "SQL object 'test_sqa/c': SQLSTATE is not five characters of [0-9A-Z]",
    );
    let message = leak_str("m".repeat(limits.condition_message_bytes));
    Build::new(vec![cond("ERP02", message)]).run().unwrap();
    let message = leak_str("m".repeat(limits.condition_message_bytes + 1));
    refused_decls(
        vec![cond("ERP02", message)],
        &format!(
            "SQL object 'test_sqa/c': condition message bytes limit {} exceeded",
            limits.condition_message_bytes
        ),
    );
    refused_decls(
        vec![cond("ERP02", "")],
        "SQL object 'test_sqa/c': condition message is empty",
    );
}

const C1: SqlDep = SqlDep::Object {
    addon: "test_sqa",
    key: "c1",
};
const C2: SqlDep = SqlDep::Object {
    addon: "test_sqa",
    key: "c2",
};

/// A host may lower a bound: the lowered value is the new boundary, zero allows none, and a raise
/// above the default is refused.
#[test]
fn spec_ormx_038_host_lowered_limits() {
    let mut lowered = SqlObjectLimits::default();
    lowered.body_bytes = 10;
    Build::new(vec![function("test_sqa", "f", sql("f", "SELECT 100"))])
        .limits(lowered)
        .run()
        .unwrap();
    refused(
        Build::new(vec![function("test_sqa", "f", sql("f", "SELECT 1000"))]).limits(lowered),
        "SQL object 'test_sqa/f': body bytes limit 10 exceeded",
    );

    let mut two = SqlObjectLimits::default();
    two.addon_objects = 2;
    let three = || {
        vec![
            function("test_sqa", "a", sql("a", "SELECT 1")),
            function("test_sqa", "b", sql("b", "SELECT 1")),
            function("test_sqa", "c", sql("c", "SELECT 1")),
        ]
    };
    let mut decls = three();
    decls.pop();
    Build::new(decls).limits(two).run().unwrap();
    refused(
        Build::new(three()).limits(two),
        "SQL object 'test_sqa/c': addon objects limit 2 exceeded",
    );

    let mut none = SqlObjectLimits::default();
    none.registry_objects = 0;
    Build::new(vec![]).limits(none).run().unwrap();
    refused(
        Build::new(vec![helper()]).limits(none),
        "SQL object 'test_sqa/fn_helper': registry objects limit 0 exceeded",
    );
    let mut none = SqlObjectLimits::default();
    none.addon_objects = 0;
    refused(
        Build::new(vec![helper()]).limits(none),
        "SQL object 'test_sqa/fn_helper': addon objects limit 0 exceeded",
    );

    let mut bytes = SqlObjectLimits::default();
    bytes.addon_bytes = 64;
    refused(
        Build::new(vec![helper(), guard()]).limits(bytes),
        "SQL object 'test_sqa/fn_helper': addon bytes limit 64 exceeded",
    );
    // body and metadata bytes of `helper()`: key, schema, name and body
    let helper_bytes = ["fn_helper", "public", "sqa_helper", "SELECT 1"]
        .iter()
        .map(|s| s.len())
        .sum::<usize>();
    let mut exact = SqlObjectLimits::default();
    exact.addon_bytes = helper_bytes;
    exact.registry_bytes = helper_bytes;
    Build::new(vec![helper()]).limits(exact).run().unwrap();
    exact.addon_bytes = helper_bytes - 1;
    refused(
        Build::new(vec![helper()]).limits(exact),
        &format!(
            "SQL object 'test_sqa/fn_helper': addon bytes limit {} exceeded",
            helper_bytes - 1
        ),
    );
    exact.addon_bytes = helper_bytes;
    exact.registry_bytes = helper_bytes - 1;
    refused(
        Build::new(vec![helper()]).limits(exact),
        &format!(
            "SQL object 'test_sqa/fn_helper': registry bytes limit {} exceeded",
            helper_bytes - 1
        ),
    );

    let mut raised = SqlObjectLimits::default();
    raised.body_bytes = SqlObjectLimits::DEFAULT.body_bytes + 1;
    refused(
        Build::new(vec![]).limits(raised),
        &format!(
            "SQL object body bytes limit {} is above the default {}; a host may only lower it",
            raised.body_bytes,
            SqlObjectLimits::DEFAULT.body_bytes
        ),
    );
}

fn trigger(decl: SqlTriggerDecl) -> Vec<SqlObjectDecl> {
    vec![guard(), SqlObjectDecl::trigger("test_sqa", "trg", decl)]
}

fn after_statement(events: &'static [SqlEvent]) -> SqlTriggerDecl {
    SqlTriggerDecl::new(
        "trg",
        MOVE,
        ("test_sqa", "fn_guard"),
        SqlTiming::After,
        events,
        SqlLevel::Statement,
    )
}

fn shape(decls: Vec<SqlObjectDecl>) -> SqlTriggerShape {
    let registry = Build::new(decls).run().unwrap();
    match registry
        .sql_objects()
        .get("test_sqa", "trg")
        .unwrap()
        .kind()
    {
        SqlObjectKind::Trigger(t) => t.shape(),
        other => panic!("not a trigger: {other:?}"),
    }
}

/// One AFTER STATEMENT trigger per INSERT, UPDATE and DELETE with the matching transition aliases
/// resolves; multi-event, UPDATE OF, BEFORE with transition tables, duplicate aliases and deferred
/// (constraint) triggers with transition tables each fail naming the object and the rule.
#[test]
fn spec_ormx_040_transition_trigger_variants_are_checked() {
    assert_eq!(
        shape(trigger(
            after_statement(&[SqlEvent::Insert]).new_table("new_rows")
        )),
        SqlTriggerShape::AfterStatement(SqlTransition::Insert {
            new_table: "new_rows"
        })
    );
    assert_eq!(
        shape(trigger(
            after_statement(&[SqlEvent::Update])
                .old_table("old_rows")
                .new_table("new_rows")
        )),
        SqlTriggerShape::AfterStatement(SqlTransition::Update {
            old_table: "old_rows",
            new_table: "new_rows"
        })
    );
    assert_eq!(
        shape(trigger(
            after_statement(&[SqlEvent::Delete]).old_table("old_rows")
        )),
        SqlTriggerShape::AfterStatement(SqlTransition::Delete {
            old_table: "old_rows"
        })
    );
    // the other three closed variants resolve too
    let base = |timing: SqlTiming, events: &'static [SqlEvent], level: SqlLevel| {
        SqlTriggerDecl::new("trg", MOVE, ("test_sqa", "fn_guard"), timing, events, level)
    };
    assert_eq!(
        shape(trigger(base(
            SqlTiming::Before,
            &[SqlEvent::Truncate],
            SqlLevel::Statement
        ))),
        SqlTriggerShape::BeforeTruncate
    );
    assert_eq!(
        shape(trigger(base(
            SqlTiming::Before,
            &[SqlEvent::Insert],
            SqlLevel::Row
        ))),
        SqlTriggerShape::BeforeInsertRow
    );
    assert_eq!(
        shape(trigger(
            base(SqlTiming::After, &[SqlEvent::Insert], SqlLevel::Row)
                .constraint(SqlDeferral::InitiallyDeferred)
        )),
        SqlTriggerShape::AfterInsertConstraintRow(SqlDeferral::InitiallyDeferred)
    );

    let rule = |decl, rule: &str| {
        refused_decls(trigger(decl), &format!("SQL object 'test_sqa/trg': {rule}"));
    };
    rule(
        after_statement(&[SqlEvent::Insert, SqlEvent::Update]).new_table("new_rows"),
        "a trigger has exactly one event; declare one trigger per event",
    );
    rule(after_statement(&[]), "a trigger needs exactly one event");
    rule(
        after_statement(&[SqlEvent::Update])
            .update_of(&["name"])
            .old_table("old_rows")
            .new_table("new_rows"),
        "UPDATE OF column lists cannot be combined with transition tables",
    );
    rule(
        after_statement(&[SqlEvent::Update]).update_of(&["name"]),
        "UPDATE OF column lists are not supported",
    );
    rule(
        base(SqlTiming::Before, &[SqlEvent::Insert], SqlLevel::Statement).new_table("new_rows"),
        "BEFORE triggers cannot have transition tables",
    );
    rule(
        after_statement(&[SqlEvent::Update])
            .old_table("rows")
            .new_table("rows"),
        "transition aliases OLD TABLE and NEW TABLE must differ",
    );
    rule(
        base(SqlTiming::After, &[SqlEvent::Insert], SqlLevel::Row)
            .constraint(SqlDeferral::InitiallyDeferred)
            .new_table("new_rows"),
        "constraint (deferrable) triggers cannot have transition tables",
    );
    // aliases must match the one event
    rule(
        after_statement(&[SqlEvent::Insert]).old_table("old_rows"),
        "AFTER INSERT FOR EACH STATEMENT needs NEW TABLE and no OLD TABLE",
    );
    rule(
        after_statement(&[SqlEvent::Delete]).new_table("new_rows"),
        "AFTER DELETE FOR EACH STATEMENT needs OLD TABLE and no NEW TABLE",
    );
    rule(
        after_statement(&[SqlEvent::Update]).new_table("new_rows"),
        "AFTER UPDATE FOR EACH STATEMENT needs both OLD TABLE and NEW TABLE",
    );
    // shapes outside the four variants
    rule(
        base(SqlTiming::After, &[SqlEvent::Insert], SqlLevel::Row),
        "AFTER FOR EACH ROW triggers must be constraint triggers with an explicit deferral",
    );
    rule(
        base(SqlTiming::After, &[SqlEvent::Update], SqlLevel::Row)
            .constraint(SqlDeferral::NotDeferrable),
        "constraint triggers are supported only as AFTER INSERT FOR EACH ROW",
    );
    rule(
        base(SqlTiming::Before, &[SqlEvent::Update], SqlLevel::Row),
        "BEFORE triggers are supported only as BEFORE INSERT FOR EACH ROW or BEFORE TRUNCATE FOR \
         EACH STATEMENT",
    );
    rule(
        after_statement(&[SqlEvent::Insert]).new_table("9bad"),
        "transition alias '9bad' is not a valid identifier",
    );
}

/// B5: the registry answers which declared condition is a `(SQLSTATE, identifier)` pair, with the
/// owning identity; several conditions share one SQLSTATE, and an unknown or crossed pair finds
/// nothing.
#[test]
fn conditions_are_found_by_sqlstate_and_identifier() {
    let other = SqlObjectDecl::condition(
        "test_sqb",
        "cond_locked",
        SqlConditionDecl::new("ERP01", "sqb_locked", "The period is locked.")
            .kind(erp_registry::SqlConditionKind::Validation),
    );
    let third = SqlObjectDecl::condition(
        "test_sqc",
        "cond_other_state",
        SqlConditionDecl::new("ERP02", "sqc_other", "Other state.")
            .kind(erp_registry::SqlConditionKind::Validation),
    );
    let registry = Build::new(vec![condition(), other, third]).run().unwrap();
    let graph = registry.sql_objects();

    let (id, found) = graph.condition("ERP01", "sqa_unbalanced").unwrap();
    assert_eq!(id.to_string(), "test_sqa/cond_unbalanced");
    assert_eq!(found.message(), "The move is not balanced.");
    let (id, found) = graph.condition("ERP01", "sqb_locked").unwrap();
    assert_eq!(id.to_string(), "test_sqb/cond_locked");
    assert_eq!(found.identifier(), "sqb_locked");
    assert_eq!(found.sqlstate(), "ERP01");
    let (id, _) = graph.condition("ERP02", "sqc_other").unwrap();
    assert_eq!(id.to_string(), "test_sqc/cond_other_state");

    // a known identifier under another SQLSTATE, an unknown identifier, and a standard state
    assert!(graph.condition("ERP02", "sqa_unbalanced").is_none());
    assert!(graph.condition("ERP01", "sqa_unknown").is_none());
    assert!(graph.condition("23505", "sqa_unbalanced").is_none());
    assert!(graph.condition("ERP01", "").is_none());
}

fn table_grant(
    key: &'static str,
    binding: SqlBinding,
    ops: &'static [SqlTableOp],
) -> SqlObjectDecl {
    SqlObjectDecl::table_grant("test_sqa", key, SqlTableGrantDecl::new(MOVE, binding, ops))
}

/// `erp-test-models/tests/sql_objects_security.rs`)
/// Role bindings are symbolic and closed: EXECUTE goes to `request`, `maintenance` or the
/// `definer` of an invoker function, never to the binding that owns the function or on a trigger
/// function; a table grant policy goes to `request` or `definer` with a non-empty set of
/// SELECT/INSERT/UPDATE/DELETE, once per table and binding.
#[test]
fn spec_ormx_051_role_bindings_are_validated_at_registry_build() {
    use SqlBinding::{Definer, Maintenance, Request, SchemaOwner};
    use SqlTableOp::{Insert, Select};
    let entry = |execute: &'static [SqlBinding], security: SqlSecurity| {
        function(
            "test_sqa",
            "fn_entry",
            sql("sqa_entry", "SELECT 1")
                .security(security)
                .grant_execute(execute),
        )
    };
    let registry = Build::new(vec![
        entry(&[Request, Maintenance], SqlSecurity::Definer),
        table_grant("grant_request", Request, &[Insert]),
        table_grant("grant_definer", Definer, &[Select, Insert]),
    ])
    .run()
    .unwrap();
    let graph = registry.sql_objects();
    let SqlObjectKind::Function(f) = graph.get("test_sqa", "fn_entry").unwrap().kind() else {
        panic!("fn_entry is a function");
    };
    assert_eq!(f.execute(), &[Request, Maintenance]);
    let SqlObjectKind::TableGrant(g) = graph.get("test_sqa", "grant_definer").unwrap().kind()
    else {
        panic!("grant_definer is a table grant");
    };
    assert_eq!(
        (g.model().table(), g.binding(), g.ops()),
        ("sqa_move", Definer, &[Select, Insert][..])
    );
    assert_eq!(
        graph.get("test_sqa", "grant_definer").unwrap().references(),
        &[SqlReference::Model(g.model().clone())]
    );
    // an invoker function may be executed by the definer (a definer body calls it)
    Build::new(vec![entry(&[Definer], SqlSecurity::Invoker)])
        .run()
        .unwrap();

    let prefix = "SQL object 'test_sqa/fn_entry': ";
    for (decl, reason) in [
        (
            entry(&[SchemaOwner], SqlSecurity::Invoker),
            "binding 'schema_owner' owns the function; EXECUTE is not granted to it",
        ),
        (
            entry(&[Definer], SqlSecurity::Definer),
            "binding 'definer' owns the function; EXECUTE is not granted to it",
        ),
        (
            entry(&[Request, Request], SqlSecurity::Invoker),
            "binding 'request' is granted EXECUTE more than once",
        ),
        (
            function(
                "test_sqa",
                "fn_entry",
                SqlFunctionDecl::new(
                    "sqa_entry",
                    SqlLanguage::PlPgSql,
                    SqlResult::Trigger,
                    "BEGIN RETURN NEW; END",
                )
                .grant_execute(&[Request]),
            ),
            "a function returning trigger is run by its triggers; EXECUTE is not granted on it",
        ),
    ] {
        refused_decls(vec![decl], &format!("{prefix}{reason}"));
    }
    for (decl, message) in [
        (
            table_grant("grant", SchemaOwner, &[Select]),
            "SQL object 'test_sqa/grant': binding 'schema_owner' owns the table; it takes no grant",
        ),
        (
            table_grant("grant", Maintenance, &[Select]),
            "SQL object 'test_sqa/grant': binding 'maintenance' executes declared entry points \
             only; it takes no table grant",
        ),
        (
            table_grant("grant", Request, &[]),
            "SQL object 'test_sqa/grant': a table grant lists no operation",
        ),
        (
            table_grant("grant", Request, &[Insert, Select, Insert]),
            "SQL object 'test_sqa/grant': operation INSERT is listed more than once",
        ),
        (
            SqlObjectDecl::table_grant(
                "test_sqa",
                "grant",
                SqlTableGrantDecl::new("sqa.absent", Request, &[Insert]),
            ),
            "SQL object 'test_sqa/grant': model 'sqa.absent' is not in the registry",
        ),
    ] {
        refused_decls(vec![decl], message);
    }
    refused_decls(
        vec![
            table_grant("grant_a", Request, &[Insert]),
            table_grant("grant_b", Request, &[Select]),
        ],
        "SQL object 'test_sqa/grant_b': a grant policy for binding 'request' on table 'sqa_move' \
         is also declared by 'test_sqa/grant_a'",
    );
    // a shared id sequence: INSERT needs USAGE on it, which no policy grants yet
    for binding in [Request, Definer] {
        let mut shared = Build::new(vec![table_grant("grant", binding, &[Select, Insert])]);
        shared.models = vec![move_model().id_sequence("sqa_ids")];
        refused(
            shared,
            "SQL object 'test_sqa/grant': model 'sqa.move' takes its ids from the shared \
             sequence 'sqa_ids', and table grants do not cover sequences yet; an INSERT policy \
             needs a model with its own identity id",
        );
    }
    let mut shared = Build::new(vec![table_grant("grant", Request, &[Select])]);
    shared.models = vec![move_model().id_sequence("sqa_ids")];
    shared.run().unwrap();
}

/// A condition without an explicit kind is refused; registration must never silently choose
/// a public classification (lead amendment 2026-11-13).
#[test]
fn condition_kind_is_required() {
    let declaration = SqlObjectDecl::condition(
        "test_sqa",
        "missing_kind",
        SqlConditionDecl::new("ERP10", "sqa_missing_kind", "Refused."),
    );
    let error = Build::new(vec![declaration]).run().unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Programming);
    assert_eq!(
        error.message(),
        "SQL object 'test_sqa/missing_kind': condition kind is missing; declare Validation or Programming"
    );
}

#[test]
fn condition_kind_is_preserved() {
    for kind in [
        erp_registry::SqlConditionKind::Validation,
        erp_registry::SqlConditionKind::Programming,
    ] {
        let decl = SqlObjectDecl::condition(
            "test_sqa",
            "kind",
            SqlConditionDecl::new("ERP10", "sqa_kind", "Refused.").kind(kind),
        );
        let registry = Build::new(vec![decl]).run().unwrap();
        let (_, condition) = registry
            .sql_objects()
            .condition("ERP10", "sqa_kind")
            .unwrap();
        assert_eq!(condition.kind(), kind);
    }
}
