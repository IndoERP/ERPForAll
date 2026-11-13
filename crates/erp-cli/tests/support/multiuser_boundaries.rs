// SPDX-License-Identifier: MIT OR Apache-2.0
//! Independent fixture records and expected error text; never calls the access evaluator as oracle.
#![allow(
    dead_code,
    reason = "shared acceptance helpers used by independent integration binaries"
)]
use crate::multiuser_fixture::{MultiuserFixture, Subject, User, UserSpec, success};
use serde_json::{Value, json};

pub const CATEGORY: &str = "product.category";
pub const ITEM: &str = "product.pricelist.item";
pub const ACCESS: &str = "erp.exceptions.AccessError";
pub const OUTSIDE: &str = "\n\nThis seems to be a multi-company issue, but you do not have access to the proper company to access the record anyhow.";

pub fn ids(value: &Value) -> Vec<i64> {
    value
        .as_array()
        .expect("identity list")
        .iter()
        .map(|v| v.as_i64().expect("identity"))
        .collect()
}
pub fn row_ids(value: &Value) -> Vec<i64> {
    value
        .as_array()
        .expect("record rows")
        .iter()
        .map(|v| v["id"].as_i64().expect("row identity"))
        .collect()
}
pub fn pair_ids(value: &Value) -> Vec<i64> {
    let mut ids: Vec<_> = value
        .as_array()
        .expect("name search pairs")
        .iter()
        .map(|v| v[0].as_i64().expect("pair identity"))
        .collect();
    ids.sort();
    ids
}
pub fn relation_id(value: &Value) -> i64 {
    if let Some(id) = value.as_i64() {
        id
    } else {
        value[0].as_i64().expect("relation identity")
    }
}
/// Setup calls carry no credentials or secret field values. Only error name/message may be printed.
fn setup(f: &MultiuserFixture<'_>, model: &str, method: &str, body: Value) -> Value {
    let reply = f.rpc(Subject::Admin, model, method, body);
    if reply.status != 200 {
        let error: Value = serde_json::from_str(&reply.body).expect("setup error JSON");
        panic!(
            "boundary seed {model}/{method}: HTTP {} {}: {}",
            reply.status, error["name"], error["message"]
        );
    }
    success(&reply)
}
pub fn create(f: &MultiuserFixture<'_>, model: &str, values: Value) -> i64 {
    setup(f, model, "create", json!({"vals_list":[values]}))[0]
        .as_i64()
        .expect("created identity")
}
pub fn write(f: &MultiuserFixture<'_>, model: &str, ids: &[i64], values: Value) {
    assert_eq!(
        setup(f, model, "write", json!({"ids":ids,"vals":values})),
        true
    );
}
pub fn admin_read(f: &MultiuserFixture<'_>, model: &str, id: i64, fields: &[&str]) -> Value {
    let rows = setup(f, model, "read", json!({"ids":[id],"fields":fields}));
    assert_eq!(rows.as_array().expect("read rows").len(), 1);
    rows[0].clone()
}
pub fn settings(f: &MultiuserFixture<'_>, subject: Subject) -> i64 {
    let value = success(&f.rpc(
        subject,
        "res.users.settings",
        "set_res_users_settings",
        json!({"new_settings":{"homemenu_config":"[]"}}),
    ));
    assert_eq!(value["homemenu_config"], "[]");
    let rows = success(&f.rpc(
        subject,
        "res.users.settings",
        "search",
        json!({"domain":[]}),
    ));
    let ids = ids(&rows);
    assert_eq!(ids.len(), 1);
    ids[0]
}
pub fn multi_user(f: &MultiuserFixture<'_>) -> User {
    f.provision(UserSpec {
        login: "boundary-multi",
        company: f.user(Subject::A).company,
        companies: vec![f.user(Subject::A).company, f.user(Subject::B).company],
        groups: vec![f.group("group_user")],
    })
}
pub fn product_manager(f: &MultiuserFixture<'_>) -> User {
    f.provision(UserSpec {
        login: "boundary-manager",
        company: f.user(Subject::A).company,
        companies: vec![f.user(Subject::A).company],
        groups: vec![f.xmlid("product", "group_product_manager")],
    })
}
/// Full stable non-debug field error, authored from the SEC-022/023 contract, not runtime output.
pub fn field_denial(label: &str, model: &str, field: &str, op: &str) -> String {
    format!(
        "You do not have enough rights to access the field \"{field}\" on {label} ({model}). Please contact your system administrator.\n\nOperation: {op}"
    )
}
pub fn model_denial(label: &str, model: &str, verb: &str, groups: &[&str]) -> String {
    let group_info = if groups.is_empty() {
        "No group currently allows this operation.".to_owned()
    } else {
        format!(
            "This operation is allowed for the following groups:\n{}",
            groups
                .iter()
                .map(|g| format!("\t- {g}"))
                .collect::<Vec<_>>()
                .join("\n")
        )
    };
    format!(
        "You are not allowed to {verb} '{label}' ({model}) records.\n\n{group_info}\n\nContact your administrator to request access if necessary."
    )
}
pub fn record_denial(user: &User, label: &str, model: &str, op: &str, outside: bool) -> String {
    let hint = if outside { OUTSIDE } else { "" };
    format!(
        "Uh-oh! Looks like you have stumbled upon some top-secret records.\n\nSorry, {} (id={}) doesn't have '{op}' access to:\n- {label} ({model})\n\nIf you really, really need access, perhaps you can win over your friendly administrator with a batch of freshly baked cookies.{hint}",
        user.login, user.id
    )
}

pub struct Categories {
    pub root: i64,
    pub own: i64,
    pub foreign: i64,
    pub shared: i64,
}
impl Categories {
    pub fn seed(f: &MultiuserFixture<'_>) -> Self {
        let root = create(
            f,
            CATEGORY,
            json!({"name":"boundary root","company_id":null}),
        );
        let own = create(
            f,
            CATEGORY,
            json!({"name":"boundary own","company_id":f.user(Subject::A).company,"parent_id":root}),
        );
        let foreign = create(
            f,
            CATEGORY,
            json!({"name":"boundary foreign","company_id":f.user(Subject::B).company,"parent_id":root}),
        );
        let shared = create(
            f,
            CATEGORY,
            json!({"name":"boundary shared","company_id":null,"parent_id":root}),
        );
        Self {
            root,
            own,
            foreign,
            shared,
        }
    }
    pub fn ids(&self) -> Vec<i64> {
        vec![self.own, self.foreign, self.shared]
    }
}
pub struct Prices {
    pub lists: Vec<i64>,
    pub items: Vec<i64>,
}
impl Prices {
    pub fn seed(f: &MultiuserFixture<'_>) -> Self {
        let currency = relation_id(
            &admin_read(
                f,
                "res.company",
                f.user(Subject::A).company,
                &["currency_id"],
            )["currency_id"],
        );
        let companies = [
            json!(f.user(Subject::A).company),
            json!(f.user(Subject::B).company),
            json!(null),
        ];
        let mut lists = Vec::new();
        let mut items = Vec::new();
        for (index, (company, price)) in companies.into_iter().zip([10, 900, 30]).enumerate() {
            let list = create(
                f,
                "product.pricelist",
                json!({"name":format!("boundary price {index}"),"currency_id":currency,"company_id":company}),
            );
            let item = create(
                f,
                ITEM,
                json!({"pricelist_id":list,"compute_price":"fixed","fixed_price":price}),
            );
            lists.push(list);
            items.push(item);
        }
        Self { lists, items }
    }
}
pub struct Actions {
    pub actions: Vec<i64>,
    pub menus: Vec<i64>,
}
impl Actions {
    pub fn seed(f: &MultiuserFixture<'_>) -> Self {
        let mut actions = Vec::new();
        let mut menus = Vec::new();
        for (label, model, groups) in [
            ("allowed", CATEGORY, vec![]),
            ("group hidden", CATEGORY, vec![f.group("group_system")]),
            ("unreadable", "ir.access", vec![]),
        ] {
            let name = format!("boundary {label}");
            let action = create(
                f,
                "ir.actions.act_window",
                json!({"name":name,"res_model":model,"view_mode":"list,form","group_ids":[[6,0,groups]]}),
            );
            let menu = create(
                f,
                "ir.ui.menu",
                json!({"name":name,"action":format!("ir.actions.act_window,{action}")}),
            );
            actions.push(action);
            menus.push(menu);
        }
        Self { actions, menus }
    }
}
