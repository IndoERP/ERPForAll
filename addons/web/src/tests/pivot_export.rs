// SPDX-License-Identifier: MIT OR Apache-2.0
//! Unit tests of `pivot_export.rs` (kept in a test path: the staging guard counts inline test modules as production).
use super::*;

const LIMITS: Limits = Limits {
    cells: 1000,
    header_depth: 8,
    columns: 50,
    label_bytes: 64,
    request_bytes: 1 << 20,
};

fn nested() -> Json {
    serde_json::json!({
        "model": "res.partner", "title": "Partners",
        "headers": [
            [{"label": "Country", "rowspan": 3}, {"label": "Total", "colspan": 4, "bold": true}, {"label": "Total", "colspan": 2, "rowspan": 2, "bold": true}],
            [{"label": "Company", "colspan": 2}, {"label": "Person", "colspan": 2}],
            [{"label": "Count"}, {"label": "Score"}, {"label": "Count"}, {"label": "Score"}, {"label": "Count", "bold": true}, {"label": "Score", "bold": true}]
        ],
        "rows": [
            {"label": "Total", "bold": true, "cells": [
                {"kind": "integer", "value": 3, "bold": true}, {"kind": "float", "value": 2.5, "digits": 2, "bold": true},
                {"kind": "integer", "value": 1, "bold": true}, {"kind": "empty", "bold": true},
                {"kind": "integer", "value": 4, "bold": true}, {"kind": "monetary", "value": 2.5, "digits": 2, "bold": true}]},
            {"label": "Indonesia", "indent": 1, "cells": [
                {"kind": "integer", "value": 3}, {"kind": "float", "value": 2.5, "digits": 2}, {"kind": "integer", "value": 1}, {"kind": "empty"},
                {"kind": "integer", "value": 4, "bold": true}, {"kind": "monetary", "value": 2.5, "digits": 2, "bold": true}]}
        ]
    })
}

#[test]
fn a_nested_document_parses_to_its_headers_and_cells() {
    let document = parse(&nested(), &LIMITS).unwrap();
    assert_eq!(document.headers.len(), 3);
    assert_eq!(document.headers[0][2].rowspan, 2);
    assert_eq!(document.rows[0].cells[3].value, CellValue::Empty);
    assert_eq!(document.rows[1].cells[5].value, CellValue::Decimal(2.5, 2));
    assert_eq!(document.rows[1].indent, 1);
}

fn refused(mut value: Json, edit: impl FnOnce(&mut Json)) -> String {
    edit(&mut value);
    parse(&value, &LIMITS).unwrap_err().to_string()
}

#[test]
fn a_malformed_document_is_refused_naming_the_path() {
    let path =
        |message: String, at: &str| assert!(message.contains(at), "{message} should name {at}");
    path(
        refused(nested(), |v| {
            v["rows"][1]["cells"][2]["kind"] = "text".into()
        }),
        "$.rows[1].cells[2].kind",
    );
    path(
        refused(nested(), |v| v["rows"][0]["cells"][0]["value"] = "3".into()),
        "$.rows[0].cells[0].value",
    );
    path(
        refused(nested(), |v| {
            v["rows"][0]["cells"][1]
                .as_object_mut()
                .unwrap()
                .remove("digits");
        }),
        "$.rows[0].cells[1].digits",
    );
    path(
        refused(nested(), |v| v["rows"][0]["cells"][3]["value"] = 1.into()),
        "$.rows[0].cells[3].value",
    );
    path(
        refused(nested(), |v| v["rows"][0]["extra"] = 1.into()),
        "$.rows[0].extra",
    );
    path(
        refused(nested(), |v| v["headers"][1][0]["colspan"] = "2".into()),
        "$.headers[1][0].colspan",
    );
    path(refused(nested(), |v| v["title"] = 5.into()), "$.title");
    path(
        refused(nested(), |v| {
            v["rows"][1]["cells"].as_array_mut().unwrap().pop();
        }),
        "$.rows[1].cells",
    );
    path(
        refused(nested(), |v| {
            v["rows"][0]["cells"][0]["value"] = 9_007_199_254_740_993_i64.into()
        }),
        "$.rows[0].cells[0].value",
    );
    path(
        refused(nested(), |v| v["rows"][0]["bold"] = "yes".into()),
        "$.rows[0].bold",
    );
}

#[test]
fn header_cells_must_tile_the_header_block() {
    let wide = refused(nested(), |v| v["headers"][0][1]["colspan"] = 5.into());
    assert!(
        wide.contains("$.headers[0][1]") || wide.contains("$.headers[0][2]"),
        "{wide}"
    );
    let missing = refused(nested(), |v| {
        v["headers"][2].as_array_mut().unwrap().pop();
    });
    assert!(
        missing.contains("$.headers[2]") && missing.contains("no header cell"),
        "{missing}"
    );
    let tall = refused(nested(), |v| v["headers"][0][0]["rowspan"] = 4.into());
    assert!(tall.contains("$.headers[0][0]"), "{tall}");
    let overlap = refused(nested(), |v| v["headers"][1][0]["rowspan"] = 2.into());
    assert!(overlap.contains("$.headers"), "{overlap}");
}

#[test]
fn each_bound_is_refused_by_the_name_of_its_setting() {
    let check = |limits: Limits, key: &str| {
        let message = parse(&nested(), &limits).unwrap_err().to_string();
        assert!(message.contains(key), "{message} should name {key}");
    };
    check(
        Limits {
            cells: 34,
            ..LIMITS
        },
        MAX_CELLS,
    ); // 7 columns x (3 header + 2 body rows) = 35
    check(
        Limits {
            header_depth: 2,
            ..LIMITS
        },
        MAX_HEADER_DEPTH,
    );
    check(
        Limits {
            columns: 5,
            ..LIMITS
        },
        MAX_COLUMNS,
    );
    check(
        Limits {
            label_bytes: 6,
            ..LIMITS
        },
        MAX_LABEL_BYTES,
    );
    parse(
        &nested(),
        &Limits {
            cells: 35,
            header_depth: 3,
            columns: 6,
            label_bytes: 9,
            ..LIMITS
        },
    )
    .unwrap();
}

#[test]
fn the_declared_bounds_are_valid_settings_with_default_between_floor_and_hard_bound() {
    let declared = settings().unwrap();
    assert_eq!(declared.len(), 5);
    for bound in &BOUNDS {
        assert!(
            bound.floor <= bound.default && bound.default <= bound.hard,
            "{}",
            bound.key
        );
    }
}

#[test]
fn a_header_of_the_largest_span_and_depth_is_refused_by_the_cells_setting_before_the_grid_is_built()
{
    let wide = || serde_json::json!({"label": "x", "colspan": 16_384});
    let document = serde_json::json!({
        "model": "res.partner", "title": "T",
        "headers": (0..64).map(|_| vec![wide()]).collect::<Vec<_>>(),
        "rows": [{"label": "r", "cells": (0..16_383).map(|_| serde_json::json!({"kind": "empty"})).collect::<Vec<_>>()}]
    });
    // The same document tiles perfectly under limits that admit it: only the cells bound can refuse it below.
    let admitting = Limits {
        cells: 2_000_000,
        header_depth: 64,
        columns: 16_383,
        label_bytes: 64,
        request_bytes: 1 << 30,
    };
    assert!(parse(&document, &admitting).is_ok());
    let message = parse(
        &document,
        &Limits {
            cells: 100_000,
            ..admitting
        },
    )
    .unwrap_err()
    .to_string();
    assert!(
        message.contains(MAX_CELLS) && !message.contains("$.headers"),
        "{message}"
    );
    // a header that would not tile is also refused by the bound first: nothing was laid out
    let mut broken = document.clone();
    broken["headers"][3][0]["colspan"] = 99.into();
    let message = parse(
        &broken,
        &Limits {
            cells: 100_000,
            ..admitting
        },
    )
    .unwrap_err()
    .to_string();
    assert!(message.contains(MAX_CELLS), "{message}");
}

#[test]
fn a_span_past_the_declared_columns_is_refused_by_path() {
    let message = refused(nested(), |v| v["headers"][1][1]["colspan"] = 16_384.into());
    assert!(
        message.contains("$.headers[1][1]") && message.contains("of 7"),
        "{message}"
    );
    let message = refused(nested(), |v| v["headers"][0][0]["colspan"] = 7.into());
    assert!(
        message.contains("$.headers[0][1]") || message.contains("$.headers[0][0]"),
        "{message}"
    );
}
