//! Lazy variant combination iteration and the 1000-variant limit.
#![allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]

mod support;

use erp_addon_product::variant_generator::{
    Combinations, MAX_EXAMINED_COMBINATIONS, VARIANT_LIMIT, valid_combinations,
};
use erp_sdk::prelude::*;
use std::collections::HashMap;

fn lists(count: usize, width: usize) -> Vec<Vec<i64>> {
    (0..count)
        .map(|l| (0..width).map(|v| (l * width + v + 1) as i64).collect())
        .collect()
}

/// `test_template_with_too_many_combinations_is_refused`)
#[test]
fn the_product_space_is_never_materialized() {
    // 10^30 combinations: building them would never finish nor fit in memory
    let big = lists(30, 10);
    let first: Vec<Vec<i64>> = Combinations::new(&big).take(VARIANT_LIMIT + 1).collect();
    assert_eq!(first.len(), VARIANT_LIMIT + 1);
    assert_eq!(first[0].len(), 30);
    let found = valid_combinations(&big, &HashMap::new(), VARIANT_LIMIT).unwrap();
    assert_eq!(found.len(), VARIANT_LIMIT + 1, "stops at limit + 1");
}

#[test]
fn combinations_cover_the_product_exactly_once() {
    let small = lists(3, 2);
    let all: Vec<Vec<i64>> = Combinations::new(&small).collect();
    assert_eq!(all.len(), 8);
    let unique: std::collections::HashSet<_> = all.iter().cloned().collect();
    assert_eq!(unique.len(), 8);
    assert!(Combinations::new(&[]).next().is_none());
    assert!(Combinations::new(&[vec![1], vec![]]).next().is_none());
}

#[test]
fn excluded_combinations_are_skipped() {
    // value 1 of the first list excludes value 3 of the second
    let lists = vec![vec![1, 2], vec![3, 4]];
    let exclusions = HashMap::from([(1, vec![3])]);
    let found = valid_combinations(&lists, &exclusions, VARIANT_LIMIT).unwrap();
    assert_eq!(found.len(), 3);
    assert!(!found.iter().any(|c| c.contains(&1) && c.contains(&3)));
}

#[test]
fn a_space_that_exclusions_empty_out_is_cut_loudly() {
    // every combination holds ids 1 and 2, which exclude each other
    let mut space = vec![vec![1], vec![2]];
    space.extend(
        lists(10, 10)
            .into_iter()
            .map(|l| l.into_iter().map(|v| v + 10).collect()),
    );
    let exclusions = HashMap::from([(1, vec![2])]);
    let err = valid_combinations(&space, &exclusions, VARIANT_LIMIT).unwrap_err();
    assert!(
        err.message()
            .contains(&MAX_EXAMINED_COMBINATIONS.to_string()),
        "unexpected error: {}",
        err.message()
    );
}

#[tokio::test]
async fn test_template_with_too_many_combinations_is_refused() {
    let h = support::base_harness().await;
    let mut tx = h.begin().await.unwrap();
    let attr_model = tx.env().records("product.attribute").unwrap();
    let val_model = tx.env().records("product.attribute.value").unwrap();
    let tmpl_model = tx.env().records("product.template").unwrap();
    let ptal_model = tx.env().records("product.template.attribute.line").unwrap();
    let prod_model = tx.env().records("product.product").unwrap();

    let tmpl = tmpl_model
        .create(
            &mut tx,
            vec![RawVals::new().set_cell("name", Cell::Str("Huge".into()))],
        )
        .await
        .unwrap();
    let tmpl_id = tmpl.rec_ids()[0].get();

    // 4 attributes of 10 values each: 10 000 combinations in one batch of lines
    let mut line_vals = Vec::new();
    for a in 0..4 {
        let attr = attr_model
            .create(
                &mut tx,
                vec![RawVals::new().set_cell("name", Cell::Str(format!("Attr{a}").into()))],
            )
            .await
            .unwrap()
            .rec_ids()[0]
            .get();
        let values: Vec<_> = (0..10)
            .map(|v| {
                RawVals::new()
                    .set_cell("name", Cell::Str(format!("V{a}-{v}").into()))
                    .set_cell("attribute_id", Cell::Id(attr))
            })
            .collect();
        let created = val_model.create(&mut tx, values).await.unwrap();
        line_vals.push(
            RawVals::new()
                .set_cell("product_tmpl_id", Cell::Id(tmpl_id))
                .set_cell("attribute_id", Cell::Id(attr))
                .set_cell("value_ids", Cell::Ids(created.rec_ids().to_vec().into())),
        );
    }
    let err = ptal_model
        .create(&mut tx, line_vals)
        .await
        .expect_err("10 000 combinations are above the limit");
    assert!(
        err.message().contains("above allowed limit (1000)"),
        "unexpected error: {}",
        err.message()
    );
    drop(prod_model);
}
