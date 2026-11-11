#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

#[path = "support/currency_fixture.rs"]
mod currency_fixture;

use std::sync::Arc;

use erp_data::{
    ExportRights, ImpexExt, ImportMessage, LoadResult, MessageType, RowSpan, SuperuserOnly,
};
use erp_orm::{BoxFuture, Env};
use erp_sdk::{Domain, ErrorKind, Records, Result, Transaction, Value};
use erp_test_models::{base_users, test_impex};
use erp_testkit::{Harness, assert_query_count};

async fn harness() -> Harness {
    Harness::builder([
        &base_users::ADDON,
        &test_impex::ADDON,
        &erp_data::model_data::ADDON,
    ])
    .with_static_decls(crate::currency_fixture::decls())
    .with_decls([erp_data::ir_model_data_decl()])
    .build()
    .await
    .expect("harness")
}

fn rows(data: &[&[&str]]) -> Vec<Vec<String>> {
    data.iter()
        .map(|r| r.iter().map(|c| (*c).to_owned()).collect())
        .collect()
}

fn model(tx: &Transaction, name: &str) -> Records {
    tx.env()
        .records(name)
        .unwrap()
        .with_context([("import_file", Value::Bool(true))])
}

async fn import(tx: &mut Transaction, name: &str, fields: &[&str], data: &[&[&str]]) -> LoadResult {
    let records = model(tx, name);
    records.load(tx, fields, &rows(data)).await.unwrap()
}

fn msg(text: &str, row: usize, field: Option<&str>) -> ImportMessage {
    let m = ImportMessage::new(MessageType::Error, text)
        .with_rows(RowSpan::new(row, row))
        .with_record(row);
    match field {
        Some(f) => m.with_field(f),
        None => m,
    }
}

/// [`msg`] as a warning.
fn warn(text: &str, row: usize, field: Option<&str>) -> ImportMessage {
    let m = ImportMessage::new(MessageType::Warning, text)
        .with_rows(RowSpan::new(row, row))
        .with_record(row);
    match field {
        Some(f) => m.with_field(f),
        None => m,
    }
}

fn action(res_model: &str, domain_model: Option<&str>) -> Value {
    let mut map = erp_sdk::ValueMap::new();
    let s = Value::str;
    map.insert("name".into(), s("Possible Values"));
    map.insert("type".into(), s("ir.actions.act_window"));
    map.insert("target".into(), s("new"));
    map.insert("view_mode".into(), s("list,form"));
    map.insert(
        "views".into(),
        Value::list([
            Value::list([Value::False, s("list")]),
            Value::list([Value::False, s("form")]),
        ]),
    );
    let mut context = erp_sdk::ValueMap::new();
    context.insert("create".into(), Value::Bool(false));
    map.insert("context".into(), Value::Map(Arc::new(context)));
    map.insert("help".into(), s("See all possible values"));
    map.insert("res_model".into(), s(res_model));
    if let Some(m) = domain_model {
        map.insert(
            "domain".into(),
            Value::list([Value::list([s("model"), s("="), s(m)])]),
        );
    }
    Value::Map(Arc::new(map))
}

async fn all(tx: &mut Transaction, name: &str) -> Records {
    tx.env()
        .records(name)
        .unwrap()
        .search(tx, &Domain::TRUE)
        .await
        .unwrap()
}

/// Integer-like values (`Integer`, `Many2one` ids) of a field over all records, in id order.
async fn ints(tx: &mut Transaction, name: &str, field: &str) -> Vec<Option<i64>> {
    let records = all(tx, name).await;
    records
        .cells(tx, field)
        .await
        .unwrap()
        .into_iter()
        .map(|c| match c {
            erp_sdk::Cell::Int(i) => Some(i64::from(i)),
            erp_sdk::Cell::Id(i) => Some(i),
            erp_sdk::Cell::Null => None,
            other => panic!("not an integer cell: {other:?}"),
        })
        .collect()
}

async fn create(tx: &mut Transaction, name: &str, value: Value) -> i64 {
    let records = tx.env().records(name).unwrap();
    let mut vals = std::collections::BTreeMap::new();
    vals.insert("value".to_owned(), value);
    let created = records.create_values(tx, &[vals]).await.unwrap();
    created.ids()[0]
}

/// External ids of a record as `module.name` (or `name` without module).
async fn xids_of(tx: &mut Transaction, model: &str, id: i64) -> Vec<String> {
    tx.flush().await.unwrap();
    tx.query(
        "SELECT module, name FROM ir_model_data WHERE model = $1 AND res_id = $2 ORDER BY id",
        &[&model, &id],
    )
    .await
    .unwrap()
    .iter()
    .map(|r| {
        let module: String = r.get(0);
        let name: String = r.get(1);
        if module.is_empty() {
            name
        } else {
            format!("{module}.{name}")
        }
    })
    .collect()
}

async fn add_xid(tx: &mut Transaction, module: &str, name: &str, model: &str, id: i64) {
    let imd = tx.env().records("ir.model.data").unwrap();
    let mut vals = std::collections::BTreeMap::new();
    vals.insert("module".to_owned(), Value::str(module));
    vals.insert("name".to_owned(), Value::str(name));
    vals.insert("model".to_owned(), Value::str(model));
    vals.insert("res_id".to_owned(), Value::Int(id));
    imd.create_values(tx, &[vals]).await.unwrap();
}

#[tokio::test]
async fn spec_data_032_load_result_shape_and_xid_prefix() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();

    // test_create_with_xid
    let result = import(
        &mut tx,
        "export.integer",
        &["id", "value"],
        &[&["somexmlid", "42"]],
    )
    .await;
    assert_eq!(result.messages, vec![]);
    assert_eq!(result.nextrow, 0);
    let ids = result.ids.clone().expect("ids");
    assert_eq!(ids.len(), 1);
    assert_eq!(
        xids_of(&mut tx, "export.integer", ids[0]).await,
        vec!["__import__.somexmlid".to_owned()]
    );

    // test_update_with_xid: the same external id updates the same record
    let again = import(
        &mut tx,
        "export.integer",
        &["id", "value"],
        &[&["somexmlid", "1234567"]],
    )
    .await;
    assert_eq!(again.messages, vec![]);
    assert_eq!(again.ids, Some(ids.clone()));
    assert_eq!(
        ints(&mut tx, "export.integer", "value").await,
        vec![Some(1_234_567)]
    );

    // a qualified external id keeps its module; `module` in the context prefixes the others
    let records = model(&tx, "export.integer").with_context([("module", Value::str("mymod"))]);
    let other = records
        .load(
            &mut tx,
            &["id", "value"],
            &rows(&[&["x1", "1"], &["elsewhere.x2", "2"]]),
        )
        .await
        .unwrap();
    assert_eq!(other.messages, vec![]);
    let other_ids = other.ids.expect("ids");
    assert_eq!(
        xids_of(&mut tx, "export.integer", other_ids[0]).await,
        vec!["mymod.x1"]
    );
    assert_eq!(
        xids_of(&mut tx, "export.integer", other_ids[1]).await,
        vec!["elsewhere.x2"]
    );
    // a file import refuses the prefix of an installed module
    let refused = import(
        &mut tx,
        "export.integer",
        &["id", "value"],
        &[&["base.x3", "3"]],
    )
    .await;
    assert_eq!(refused.ids, None);
    assert_eq!(
        refused.messages,
        vec![msg(
            "The record base.x3 has the module prefix base. This is the part before the '.' in \
             the external id. Because the prefix refers to an existing module, the record would \
             be deleted when the module is upgraded. Use either no prefix and no dot or a prefix \
             that isn't an existing module. For example, __import__, resulting in the external \
             id __import__.x3.",
            0,
            None
        )]
    );
    // a mixed batch: the global message first, then the failing record
    let mixed = import(
        &mut tx,
        "export.integer",
        &["id", "value"],
        &[&["fine", "4"], &["base.x5", "5"]],
    )
    .await;
    assert_eq!(mixed.ids, None);
    let text = |name: &str| {
        format!(
            "The record base.{name} has the module prefix base. This is the part before the '.' \
             in the external id. Because the prefix refers to an existing module, the record \
             would be deleted when the module is upgraded. Use either no prefix and no dot or a \
             prefix that isn't an existing module. For example, __import__, resulting in the \
             external id __import__.{name}."
        )
    };
    assert_eq!(
        mixed.messages,
        vec![msg(&text("x5"), 0, None), msg(&text("x5"), 1, None)]
    );
    assert_eq!(all(&mut tx, "export.integer").await.len(), 3);

    // test_empty: `{'ids': [], 'messages': [], 'nextrow': 0}`
    let empty = import(&mut tx, "export.boolean", &["value"], &[]).await;
    assert_eq!(
        (empty.ids.clone(), empty.messages.clone(), empty.nextrow),
        (Some(vec![]), vec![], 0)
    );
    let mut shape = erp_sdk::ValueMap::new();
    shape.insert("ids".into(), Value::list([]));
    shape.insert("messages".into(), Value::list([]));
    shape.insert("nextrow".into(), Value::Int(0));
    assert_eq!(empty.to_value(), Value::Map(Arc::new(shape)));
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_data_033_load_with_database_id() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let id = create(&mut tx, "export.integer", Value::Int(36)).await;

    // test_update_with_id
    let result = import(
        &mut tx,
        "export.integer",
        &[".id", "value"],
        &[&[&id.to_string(), "42"]],
    )
    .await;
    assert_eq!(result.messages, vec![]);
    assert_eq!(result.ids, Some(vec![id]));
    assert_eq!(
        ints(&mut tx, "export.integer", "value").await,
        vec![Some(42)]
    );

    // test_create_with_id: an unknown id is an error and nothing is created
    let result = import(
        &mut tx,
        "export.integer",
        &[".id", "value"],
        &[&["424242", "36"]],
    )
    .await;
    assert_eq!(result.ids, None);
    assert_eq!(
        result.messages,
        vec![msg("Unknown database identifier '424242'", 0, Some(".id"))]
    );
    assert_eq!(
        ints(&mut tx, "export.integer", "value").await,
        vec![Some(42)]
    );
    let shape = result.to_value();
    let Value::Map(map) = &shape else {
        panic!("not a map")
    };
    assert_eq!(map.get("ids"), Some(&Value::False));
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_data_034_boolean_import_values() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();

    // test_falses (English words)
    let falses: &[&[&str]] = &[&["0"], &["no"], &["false"], &["FALSE"], &[""]];
    let result = import(&mut tx, "export.boolean", &["value"], falses).await;
    assert_eq!(result.messages, vec![]);
    assert_eq!(result.ids.as_ref().map(Vec::len), Some(falses.len()));
    let values = all(&mut tx, "export.boolean")
        .await
        .values(&mut tx, "value")
        .await
        .unwrap();
    assert_eq!(values, vec![Value::Bool(false); falses.len()]);

    // test_trues: only true/yes/1 are accepted
    let trues = [
        "None", "nil", "()", "f", "#f", "VRAI", "ok", "true", "yes", "1",
    ];
    let data: Vec<Vec<String>> = trues.iter().map(|v| vec![(*v).to_owned()]).collect();
    let records = model(&tx, "export.boolean");
    let result = records.load(&mut tx, &["value"], &data).await.unwrap();
    assert_eq!(result.ids, None);
    let expected: Vec<ImportMessage> = trues
        .iter()
        .enumerate()
        .filter(|(_, v)| !["true", "yes", "1"].contains(v))
        .map(|(i, v)| {
            msg(
                &format!("Unknown value '{v}' for boolean field 'Value'"),
                i,
                Some("value"),
            )
            .with_moreinfo(Value::str("Use '1' for yes and '0' for no"))
            .with_field_name("Value")
        })
        .collect();
    assert_eq!(result.messages, expected);
    assert_eq!(all(&mut tx, "export.boolean").await.len(), falses.len());

    let result = import(
        &mut tx,
        "export.boolean",
        &["value"],
        &[&["1"], &["yes"], &["true"]],
    )
    .await;
    assert_eq!(result.messages, vec![]);
    assert_eq!(result.ids.as_ref().map(Vec::len), Some(3));

    // without installed languages a translated word is unknown
    let result = import(&mut tx, "export.boolean", &["value"], &[&["faux"]]).await;
    assert_eq!(result.ids, None);
    assert_eq!(
        result.messages,
        vec![
            msg(
                "Unknown value 'faux' for boolean field 'Value'",
                0,
                Some("value")
            )
            .with_moreinfo(Value::str("Use '1' for yes and '0' for no"))
            .with_field_name("Value")
        ]
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_data_035_numeric_import_errors() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();

    // test_nonsense
    let result = import(&mut tx, "export.integer", &["value"], &[&["zorglub"]]).await;
    assert_eq!(result.ids, None);
    assert_eq!(
        result.messages,
        vec![
            msg(
                "'zorglub' does not seem to be an integer for field 'Value'",
                0,
                Some("value")
            )
            .with_field_name("Value")
        ]
    );

    // test_out_of_range: the database's message, without `field`
    for big in [(1_i64 << 31).to_string(), (-(1_i64 << 32)).to_string()] {
        let result = import(&mut tx, "export.integer", &["value"], &[&[big.as_str()]]).await;
        assert_eq!(result.ids, None);
        assert_eq!(
            result.messages,
            vec![msg("integer out of range\n", 0, None)]
        );
    }

    // test_empty, test_zero, the int4 bounds
    let result = import(
        &mut tx,
        "export.integer",
        &["value"],
        &[&[""], &["-0"], &["2147483647"], &["-2147483648"]],
    )
    .await;
    assert_eq!(result.messages, vec![]);
    assert_eq!(result.ids.as_ref().map(Vec::len), Some(4));
    assert_eq!(
        ints(&mut tx, "export.integer", "value").await,
        vec![Some(0), Some(0), Some(2_147_483_647), Some(-2_147_483_648)]
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_data_036_many2one_import_references() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let r1 = create(&mut tx, "export.integer", Value::Int(42)).await;
    let r2 = create(&mut tx, "export.integer", Value::Int(36)).await;

    // test_by_name
    let result = import(
        &mut tx,
        "export.many2one",
        &["value"],
        &[&["42"], &["42"], &["36"]],
    )
    .await;
    assert_eq!(result.messages, vec![]);
    assert_eq!(result.ids.as_ref().map(Vec::len), Some(3));
    assert_eq!(
        ints(&mut tx, "export.many2one", "value").await,
        vec![Some(r1), Some(r1), Some(r2)]
    );
    let base = all(&mut tx, "export.many2one").await.len();

    // test_by_xid
    add_xid(&mut tx, "__test__", "r1", "export.integer", r1).await;
    let result = import(
        &mut tx,
        "export.many2one",
        &["value/id"],
        &[&["__test__.r1"]],
    )
    .await;
    assert_eq!(result.messages, vec![]);
    // test_by_id
    let r2_text = r2.to_string();
    let result2 = import(
        &mut tx,
        "export.many2one",
        &["value/.id"],
        &[&[r2_text.as_str()]],
    )
    .await;
    assert_eq!(result2.messages, vec![]);
    let values = ints(&mut tx, "export.many2one", "value").await;
    assert_eq!(&values[base..], &[Some(r1), Some(r2)]);

    // test_fail_by_implicit_id: raw ids are not names
    let (t1, t2) = (r1.to_string(), r2.to_string());
    let result = import(
        &mut tx,
        "export.many2one",
        &["value"],
        &[&[t1.as_str()], &[t2.as_str()]],
    )
    .await;
    assert_eq!(result.ids, None);
    let expected: Vec<ImportMessage> = [&t1, &t2]
        .iter()
        .enumerate()
        .map(|(i, id)| {
            msg(
                &format!("No matching record found for name '{id}' in field 'Value'"),
                i,
                Some("value"),
            )
            .with_moreinfo(action("export.integer", None))
            .with_field_name("Value")
            .with_field_path(vec!["value".to_owned()])
            .with_field_type("name")
            .with_value((*id).clone())
        })
        .collect();
    assert_eq!(result.messages, expected);

    // test_fail_noids: unknown name, external id and database id
    let result = import(
        &mut tx,
        "export.many2one",
        &["value"],
        &[&["nameisnoexist:3"]],
    )
    .await;
    assert_eq!(result.ids, None);
    assert_eq!(
        result.messages,
        vec![
            msg(
                "No matching record found for name 'nameisnoexist:3' in field 'Value'",
                0,
                Some("value")
            )
            .with_moreinfo(action("export.integer", None))
            .with_field_name("Value")
            .with_field_path(vec!["value".to_owned()])
            .with_field_type("name")
            .with_value("nameisnoexist:3")
        ]
    );
    let result = import(&mut tx, "export.many2one", &["value/id"], &[&["noxidhere"]]).await;
    assert_eq!(
        result.messages,
        vec![
            msg(
                "No matching record found for external id 'noxidhere' in field 'Value'",
                0,
                Some("value")
            )
            .with_moreinfo(action("ir.model.data", Some("export.integer")))
            .with_field_name("Value")
            .with_field_path(vec!["value".to_owned(), "id".to_owned()])
            .with_field_type("external id")
            .with_value("noxidhere")
        ]
    );
    let result = import(&mut tx, "export.many2one", &["value/.id"], &[&["66666"]]).await;
    assert_eq!(
        result.messages,
        vec![
            msg(
                "No matching record found for database id '66666' in field 'Value'",
                0,
                Some("value")
            )
            .with_moreinfo(action("ir.model.data", Some("export.integer")))
            .with_field_name("Value")
            .with_field_path(vec!["value".to_owned(), ".id".to_owned()])
            .with_field_type("database id")
            .with_value("66666")
        ]
    );

    // test_fail_id_mistype
    let result = import(&mut tx, "export.many2one", &["value/.id"], &[&["foo"]]).await;
    assert_eq!(
        result.messages,
        vec![
            msg(
                "Invalid database id 'foo' for the field 'Value'",
                0,
                Some("value")
            )
            .with_moreinfo(action("ir.model.data", Some("export.integer")))
            .with_field_name("Value")
            .with_field_path(vec!["value".to_owned(), ".id".to_owned()])
        ]
    );

    // test_sub_field
    let result = import(&mut tx, "export.many2one", &["value/value"], &[&["42"]]).await;
    assert_eq!(result.ids, None);
    assert_eq!(
        result.messages,
        vec![msg(
            "Can not create Many-To-One records indirectly, import the field separately",
            0,
            Some("value")
        )]
    );

    // test_fail_multiple
    let result = import(
        &mut tx,
        "export.many2one",
        &["value", "value/id"],
        &[&["somename", "somexid"]],
    )
    .await;
    assert_eq!(
        result.messages,
        vec![msg(
            "Ambiguous specification for field 'Value', only provide one of name, external id \
             or database id",
            0,
            Some("value")
        )]
    );

    // test_by_names: the first match wins, with a warning
    let r3 = create(&mut tx, "export.integer", Value::Int(42)).await;
    assert!(r3 > r1);
    let result = import(&mut tx, "export.many2one", &["value"], &[&["42"]]).await;
    assert_eq!(
        result.messages,
        vec![warn(
            "Found multiple matches for value \"42\" in field \"Value\" (2 matches)",
            0,
            Some("value")
        )]
    );
    let id = result.ids.expect("a warning keeps the import")[0];
    let created = tx
        .env()
        .records("export.many2one")
        .unwrap()
        .browse([id])
        .unwrap();
    assert_eq!(
        created.cell(&mut tx, "value").await.unwrap(),
        erp_sdk::Cell::Id(r1)
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_data_037_many2one_by_name_query_count() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    create(&mut tx, "export.integer", Value::Int(42)).await;
    create(&mut tx, "export.integer", Value::Int(36)).await;
    tx.flush().await.unwrap();
    let data: &[&[&str]] = &[&["42"], &["42"], &["36"]];
    // preheat (statement shapes, prepared statements)
    import(&mut tx, "export.many2one", &["value"], data).await;

    tx.capture_statements(true);
    // Here: savepoint, one name search per distinct name (2), savepoint, insert, 2 releases.
    let result = assert_query_count!(tx, >= 5, <= 9, {
        import(&mut tx, "export.many2one", &["value"], data).await
    });
    assert_eq!(result.messages, vec![]);
    assert_eq!(result.ids.as_ref().map(Vec::len), Some(3));
    let inserts = tx
        .captured()
        .iter()
        .filter(|s| s.contains("INSERT INTO \"export_many2one\""))
        .count();
    assert_eq!(inserts, 1, "{:#?}", tx.captured());
    let searches = tx
        .captured()
        .iter()
        .filter(|s| s.contains("\"export_integer\""))
        .count();
    assert_eq!(searches, 2, "{:#?}", tx.captured());
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_data_038_one2many_multi_line_import() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();

    // test_multi_subfields: the following rows only fill one2many columns
    let result = import(
        &mut tx,
        "export.one2many",
        &["value/str", "const", "value/value"],
        &[
            &["this", "5", "63"],
            &["is", "", "64"],
            &["the", "", "65"],
            &["rhythm", "", "66"],
        ],
    )
    .await;
    assert_eq!(result.messages, vec![]);
    assert_eq!(result.ids.as_ref().map(Vec::len), Some(1));
    assert_eq!(
        ints(&mut tx, "export.one2many", "const").await,
        vec![Some(5)]
    );
    let lines = all(&mut tx, "export.one2many.child").await;
    assert_eq!(
        lines.values(&mut tx, "value").await.unwrap(),
        [63, 64, 65, 66].map(Value::Int).to_vec()
    );
    assert_eq!(
        lines.values(&mut tx, "str").await.unwrap(),
        ["this", "is", "the", "rhythm"].map(Value::str).to_vec()
    );
    let parent = result.ids.unwrap()[0];
    assert_eq!(
        ints(&mut tx, "export.one2many.child", "parent_id").await,
        vec![Some(parent); 4]
    );

    // test_o2m_multiple::test_multi: two one2many fields over different rows
    let result = import(
        &mut tx,
        "export.one2many.multiple",
        &["const", "child1/value", "child2/value"],
        &[
            &["5", "11", "21"],
            &["", "12", ""],
            &["", "13", ""],
            &["", "14", ""],
            &["", "", "22"],
            &["", "", "23"],
        ],
    )
    .await;
    assert_eq!(result.messages, vec![]);
    assert_eq!(result.ids.as_ref().map(Vec::len), Some(1));
    assert_eq!(
        ints(&mut tx, "export.one2many.child.1", "value").await,
        [11, 12, 13, 14].map(Some).to_vec()
    );
    assert_eq!(
        ints(&mut tx, "export.one2many.child.2", "value").await,
        [21, 22, 23].map(Some).to_vec()
    );

    // two parents; an error on the first one's second line spans rows 0..1
    let result = import(
        &mut tx,
        "export.one2many",
        &["const", "value/value"],
        &[&["7", "70"], &["", "x"], &["8", "80"]],
    )
    .await;
    assert_eq!(result.ids, None);
    assert_eq!(
        result.messages,
        vec![
            msg(
                "'x' does not seem to be an integer for field 'Value/Value'",
                0,
                Some("value")
            )
            .with_rows(RowSpan::new(0, 1))
            .with_field_name("Value")
        ]
    );
    let result = import(
        &mut tx,
        "export.one2many",
        &["const", "value/value"],
        &[&["7", "70"], &["", "71"], &["8", "80"]],
    )
    .await;
    assert_eq!(result.messages, vec![]);
    let two = result.ids.unwrap();
    assert_eq!(two.len(), 2);
    for (parent, expected) in [(two[0], vec![70, 71]), (two[1], vec![80])] {
        let children = tx
            .env()
            .records("export.one2many.child")
            .unwrap()
            .search(
                &mut tx,
                &Domain::condition("parent_id", erp_sdk::Operator::Eq, parent).unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            children.values(&mut tx, "value").await.unwrap(),
            expected.into_iter().map(Value::Int).collect::<Vec<_>>()
        );
    }
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_data_039_import_error_throttling_and_rollback() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let data: Vec<Vec<String>> = (0..100).map(|n| vec![n.to_string()]).collect();
    let records = model(&tx, "export.string.required");
    let result = records.load(&mut tx, &["const"], &data).await.unwrap();
    assert_eq!(result.ids, None);
    assert_eq!(result.messages.len(), 11, "{:#?}", result.messages);
    for (i, m) in result.messages[..10].iter().enumerate() {
        assert_eq!(m.kind, MessageType::Error);
        assert_eq!(
            m.message,
            "Missing required field 'Value' (value) for model 'Export: string.required' \
             (export.string.required)"
        );
        assert_eq!(m.rows, Some(RowSpan::new(i, i)));
        assert_eq!(m.record, Some(i));
        assert_eq!(m.field.as_deref(), Some("value"));
    }
    let last = &result.messages[10];
    assert_eq!(last.kind, MessageType::Warning);
    assert_eq!(
        last.message,
        "Found more than 10 errors and more than one error per 10 records, interrupted to \
         avoid showing too many errors."
    );
    assert_eq!(
        (last.rows, last.record, last.field.as_deref()),
        (None, None, None)
    );
    assert_eq!(all(&mut tx, "export.string.required").await.len(), 0);
    // the transaction is still usable and committable
    assert!(!tx.is_poisoned());
    tx.commit().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

/// An `__export__.<table>_<id>_<8 hex>` external id.
fn is_export_xid(xid: &str, table: &str, id: i64) -> bool {
    let prefix = format!("__export__.{table}_{id}_");
    xid.strip_prefix(&prefix)
        .is_some_and(|h| h.len() == 8 && h.chars().all(|c| c.is_ascii_hexdigit()))
}

fn text(v: &Value) -> &str {
    match v {
        Value::Str(s) => s,
        other => panic!("not a string: {other:?}"),
    }
}

#[tokio::test]
async fn spec_data_040_export_basics() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let target = create(&mut tx, "export.integer", Value::Int(42)).await;
    let tag1 = create(&mut tx, "export.many2many.other", Value::Int(1)).await;
    let tag2 = create(&mut tx, "export.many2many.other", Value::Int(2)).await;
    let mixed = tx.env().records("export.mixed").unwrap();
    let mut vals = std::collections::BTreeMap::new();
    vals.insert("value".to_owned(), Value::Int(0));
    vals.insert("flag".to_owned(), Value::Bool(false));
    vals.insert("m2o".to_owned(), Value::Int(target));
    vals.insert(
        "m2m".to_owned(),
        Value::list([Value::list([
            Value::Int(6),
            Value::Int(0),
            Value::list([Value::Int(tag1), Value::Int(tag2)]),
        ])]),
    );
    let record = mixed.create_values(&mut tx, &[vals]).await.unwrap();
    let id = record.ids()[0];
    tx.invalidate_all().await.unwrap();

    let fields = ["id", "value", "flag", "m2o", "m2o/id", "m2m/id"];
    let first = record
        .export_data(&mut tx, &fields, &SuperuserOnly)
        .await
        .unwrap();
    assert_eq!(first.datas.len(), 1);
    let line = &first.datas[0];
    assert!(
        is_export_xid(text(&line[0]), "export_mixed", id),
        "{line:?}"
    );
    assert_eq!(line[1], Value::Int(0), "0 stays 0");
    assert_eq!(line[2], Value::Bool(false));
    assert_eq!(
        line[3],
        Value::str("42"),
        "many2one exports its display name"
    );
    assert!(
        is_export_xid(text(&line[4]), "export_integer", target),
        "{line:?}"
    );
    let tags: Vec<&str> = text(&line[5]).split(',').collect();
    assert_eq!(tags.len(), 2);
    assert!(
        is_export_xid(tags[0], "export_many2many_other", tag1),
        "{tags:?}"
    );
    assert!(
        is_export_xid(tags[1], "export_many2many_other", tag2),
        "{tags:?}"
    );

    // stable on re-export
    let second = record
        .export_data(&mut tx, &fields, &SuperuserOnly)
        .await
        .unwrap();
    assert_eq!(second, first);
    assert_eq!(xids_of(&mut tx, "export.mixed", id).await.len(), 1);

    // test_xids::test_no_module: an existing external id without module
    let flagged = create(&mut tx, "export.boolean", Value::Bool(true)).await;
    add_xid(&mut tx, "", "x", "export.boolean", flagged).await;
    let boolean = tx
        .env()
        .records("export.boolean")
        .unwrap()
        .browse([flagged])
        .unwrap();
    let result = boolean
        .export_data(&mut tx, &["id", "value"], &SuperuserOnly)
        .await
        .unwrap();
    assert_eq!(result.datas, vec![vec![Value::str("x"), Value::Bool(true)]]);

    // one2many lines: the first merges into the record's line, the others follow
    let parent = import(
        &mut tx,
        "export.one2many",
        &["const", "value/value"],
        &[&["5", "63"], &["", "64"]],
    )
    .await
    .ids
    .unwrap();
    let parents = tx
        .env()
        .records("export.one2many")
        .unwrap()
        .browse(parent)
        .unwrap();
    let result = parents
        .export_data(&mut tx, &["const", "value/value"], &SuperuserOnly)
        .await
        .unwrap();
    assert_eq!(
        result.datas,
        vec![
            vec![Value::Int(5), Value::Int(63)],
            vec![Value::str(""), Value::Int(64)]
        ]
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

/// Users allowed to export, standing in for `base.group_allow_export` members.
struct AllowExport(Vec<i64>);

impl ExportRights for AllowExport {
    fn may_export<'a>(
        &'a self,
        _tx: &'a mut Transaction,
        env: &'a Env,
    ) -> BoxFuture<'a, Result<bool>> {
        let allowed = self.0.contains(&env.uid());
        Box::pin(async move { Ok(allowed) })
    }
}

#[tokio::test]
async fn spec_data_041_export_rights_and_translations() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let id = create(&mut tx, "export.integer", Value::Int(42)).await;
    let as_user = tx
        .env()
        .records("export.integer")
        .unwrap()
        .browse([id])
        .unwrap()
        .with_user(2);

    let err = as_user
        .export_data(&mut tx, &["value"], &SuperuserOnly)
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::UserError);
    assert_eq!(
        err.message(),
        "You don't have the rights to export data. Please contact an Administrator."
    );
    let err = as_user
        .export_data(&mut tx, &["value"], &AllowExport(vec![3]))
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::UserError);
    assert_eq!(
        err.message(),
        "You don't have the rights to export data. Please contact an Administrator."
    );

    let allowed = as_user
        .export_data(&mut tx, &["value"], &AllowExport(vec![2]))
        .await
        .unwrap();
    assert_eq!(allowed.datas, vec![vec![Value::Int(42)]]);

    // superuser mode needs no group
    let admin = as_user.sudo(true);
    let result = admin
        .export_data(
            &mut tx,
            &["value", "value@fr_FR", "value@en_US"],
            &SuperuserOnly,
        )
        .await
        .unwrap();
    assert_eq!(
        result.datas,
        vec![vec![Value::Int(42), Value::Int(42), Value::Int(42)]]
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

/// API misuse is an error, not a message.
#[tokio::test]
async fn load_and_export_reject_misuse() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let records = model(&tx, "export.integer");

    let err = records
        .load(&mut tx, &["nope"], &rows(&[&["1"]]))
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Programming);
    assert_eq!(err.message(), "Invalid field name 'nope'");
    // checked even without data, and below relational fields
    let m2o = model(&tx, "export.many2one");
    let err = m2o.load(&mut tx, &["value/nope"], &[]).await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Programming);
    assert_eq!(err.message(), "Invalid field name 'nope'");
    // sub-paths only below relational fields
    for (column, first) in [("value/id", "value"), ("id/x", "id"), (".id/x", ".id")] {
        let err = records.load(&mut tx, &[column], &[]).await.unwrap_err();
        assert_eq!(err.kind(), ErrorKind::Programming);
        assert_eq!(
            err.message(),
            format!(
                "Invalid field name '{column}': '{first}' of model 'export.integer' is not a \
                 relational field and has no sub-fields"
            )
        );
    }
    let err = m2o
        .load(&mut tx, &["value/value/id"], &[])
        .await
        .unwrap_err();
    assert_eq!(
        err.message(),
        "Invalid field name 'value/id': 'value' of model 'export.integer' is not a relational \
         field and has no sub-fields"
    );
    let bad_mode = records.with_context([("mode", Value::str("bogus"))]);
    let err = bad_mode.load(&mut tx, &["value"], &[]).await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Programming);
    assert_eq!(
        err.message(),
        "context key 'mode' must be 'init' or 'update', got 'bogus'"
    );
    let too_many: Vec<Vec<String>> = vec![Vec::new(); erp_data::MAX_ROWS + 1];
    let err = records
        .load(&mut tx, &["value"], &too_many)
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Programming);
    assert_eq!(
        err.message(),
        "load received 1000001 rows; at most 1000000 are accepted per call"
    );

    let wizard =
        records.with_context([("import_skip_records", Value::list([Value::str("value")]))]);
    let err = wizard
        .load(&mut tx, &["value"], &rows(&[&["1"]]))
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Programming);
    assert_eq!(
        err.message(),
        "context key 'import_skip_records' is not supported by load until M4 (the web \
         client's import wizard)"
    );

    let bad = records.with_context([("module", Value::Int(3))]);
    let err = bad
        .load(&mut tx, &["value"], &rows(&[&["1"]]))
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Programming);
    assert_eq!(
        err.message(),
        "context key 'module' must be a string, got 3"
    );

    let id = create(&mut tx, "export.integer", Value::Int(1)).await;
    let one = tx
        .env()
        .records("export.integer")
        .unwrap()
        .browse([id])
        .unwrap();
    let err = one
        .export_data(&mut tx, &["nope"], &SuperuserOnly)
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Programming);
    assert_eq!(
        err.message(),
        "Invalid field 'nope' on model 'export.integer'"
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

/// Without `ir.model.data` in the registry, external ids fail with the fix in the message.
#[tokio::test]
async fn external_ids_need_ir_model_data() {
    let h = Harness::builder([&base_users::ADDON, &test_impex::ADDON])
        .with_static_decls(crate::currency_fixture::decls())
        .build()
        .await
        .expect("harness");
    let mut tx = h.begin().await.unwrap();
    let records = model(&tx, "export.integer");
    // no external id: works without the model
    let result = records
        .load(&mut tx, &["value"], &rows(&[&["1"]]))
        .await
        .unwrap();
    assert_eq!(result.messages, vec![]);
    let err = records
        .load(&mut tx, &["id", "value"], &rows(&[&["a", "1"]]))
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Programming);
    assert_eq!(
        err.message(),
        "model 'ir.model.data' is not in the registry: add the addon \
         erp_data::model_data::ADDON and the declaration erp_data::IR_MODEL_DATA \
         (erp_data::ir_model_data_decl())"
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn load_resolves_references_to_earlier_rows_of_the_same_import() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let result = import(
        &mut tx,
        "export.self",
        &["id", "value", "parent_id/id"],
        &[&["a", "1", ""], &["b", "2", "a"], &["c", "3", "b"]],
    )
    .await;
    assert_eq!(result.messages, vec![]);
    let ids = result.ids.unwrap();
    assert_eq!(
        ints(&mut tx, "export.self", "parent_id").await,
        vec![None, Some(ids[0]), Some(ids[1])]
    );

    let result = import(
        &mut tx,
        "export.self",
        &["value", "parent_id"],
        &[&["10", ""], &["11", "10"], &["12", "10"]],
    )
    .await;
    assert_eq!(result.messages, vec![]);
    let named = result.ids.unwrap();
    let parents = ints(&mut tx, "export.self", "parent_id").await;
    assert_eq!(&parents[3..], &[None, Some(named[0]), Some(named[0])]);
    drop(tx);
    h.cleanup().await.unwrap();
}

/// Duplicate columns, non-ASCII text and a column deeper than the limit.
#[tokio::test]
async fn load_edge_cases() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    // a column given twice is refused, naming the column
    let result = import(
        &mut tx,
        "export.many2one",
        &["value", "const", "value.id", "value/.id"],
        &[&["1", "2", "3", "4"]],
    )
    .await;
    assert_eq!(result.ids, None);
    assert_eq!(
        result.messages,
        vec![
            ImportMessage::new(
                MessageType::Error,
                "Column 'value/.id' appears more than once"
            )
            .with_field("value")
        ]
    );
    // rows shorter or longer than the header are refused, nothing is written
    let result = import(
        &mut tx,
        "export.integer",
        &["const", "value"],
        &[&["1", "2"], &["3"], &[], &["4", "5", "6"]],
    )
    .await;
    assert_eq!(result.ids, None);
    let row = |i: usize, n: usize| {
        ImportMessage::new(
            MessageType::Error,
            format!("This row has {n} cells but there are 2 columns"),
        )
        .with_rows(RowSpan::new(i, i))
    };
    assert_eq!(result.messages, vec![row(1, 1), row(2, 0), row(3, 3)]);
    assert_eq!(all(&mut tx, "export.integer").await.len(), 0);

    let result = import(
        &mut tx,
        "export.one2many",
        &["const", "value/str", "value/value"],
        &[&["1", "ложь", "1"], &["", "日本\r\n", "2"]],
    )
    .await;
    assert_eq!(result.messages, vec![]);
    let lines = all(&mut tx, "export.one2many.child").await;
    assert_eq!(
        lines.values(&mut tx, "str").await.unwrap(),
        vec![Value::str("ложь"), Value::str("日本\r\n")]
    );
    // a non-ASCII name that matches nothing is reported verbatim
    let result = import(&mut tx, "export.many2one", &["value"], &[&["é"]]).await;
    assert_eq!(
        result.messages,
        vec![
            msg(
                "No matching record found for name 'é' in field 'Value'",
                0,
                Some("value")
            )
            .with_moreinfo(action("export.integer", None))
            .with_field_name("Value")
            .with_field_path(["value"])
            .with_field_type("name")
            .with_value("é")
        ]
    );

    let deep = vec!["value"; 17].join("/");
    let records = model(&tx, "export.many2one");
    let err = records
        .load(&mut tx, &[deep.as_str()], &[])
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Programming);
    assert_eq!(
        err.message(),
        format!("column '{deep}' is nested 17 levels deep; at most 16 are accepted")
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn load_scalar_coercions() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let target = create(&mut tx, "export.integer", Value::Int(42)).await;
    let reference = format!("export.integer,{target}");
    let fields = [
        "selection",
        "date",
        "moment",
        "float",
        "decimal",
        "amount",
        "json",
        "reference",
    ];
    let result = import(
        &mut tx,
        "export.typed",
        &fields,
        &[&[
            "bar",
            "2012-02-03",
            "2012-02-03 10:20:30",
            " 1.5 ",
            "1.2345",
            "1e2",
            "{\"a\": [1, 2]}",
            reference.as_str(),
        ]],
    )
    .await;
    assert_eq!(result.messages, vec![]);
    let id = result.ids.unwrap()[0];
    let record = tx
        .env()
        .records("export.typed")
        .unwrap()
        .browse([id])
        .unwrap();
    let read = record.read(&mut tx, &fields).await.unwrap();
    let row = &read[0];
    assert_eq!(
        row["selection"],
        Value::str("2"),
        "matched by label, any case"
    );
    assert_eq!(
        row["date"],
        Value::Date(erp_sdk::Date::parse_odoo("2012-02-03").unwrap())
    );
    assert_eq!(
        row["moment"],
        Value::DateTime(erp_sdk::DateTime::parse_odoo("2012-02-03 10:20:30").unwrap())
    );
    assert_eq!(row["float"], Value::Float(1.5));
    assert_eq!(
        row["decimal"],
        Value::Decimal(erp_sdk::Decimal::new(1235, 3)),
        "rounded to the field's digits"
    );
    assert_eq!(row["amount"], Value::Decimal(erp_sdk::Decimal::from(100)));
    assert_eq!(
        row["json"],
        Value::Json(Arc::new(erp_sdk::serde_json::json!({"a": [1, 2]})))
    );
    assert_eq!(row["reference"], Value::str(&reference));

    // datetimes are read in the environment's timezone
    let brussels = model(&tx, "export.typed").with_context([("tz", Value::str("Europe/Brussels"))]);
    let result = brussels
        .load(&mut tx, &["moment"], &rows(&[&["2012-01-01 10:00:00"]]))
        .await
        .unwrap();
    assert_eq!(result.messages, vec![]);
    let local = tx
        .env()
        .records("export.typed")
        .unwrap()
        .browse(result.ids.unwrap())
        .unwrap();
    assert_eq!(
        local.value(&mut tx, "moment").await.unwrap(),
        Value::DateTime(erp_sdk::DateTime::parse_odoo("2012-01-01 09:00:00").unwrap())
    );

    let cases: Vec<(&str, &str, ImportMessage)> = vec![
        (
            "selection",
            "nope",
            msg(
                "Value 'nope' not found in selection field 'Selection'",
                0,
                Some("selection"),
            )
            .with_moreinfo(Value::list(["Foo", "Bar", "Qux"].map(Value::str)))
            .with_field_name("Selection")
            .with_field_path(["selection"]),
        ),
        (
            "date",
            "2012-13-45",
            msg(
                "'2012-13-45' does not seem to be a valid date for field 'Date'",
                0,
                Some("date"),
            )
            .with_moreinfo(Value::str("Use the format '2012-12-31'"))
            .with_field_name("Date")
            .with_field_path(["date"]),
        ),
        (
            "moment",
            "yesterday",
            msg(
                "'yesterday' does not seem to be a valid datetime for field 'Moment'",
                0,
                Some("moment"),
            )
            .with_moreinfo(Value::str("Use the format '2012-12-31 23:59:59'"))
            .with_field_name("Moment")
            .with_field_path(["moment"]),
        ),
        (
            "float",
            "abc",
            msg(
                "'abc' does not seem to be a number for field 'Float'",
                0,
                Some("float"),
            )
            .with_field_name("Float"),
        ),
        (
            "decimal",
            "1e999",
            msg(
                "'1e999' does not seem to be a number for field 'Decimal'",
                0,
                Some("decimal"),
            )
            .with_field_name("Decimal"),
        ),
        (
            "amount",
            "ten",
            msg(
                "'ten' does not seem to be a number for field 'Amount'",
                0,
                Some("amount"),
            )
            .with_field_name("Amount"),
        ),
        (
            "json",
            "{bad",
            msg(
                "'{bad' does not seem to be a valid JSON for field 'Json'",
                0,
                Some("json"),
            )
            .with_field_name("Json"),
        ),
        (
            "reference",
            "res.partner,1",
            msg(
                "Wrong value for export.typed.reference: 'res.partner,1'",
                0,
                Some("reference"),
            ),
        ),
    ];
    for (field, value, expected) in cases {
        let result = import(&mut tx, "export.typed", &[field], &[&[value]]).await;
        assert_eq!(result.ids, None, "{field}");
        assert_eq!(result.messages, vec![expected], "{field}");
    }
    assert_eq!(all(&mut tx, "export.typed").await.len(), 2);
    drop(tx);
    h.cleanup().await.unwrap();
}

/// `mode=update` leaves records whose external id is `noupdate`; `init` writes them.
#[tokio::test]
async fn load_update_mode_respects_noupdate() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let flagged = model(&tx, "export.integer").with_context([("noupdate", Value::Bool(true))]);
    let first = flagged
        .load(&mut tx, &["id", "value"], &rows(&[&["a", "1"]]))
        .await
        .unwrap();
    let id = first.ids.unwrap()[0];
    let env = tx.env();
    let found = erp_data::model_data::lookup(
        &mut tx,
        &env,
        &[erp_data::XmlId::parse("__import__.a").unwrap()],
    )
    .await
    .unwrap();
    let row = &found[&erp_data::XmlId::parse("__import__.a").unwrap()];
    assert_eq!((row.res_id, row.noupdate), (Some(id), true));

    let update = model(&tx, "export.integer").with_context([("mode", Value::str("update"))]);
    let result = update
        .load(&mut tx, &["id", "value"], &rows(&[&["a", "2"]]))
        .await
        .unwrap();
    assert_eq!(result.messages, vec![]);
    assert_eq!(result.ids, Some(vec![id]));
    assert_eq!(
        ints(&mut tx, "export.integer", "value").await,
        vec![Some(1)]
    );

    let result = import(&mut tx, "export.integer", &["id", "value"], &[&["a", "3"]]).await;
    assert_eq!(result.ids, Some(vec![id]));
    assert_eq!(
        ints(&mut tx, "export.integer", "value").await,
        vec![Some(3)]
    );

    // update mode needs an id or an external id
    let result = update
        .load(&mut tx, &["value"], &rows(&[&["5"]]))
        .await
        .unwrap();
    assert_eq!(result.ids, None);
    assert_eq!(
        result.messages,
        vec![msg(
            "Cannot update a record without specifying its id or xml_id",
            0,
            None
        )]
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

/// `_import_limit` stops after that many rows (whole records) and `nextrow` says where to resume.
#[tokio::test]
async fn load_import_limit_sets_nextrow() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let limited = model(&tx, "export.integer").with_context([("_import_limit", Value::Int(2))]);
    let data = rows(&[&["1"], &["2"], &["3"], &["4"], &["5"]]);
    let result = limited.load(&mut tx, &["value"], &data).await.unwrap();
    assert_eq!(result.messages, vec![]);
    assert_eq!(result.ids.as_ref().map(Vec::len), Some(2));
    assert_eq!(result.nextrow, 2);

    // a record spanning rows is never cut
    let lines = model(&tx, "export.one2many").with_context([("_import_limit", Value::Int(1))]);
    let data = rows(&[&["5", "1"], &["", "2"], &["6", "3"]]);
    let result = lines
        .load(&mut tx, &["const", "value/value"], &data)
        .await
        .unwrap();
    assert_eq!(result.ids.as_ref().map(Vec::len), Some(1));
    assert_eq!(result.nextrow, 2);

    let wide = model(&tx, "export.one2many").with_context([("_import_limit", Value::Int(10))]);
    let result = wide
        .load(&mut tx, &["const", "value/value"], &data)
        .await
        .unwrap();
    assert_eq!(result.ids.as_ref().map(Vec::len), Some(2));
    assert_eq!(result.nextrow, 0);
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn load_multi_batch_across_boundary() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();

    // 1050 records (exceeds DEFAULT_BATCH_SIZE 1000)
    let mut data: Vec<Vec<String>> = Vec::with_capacity(1050);
    for i in 0..1050 {
        data.push(vec![format!("batch_xid_{i}"), i.to_string()]);
    }
    let m = model(&tx, "export.integer");
    let result = m.load(&mut tx, &["id", "value"], &data).await.unwrap();
    assert_eq!(result.messages, vec![]);
    let ids = result.ids.expect("loaded ids");
    assert_eq!(ids.len(), 1050);
    assert_eq!(result.nextrow, 0);

    // every row was written with its own value, in input order
    assert_eq!(
        ints(&mut tx, "export.integer", "value").await,
        (0..1050).map(Some).collect::<Vec<_>>()
    );
    let written = tx
        .env()
        .records("export.integer")
        .unwrap()
        .browse(ids.clone())
        .unwrap();
    assert_eq!(written.exists(&mut tx).await.unwrap().len(), 1050);

    drop(tx);
    h.cleanup().await.unwrap();
}

/// An error in a row after the first 1,000-record batch reports its global row and record index.
#[tokio::test]
async fn load_error_after_batch_boundary_reports_global_index() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let mut data: Vec<Vec<String>> = (0..1050)
        .map(|i| vec!["4".to_owned(), format!("v{i}")])
        .collect();
    data[1020][1] = String::new();
    let records = model(&tx, "export.string.required");
    let result = records
        .load(&mut tx, &["const", "value"], &data)
        .await
        .unwrap();
    assert_eq!(result.ids, None);
    assert_eq!(
        result.messages,
        vec![msg(
            "Missing required field 'Value' (value) for model 'Export: string.required' \
             (export.string.required)",
            1020,
            Some("value")
        )]
    );
    // the failed import is rolled back in full, including the first batch
    assert_eq!(all(&mut tx, "export.string.required").await.len(), 0);
    drop(tx);
    h.cleanup().await.unwrap();
}

/// A row in the second batch refers, by external id, to rows written by the first batch.
#[tokio::test]
async fn load_second_batch_resolves_xid_created_in_first_batch() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let mut data: Vec<Vec<String>> = (0..1003)
        .map(|i| vec![format!("x{i}"), i.to_string(), String::new()])
        .collect();
    data[1000][2] = "x999".to_owned(); // last row of batch 1
    data[1001][2] = "x0".to_owned(); // first row of batch 1
    data[1002][2] = "x1001".to_owned(); // an earlier row of batch 2
    let records = model(&tx, "export.self");
    let result = records
        .load(&mut tx, &["id", "value", "parent_id/id"], &data)
        .await
        .unwrap();
    assert_eq!(result.messages, vec![]);
    let ids = result.ids.expect("loaded ids");
    assert_eq!(ids.len(), 1003);
    let mut expected: Vec<Option<i64>> = vec![None; 1003];
    expected[1000] = Some(ids[999]);
    expected[1001] = Some(ids[0]);
    expected[1002] = Some(ids[1001]);
    assert_eq!(ints(&mut tx, "export.self", "parent_id").await, expected);
    assert_eq!(
        ints(&mut tx, "export.self", "value").await,
        (0..1003).map(Some).collect::<Vec<_>>()
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn load_too_many_errors_stops_only_its_own_flush_group() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let mut data: Vec<Vec<String>> = (0..1018)
        .map(|i| vec![format!("s{i}"), format!("n{i}"), String::new()])
        .collect();
    // group 1: row 0 valid; rows 1..=10 have no name and fail
    for row in &mut data[1..=10] {
        row[1] = String::new();
    }
    // row 12 refers to the pending row 0: group 1 is flushed first
    data[12][2] = "s0".to_owned();
    // group 2: rows 13..=22 fail
    for row in &mut data[13..=22] {
        row[1] = String::new();
    }
    // row 1005 refers to the pending row 12 (the group spans the 1,000-record cap): group 2 is
    // flushed first.
    data[1005][2] = "s12".to_owned();
    // group 3: rows 1006..=1015 fail
    for row in &mut data[1006..=1015] {
        row[1] = String::new();
    }
    let records = model(&tx, "export.self.required");
    let result = records
        .load(&mut tx, &["id", "name", "parent_id/id"], &data)
        .await
        .unwrap();
    assert_eq!(result.ids, None);
    let text = "Missing required field 'Name' (name) for model 'Export: self.required' \
                (export.self.required)";
    let too_many = ImportMessage::new(
        MessageType::Warning,
        "Found more than 10 errors and more than one error per 10 records, interrupted to \
         avoid showing too many errors.",
    );
    let mut expected = Vec::new();
    for rows in [1..=10, 13..=22, 1006..=1015] {
        expected.extend(rows.map(|row| msg(text, row, Some("name"))));
        expected.push(too_many.clone());
    }
    assert_eq!(result.messages, expected);
    drop(tx);
    h.cleanup().await.unwrap();
}

const TOO_MANY_TEXT: &str = "Found more than 10 errors and more than one error per 10 records, \
                             interrupted to avoid showing too many errors.";
const REQUIRED_VALUE: &str = "Missing required field 'Value' (value) for model \
                              'Export: string.required' (export.string.required)";

fn odoo_flush_group_reports(rows: usize, failing: &[usize]) -> (Vec<usize>, bool) {
    let mut reported = Vec::new();
    for (i, row) in (1..=rows).zip(0..rows) {
        if failing.contains(&row) {
            reported.push(row);
        }
        let errors = reported.len();
        if errors >= 10 && errors * 10 >= i {
            return (reported, true);
        }
    }
    (reported, false)
}

/// One flush group of 2,000 rows (no references, so no flush trigger) with errors in rows
/// 1000..=1009 and in rows 1500 and 1600.
/// Restarting the count at the chunk boundary (row 1000) would stop at row 1009.
/// There is no `global` message: the batch attempt fails with a not-null violation (a database
/// error), and only a `UserError` becomes `global_error_message`.
#[tokio::test]
async fn load_too_many_counts_over_the_whole_flush_group() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let mut failing: Vec<usize> = (1000..=1009).collect();
    failing.extend([1500, 1600]);
    let mut data: Vec<Vec<String>> = (0..2000)
        .map(|i| vec!["4".to_owned(), format!("v{i}")])
        .collect();
    for row in &failing {
        data[*row][1] = String::new();
    }
    let (reported, stops) = odoo_flush_group_reports(2000, &failing);
    assert!(!stops);
    assert_eq!(reported, failing);
    let records = model(&tx, "export.string.required");
    let result = records
        .load(&mut tx, &["const", "value"], &data)
        .await
        .unwrap();
    assert_eq!(result.ids, None);
    assert_eq!(
        result.messages,
        reported
            .iter()
            .map(|row| msg(REQUIRED_VALUE, *row, Some("value")))
            .collect::<Vec<_>>()
    );
    assert_eq!(all(&mut tx, "export.string.required").await.len(), 0);
    drop(tx);
    h.cleanup().await.unwrap();
}

/// One flush group of 1,200 rows. Errors in every 12th row up to row 996 (84 errors, always below
/// one per 10 records), then in every row from 1000 to 1029, plus rows 1100 and 1150. Counting by
/// hand with `i` = row + 1 and `j` dense errors after the boundary: `10 * (84 + j) >= 1000 + j`
/// first holds at j = 18, row 1017.
/// are neither written nor reported, though they sit in the
/// same group and the same second chunk.
#[tokio::test]
async fn load_too_many_trips_inside_a_group_spanning_the_chunk_boundary() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let mut failing: Vec<usize> = (0..=996).step_by(12).collect();
    assert_eq!(failing.len(), 84);
    failing.extend(1000..=1029);
    failing.extend([1100, 1150]);
    let mut data: Vec<Vec<String>> = (0..1200)
        .map(|i| vec!["4".to_owned(), format!("v{i}")])
        .collect();
    for row in &failing {
        data[*row][1] = String::new();
    }
    let (reported, stops) = odoo_flush_group_reports(1200, &failing);
    assert!(stops);
    assert_eq!(reported.last(), Some(&1017));
    let records = model(&tx, "export.string.required");
    let result = records
        .load(&mut tx, &["const", "value"], &data)
        .await
        .unwrap();
    assert_eq!(result.ids, None);
    let mut expected: Vec<ImportMessage> = reported
        .iter()
        .map(|row| msg(REQUIRED_VALUE, *row, Some("value")))
        .collect();
    expected.push(ImportMessage::new(MessageType::Warning, TOO_MANY_TEXT));
    assert_eq!(result.messages, expected);
    assert_eq!(all(&mut tx, "export.string.required").await.len(), 0);
    drop(tx);
    h.cleanup().await.unwrap();
}

/// Many2many references by name, external id and database id; `update_many2many` adds links.
#[tokio::test]
async fn load_many2many_references() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let o1 = create(&mut tx, "export.many2many.other", Value::Int(1)).await;
    let o2 = create(&mut tx, "export.many2many.other", Value::Int(2)).await;
    add_xid(&mut tx, "__test__", "o2", "export.many2many.other", o2).await;
    let links = async |tx: &mut Transaction, id: i64| -> Vec<i64> {
        let record = tx
            .env()
            .records("export.mixed")
            .unwrap()
            .browse([id])
            .unwrap();
        match record.cell(tx, "m2m").await.unwrap() {
            erp_sdk::Cell::Ids(ids) => ids.iter().map(|r| r.get()).collect(),
            other => panic!("not ids: {other:?}"),
        }
    };

    let by_name = import(&mut tx, "export.mixed", &["id", "m2m"], &[&["m", "1,2"]]).await;
    assert_eq!(by_name.messages, vec![]);
    let id = by_name.ids.unwrap()[0];
    assert_eq!(links(&mut tx, id).await, vec![o1, o2]);

    let text = o1.to_string();
    let by_id = import(
        &mut tx,
        "export.mixed",
        &["id", "m2m/.id"],
        &[&["m", text.as_str()]],
    )
    .await;
    assert_eq!(by_id.ids, Some(vec![id]));
    assert_eq!(links(&mut tx, id).await, vec![o1], "replaced (SET)");

    let adding = model(&tx, "export.mixed").with_context([("update_many2many", Value::Bool(true))]);
    let added = adding
        .load(&mut tx, &["id", "m2m/id"], &rows(&[&["m", "__test__.o2"]]))
        .await
        .unwrap();
    assert_eq!(added.messages, vec![]);
    assert_eq!(links(&mut tx, id).await, vec![o1, o2], "added (LINK)");

    let unknown = import(&mut tx, "export.mixed", &["m2m"], &[&["1,9"]]).await;
    assert_eq!(unknown.ids, None);
    assert_eq!(
        unknown.messages,
        vec![
            msg(
                "No matching record found for name '9' in field 'M2M'",
                0,
                Some("m2m")
            )
            .with_moreinfo(action("export.many2many.other", None))
            .with_field_name("M2M")
            .with_field_path(["m2m"])
            .with_field_type("name")
            .with_value("9")
        ]
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

/// One2many lines referencing existing records: linked (and written), inline or one per row.
#[tokio::test]
async fn load_one2many_lines_by_reference() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let c1 = create(&mut tx, "export.one2many.child", Value::Int(109)).await;
    let c2 = create(&mut tx, "export.one2many.child", Value::Int(262)).await;
    let (t1, t2) = (c1.to_string(), c2.to_string());

    // test_link: one line per row
    let result = import(
        &mut tx,
        "export.one2many",
        &["const", "value/.id"],
        &[&["42", t1.as_str()], &["", t2.as_str()]],
    )
    .await;
    assert_eq!(result.messages, vec![]);
    let parent = result.ids.unwrap()[0];
    assert_eq!(
        ints(&mut tx, "export.one2many.child", "parent_id").await,
        vec![Some(parent), Some(parent)]
    );

    // test_link_inline and test_link_2: inline references, then references with values
    let inline = format!("{c1},{c2}");
    let result = import(
        &mut tx,
        "export.one2many",
        &["const", "value/.id"],
        &[&["43", inline.as_str()]],
    )
    .await;
    assert_eq!(result.messages, vec![]);
    let second = result.ids.unwrap()[0];
    assert_eq!(
        ints(&mut tx, "export.one2many.child", "parent_id").await,
        vec![Some(second), Some(second)]
    );
    let result = import(
        &mut tx,
        "export.one2many",
        &["const", "value/.id", "value/value"],
        &[&["44", t1.as_str(), "1"], &["", t2.as_str(), "2"]],
    )
    .await;
    assert_eq!(result.messages, vec![]);
    assert_eq!(
        ints(&mut tx, "export.one2many.child", "value").await,
        vec![Some(1), Some(2)]
    );

    // a line with a new external id is lifted and created with its xid
    let result = import(
        &mut tx,
        "export.one2many",
        &["const", "value/id"],
        &[&["45", "zz"]],
    )
    .await;
    assert_eq!(result.messages, vec![]);
    let third = result.ids.unwrap()[0];
    let zz_parent = ints(&mut tx, "export.one2many.child", "parent_id").await;
    assert!(zz_parent.contains(&Some(third)));
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_data_083_load_one2many_lines_with_new_external_ids_lifted_and_updated_on_reload() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();

    // First import: parent 'a' with children 'aa'..'ad'; parent 'b' with children 'ba', 'bb'
    let result = import(
        &mut tx,
        "export.one2many",
        &["id", "const", "value/id", "value/value"],
        &[
            &["a", "5", "aa", "11"],
            &["", "", "ab", "12"],
            &["", "", "ac", "13"],
            &["", "", "ad", "14"],
            &["b", "10", "ba", "15"],
            &["", "", "bb", "16"],
        ],
    )
    .await;
    assert_eq!(result.messages, vec![]);
    assert!(result.ids.is_some());

    // Verify children created and initial values
    let child_vals = ints(&mut tx, "export.one2many.child", "value").await;
    assert_eq!(
        child_vals,
        vec![Some(11), Some(12), Some(13), Some(14), Some(15), Some(16)]
    );

    // Verify xids in ir.model.data
    let children = all(&mut tx, "export.one2many.child").await;
    assert_eq!(children.ids().len(), 6);
    assert_eq!(
        xids_of(&mut tx, "export.one2many.child", children.ids()[0]).await,
        vec!["__import__.aa"]
    );
    assert_eq!(
        xids_of(&mut tx, "export.one2many.child", children.ids()[4]).await,
        vec!["__import__.ba"]
    );

    // Second import: re-import with updated 'b.const' = 8, 'ba.value' = 25
    let result = import(
        &mut tx,
        "export.one2many",
        &["id", "const", "value/id", "value/value"],
        &[
            &["a", "5", "aa", "11"],
            &["", "", "ab", "12"],
            &["", "", "ac", "13"],
            &["", "", "ad", "14"],
            &["b", "8", "ba", "25"],
            &["", "", "bb", "16"],
        ],
    )
    .await;
    assert_eq!(result.messages, vec![]);

    // Verify no duplicates created: total 6 children
    let children_after = all(&mut tx, "export.one2many.child").await;
    assert_eq!(children_after.ids(), children.ids());
    let child_vals_after = ints(&mut tx, "export.one2many.child", "value").await;
    assert_eq!(
        child_vals_after,
        vec![Some(11), Some(12), Some(13), Some(14), Some(25), Some(16)]
    );

    // Re-verify xids and inverse parent_id after reload
    assert_eq!(
        xids_of(&mut tx, "export.one2many.child", children_after.ids()[0]).await,
        vec!["__import__.aa"]
    );
    assert_eq!(
        xids_of(&mut tx, "export.one2many.child", children_after.ids()[4]).await,
        vec!["__import__.ba"]
    );

    // Verify parent 'b' updated to 8
    let parents = all(&mut tx, "export.one2many").await;
    let b_const = parents.cells(&mut tx, "const").await.unwrap();
    assert_eq!(b_const, vec![erp_sdk::Cell::Int(5), erp_sdk::Cell::Int(8)]);
    let parent_ids = parents.ids();
    let child_parents = ints(&mut tx, "export.one2many.child", "parent_id").await;
    assert_eq!(
        child_parents,
        vec![
            Some(parent_ids[0]),
            Some(parent_ids[0]),
            Some(parent_ids[0]),
            Some(parent_ids[0]),
            Some(parent_ids[1]),
            Some(parent_ids[1]),
        ]
    );

    // Nested lifting: both child and grandchild have xids
    let result = import(
        &mut tx,
        "export.one2many",
        &[
            "id",
            "const",
            "value/id",
            "value/value",
            "value/grandchild_ids/id",
            "value/grandchild_ids/value",
        ],
        &[&["p_nested", "1", "c_nested", "100", "gc_nested", "1000"]],
    )
    .await;
    assert_eq!(result.messages, vec![]);
    assert!(result.ids.is_some());
    let nested_parent_id = result.ids.unwrap()[0];

    let gc_recs = all(&mut tx, "export.one2many.grandchild").await;
    assert_eq!(gc_recs.ids().len(), 1);
    let gc_id = gc_recs.ids()[0];
    assert_eq!(
        xids_of(&mut tx, "export.one2many.grandchild", gc_id).await,
        vec!["__import__.gc_nested"]
    );

    // Grandchild's parent_id points to the child, NOT the top-level parent!
    let gc_parent = ints(&mut tx, "export.one2many.grandchild", "parent_id").await[0].unwrap();
    assert_ne!(gc_parent, nested_parent_id);
    let c_nested_parents = ints(&mut tx, "export.one2many.child", "parent_id").await;
    assert!(c_nested_parents.contains(&Some(nested_parent_id)));

    drop(tx);
    h.cleanup().await.unwrap();
}

/// Nested child line without xid having a grandchild with new xid is refused
/// (naming) with no wrong links or partial writes committed.
#[tokio::test]
async fn nested_lines_without_xid_refuse_lifted_grandchildren() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();

    let initial_parents = all(&mut tx, "export.one2many").await.ids().len();
    let initial_children = all(&mut tx, "export.one2many.child").await.ids().len();
    let initial_gc = all(&mut tx, "export.one2many.grandchild").await.ids().len();

    // Child line has NO xid ('value/value' only), but grandchild has 'value/grandchild_ids/id'
    let result = import(
        &mut tx,
        "export.one2many",
        &[
            "const",
            "value/value",
            "value/grandchild_ids/id",
            "value/grandchild_ids/value",
        ],
        &[&["99", "200", "gc_fail", "2000"]],
    )
    .await;
    assert!(result.ids.is_none());
    assert_eq!(result.messages.len(), 1);
    assert_eq!(result.messages[0].kind, MessageType::Error);
    assert!(
        result.messages[0].message.contains("creating one2many lines with new external ids nested under a line without an external id in field 'Value' is not supported until a later release"),
        "message was: {}",
        result.messages[0].message
    );

    // Assert no partial writes committed to the database
    assert_eq!(
        all(&mut tx, "export.one2many").await.ids().len(),
        initial_parents
    );
    assert_eq!(
        all(&mut tx, "export.one2many.child").await.ids().len(),
        initial_children
    );
    assert_eq!(
        all(&mut tx, "export.one2many.grandchild").await.ids().len(),
        initial_gc
    );

    drop(tx);
    h.cleanup().await.unwrap();
}

/// Conflicting inverse value on a lifted line is refused loudly.
#[tokio::test]
async fn lifted_line_conflicting_inverse_is_refused() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();

    // Trying to import with a conflicting parent_id value directly in the child line
    let result = import(
        &mut tx,
        "export.one2many",
        &["const", "value/id", "value/parent_id"],
        &[&["1", "child_conflict", "99999"]],
    )
    .await;
    assert!(result.ids.is_none());
    assert_eq!(result.messages.len(), 1);
    assert_eq!(result.messages[0].kind, MessageType::Error);

    drop(tx);
    h.cleanup().await.unwrap();
}

/// `_inherits` parents created with a record get `<xid>_<parent model>`.
#[tokio::test]
async fn load_gives_inherits_parents_external_ids() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let result = import(
        &mut tx,
        "export.inherits.child",
        &["id", "value", "value_parent"],
        &[&["kid", "1", "7"]],
    )
    .await;
    assert_eq!(result.messages, vec![]);
    let child = result.ids.unwrap()[0];
    let parent = ints(&mut tx, "export.inherits.child", "parent_id").await[0].unwrap();
    assert_eq!(
        xids_of(&mut tx, "export.inherits.child", child).await,
        vec!["__import__.kid"]
    );
    assert_eq!(
        xids_of(&mut tx, "export.inherits.parent", parent).await,
        vec!["__import__.kid_export_inherits_parent"]
    );
    assert_eq!(
        ints(&mut tx, "export.inherits.parent", "value_parent").await,
        vec![Some(7)]
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

/// Good rows of a failing batch are rolled back too; 10 errors in 200 rows do not stop it.
#[tokio::test]
async fn load_rolls_back_good_rows_and_throttles_only_above_threshold() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let required = "Missing required field 'Value' (value) for model 'Export: string.required' \
                    (export.string.required)";
    let result = import(
        &mut tx,
        "export.string.required",
        &["const", "value"],
        &[&["1", "a"], &["2", ""], &["3", "c"]],
    )
    .await;
    assert_eq!(result.ids, None);
    assert_eq!(result.messages, vec![msg(required, 1, Some("value"))]);
    assert_eq!(all(&mut tx, "export.string.required").await.len(), 0);

    // errors on rows 19, 39, …, 199: the 10th comes at row 199, below one in ten
    let data: Vec<Vec<String>> = (0..200)
        .map(|n| {
            let value = if n % 20 == 19 {
                String::new()
            } else {
                format!("v{n}")
            };
            vec![n.to_string(), value]
        })
        .collect();
    let records = model(&tx, "export.string.required");
    let result = records
        .load(&mut tx, &["const", "value"], &data)
        .await
        .unwrap();
    assert_eq!(result.ids, None);
    let expected: Vec<ImportMessage> = (0..10)
        .map(|k| msg(required, k * 20 + 19, Some("value")))
        .collect();
    assert_eq!(result.messages, expected);
    assert_eq!(all(&mut tx, "export.string.required").await.len(), 0);
    drop(tx);
    h.cleanup().await.unwrap();
}

/// Sub-paths of non-relational export columns are refused.
#[tokio::test]
async fn export_rejects_sub_paths_of_scalars() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let id = create(&mut tx, "export.integer", Value::Int(1)).await;
    let one = tx
        .env()
        .records("export.integer")
        .unwrap()
        .browse([id])
        .unwrap();
    let err = one
        .export_data(&mut tx, &["value/id"], &SuperuserOnly)
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Programming);
    assert_eq!(
        err.message(),
        "Invalid field 'value/id': 'value' of model 'export.integer' is not a relational field \
         and has no sub-fields"
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn update_xmlids_upserts_like_reference() {
    use erp_data::model_data::{lookup, update_xmlids, xmlid_to_res_id};
    use erp_data::{XidBinding, XmlId};
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let env = tx.env();
    let x = XmlId::parse("m.x").unwrap();
    update_xmlids(
        &mut tx,
        &env,
        &[XidBinding::new(x.clone(), "export.integer", 5, true)],
        false,
    )
    .await
    .unwrap();
    assert_eq!(
        xmlid_to_res_id(&mut tx, &env, &x).await.unwrap(),
        Some(("export.integer".to_owned(), 5))
    );
    // a module update leaves noupdate rows alone
    update_xmlids(
        &mut tx,
        &env,
        &[XidBinding::new(x.clone(), "export.integer", 6, false)],
        true,
    )
    .await
    .unwrap();
    assert_eq!(
        lookup(&mut tx, &env, std::slice::from_ref(&x))
            .await
            .unwrap()[&x]
            .res_id,
        Some(5)
    );
    // otherwise the row is re-pointed; its noupdate flag is kept
    update_xmlids(
        &mut tx,
        &env,
        &[XidBinding::new(x.clone(), "export.integer", 6, false)],
        false,
    )
    .await
    .unwrap();
    let row = lookup(&mut tx, &env, std::slice::from_ref(&x))
        .await
        .unwrap()[&x]
        .clone();
    assert_eq!((row.res_id, row.noupdate), (Some(6), true));
    // repeats are written once; conflicting targets are an error
    let twice = [
        XidBinding::new(x.clone(), "export.integer", 6, false),
        XidBinding::new(x.clone(), "export.integer", 6, false),
    ];
    update_xmlids(&mut tx, &env, &twice, false).await.unwrap();
    let conflict = [
        XidBinding::new(x.clone(), "export.integer", 6, false),
        XidBinding::new(x.clone(), "export.integer", 7, false),
    ];
    let err = update_xmlids(&mut tx, &env, &conflict, false)
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Programming);
    assert_eq!(
        err.message(),
        "external id m.x bound twice: to export.integer(6) and export.integer(7)"
    );
    assert_eq!(
        xmlid_to_res_id(&mut tx, &env, &XmlId::parse("m.none").unwrap())
            .await
            .unwrap(),
        None
    );
    tx.commit().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

/// Two transactions binding the same external id: the later one never fails on the unique
/// index; it waits and gets a serialization failure, which the unit of work retries.
#[tokio::test]
async fn concurrent_bindings_of_one_xid_do_not_violate_uniqueness() {
    use erp_data::model_data::update_xmlids;
    use erp_data::{XidBinding, XmlId};
    let h = harness().await;
    let mut a = h.begin().await.unwrap();
    let mut b = h.begin().await.unwrap();
    // open both snapshots
    a.query("SELECT 1", &[]).await.unwrap();
    b.query("SELECT 1", &[]).await.unwrap();
    let (env_a, env_b) = (a.env(), b.env());
    let x = XmlId::parse("m.shared").unwrap();
    let bind_a = [XidBinding::new(x.clone(), "export.integer", 1, false)];
    let bind_b = [XidBinding::new(x.clone(), "export.integer", 2, false)];
    let (first, second) = tokio::join!(
        async {
            update_xmlids(&mut a, &env_a, &bind_a, false).await?;
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            a.commit().await
        },
        async {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            update_xmlids(&mut b, &env_b, &bind_b, false).await
        }
    );
    first.unwrap();
    let err = second.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Concurrency, "{err:?}");
    assert_eq!(err.sqlstate(), Some("40001"));
    drop(a);
    drop(b);
    h.cleanup().await.unwrap();
}
