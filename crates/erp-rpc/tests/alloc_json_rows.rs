//! Allocation budget of writing a result batch as JSON (design note,
//! §10): orders with 10 lines each, integers, text, floats, decimals, dates, datetimes, raw bytes,
//! id lists, many2one labels, many2one sub-rows and indexed x2many children, written with no
//! allocation per order, line or value; only the output buffer grows. One test in its own binary,
//! run on the main thread by `erp_alloc_budget::harness` (`erp_alloc_budget`: one profiler, all
//! threads counted).
#![allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]

use std::sync::Arc;

use erp_alloc_budget::{self as alloc, AllocStats, Budget};
use erp_fields::rows::{Children, Column, ColumnData, Key, Layout, Rows, RowsBuilder, Targets};
use erp_fields::{Date, DateTime, Decimal, RecId, Value};

#[global_allocator]
static ALLOC: alloc::Alloc = alloc::Alloc;

alloc::harness!(
    #[ignore = "DEFERRED_PERFORMANCE: milestone end"]
    writing_a_batch_allocates_only_the_buffer,
    writing_a_batch_preserves_json,
);

const LINES_PER_ORDER: usize = 10;
const PARTNERS: usize = 10;

fn layout(model: &'static str, keys: &[&'static str]) -> Arc<Layout> {
    Layout::for_model(model, keys.iter().map(|k| Key::from(*k))).unwrap()
}

fn orders(n: usize) -> Rows {
    let lines = n * LINES_PER_ORDER;
    let mut b = RowsBuilder::new(
        layout(
            "bench.order.line",
            &["id", "code", "qty", "price_unit", "price_subtotal"],
        ),
        lines,
    )
    .unwrap();
    b.column(
        "id",
        Column::new(ColumnData::Int((1..=lines as i64).collect())),
    )
    .unwrap();
    b.column(
        "code",
        Column::new(ColumnData::Str(
            (0..lines)
                .map(|i| Some(Arc::from(format!("L{i:08}"))))
                .collect(),
        )),
    )
    .unwrap();
    b.column(
        "qty",
        Column::new(ColumnData::Float((0..lines).map(|i| i as f64).collect())),
    )
    .unwrap();
    b.column(
        "price_unit",
        Column::new(ColumnData::Float(
            (0..lines).map(|i| i as f64 * 0.25).collect(),
        )),
    )
    .unwrap();
    b.column(
        "price_subtotal",
        Column::new(ColumnData::Decimal(
            (0..lines)
                .map(|i| Decimal::new(i as i64 * 125, 2))
                .collect(),
        )),
    )
    .unwrap();
    let line_rows = b.finish().unwrap();

    // ten partners, shared by the orders' many2one sub-rows and their indexed followers
    let mut b = RowsBuilder::new(layout("res.partner", &["id", "name"]), PARTNERS).unwrap();
    b.column(
        "id",
        Column::new(ColumnData::Int((1..=PARTNERS as i64).collect())),
    )
    .unwrap();
    b.column(
        "name",
        Column::new(ColumnData::Str(
            (0..PARTNERS)
                .map(|i| Some(Arc::from(format!("Partner {i}"))))
                .collect(),
        )),
    )
    .unwrap();
    let partners = b.finish().unwrap();

    let day = Date::from_ymd(2026, 10, 19).unwrap();
    let tags: Arc<[RecId]> = (1..=3).map(|i| RecId::from_real(i).unwrap()).collect();
    let mut b = RowsBuilder::new(
        layout(
            "bench.order",
            &[
                "id",
                "name",
                "date_order",
                "validity",
                "signature",
                "tag_ids",
                "user_id",
                "partner_id",
                "follower_ids",
                "line_ids",
            ],
        ),
        n,
    )
    .unwrap();
    b.column("id", Column::new(ColumnData::Int((1..=n as i64).collect())))
        .unwrap();
    b.column(
        "name",
        Column::new(ColumnData::Str(
            (0..n)
                .map(|i| Some(Arc::from(format!("SO{i:07}"))))
                .collect(),
        )),
    )
    .unwrap();
    b.column(
        "date_order",
        Column::new(ColumnData::DateTime(
            (0..n)
                .map(|i| DateTime::from_micros(i as i64 * 1_000_000_007))
                .collect(),
        )),
    )
    .unwrap();
    b.column("validity", Column::new(ColumnData::Date(vec![day; n])))
        .unwrap();
    b.column(
        "signature",
        Column::new(ColumnData::Bytes(
            (0..n)
                .map(|i| Some(Arc::from(&(i as u64).to_le_bytes()[..])))
                .collect(),
        )),
    )
    .unwrap();
    b.column("tag_ids", Column::new(ColumnData::Ids(vec![Some(tags); n])))
        .unwrap();
    b.column(
        "user_id",
        Column::new(ColumnData::Label {
            ids: vec![2; n],
            names: vec![Some(Arc::from("Mitchell Admin")); n],
        }),
    )
    .unwrap();
    b.column(
        "partner_id",
        Column::new(ColumnData::One(
            Targets::new(
                partners.clone(),
                (0..n).map(|i| (i % PARTNERS) as u32).collect(),
            )
            .unwrap(),
        )),
    )
    .unwrap();
    b.column(
        "follower_ids",
        Column::new(ColumnData::Many(
            Children::indexed(
                partners,
                (0..=n).map(|o| (o * 2) as u32).collect(),
                (0..n)
                    .flat_map(|i| [((i + 1) % PARTNERS) as u32, (i % PARTNERS) as u32])
                    .collect(),
            )
            .unwrap(),
        )),
    )
    .unwrap();
    let offsets = (0..=n).map(|o| (o * LINES_PER_ORDER) as u32).collect();
    b.column(
        "line_ids",
        Column::new(ColumnData::Many(
            Children::contiguous(line_rows, offsets).unwrap(),
        )),
    )
    .unwrap();
    b.finish().unwrap()
}

fn write(n: usize) -> (AllocStats, usize) {
    let value = Value::from(orders(n));
    let (stats, body) =
        alloc::measure(|| erp_rpc::__output::serialize(&value, usize::MAX)).expect("measure");
    let body = body.unwrap();
    let first = br#"[{"id":1,"name":"SO0000000","date_order":"1970-01-01 00:00:00","validity":"2026-10-19","signature":"AAAAAAAAAAA=","tag_ids":[1,2,3],"user_id":[2,"Mitchell Admin"],"partner_id":{"id":1,"name":"Partner 0"},"follower_ids":[{"id":2,"name":"Partner 1"},{"id":1,"name":"Partner 0"}],"line_ids":[{"id":1,"code":"L00000000""#;
    assert!(
        body.starts_with(first),
        "{}",
        String::from_utf8_lossy(&body[..first.len().min(body.len())])
    );
    (stats, body.len())
}

fn writing_a_batch_allocates_only_the_buffer() {
    let (small, small_len) = write(1_000);
    let (large, large_len) = write(2_000);
    println!("json: {small:?} ({small_len} B) / {large:?} ({large_len} B)");
    // no allocation per order or line: twice the rows cost at most one more buffer doubling
    assert!(
        large.total_blocks <= small.total_blocks + 1,
        "{small:?} {large:?}"
    );
    // perf-rows §10: ≤ 40 blocks in all (buffer growth), and the growth requests stay within
    // what doubling costs, never bytes per value. The doublings sum to under twice the final
    // capacity, and the capacity is under twice the body, so 4× the body is the asymptotic worst
    // case (reached when the body ends just past a power of two); the 64 KiB headroom covers the
    // small first capacities and the rounding of that bound (measured: 2,379,950 B body →
    // 8,388,360 B requested, 3.5×)
    let budget = Budget::new(40, 4 * large_len as u64 + 64 * 1024);
    small.assert_within(&budget);
    large.assert_within(&budget);
    // the budget can fail: a budget of no allocation at all is refused
    assert!(large.check(&Budget::new(0, 0)).is_err());
}

fn writing_a_batch_preserves_json() {
    for n in [1_000, 2_000] {
        let value = Value::from(orders(n));
        let body = erp_rpc::__output::serialize(&value, usize::MAX).unwrap();
        let first = br#"[{"id":1,"name":"SO0000000","date_order":"1970-01-01 00:00:00","validity":"2026-10-19","signature":"AAAAAAAAAAA=","tag_ids":[1,2,3],"user_id":[2,"Mitchell Admin"],"partner_id":{"id":1,"name":"Partner 0"},"follower_ids":[{"id":2,"name":"Partner 1"},{"id":1,"name":"Partner 0"}],"line_ids":[{"id":1,"code":"L00000000""#;
        assert!(
            body.starts_with(first),
            "{}",
            String::from_utf8_lossy(&body[..first.len().min(body.len())])
        );
        let parsed = erp_fields::json::parse(&body, 64).unwrap();
        assert!(matches!(parsed, erp_fields::json::Json::Array(rows) if rows.len() == n));
    }
}
