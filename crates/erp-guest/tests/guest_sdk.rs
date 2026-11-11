//! Guest-side reading, buffering and caching of the addon contract.
//!
//! These run natively: the `orm-host` imports are not linked outside a component and would trap if
//! called, so a test that iterates a store also shows it never reaches the host.

#![allow(
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_used,
    reason = "test assertions"
)]

use erp_guest::{
    BoundaryErrorKind, ColumnData, ColumnKind, ColumnStore, ColumnVector, EpochCache, RecordBatch,
    ScalarValue, WriteBuffer, X2manyCommand,
};

fn column(field: &str, data: ColumnData) -> ColumnVector {
    ColumnVector {
        field: field.to_owned(),
        data,
    }
}

fn books() -> RecordBatch {
    RecordBatch {
        model: "sample_library.book".to_owned(),
        ids: vec![7, 3, 9],
        columns: vec![
            column(
                "title",
                ColumnData::ColString(vec!["Dune".into(), "Emma".into(), "Ulysses".into()]),
            ),
            column(
                "author_id",
                ColumnData::ColOptId(vec![Some(11), None, Some(12)]),
            ),
            column("available", ColumnData::ColBool(vec![true, false, true])),
            column(
                "tag_ids",
                ColumnData::ColIds(vec![vec![1, 2], vec![], vec![2]]),
            ),
        ],
    }
}

#[test]
fn a_store_reads_typed_columns_row_by_row_without_the_host() {
    let store = ColumnStore::new(books(), 4).expect("a well-formed batch is accepted");
    assert_eq!(store.model(), "sample_library.book");
    assert_eq!(store.ids(), [7, 3, 9]);
    assert_eq!(store.len(), 3);
    assert_eq!(store.epoch(), 4);
    assert_eq!(
        store.fields().collect::<Vec<_>>(),
        ["title", "author_id", "available", "tag_ids"]
    );
    let titles = store.strings("title").expect("title is a string column");
    let authors = store
        .opt_ids("author_id")
        .expect("author_id is an id column");
    let available = store
        .bools("available")
        .expect("available is a bool column");
    let tags = store.id_lists("tag_ids").expect("tag_ids is an ids column");
    let seen: Vec<(i64, &str, Option<i64>, bool, usize)> = store
        .rows()
        .map(|row| {
            let i = row.index();
            (
                row.id(),
                titles[i].as_str(),
                authors[i],
                available[i],
                tags[i].len(),
            )
        })
        .collect();
    assert_eq!(
        seen,
        [
            (7, "Dune", Some(11), true, 2),
            (3, "Emma", None, false, 0),
            (9, "Ulysses", Some(12), true, 1)
        ]
    );
    assert_eq!(store.kind("tag_ids").expect("present"), ColumnKind::Ids);
}

#[test]
fn a_column_of_the_wrong_length_or_a_repeated_field_is_refused() {
    let mut short = books();
    short.columns[1] = column("author_id", ColumnData::ColOptId(vec![Some(11)]));
    let err = ColumnStore::new(short, 0).expect_err("a short column is refused");
    assert_eq!(err.kind, BoundaryErrorKind::InvalidArgument);
    assert_eq!(
        err.message,
        "record batch of model 'sample_library.book': column 'author_id' holds 1 values for 3 ids"
    );

    let mut twice = books();
    twice.columns.push(column(
        "title",
        ColumnData::ColString(vec!["a".into(), "b".into(), "c".into()]),
    ));
    let err = ColumnStore::new(twice, 0).expect_err("a repeated field is refused");
    assert_eq!(err.kind, BoundaryErrorKind::InvalidArgument);
    assert_eq!(
        err.message,
        "record batch of model 'sample_library.book': column 'title' appears twice"
    );
}

#[test]
fn an_empty_batch_is_a_store_without_rows() {
    let store = ColumnStore::new(
        RecordBatch {
            model: "res.partner".into(),
            ids: vec![],
            columns: vec![column("name", ColumnData::ColString(vec![]))],
        },
        1,
    )
    .expect("an empty batch with empty columns is well formed");
    assert!(store.is_empty());
    assert_eq!(store.rows().count(), 0);
    assert!(store.strings("name").expect("present").is_empty());
}

#[test]
fn reading_a_missing_column_or_the_wrong_kind_fails_with_its_name() {
    let store = ColumnStore::new(books(), 0).expect("well formed");
    let err = store.strings("isbn").expect_err("isbn was not read");
    assert_eq!(err.kind, BoundaryErrorKind::NotFound);
    assert_eq!(
        err.message,
        "record batch of model 'sample_library.book' holds no column 'isbn'"
    );

    let err = store
        .opt_ints("available")
        .expect_err("available is a bool column");
    assert_eq!(err.kind, BoundaryErrorKind::InvalidArgument);
    assert_eq!(
        err.message,
        "column 'available' of model 'sample_library.book' is col-bool, not col-opt-int"
    );

    let err = store
        .strings("author_id")
        .expect_err("author_id is an id column");
    assert_eq!(
        err.message,
        "column 'author_id' of model 'sample_library.book' is col-opt-id, not col-string"
    );
}

#[test]
fn a_store_gives_its_batch_back_unchanged() {
    let store = ColumnStore::new(books(), 0).expect("well formed");
    assert_eq!(store.into_batch(), books());
}

#[test]
fn assignments_become_one_column_write_per_group_of_records() {
    let mut buffer = WriteBuffer::new("sample_library.loan");
    buffer
        .set(5, "state", ScalarValue::ValStr("out".into()))
        .expect("set");
    buffer
        .set(5, "due_date", ScalarValue::ValDate(20_400))
        .expect("set");
    buffer
        .set(6, "state", ScalarValue::ValStr("out".into()))
        .expect("set");
    buffer
        .set(6, "due_date", ScalarValue::ValNull)
        .expect("set");
    buffer
        .set(8, "state", ScalarValue::ValStr("returned".into()))
        .expect("set");
    // A second assignment to the same field of the same record replaces the first.
    buffer
        .set(5, "state", ScalarValue::ValStr("draft".into()))
        .expect("set");
    let writes = buffer.into_writes().expect("all values fit their columns");
    assert_eq!(writes.model, "sample_library.loan");
    assert!(writes.batches.is_empty());
    assert_eq!(
        writes.columns.len(),
        2,
        "records 5 and 6 share their fields; record 8 has its own"
    );
    assert_eq!(writes.columns[0].ids, [5, 6]);
    assert_eq!(
        writes.columns[0].columns,
        [
            column("due_date", ColumnData::ColOptDate(vec![Some(20_400), None])),
            column(
                "state",
                ColumnData::ColString(vec!["draft".into(), "out".into()])
            ),
        ]
    );
    assert_eq!(writes.columns[1].ids, [8]);
    assert_eq!(
        writes.columns[1].columns,
        [column(
            "state",
            ColumnData::ColString(vec!["returned".into()])
        )]
    );
}

#[test]
fn a_value_for_all_records_is_one_batch_write_and_carries_x2many_commands() {
    let mut buffer = WriteBuffer::new("sample_library.book");
    buffer
        .set_all(
            &[7, 9],
            vec![
                ("available".into(), ScalarValue::ValBool(false)),
                (
                    "tag_ids".into(),
                    ScalarValue::ValX2many(vec![X2manyCommand::Link(4), X2manyCommand::Unlink(2)]),
                ),
            ],
        )
        .expect("distinct fields");
    buffer
        .set(7, "publication_year", ScalarValue::ValInt(1965))
        .expect("set");
    let writes = buffer.into_writes().expect("fits");
    assert_eq!(writes.batches.len(), 1);
    assert_eq!(writes.batches[0].ids, [7, 9]);
    assert_eq!(writes.batches[0].values.len(), 2);
    assert_eq!(writes.columns.len(), 1);
    assert_eq!(
        writes.columns[0].columns,
        [column("publication_year", ColumnData::ColInt(vec![1965]))]
    );
}

#[test]
fn a_field_left_empty_on_every_record_of_a_group_is_a_batch_write_of_an_empty_value() {
    let mut buffer = WriteBuffer::new("sample_library.loan");
    buffer
        .set(1, "return_date", ScalarValue::ValNull)
        .expect("set");
    buffer
        .set(2, "return_date", ScalarValue::ValNull)
        .expect("set");
    let writes = buffer.into_writes().expect("fits");
    assert!(writes.columns.is_empty(), "an all-empty column has no type");
    assert_eq!(writes.batches.len(), 1);
    assert_eq!(writes.batches[0].ids, [1, 2]);
    assert_eq!(
        writes.batches[0].values,
        [("return_date".to_owned(), ScalarValue::ValNull)]
    );
}

#[test]
fn values_of_two_kinds_for_one_field_are_refused_with_the_field_name() {
    let mut buffer = WriteBuffer::new("sample_library.book");
    buffer
        .set(1, "publication_year", ScalarValue::ValInt(1999))
        .expect("set");
    buffer
        .set(2, "publication_year", ScalarValue::ValStr("1999".into()))
        .expect("set");
    let err = buffer.into_writes().expect_err("mixed kinds are refused");
    assert_eq!(err.kind, BoundaryErrorKind::InvalidArgument);
    assert_eq!(
        err.message,
        "field 'publication_year' of model 'sample_library.book' mixes val-int and val-str values"
    );
}

#[test]
fn per_record_x2many_commands_and_empty_x2many_values_are_refused() {
    let mut buffer = WriteBuffer::new("sample_library.book");
    let err = buffer
        .set(
            1,
            "tag_ids",
            ScalarValue::ValX2many(vec![X2manyCommand::Clear]),
        )
        .expect_err("x2many commands go through set_all");
    assert_eq!(err.kind, BoundaryErrorKind::InvalidArgument);
    assert_eq!(
        err.message,
        "field 'tag_ids' of model 'sample_library.book': x2many commands are not written per record; use set_all"
    );

    buffer
        .set(1, "tag_ids", ScalarValue::ValIds(vec![3]))
        .expect("set");
    buffer.set(2, "tag_ids", ScalarValue::ValNull).expect("set");
    let err = buffer
        .into_writes()
        .expect_err("an empty x2many value is refused");
    assert_eq!(err.kind, BoundaryErrorKind::InvalidArgument);
    assert!(err.message.starts_with(
        "field 'tag_ids' of model 'sample_library.book': an x2many value is a list of ids"
    ));
}

#[test]
fn a_batch_assigning_one_field_twice_is_refused() {
    let mut buffer = WriteBuffer::new("res.partner");
    let err = buffer
        .set_all(
            &[1],
            vec![
                ("name".into(), ScalarValue::ValStr("a".into())),
                ("name".into(), ScalarValue::ValStr("b".into())),
            ],
        )
        .expect_err("one field twice");
    assert_eq!(err.kind, BoundaryErrorKind::InvalidArgument);
    assert_eq!(
        err.message,
        "field 'name' of model 'res.partner' is assigned twice in one batch"
    );
}

#[test]
fn an_untouched_buffer_sends_nothing() {
    let buffer = WriteBuffer::new("res.partner");
    assert!(buffer.is_empty());
    assert!(buffer.into_writes().expect("nothing to check").is_empty());
}

#[test]
fn cached_columns_are_dropped_when_the_epoch_changes() {
    let mut cache: EpochCache<String, ColumnStore> = EpochCache::new();
    assert_eq!(cache.epoch(), None);
    let store = ColumnStore::new(books(), 4).expect("well formed");
    cache.insert(4, "sample_library.book".into(), store.clone());
    assert_eq!(
        cache.get(4, &"sample_library.book".into()),
        Some(&store),
        "same epoch: the columns are reused"
    );
    assert!(!cache.begin(4), "nothing is dropped under the same epoch");
    assert!(cache.begin(5), "a new epoch drops the held columns");
    assert!(cache.is_empty());
    assert_eq!(cache.get(5, &"sample_library.book".into()), None);
    assert_eq!(cache.epoch(), Some(5));
    cache.insert(6, "res.partner".into(), store);
    assert_eq!(cache.len(), 1);
    assert_eq!(
        cache.get(4, &"res.partner".into()),
        None,
        "an older epoch is a change too"
    );
}
