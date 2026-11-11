//! `<function>` and `<delete>` on a harness transaction: (both halves),
//! 018, 019, 020, and the refusals of the data-callable level.
//! No module system: XML text through `XmlDataReader` into `LoadSession::load_file`.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

use std::collections::HashMap;
use std::sync::Arc;

use erp_data::load::{
    AddonFiles, DataNotice, FileContext, FileReadError, LoadError, LoadErrorKind, LoadSession,
    NoFiles, NoticeKind, SessionOptions,
};
use erp_data::model_data::{lookup, update_xmlids};
use erp_data::{ConvertMode, ParseErrorKind, XidBinding, XmlDataReader, XmlId};
use erp_orm::BoxFuture;
use erp_sdk::{Cell, Domain, ErrorKind, Records, Transaction, Value};
use erp_test_models::{base_users, test_convert, test_convert_ui};
use erp_testkit::Harness;

const USERED: &str = "test_convert.usered";
const CALL: &str = "test_convert.call";

async fn harness() -> Harness {
    Harness::builder([
        &base_users::ADDON,
        &test_convert::ADDON,
        &test_convert_ui::ADDON,
        &erp_data::model_data::ADDON,
    ])
    .with_decls([erp_data::ir_model_data_decl()])
    // as the module system registers it: unlinked records lose their external ids
    .unlink_hook(erp_data::model_data::UNLINK_HOOK)
    .build()
    .await
    .expect("harness")
}

fn session() -> LoadSession {
    LoadSession::new(SessionOptions::new("20.0", Arc::new(NoFiles)))
}

async fn load(
    session: &mut LoadSession,
    tx: &mut Transaction,
    xml: &str,
    mode: ConvertMode,
) -> Result<(), LoadError> {
    let file = FileContext::new("m", "data/f.xml", mode);
    let mut ops = XmlDataReader::new(xml, "data/f.xml", "m", false);
    session.load_file(tx, &file, &mut ops).await
}

async fn init(session: &mut LoadSession, tx: &mut Transaction, xml: &str) -> Result<(), LoadError> {
    load(session, tx, xml, ConvertMode::Init).await
}

async fn all(tx: &mut Transaction, model: &str) -> Records {
    tx.env()
        .records(model)
        .unwrap()
        .search(tx, &Domain::TRUE)
        .await
        .unwrap()
}

async fn cells(tx: &mut Transaction, model: &str, field: &str) -> Vec<Cell> {
    let records = all(tx, model).await;
    records.cells(tx, field).await.unwrap()
}

fn s(text: &str) -> Cell {
    Cell::Str(Arc::from(text))
}

/// What the fixture methods logged: (method, ids, args, kwargs, ctx_foo, tz), one entry per call.
type Logged = (String, String, String, String, Cell, Cell);

async fn calls(tx: &mut Transaction) -> Vec<Logged> {
    let mut columns = Vec::new();
    for field in ["method", "ids", "args", "kwargs", "ctx_foo", "ctx_tz"] {
        columns.push(cells(tx, CALL, field).await);
    }
    let text = |cell: &Cell| match cell {
        Cell::Str(text) => text.to_string(),
        Cell::Null => String::new(),
        other => panic!("not a text cell: {other:?}"),
    };
    (0..columns[0].len())
        .map(|i| {
            (
                text(&columns[0][i]),
                text(&columns[1][i]),
                text(&columns[2][i]),
                text(&columns[3][i]),
                columns[4][i].clone(),
                columns[5][i].clone(),
            )
        })
        .collect()
}

fn logged(method: &str, ids: &str, args: &str, kwargs: &str, ctx_foo: Cell, tz: Cell) -> Logged {
    (
        method.to_owned(),
        ids.to_owned(),
        args.to_owned(),
        kwargs.to_owned(),
        ctx_foo,
        tz,
    )
}

/// A load that must fail: the error, with the transaction rolled back.
async fn failing(xml: &str, mode: ConvertMode) -> LoadError {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let mut session = session();
    let err = load(&mut session, &mut tx, xml, mode).await.unwrap_err();
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
    err
}

fn function_error(err: &LoadError) -> &str {
    match err.kind() {
        LoadErrorKind::FunctionArguments { message, .. } => message,
        other => panic!("expected FunctionArguments, got {other:?}"),
    }
}

#[tokio::test]
async fn spec_data_018_function_arguments_bind_to_a_model_and_a_records_method() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let mut session = session();
    init(
        &mut session,
        &mut tx,
        r#"<odoo context="{'tz': 'outer'}">
  <record id="u1" model="test_convert.usered"><field name="name">U</field></record>
  <!-- arguments in eval -->
  <function model="test_convert.usered" name="model_method" eval="[1, 2]"/>
  <function model="test_convert.usered" name="method" eval="[ref('u1'), 2]"/>
  <!-- arguments in children -->
  <function model="test_convert.usered" name="model_method">
    <value eval="1"/><value type="int">2</value>
  </function>
  <function model="test_convert.usered" name="method">
    <value eval="[ref('u1')]"/><value eval="2"/>
  </function>
  <!-- a keyword -->
  <function model="test_convert.usered" name="model_method">
    <value eval="1"/><value name="extra" eval="2"/>
  </function>
  <function model="test_convert.usered" name="method">
    <value eval="ref('u1')"/><value name="extra" eval="2"/>
  </function>
  <!-- a context keyword is merged into the context and not passed on -->
  <function model="test_convert.usered" name="model_method">
    <value eval="1"/><value name="context" eval="{'foo': 2}"/>
  </function>
  <function model="test_convert.usered" name="method">
    <value eval="[ref('u1')]"/><value name="context" eval="{'foo': 2, 'tz': 'inner'}"/>
  </function>
  <!-- a nested function: its recordset result is the ids -->
  <function model="test_convert.usered" name="method">
    <function model="test_convert.usered" name="search" eval="[[]]"/>
  </function>
  <!-- falsy ids are dropped, an empty list is an empty recordset -->
  <function model="test_convert.usered" name="method" eval="[[0, False, ref('u1')], 5]"/>
  <function model="test_convert.usered" name="method" eval="[False]"/>
</odoo>"#,
    )
    .await
    .unwrap();
    let u1 = all(&mut tx, USERED).await.ids()[0].to_string();
    let nil = Cell::Null;
    assert_eq!(
        calls(&mut tx).await,
        [
            logged("model_method", "", "1,2", "", nil.clone(), s("outer")),
            logged("method", &u1, "2", "", nil.clone(), s("outer")),
            logged("model_method", "", "1,2", "", nil.clone(), s("outer")),
            logged("method", &u1, "2", "", nil.clone(), s("outer")),
            logged("model_method", "", "1", "extra=2", nil.clone(), s("outer")),
            logged("method", &u1, "", "extra=2", nil.clone(), s("outer")),
            logged("model_method", "", "1", "", s("2"), s("outer")),
            logged("method", &u1, "", "", s("2"), s("inner")),
            logged("method", &u1, "", "", nil.clone(), s("outer")),
            logged("method", &u1, "5", "", nil.clone(), s("outer")),
            logged("method", "", "", "", nil, s("outer")),
        ]
    );
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

// ---- (both halves) ----------------------------------------------------------

#[tokio::test]
async fn spec_data_017_uid_and_context_chain_for_records_and_functions() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let env = tx.env();
    let admin = env
        .records("res.users")
        .unwrap()
        .create_values(
            &mut tx,
            &[[
                ("name".to_owned(), Value::str("Admin")),
                ("login".to_owned(), Value::str("admin")),
            ]
            .into_iter()
            .collect()],
        )
        .await
        .unwrap()
        .ids()[0];
    update_xmlids(
        &mut tx,
        &env,
        &[
            XidBinding::new(
                XmlId::parse("base.user_root").unwrap(),
                "res.users",
                1,
                true,
            ),
            XidBinding::new(
                XmlId::parse("base.user_admin").unwrap(),
                "res.users",
                admin,
                true,
            ),
        ],
        false,
    )
    .await
    .unwrap();
    let mut session = session();
    init(
        &mut session,
        &mut tx,
        r#"<odoo uid="base.user_root" context="{'default_tz': 'A', 'tz': 'A', 'foo': 1}">
  <record id="outer_rec" model="test_convert.usered"><field name="name">outer</field></record>
  <record id="inner_rec" model="test_convert.usered" uid="base.user_admin" context="{'default_tz': 'B'}">
    <field name="name">inner</field>
  </record>
  <function model="test_convert.usered" name="model_method"/>
  <function model="test_convert.usered" name="model_method" uid="base.user_admin" context="{'tz': 'B'}"/>
  <function model="test_convert.usered" name="create" uid="base.user_admin" eval="[[{'name': 'made'}]]"/>
</odoo>"#,
    )
    .await
    .unwrap();

    // records: without inner attributes the outer ones apply, the inner ones win
    let names = cells(&mut tx, USERED, "name").await;
    let created_by = cells(&mut tx, USERED, "create_uid").await;
    let user = cells(&mut tx, USERED, "user_id").await;
    let tz = cells(&mut tx, USERED, "tz").await;
    assert_eq!(names, [s("outer"), s("inner"), s("made")]);
    assert_eq!(
        created_by,
        [Cell::Id(1), Cell::Id(admin), Cell::Id(admin)],
        "create_uid"
    );
    assert_eq!(
        user,
        [Cell::Id(1), Cell::Id(admin), Cell::Id(admin)],
        "the default of user_id is the acting user"
    );
    assert_eq!(tz[..2], [s("A"), s("B")]);

    // functions: the method sees the acting user and the merged context
    let run_by = cells(&mut tx, CALL, "run_by").await;
    assert_eq!(run_by, [Cell::Id(1), Cell::Id(admin)]);
    let seen = calls(&mut tx).await;
    assert_eq!(
        seen.iter()
            .map(|c| (c.4.clone(), c.5.clone()))
            .collect::<Vec<_>>(),
        // `foo` comes from the outer context in both calls; `tz` is the inner one in the second
        [(s("1"), s("A")), (s("1"), s("B"))]
    );
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_data_019_noupdate_function_runs_on_install_not_on_update_and_names_are_refused() {
    let xml = r#"<odoo>
  <data noupdate="1"><function model="test_convert.usered" name="model_method" eval="[1]"/></data>
  <function model="test_convert.usered" name="model_method" eval="[2]"/>
</odoo>"#;
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let mut session = session();

    // install: both run
    init(&mut session, &mut tx, xml).await.unwrap();
    let args: Vec<String> = calls(&mut tx).await.into_iter().map(|c| c.2).collect();
    assert_eq!(args, ["1", "2"]);

    // upgrade: only the one outside noupdate runs
    load(&mut session, &mut tx, xml, ConvertMode::Update)
        .await
        .unwrap();
    let args: Vec<String> = calls(&mut tx).await.into_iter().map(|c| c.2).collect();
    assert_eq!(args, ["1", "2", "2"]);
    tx.rollback().await.unwrap();

    let mut tx = h.begin().await.unwrap();
    let mut session = self::session();
    let err = init(
        &mut session,
        &mut tx,
        "<odoo><function model=\"test_convert.usered\" name=\"__init__\"/></odoo>",
    )
    .await
    .unwrap_err();
    match err.kind() {
        LoadErrorKind::Parse(parse) => {
            assert_eq!(
                parse.kind(),
                &ParseErrorKind::ForbiddenName {
                    name: "__init__".into()
                }
            );
            assert_eq!(parse.detail(), "Access to forbidden name '__init__'");
        }
        other => panic!("expected a parse error, got {other:?}"),
    }
    assert_eq!(calls(&mut tx).await, []);

    // a method that is not data-callable is refused, and so is one the model does not have
    for (name, kind_matches) in [("internal_only", true), ("no_such_method", false)] {
        let err = init(
            &mut session,
            &mut tx,
            &format!("<odoo><function model=\"test_convert.usered\" name=\"{name}\"/></odoo>"),
        )
        .await
        .unwrap_err();
        assert_eq!(
            matches!(err.kind(), LoadErrorKind::NotDataCallable { .. }),
            kind_matches,
            "{err:?}"
        );
        let expected = if kind_matches {
            "<function> test_convert.usered.internal_only: the method is not data-callable \
             (declare it with #[data_callable] or #[rpc])"
        } else {
            "<function> test_convert.usered.no_such_method: the model has no such method"
        };
        assert_eq!(
            err.to_string(),
            format!(
                "while parsing data/f.xml:1, somewhere inside\n\
                 <function model=\"test_convert.usered\" name=\"{name}\">\n{expected}"
            )
        );
    }
    assert_eq!(calls(&mut tx).await, []);
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn rpc_hidden_and_private_methods_are_callable_from_data_when_declared() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let mut session = session();
    init(
        &mut session,
        &mut tx,
        "<odoo>\
         <function model=\"test_convert.usered\" name=\"rpc_write\"/>\
         <function model=\"test_convert.usered\" name=\"hidden_but_callable\"/>\
         <function model=\"test_convert.usered\" name=\"_private_ok\"/>\
         </odoo>",
    )
    .await
    .unwrap();
    let methods: Vec<String> = calls(&mut tx).await.into_iter().map(|c| c.0).collect();
    assert_eq!(methods, ["rpc_write", "hidden_but_callable", "_private_ok"]);
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn a_function_on_a_model_the_registry_lacks_is_a_model_missing_error() {
    let err = failing(
        "<odoo>\n<function model=\"no.such\" name=\"x\"/>\n</odoo>",
        ConvertMode::Init,
    )
    .await;
    assert!(
        matches!(err.kind(), LoadErrorKind::ModelMissing { .. }),
        "{err:?}"
    );
    assert_eq!(
        err.to_string(),
        "while parsing data/f.xml:2, somewhere inside\n<function model=\"no.such\" name=\"x\">\n\
         <function> model 'no.such' is not in the registry of this install step"
    );
}

// ---- argument errors ---------------------------------------------------------------------

#[tokio::test]
async fn bad_arguments_are_refused_with_the_binders_words() {
    let cases: [(&str, &str); 9] = [
        (
            "<function model=\"test_convert.usered\" name=\"typed_args\" eval=\"[1, 'a', 3]\"/>",
            "takes 2 positional arguments but 3 were given",
        ),
        (
            "<function model=\"test_convert.usered\" name=\"refuse\" eval=\"[1]\"/>",
            "takes 0 positional arguments but 1 was given",
        ),
        (
            "<function model=\"test_convert.usered\" name=\"typed_args\">\
             <value eval=\"1\"/><value name=\"nope\" eval=\"1\"/></function>",
            "got an unexpected keyword argument 'nope'",
        ),
        (
            "<function model=\"test_convert.usered\" name=\"typed_args\">\
             <value eval=\"1\"/><value name=\"count\" eval=\"2\"/></function>",
            "got multiple values for argument 'count'",
        ),
        (
            "<function model=\"test_convert.usered\" name=\"typed_args\"/>",
            "missing a required argument: 'count'",
        ),
        (
            "<function model=\"test_convert.usered\" name=\"typed_args\"><value>abc</value></function>",
            "argument 'count': expected an integer, got string",
        ),
        (
            "<function model=\"test_convert.usered\" name=\"typed_args\" eval=\"{'a': 1}\"/>",
            "eval \"{'a': 1}\" gave a dict, not a list or tuple of arguments",
        ),
        (
            "<function model=\"test_convert.usered\" name=\"method\"/>",
            "a records method takes the record ids as its first argument",
        ),
        (
            "<function model=\"test_convert.usered\" name=\"method\" eval=\"[['a']]\"/>",
            "record ids must be integers, got the value 'a'",
        ),
    ];
    for (function, message) in cases {
        let err = failing(&format!("<odoo>{function}</odoo>"), ConvertMode::Init).await;
        assert_eq!(function_error(&err), message, "{err:?}");
        let call = match err.kind() {
            LoadErrorKind::FunctionArguments { call, .. } => call.clone(),
            other => panic!("{other:?}"),
        };
        assert!(call.starts_with("test_convert.usered."), "{call}");
        assert_eq!(
            err.to_string(),
            format!(
                "while parsing data/f.xml:1, somewhere inside\n{}\n<function> {call}: {message}",
                err.op().unwrap()
            )
        );
    }
    // the context keyword must be a dict
    let err = failing(
        "<odoo><function model=\"test_convert.usered\" name=\"model_method\">\
         <value name=\"context\" eval=\"[1]\"/></function></odoo>",
        ConvertMode::Init,
    )
    .await;
    assert_eq!(
        function_error(&err),
        "the 'context' keyword must be a dict, got the value [1]"
    );
}

#[tokio::test]
async fn a_method_error_keeps_its_kind_and_stops_the_load() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let mut session = session();
    let err = init(
        &mut session,
        &mut tx,
        "<odoo>\
         <function model=\"test_convert.usered\" name=\"model_method\" eval=\"[1]\"/>\
         <function model=\"test_convert.usered\" name=\"refuse\"/>\
         <function model=\"test_convert.usered\" name=\"model_method\" eval=\"[2]\"/>\
         </odoo>",
    )
    .await
    .unwrap_err();
    assert_eq!(err.orm_kind(), Some(ErrorKind::Validation));
    let source = std::error::Error::source(&err).unwrap();
    assert_eq!(source.to_string(), "the fixture refuses this call");
    assert_eq!(
        err.to_string(),
        "while parsing data/f.xml:1, somewhere inside\n\
         <function model=\"test_convert.usered\" name=\"refuse\">"
    );
    // the call before it ran, the one after it did not
    let args: Vec<String> = calls(&mut tx).await.into_iter().map(|c| c.2).collect();
    assert_eq!(args, ["1"]);
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

async fn names_of(tx: &mut Transaction, model: &str) -> Vec<Cell> {
    cells(tx, model, "name").await
}

/// on with the `id` target)
#[tokio::test]
async fn spec_data_020_delete_runs_under_noupdate_and_tolerates_a_missing_target() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let mut session = session();
    init(
        &mut session,
        &mut tx,
        r#"<odoo>
  <record id="keep" model="test_convert.usered"><field name="name">keep</field></record>
  <record id="gone" model="test_convert.usered"><field name="name">gone</field></record>
  <record id="match_a" model="test_convert.usered"><field name="name">match a</field></record>
  <record id="match_b" model="test_convert.usered"><field name="name">match b</field></record>
</odoo>"#,
    )
    .await
    .unwrap();
    session.take_notices();
    assert_eq!(names_of(&mut tx, USERED).await.len(), 4);

    // upgrade: under noupdate a delete still runs; a missing id is a notice; the search deletes
    load(
        &mut session,
        &mut tx,
        r#"<odoo><data noupdate="1">
  <delete model="test_convert.usered" id="gone"/>
  <delete model="test_convert.usered" id="never_existed"/>
  <delete model="test_convert.usered" search="[('name', 'like', 'match')]"/>
</data></odoo>"#,
        ConvertMode::Update,
    )
    .await
    .unwrap();
    assert_eq!(names_of(&mut tx, USERED).await, [s("keep")]);
    assert_eq!(
        session.take_notices(),
        [DataNotice::new(
            "m",
            "data/f.xml",
            NoticeKind::DeleteTargetMissing {
                xid: "m.never_existed".to_owned()
            }
        )]
    );

    // deleting what is already gone (its record was removed behind the external id) is also a
    // notice, and the external id of a deleted record is not left pointing at nothing by this
    // loader: it reports and goes on
    load(
        &mut session,
        &mut tx,
        r#"<odoo><delete model="test_convert.usered" id="gone"/></odoo>"#,
        ConvertMode::Update,
    )
    .await
    .unwrap();
    assert_eq!(
        session.take_notices(),
        [DataNotice::new(
            "m",
            "data/f.xml",
            NoticeKind::DeleteTargetMissing {
                xid: "m.gone".to_owned()
            }
        )]
    );
    assert_eq!(names_of(&mut tx, USERED).await, [s("keep")]);

    // a search that cannot be evaluated or run fails the load (the recorded divergence)
    let err = load(
        &mut session,
        &mut tx,
        "<odoo><delete model=\"test_convert.usered\" search=\"[('no_such_field', '=', 1)]\"/></odoo>",
        ConvertMode::Update,
    )
    .await
    .unwrap_err();
    assert!(err.orm_kind().is_some(), "{err:?}");
    assert_eq!(names_of(&mut tx, USERED).await, [s("keep")]);
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn delete_refuses_an_external_id_of_another_model_and_deletes_nothing() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let mut session = session();
    init(
        &mut session,
        &mut tx,
        r#"<odoo>
  <record id="rec" model="test_convert.record"><field name="name">r</field></record>
  <record id="u" model="test_convert.usered"><field name="name">u</field></record>
</odoo>"#,
    )
    .await
    .unwrap();
    let err = load(
        &mut session,
        &mut tx,
        "<odoo>\n<delete model=\"test_convert.usered\" id=\"rec\"/>\n</odoo>",
        ConvertMode::Update,
    )
    .await
    .unwrap_err();
    assert!(
        matches!(err.kind(), LoadErrorKind::DeleteWrongModel { .. }),
        "{err:?}"
    );
    assert_eq!(
        err.to_string(),
        "while parsing data/f.xml:2, somewhere inside\n<delete model=\"test_convert.usered\">\n\
         <delete> the external ID \"m.rec\" is a record of model 'test_convert.record', not of \
         'test_convert.usered'"
    );
    assert_eq!(names_of(&mut tx, USERED).await, [s("u")]);
    assert_eq!(names_of(&mut tx, "test_convert.record").await, [s("r")]);
    let env = tx.env();
    let found = lookup(&mut tx, &env, &[XmlId::parse("m.rec").unwrap()])
        .await
        .unwrap();
    assert_eq!(found.len(), 1);
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

// ---- <function> arguments use the value forms of <field> (integration) ----------------

/// Files by `(module, path)`.
struct Files(HashMap<(String, String), Vec<u8>>);

impl AddonFiles for Files {
    fn read<'a>(
        &'a self,
        module: &'a str,
        path: &'a str,
    ) -> BoxFuture<'a, Result<Vec<u8>, FileReadError>> {
        Box::pin(async move {
            self.0
                .get(&(module.to_owned(), path.to_owned()))
                .cloned()
                .ok_or_else(|| FileReadError::NotFound {
                    module: module.to_owned(),
                    path: path.to_owned(),
                })
        })
    }
}

fn session_with_files() -> LoadSession {
    let files = Files(
        [(("m", "data/hello.txt"), "héllo\n".as_bytes())]
            .into_iter()
            .map(|((m, p), c)| ((m.to_owned(), p.to_owned()), c.to_vec()))
            .collect(),
    );
    LoadSession::new(SessionOptions::new("20.0", Arc::new(files)))
}

/// `<value search=…>`, `type="file"`, `file=` and `type="base64"` are arguments of a `<function>`,
/// read by the code a `<field>` uses.
#[tokio::test]
async fn function_values_take_search_file_and_module_path_forms() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let mut session = session_with_files();
    init(
        &mut session,
        &mut tx,
        r#"<odoo>
  <record id="u1" model="test_convert.usered"><field name="name">x</field></record>
  <function model="test_convert.usered" name="typed_args">
    <value search="[('name', '=', 'x')]" model="test_convert.usered"/>
    <value type="file">data/hello.txt</value>
  </function>
  <function model="test_convert.usered" name="typed_args">
    <value search="[('name', '=', 'x')]" model="test_convert.usered" use="id"/>
    <value file="m/data/hello.txt"/>
  </function>
  <function model="test_convert.usered" name="typed_args">
    <value eval="1"/>
    <value name="label" type="base64" file="m/data/hello.txt"/>
  </function>
</odoo>"#,
    )
    .await
    .unwrap();
    let u1 = all(&mut tx, USERED).await.ids()[0].to_string();
    let nil = Cell::Null;
    assert_eq!(
        calls(&mut tx).await,
        [
            logged(
                "typed_args",
                "",
                &format!("{u1},m,data/hello.txt"),
                "",
                nil.clone(),
                nil.clone()
            ),
            logged(
                "typed_args",
                "",
                &format!("{u1},héllo\n"),
                "",
                nil.clone(),
                nil.clone()
            ),
            logged("typed_args", "", "1,aMOpbGxvCg==", "", nil.clone(), nil),
        ]
    );
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

/// The refusals of the `<field>` forms hold inside a `<function>`: a `search=` with no model, a
/// file the addon lacks.
#[tokio::test]
async fn function_values_refuse_what_a_field_form_refuses() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let mut session = session_with_files();
    let err = init(
        &mut session,
        &mut tx,
        "<odoo><function model=\"test_convert.usered\" name=\"typed_args\">\
         <value search=\"[]\"/></function></odoo>",
    )
    .await
    .unwrap_err();
    assert!(
        matches!(err.kind(), LoadErrorKind::SearchNeedsModel),
        "{err:?}"
    );
    assert_eq!(
        err.to_string(),
        format!(
            "while parsing data/f.xml:1, somewhere inside\n{}\n\
             Define an attribute model=\"...\" in your .XML file!",
            err.op().unwrap()
        )
    );
    let err = init(
        &mut session,
        &mut tx,
        "<odoo><function model=\"test_convert.usered\" name=\"typed_args\">\
         <value type=\"file\">static/missing.png</value></function></odoo>",
    )
    .await
    .unwrap_err();
    assert!(matches!(err.kind(), LoadErrorKind::File(_)), "{err:?}");
    assert_eq!(
        err.to_string(),
        format!(
            "while parsing data/f.xml:1, somewhere inside\n{}\n\
             File not found: m/static/missing.png",
            err.op().unwrap()
        )
    );
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

// ---- a deleted record's external id leaves the session (integration) ------------------

/// A `<delete>` removes the external id of its record from the session's cache, and a later
/// `ref` to it in the same session fails like any unknown external id.
#[tokio::test]
async fn a_ref_to_a_record_deleted_in_the_session_is_refused() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let mut session = session();
    // the record, then a ref to it (which puts its external id in the session's cache)
    init(
        &mut session,
        &mut tx,
        r#"<odoo>
  <record id="victim" model="test_convert.usered"><field name="name">victim</field></record>
  <record id="holder" model="test_convert.record"><field name="name" eval="str(ref('victim'))"/></record>
</odoo>"#,
    )
    .await
    .unwrap();
    let xid = XmlId::parse("m.victim").unwrap();
    assert!(
        session.cache().get(&xid).is_some(),
        "the fixture must have cached the id"
    );

    // the delete and the later ref are in one file
    let err = init(
        &mut session,
        &mut tx,
        r#"<odoo>
  <delete model="test_convert.usered" id="victim"/>
  <record id="late" model="test_convert.record"><field name="usered_id" ref="victim"/></record>
</odoo>"#,
    )
    .await
    .unwrap_err();
    assert!(
        matches!(err.kind(), LoadErrorKind::ExternalIdNotFound { xid } if xid == "m.victim"),
        "{err:?}"
    );
    assert!(session.cache().get(&xid).is_none());
    let env = tx.env();
    assert!(
        lookup(&mut tx, &env, &[xid]).await.unwrap().is_empty(),
        "the unlink hook removes the external id row"
    );
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

// ---- a CSV row naming a missing external id of another module (F6) ----------------------------

/// a CSV row whose external id belongs to another module and does not
/// exist creates the record, and the load says so with a notice; an id of the file's own module
/// or of another module that exists does not.
#[tokio::test]
async fn a_csv_row_with_a_missing_foreign_external_id_is_noticed() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let mut session = session();
    init(
        &mut session,
        &mut tx,
        r#"<odoo><record id="exists" model="test_convert.record"><field name="name">e</field></record></odoo>"#,
    )
    .await
    .unwrap();
    session.take_notices();
    let content = "id,name\nother.fresh,A\nm.mine,B\nm.exists,C\n";
    let erp_data::CsvImportResult::Loaded(import) = erp_data::parse_csv(
        content,
        "data/test_convert.record.csv",
        "m",
        ConvertMode::Init,
    )
    .unwrap() else {
        panic!("the file has rows");
    };
    let file = FileContext::new("m", "data/test_convert.record.csv", ConvertMode::Init);
    let mut ops = vec![Ok(import.into_operation(false))].into_iter();
    session.load_file(&mut tx, &file, &mut ops).await.unwrap();
    assert_eq!(
        session.take_notices(),
        [DataNotice::new(
            "m",
            "data/test_convert.record.csv",
            NoticeKind::ForeignRecordCreated {
                xid: "other.fresh".to_owned()
            }
        )]
    );
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}
