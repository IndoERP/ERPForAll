#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

mod bench_fixture;

use std::sync::Arc;

use bench_fixture::{create, harness, set_ids, vals};
use erp_orm::{Spec, SpecItem};
use erp_sdk::{DateTime, RecId, Transaction, Value};

fn text(s: &str) -> Value {
    Value::str(s)
}

fn body(values: &[Value]) -> String {
    let bytes =
        erp_rpc::__output::serialize(&Value::list(values.iter().cloned()), 64 << 20).unwrap();
    String::from_utf8(bytes).unwrap()
}

fn as_list(val: &Value) -> Vec<Value> {
    match val {
        Value::List(items) => items.to_vec(),
        Value::Rows(rows) => rows.iter().unwrap().map(Value::Row).collect(),
        _ => panic!("expected List or Rows, got {val:?}"),
    }
}

/// Panic when a negative integer appears below the top-level `id` key (a leaked virtual id).
fn assert_no_negative_ids(json: &str) {
    fn walk(v: &erp_fields::serde_json::Value, path: &str) {
        use erp_fields::serde_json::Value as J;
        match v {
            J::Number(n) => assert!(
                n.as_i64().is_none_or(|i| i >= 0),
                "negative id {n} leaked at {path}"
            ),
            J::Array(items) => {
                for (i, item) in items.iter().enumerate() {
                    walk(item, &format!("{path}[{i}]"));
                }
            }
            J::Object(map) => {
                for (k, item) in map {
                    walk(item, &format!("{path}.{k}"));
                }
            }
            _ => {}
        }
    }
    let parsed: erp_fields::serde_json::Value = erp_fields::serde_json::from_str(json).unwrap();
    let rows = parsed.as_array().expect("array of rows");
    for (i, row) in rows.iter().enumerate() {
        for (k, item) in row.as_object().expect("row object") {
            // the top-level row keeps its own virtual identity
            if k != "id" {
                walk(item, &format!("[{i}].{k}"));
            }
        }
    }
}

/// Fixture for origin relation tests: 2 partners, 3 tags, 2 orders with lines.
async fn fixture(tx: &mut Transaction) -> [i64; 2] {
    let partners = create(
        tx,
        "bench.partner",
        &[
            vals(vec![
                ("code", text("P1")),
                ("name", text("Partner 1")),
                ("country", text("id")),
            ]),
            vals(vec![
                ("code", text("P2")),
                ("name", text("Partner 2")),
                ("country", text("id")),
            ]),
        ],
    )
    .await;
    let tags = create(
        tx,
        "bench.tag",
        &[
            vals(vec![("code", text("T1")), ("name", text("Tag 1"))]),
            vals(vec![("code", text("T2")), ("name", text("Tag 2"))]),
            vals(vec![("code", text("T3")), ("name", text("Tag 3"))]),
        ],
    )
    .await;
    let when = DateTime::parse_odoo("2024-02-29 13:45:01").unwrap();
    let orders = create(
        tx,
        "bench.order",
        &[
            vals(vec![
                ("code", text("O1")),
                ("name", text("Order 1")),
                ("partner_id", Value::Int(partners[0])),
                ("date_order", Value::DateTime(when)),
                ("tag_ids", set_ids(&[tags[0], tags[1]])),
            ]),
            vals(vec![
                ("code", text("O2")),
                ("name", text("Order 2")),
                ("partner_id", Value::Int(partners[1])),
                ("date_order", Value::False),
                ("tag_ids", set_ids(&[tags[0], tags[2]])),
            ]),
        ],
    )
    .await;
    let line = |code: &str, order: i64, qty: f64, price: Option<f64>| {
        let mut v = vec![
            ("code", text(code)),
            ("order_id", Value::Int(order)),
            ("product_name", text("Product")),
            ("qty", Value::Float(qty)),
        ];
        if let Some(p) = price {
            v.push(("price_unit", Value::Float(p)));
        }
        vals(v)
    };
    create(
        tx,
        "bench.order.line",
        &[
            line("L1a", orders[0], 2.0, Some(3.5)),
            line("L1b", orders[0], 1.0, Some(10.0)),
            line("L1c", orders[0], 0.5, None),
            line("L2a", orders[1], 4.0, Some(0.25)),
            line("L2b", orders[1], 1.0, Some(1.0)),
        ],
    )
    .await;
    [orders[0], orders[1]]
}

#[tokio::test]
async fn spec_grp_066_virtual_row_identities_and_raw_relation_origins_are_distinct() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let [o1, _] = fixture(&mut tx).await;
    tx.commit().await.unwrap();

    let mut tx = h.begin().await.unwrap();
    let order_model = tx.env().records("bench.order").unwrap();
    let partner_model = tx.env().records("bench.partner").unwrap();
    let line_model = tx.env().records("bench.order.line").unwrap();

    // 1. Virtual parent with an origin (o1):
    // Has origin-backed partner_id, line_ids (L1a, L1b, L1c), and tag_ids (T1, T2).
    let parent_with_origin = tx
        .new_record(
            &order_model,
            erp_sdk::RawVals::new(),
            RecId::from_real(o1),
            None,
        )
        .await
        .unwrap();
    let top_virtual_id = parent_with_origin.rec_ids()[0].get();
    assert!(top_virtual_id < 0, "parent must have a negative virtual ID");

    // Also attach an originless child line to parent_with_origin to verify omission
    let originless_line = tx
        .new_record(
            &line_model,
            erp_sdk::RawVals::new()
                .set_cell("code", erp_sdk::Cell::Str("L_ORIGINLESS".into()))
                .set_cell("qty", erp_sdk::Cell::Float(7.0)),
            None,
            None,
        )
        .await
        .unwrap();
    let existing_lines = parent_with_origin
        .related(&mut tx, "line_ids")
        .await
        .unwrap();
    let combined_lines = existing_lines.concat(&originless_line).unwrap();
    parent_with_origin
        .assign(&mut tx, "line_ids", &combined_lines)
        .await
        .unwrap();

    // A virtual many2one target with an origin: a new partner made from the stored one, edited,
    // assigned to the virtual parent (its raw id is the partner's origin id).
    let stored_partner = match order_model
        .browse([o1])
        .unwrap()
        .cell(&mut tx, "partner_id")
        .await
        .unwrap()
    {
        erp_sdk::Cell::Id(id) => id,
        other => panic!("expected Cell::Id, got {other:?}"),
    };
    let virtual_partner = tx
        .new_record(
            &partner_model,
            erp_sdk::RawVals::new(),
            RecId::from_real(stored_partner),
            None,
        )
        .await
        .unwrap();
    assert!(virtual_partner.rec_ids()[0].get() < 0);
    virtual_partner
        .set_value(&mut tx, "name", Value::str("Edited Partner"))
        .await
        .unwrap();
    parent_with_origin
        .assign(&mut tx, "partner_id", &virtual_partner)
        .await
        .unwrap();

    // Stored origin IDs of o1:
    let origin_order = order_model.browse([o1]).unwrap();
    let origin_partner_id = match origin_order.cell(&mut tx, "partner_id").await.unwrap() {
        erp_sdk::Cell::Id(id) => id,
        other => panic!("expected Cell::Id, got {other:?}"),
    };
    let origin_line_ids: Vec<i64> = existing_lines.origin().ids();
    assert_eq!(origin_line_ids.len(), 3);
    let origin_tag_ids: Vec<i64> = parent_with_origin
        .related(&mut tx, "tag_ids")
        .await
        .unwrap()
        .origin()
        .ids();
    assert_eq!(origin_tag_ids.len(), 2);

    // 2. New parent whose one2many child has NO origin, and completely new many2one target without origin:
    let originless_parent = tx
        .new_record(
            &order_model,
            erp_sdk::RawVals::new().set_cell("code", erp_sdk::Cell::Str("N_ORIGINLESS".into())),
            None,
            None,
        )
        .await
        .unwrap();
    let sole_originless_line = tx
        .new_record(
            &line_model,
            erp_sdk::RawVals::new()
                .set_cell("code", erp_sdk::Cell::Str("SOLE_LINE".into()))
                .set_cell("qty", erp_sdk::Cell::Float(1.0)),
            None,
            None,
        )
        .await
        .unwrap();
    originless_parent
        .assign(&mut tx, "line_ids", &sole_originless_line)
        .await
        .unwrap();
    let originless_partner = tx
        .new_record(
            &partner_model,
            erp_sdk::RawVals::new()
                .set_cell("code", erp_sdk::Cell::Str("P_NEW".into()))
                .set_cell("name", erp_sdk::Cell::Str("New Partner".into())),
            None,
            None,
        )
        .await
        .unwrap();
    originless_parent
        .assign(&mut tx, "partner_id", &originless_partner)
        .await
        .unwrap();

    // 3. Test raw relations and expanded many2one on parent_with_origin:
    let spec_raw = Spec::new()
        .field("name")
        .field("partner_id")
        .field("line_ids")
        .field("tag_ids");
    let res = parent_with_origin
        .web_read(&mut tx, spec_raw)
        .await
        .unwrap();
    assert_eq!(res.len(), 1);
    assert_no_negative_ids(&body(&res));
    let row = res[0].dict().unwrap();

    // Then: top-level parent keeps its virtual identity
    assert_eq!(row.get("id").unwrap(), Value::Int(top_virtual_id));

    // Then: raw many2one is its target's real origin ID
    assert_eq!(
        row.get("partner_id").unwrap(),
        Value::Int(origin_partner_id)
    );

    // Then: raw x2many lists expose real origin IDs, not internal virtual IDs, and omit originless children
    assert_eq!(
        row.get("line_ids").unwrap(),
        Value::list(origin_line_ids.iter().map(|id| Value::Int(*id)))
    );
    assert_eq!(
        row.get("tag_ids").unwrap(),
        Value::list(origin_tag_ids.iter().map(|id| Value::Int(*id)))
    );

    // Then: expanded many2one dictionary contains that origin ID and the requested name
    let spec_expanded_m2o = Spec::new().with(
        "partner_id",
        SpecItem::fields(Spec::new().field("code").field("name")),
    );
    let res = parent_with_origin
        .web_read(&mut tx, spec_expanded_m2o)
        .await
        .unwrap();
    assert_no_negative_ids(&body(&res));
    let row = res[0].dict().unwrap();
    let m2o_val = row.get("partner_id").unwrap();
    let m2o = m2o_val.dict().unwrap();
    assert_eq!(m2o.get("id").unwrap(), Value::Int(origin_partner_id));
    assert_eq!(m2o.get("code").unwrap(), Value::str("P1"));
    assert_eq!(m2o.get("name").unwrap(), Value::str("Edited Partner"));

    // 4. Test originless parent with originless line and originless partner:
    // Raw format:
    let spec_orig_raw = Spec::new().field("partner_id").field("line_ids");
    let res = originless_parent
        .web_read(&mut tx, spec_orig_raw)
        .await
        .unwrap();
    let row = res[0].dict().unwrap();

    // Then: A parent with only an originless one2many child reads an empty list in raw format
    assert_eq!(row.get("line_ids").unwrap(), Value::list([]));

    // Then: An originless many2one yields JSON null in the raw format
    let null_val = Value::Json(Arc::new(erp_fields::serde_json::Value::Null));
    assert_eq!(row.get("partner_id").unwrap(), null_val);

    // RPC serialization of raw format produces partner_id: null
    let json_raw = body(&[res[0].clone()]);
    assert!(
        json_raw.contains(r#""partner_id":null"#),
        "raw JSON must contain '\"partner_id\":null', got: {json_raw}"
    );
    assert!(
        json_raw.contains(r#""line_ids":[]"#),
        "raw JSON must contain '\"line_ids\":[]', got: {json_raw}"
    );

    // Expanded format:
    let spec_orig_exp = Spec::new()
        .with(
            "partner_id",
            SpecItem::fields(Spec::new().field("code").field("name")),
        )
        .with("line_ids", SpecItem::fields(Spec::new().field("code")));
    let res = originless_parent
        .web_read(&mut tx, spec_orig_exp)
        .await
        .unwrap();
    let row = res[0].dict().unwrap();

    // Then: A parent with only an originless one2many child reads an empty list in expanded format
    assert_eq!(row.get("line_ids").unwrap(), Value::list([]));

    // Then: An originless many2one yields false in the expanded format, with no internal virtual ID leaked
    assert_eq!(row.get("partner_id").unwrap(), Value::False);

    // RPC serialization of expanded format produces partner_id: false
    let json_exp = body(&[res[0].clone()]);
    assert!(
        json_exp.contains(r#""partner_id":false"#),
        "expanded JSON must contain '\"partner_id\":false', got: {json_exp}"
    );
    assert!(
        json_exp.contains(r#""line_ids":[]"#),
        "expanded JSON must contain '\"line_ids\":[]', got: {json_exp}"
    );
    assert_no_negative_ids(&json_exp);
    assert_no_negative_ids(&json_raw);

    tx.rollback().await.unwrap();
}

#[tokio::test]
async fn spec_grp_067_origin_backed_x2many_expansion_preserves_requested_virtual_values() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let [o1, o2] = fixture(&mut tx).await;
    tx.commit().await.unwrap();

    let mut tx = h.begin().await.unwrap();
    let order_model = tx.env().records("bench.order").unwrap();
    let line_model = tx.env().records("bench.order.line").unwrap();

    // Virtual parent whose one2many and many2many children have persisted origins:
    let copy = tx
        .new_record(
            &order_model,
            erp_sdk::RawVals::new(),
            RecId::from_real(o1),
            None,
        )
        .await
        .unwrap();

    let lines = copy.related(&mut tx, "line_ids").await.unwrap();
    let origins = lines.origin().ids();
    assert_eq!(origins.len(), 3);
    let [l1_orig, l2_orig, l3_orig] = [origins[0], origins[1], origins[2]];

    // Virtual edit on line 0 (origin l1_orig): change code and qty distinct from stored values
    let line0 = lines.browse_ids([lines.rec_ids()[0]]);
    line0
        .set_value(&mut tx, "code", Value::str("ZEDITED_L1A"))
        .await
        .unwrap();
    line0
        .set_value(&mut tx, "qty", Value::Float(88.5))
        .await
        .unwrap();

    // Add an originless child to copy to confirm exclusion
    let originless_line = tx
        .new_record(
            &line_model,
            erp_sdk::RawVals::new()
                .set_cell("code", erp_sdk::Cell::Str("UNORIGINED".into()))
                .set_cell("qty", erp_sdk::Cell::Float(55.0)),
            None,
            None,
        )
        .await
        .unwrap();
    let with_fresh = lines.concat(&originless_line).unwrap();
    copy.assign(&mut tx, "line_ids", &with_fresh).await.unwrap();

    // 1. Without options (all included, no order/limit):
    // Included child rows expose their origin IDs and requested fields from virtual records (including edits)
    // BUG-080 fix: they never become ID-only because a virtual lookup key was compared to integer origin.
    // Originless children remain excluded.
    let spec_no_opts = Spec::new().with(
        "line_ids",
        SpecItem::fields(Spec::new().field("code").field("qty")),
    );
    let res = copy.web_read(&mut tx, spec_no_opts).await.unwrap();
    let row = res[0].dict().unwrap();
    let child_rows = as_list(&row.get("line_ids").unwrap());
    assert_eq!(
        child_rows.len(),
        3,
        "must omit originless child and keep all 3 origin children"
    );

    let c0 = child_rows[0].dict().unwrap();
    assert_eq!(c0.get("id").unwrap(), Value::Int(l1_orig));
    assert_eq!(c0.get("code").unwrap(), Value::str("ZEDITED_L1A"));
    assert_eq!(c0.get("qty").unwrap(), Value::Float(88.5));

    let c1 = child_rows[1].dict().unwrap();
    assert_eq!(c1.get("id").unwrap(), Value::Int(l2_orig));
    assert_eq!(c1.get("code").unwrap(), Value::str("L1b"));
    assert_eq!(c1.get("qty").unwrap(), Value::Float(1.0));

    let c2 = child_rows[2].dict().unwrap();
    assert_eq!(c2.get("id").unwrap(), Value::Int(l3_orig));
    assert_eq!(c2.get("code").unwrap(), Value::str("L1c"));
    assert_eq!(c2.get("qty").unwrap(), Value::Float(0.5));

    // 2. With limit = 1:
    // First line included with virtual edits; limited-out lines retain ID-only origin stubs
    let spec_lim1 = Spec::new().with(
        "line_ids",
        SpecItem::fields(Spec::new().field("code").field("qty")).limit(1),
    );
    let res = copy.web_read(&mut tx, spec_lim1).await.unwrap();
    let row = res[0].dict().unwrap();
    let child_rows = as_list(&row.get("line_ids").unwrap());
    assert_eq!(child_rows.len(), 3);

    // Included child:
    let c0 = child_rows[0].dict().unwrap();
    assert_eq!(c0.get("id").unwrap(), Value::Int(l1_orig));
    assert_eq!(c0.get("code").unwrap(), Value::str("ZEDITED_L1A"));
    assert_eq!(c0.get("qty").unwrap(), Value::Float(88.5));

    // Limited-out children are ID-only origin stubs:
    let c1 = child_rows[1].dict().unwrap();
    assert_eq!(c1.get("id").unwrap(), Value::Int(l2_orig));
    assert!(
        !c1.contains("code"),
        "limited-out child must be ID-only stub, got code: {:?}",
        c1.get_opt("code")
    );
    assert_eq!(c1.len(), 1, "limited-out child holds only its id");

    let c2 = child_rows[2].dict().unwrap();
    assert_eq!(c2.get("id").unwrap(), Value::Int(l3_orig));
    assert!(
        !c2.contains("code"),
        "limited-out child must be ID-only stub, got code: {:?}",
        c2.get_opt("code")
    );
    assert_eq!(c2.len(), 1, "limited-out child holds only its id");

    // 2b. With limit = 10 (>= the three children): every child is read, edits included
    let spec_lim_all = Spec::new().with(
        "line_ids",
        SpecItem::fields(Spec::new().field("code").field("qty")).limit(10),
    );
    let res = copy.web_read(&mut tx, spec_lim_all).await.unwrap();
    let row = res[0].dict().unwrap();
    let child_rows = as_list(&row.get("line_ids").unwrap());
    assert_eq!(child_rows.len(), 3);
    for (i, (want_orig, want_code)) in
        [(l1_orig, "ZEDITED_L1A"), (l2_orig, "L1b"), (l3_orig, "L1c")]
            .iter()
            .enumerate()
    {
        let c = child_rows[i].dict().unwrap();
        assert_eq!(c.get("id").unwrap(), Value::Int(*want_orig));
        assert_eq!(c.get("code").unwrap(), Value::str(want_code));
    }
    assert_eq!(
        child_rows[0].dict().unwrap().get("qty").unwrap(),
        Value::Float(88.5)
    );
    assert_no_negative_ids(&body(&res));

    // 3. With limit = 0:
    // ALL children are limited out: all retain ID-only origin stubs
    let spec_lim0 = Spec::new().with(
        "line_ids",
        SpecItem::fields(Spec::new().field("code").field("qty")).limit(0),
    );
    let res = copy.web_read(&mut tx, spec_lim0).await.unwrap();
    let row = res[0].dict().unwrap();
    let child_rows = as_list(&row.get("line_ids").unwrap());
    assert_eq!(child_rows.len(), 3);
    for (i, want_orig) in [l1_orig, l2_orig, l3_orig].iter().enumerate() {
        let c = child_rows[i].dict().unwrap();
        assert_eq!(c.get("id").unwrap(), Value::Int(*want_orig));
        assert!(
            !c.contains("code"),
            "limit 0: line {i} must be ID-only stub"
        );
        assert_eq!(c.len(), 1, "limit 0: line {i} holds only its id");
    }

    // 4. With order:
    // Order by "code desc": stored codes were "L1a", "L1b", "L1c", so the database search on the
    // origin ids gives L1c, L1b, L1a (the documented contract: the relation ordering is the
    // database's, over origin ids). The edited code "ZEDITED_L1A" sorts first if virtual values
    // were used for ordering, so this order tells the two apart; the row still shows the edit.
    let spec_order = Spec::new().with(
        "line_ids",
        SpecItem::fields(Spec::new().field("code").field("qty")).order("code desc"),
    );
    let res = copy.web_read(&mut tx, spec_order).await.unwrap();
    let row = res[0].dict().unwrap();
    let child_rows = as_list(&row.get("line_ids").unwrap());
    assert_eq!(child_rows.len(), 3);
    assert_eq!(
        child_rows[0].dict().unwrap().get("id").unwrap(),
        Value::Int(l3_orig)
    );
    assert_eq!(
        child_rows[0].dict().unwrap().get("code").unwrap(),
        Value::str("L1c")
    );
    assert_eq!(
        child_rows[1].dict().unwrap().get("id").unwrap(),
        Value::Int(l2_orig)
    );
    assert_eq!(
        child_rows[1].dict().unwrap().get("code").unwrap(),
        Value::str("L1b")
    );
    assert_eq!(
        child_rows[2].dict().unwrap().get("id").unwrap(),
        Value::Int(l1_orig)
    );
    assert_eq!(
        child_rows[2].dict().unwrap().get("code").unwrap(),
        Value::str("ZEDITED_L1A")
    );
    assert_eq!(
        child_rows[2].dict().unwrap().get("qty").unwrap(),
        Value::Float(88.5)
    );

    // 5. Many2many expansion with virtual parent, one tag edited virtually:
    let tags = copy.related(&mut tx, "tag_ids").await.unwrap();
    let tag_origins = tags.origin().ids();
    assert_eq!(tag_origins.len(), 2);
    tags.browse_ids([tags.rec_ids()[0]])
        .set_value(&mut tx, "code", Value::str("EDITED_T1"))
        .await
        .unwrap();
    let spec_m2m = Spec::new().with(
        "tag_ids",
        SpecItem::fields(Spec::new().field("code").field("name")),
    );
    let res = copy.web_read(&mut tx, spec_m2m).await.unwrap();
    let row = res[0].dict().unwrap();
    let tag_rows = as_list(&row.get("tag_ids").unwrap());
    assert_eq!(tag_rows.len(), 2);
    let t0 = tag_rows[0].dict().unwrap();
    assert_eq!(t0.get("id").unwrap(), Value::Int(tag_origins[0]));
    assert_eq!(t0.get("code").unwrap(), Value::str("EDITED_T1"));
    let t1 = tag_rows[1].dict().unwrap();
    assert_eq!(t1.get("id").unwrap(), Value::Int(tag_origins[1]));
    assert_eq!(t1.get("code").unwrap(), Value::str("T2"));
    assert_no_negative_ids(&body(&res));

    // 6. Shared children: second virtual copy of order o2 sharing T1 with copy.
    let copy2 = tx
        .new_record(
            &order_model,
            erp_sdk::RawVals::new(),
            RecId::from_real(o2),
            None,
        )
        .await
        .unwrap();
    let spec_m2m_shared = Spec::new().with(
        "tag_ids",
        SpecItem::fields(Spec::new().field("code").field("name")),
    );
    let res2 = copy2.web_read(&mut tx, spec_m2m_shared).await.unwrap();
    let row2 = res2[0].dict().unwrap();
    let tag_rows2 = as_list(&row2.get("tag_ids").unwrap());
    assert_eq!(tag_rows2.len(), 2);
    assert_eq!(
        tag_rows2[0].dict().unwrap().get("code").unwrap(),
        Value::str("EDITED_T1")
    );
    assert_eq!(
        tag_rows2[1].dict().unwrap().get("code").unwrap(),
        Value::str("T3")
    );
    // The edit is shared by the new records only: the stored T1 itself still reads its stored
    // code, so the shared value above is not a leak into the stored record's cache.
    let stored_tags = tags.origin();
    let stored_t1 = stored_tags.browse_ids([stored_tags.rec_ids()[0]]);
    let res_stored = stored_t1
        .web_read(&mut tx, Spec::new().field("code"))
        .await
        .unwrap();
    let stored_row = res_stored[0].dict().unwrap();
    assert_eq!(stored_row.get("id").unwrap(), Value::Int(tag_origins[0]));
    assert_eq!(stored_row.get("code").unwrap(), Value::str("T1"));

    // 7. Nested expansions: parent -> line_ids -> order_id (many2one back to order)
    let spec_nested = Spec::new().with(
        "line_ids",
        SpecItem::fields(
            Spec::new()
                .field("code")
                .with("order_id", SpecItem::fields(Spec::new().field("code"))),
        ),
    );
    let res = copy.web_read(&mut tx, spec_nested).await.unwrap();
    let row = res[0].dict().unwrap();
    let child_rows = as_list(&row.get("line_ids").unwrap());
    assert_eq!(child_rows.len(), 3);
    let c0 = child_rows[0].dict().unwrap();
    assert_eq!(c0.get("code").unwrap(), Value::str("ZEDITED_L1A"));
    let p_val = c0.get("order_id").unwrap();
    let parent_in_child = p_val.dict().unwrap();
    assert_eq!(parent_in_child.get("id").unwrap(), Value::Int(o1));
    assert_eq!(parent_in_child.get("code").unwrap(), Value::str("O1"));

    tx.rollback().await.unwrap();
}

/// Semantic query-count contract (not a benchmark): a raw x2many web_read of records with no
/// new id sends the statements a plain raw read of the same fields sends, so the shared
/// normalisation path adds none.
#[tokio::test]
async fn raw_x2many_web_read_adds_no_statement_for_real_ids() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let [o1, o2] = fixture(&mut tx).await;
    tx.commit().await.unwrap();

    let mut tx = h.begin().await.unwrap();
    let orders = bench_fixture::orders(&tx, &[o1, o2]);

    tx.invalidate_all().await.unwrap();
    let before = tx.query_count();
    let plain = orders
        .read_with(
            &mut tx,
            &["name", "line_ids", "tag_ids"],
            erp_orm::Load::Raw,
        )
        .await
        .unwrap();
    let plain_statements = tx.query_count() - before;
    assert_eq!(plain.len(), 2);

    tx.invalidate_all().await.unwrap();
    let before = tx.query_count();
    let got = orders
        .web_read(
            &mut tx,
            Spec::new().field("name").field("line_ids").field("tag_ids"),
        )
        .await
        .unwrap();
    let web_statements = tx.query_count() - before;
    assert_eq!(got.len(), 2);
    assert!(plain_statements >= 1, "the plain read must query");
    assert_eq!(
        web_statements, plain_statements,
        "raw x2many web_read statements vs plain raw read"
    );
    let first = got[0].dict().unwrap();
    assert_eq!(as_list(&first.get("line_ids").unwrap()).len(), 3);
    tx.rollback().await.unwrap();
}
