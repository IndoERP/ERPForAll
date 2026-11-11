//! Allocation pin and semantics of streaming import extraction (PERF.LOAD-STREAM).
//! Pure: no database.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::print_stdout,
    clippy::panic,
    reason = "tests"
)]

use erp_alloc_budget::{self as alloc, Budget};
use erp_data::internal_impex::{Key, Raw, extract_batch};
use erp_registry::{AddonDescriptor, FieldDecl, ModelDecl, Registry, RegistryBuilder};

#[global_allocator]
static ALLOC: alloc::Alloc = alloc::Alloc;

alloc::harness!(
    #[ignore = "DEFERRED_PERFORMANCE: milestone end"]
    load_stream_allocation_measurement,
    load_stream_semantics,
);

const PIN: Budget = Budget::new(PIN_BLOCKS, PIN_BYTES);
// TODO-UNMEASURED: PIN_BLOCKS and PIN_BYTES are placeholders, not measured values. They gate only
// the DEFERRED_PERFORMANCE test; defers measuring them to the milestone-end phase.
const PIN_BLOCKS: u64 = 5_000;
const PIN_BYTES: u64 = 500_000;

static ADDON: AddonDescriptor = AddonDescriptor {
    name: "test_stream",
    depends: &["base"],
    exact_only: false,
};

const PARENT_FIELDS: &[FieldDecl] = &[
    FieldDecl::char("name"),
    FieldDecl::one2many("line_ids", "test.line", "parent_id"),
];

const LINE_FIELDS: &[FieldDecl] = &[
    FieldDecl::integer("value"),
    FieldDecl::many2one("parent_id", "test.parent"),
];

const DECLS: &[ModelDecl] = &[
    ModelDecl::new("test_stream", "test.parent")
        .description("Parent")
        .fields(PARENT_FIELDS),
    ModelDecl::new("test_stream", "test.line")
        .description("Line")
        .fields(LINE_FIELDS),
];

fn registry() -> Registry {
    RegistryBuilder::new([&ADDON])
        .linked(false)
        .with_decls(DECLS.iter())
        .build()
        .unwrap()
}

fn load_stream_semantics() {
    let reg = registry();
    let spec = reg.model("test.parent").unwrap();
    let paths = [
        vec![Key::field_name("name")],
        vec![Key::field_name("line_ids"), Key::field_name("value")],
    ];

    // 2500 rows, each parent spanning 2 rows (1250 parent records, each with 2 lines)
    let mut data: Vec<Vec<String>> = Vec::with_capacity(2500);
    for i in 0..1250 {
        data.push(vec![format!("P_{i}"), "10".to_owned()]);
        data.push(vec![String::new(), "20".to_owned()]);
    }

    let rows: &[Vec<String>] = &data;
    let batch_size = 500;
    let mut current_row = 0;
    let mut total_records = 0;
    let mut batch_count = 0;

    while current_row < rows.len() {
        let (batch, next_row) = extract_batch(
            &reg,
            spec,
            &paths,
            rows,
            current_row,
            batch_size,
            usize::MAX,
        )
        .unwrap();

        if batch.is_empty() {
            break;
        }

        assert!(batch.len() <= batch_size, "batch size must be bounded");
        total_records += batch.len();
        batch_count += 1;

        for (b_idx, item) in batch.iter().enumerate() {
            let record_idx = (batch_count - 1) * batch_size + b_idx;
            assert_eq!(item.rows.from, record_idx * 2);
            assert_eq!(item.rows.to, record_idx * 2 + 1);

            let name_val = item.record.get_field("name").unwrap();
            let expected_name = format!("P_{record_idx}");
            assert_eq!(name_val, &Raw::Text(&expected_name));

            let lines_val = item.record.get_field("line_ids").unwrap();
            let lines = match lines_val {
                Raw::Records(lines) => lines.as_slice(),
                _ => &[],
            };
            assert_eq!(lines.len(), 2);
            assert_eq!(lines[0].get_field("value"), Some(&Raw::Text("10")));
            assert_eq!(lines[1].get_field("value"), Some(&Raw::Text("20")));
        }

        current_row = next_row;
    }

    assert_eq!(total_records, 1250);
    assert_eq!(batch_count, 3); // 500 + 500 + 250
    assert_eq!(current_row, 2500);

    // Limit check: row limit 1 spans 1 record (2 rows) and returns nextrow 2
    let (limited_batch, next_row) =
        extract_batch(&reg, spec, &paths, rows, 0, batch_size, 1).unwrap();
    assert_eq!(limited_batch.len(), 1);
    assert_eq!(next_row, 2);
}

fn load_stream_allocation_measurement() {
    let reg = registry();
    let spec = reg.model("test.parent").unwrap();
    let paths = [
        vec![Key::field_name("name")],
        vec![Key::field_name("line_ids"), Key::field_name("value")],
    ];

    let mut data: Vec<Vec<String>> = Vec::with_capacity(1000);
    for i in 0..500 {
        data.push(vec![format!("P_{i}"), "10".to_owned()]);
        data.push(vec![String::new(), "20".to_owned()]);
    }
    let rows: &[Vec<String>] = &data;

    let (stats, batch) =
        alloc::measure(|| extract_batch(&reg, spec, &paths, rows, 0, 500, usize::MAX).unwrap())
            .expect("measure");
    drop(batch);

    stats.assert_within(&PIN);
}
