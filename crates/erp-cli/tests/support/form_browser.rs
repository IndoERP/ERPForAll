// SPDX-License-Identifier: MIT OR Apache-2.0
//! Common native browser setup for form workload and shell acceptance fixtures.
use super::native_http::{self, NativeFixture};
use serde_json::{Value, json};
use std::{path::Path, process::Command};

/// Lines of the two orders with lines.
pub(super) const FIFTY: usize = 50;
pub(super) const MANY: usize = 500;

pub(super) fn call(
    fixture: &NativeFixture,
    model: &str,
    method: &str,
    company: i64,
    body: Value,
) -> Value {
    let mut body = body;
    body["context"] = json!({"allowed_company_ids": [company]});
    let reply = native_http::request_within(
        fixture.port,
        "POST",
        &format!("/erp/json/2/{model}/{method}"),
        &[
            ("cookie", &fixture.cookies),
            ("content-type", "application/json"),
            ("sec-fetch-site", "same-origin"),
        ],
        &body.to_string(),
        std::time::Duration::from_secs(300),
    );
    assert_eq!(reply.status, 200, "{model}.{method}: {}", reply.body);
    serde_json::from_str(&reply.body).unwrap()
}

pub(super) fn create(
    fixture: &NativeFixture,
    model: &str,
    company: i64,
    vals: Vec<Value>,
) -> Vec<i64> {
    let ids = call(
        fixture,
        model,
        "create",
        company,
        json!({"vals_list": vals}),
    );
    ids.as_array()
        .expect("create returns record ids")
        .iter()
        .map(|id| id.as_i64().unwrap())
        .collect()
}

pub(super) fn order_with_lines(
    fixture: &NativeFixture,
    company: i64,
    partner: i64,
    products: &[i64],
    lines: usize,
) -> i64 {
    let order = create(
        fixture,
        "sale.order",
        company,
        vec![json!({"partner_id": partner, "company_id": company})],
    )[0];
    let vals: Vec<Value> = (0..lines)
        .map(|n| json!({"order_id": order, "product_id": products[n % products.len()], "product_uom_qty": 1 + n % 7, "price_unit": 3 + n % 11}))
        .collect();
    for chunk in vals.chunks(100) {
        create(fixture, "sale.order.line", company, chunk.to_vec());
    }
    order
}

pub(super) fn build_assets(root: &Path, compiler: &Path) -> tempfile::TempDir {
    let assets = tempfile::tempdir().unwrap();
    let built = Command::new("bash")
        .current_dir(root)
        .args([
            "-c",
            "set -euo pipefail; scripts/web-toolchain.sh check; eval \"$(scripts/web-toolchain.sh env)\"; exec node \"$1\" --addons web,acceptance_notes,reporting,stock_account",
            "form-open-assets",
        ])
        .arg(root.join("web/scripts/build-human-ui.mjs"))
        .env("ERP_UI_COMPILER", compiler)
        .env("ERP_UI_OUTPUT_DIR", assets.path())
        .status()
        .unwrap();
    assert!(built.success(), "client asset compilation failed");
    assets
}
