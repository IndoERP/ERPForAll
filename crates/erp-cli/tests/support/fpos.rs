// SPDX-License-Identifier: MIT OR Apache-2.0
#![allow(dead_code, reason = "shared fiscal-position fixtures")]
use erp_sdk::{Command, ErrorKind, prelude::*};
use erp_testkit::Harness;

pub async fn harness() -> Harness {
    let h = Harness::builder([
        &erp_data::model_data::ADDON,
        &erp_addon_base::ADDON,
        &erp_addon_uom::ADDON,
        &erp_addon_product::ADDON,
        &erp_addon_analytic::ADDON,
        &erp_addon_account::ADDON,
    ])
    .with_static_decls([&erp_data::IR_MODEL_DATA])
    .build()
    .await
    .unwrap();
    // The registry-only harness loads no security XML. Seed real group rows and external
    // identifiers for the linked settings catalog; permissions are tested on a real install.
    let mut groups = std::collections::BTreeSet::new();
    for entry in erp_sdk::settings::CONTRIBUTIONS {
        if ["base", "uom", "product", "analytic", "account"].contains(&entry.addon.name) {
            for decl in (entry.declarations)().unwrap() {
                groups.extend(decl.read_groups().iter().cloned());
                groups.extend(decl.write_groups().iter().cloned());
            }
        }
    }
    let mut tx = h.begin().await.unwrap();
    for group in groups {
        let (module, name) = group.split_once('.').unwrap();
        let row = tx
            .query(
                "INSERT INTO res_groups(name) VALUES($1) RETURNING id::int8",
                &[&group],
            )
            .await
            .unwrap();
        let id: i64 = row[0].get(0);
        tx.execute("INSERT INTO ir_model_data(module,name,model,res_id,noupdate) VALUES($1,$2,'res.groups',$3,true)",&[&module,&name,&id]).await.unwrap();
    }
    tx.commit().await.unwrap();
    h
}
pub fn text(s: &str) -> Cell {
    Cell::Str(s.into())
}
pub fn model(tx: &Transaction, company: i64, name: &str) -> Records {
    tx.env()
        .with_company(company)
        .unwrap()
        .records(name)
        .unwrap()
}
pub async fn country(tx: &mut Transaction, code: &str) -> i64 {
    tx.env()
        .records("res.country")
        .unwrap()
        .create(
            tx,
            vec![
                RawVals::new()
                    .set_cell("name", text(code))
                    .set_cell("code", text(code)),
            ],
        )
        .await
        .unwrap()
        .ids()[0]
}
pub async fn company(tx: &mut Transaction, country: i64, parent: Option<i64>) -> i64 {
    let mut vals = RawVals::new().set_cell("name", text("Fiscal company"));
    if let Some(parent) = parent {
        vals = vals.set_cell("parent_id", Cell::Id(parent));
    } else {
        let currency = erp_testkit::fixtures::seed_currency_in(tx).await.unwrap();
        vals = vals.set_cell("currency_id", Cell::Id(currency));
    }
    let created = tx
        .env()
        .records("res.company")
        .unwrap()
        .create(tx, vec![vals])
        .await
        .unwrap();
    created
        .related(tx, "partner_id")
        .await
        .unwrap()
        .write(tx, RawVals::new().set_cell("country_id", Cell::Id(country)))
        .await
        .unwrap();
    created.ids()[0]
}
pub async fn partner(tx: &mut Transaction, country: i64) -> Records {
    tx.env()
        .records("res.partner")
        .unwrap()
        .create(
            tx,
            vec![
                RawVals::new()
                    .set_cell("name", text("Fiscal partner"))
                    .set_cell("country_id", Cell::Id(country)),
            ],
        )
        .await
        .unwrap()
}
pub async fn position(tx: &mut Transaction, company: i64, name: &str, vals: RawVals) -> Records {
    let mut base = RawVals::new().set_cell("name", text(name));
    for (k, v) in vals.iter() {
        base.insert(k.to_owned().into(), v.clone());
    }
    model(tx, company, "account.fiscal.position")
        .create(tx, vec![base])
        .await
        .unwrap()
}
pub async fn choose(
    tx: &mut Transaction,
    company: i64,
    partner: i64,
    delivery: Option<i64>,
) -> Option<i64> {
    model(tx, company, "account.fiscal.position")
        .call(tx, "_get_fiscal_position", (partner, delivery))
        .await
        .unwrap()
}
pub fn links(field: &str, ids: &[i64]) -> RawVals {
    RawVals::new().set_commands(
        field.to_owned(),
        vec![Command::set(
            ids.iter().map(|id| RecId::from_real(*id).unwrap()),
        )],
    )
}
pub async fn write(tx: &mut Transaction, rec: &Records, vals: RawVals) -> Result<()> {
    tx.savepoint(async |tx| rec.write(tx, vals).await).await
}
pub fn refused(error: Error, message: &str) {
    assert_eq!(error.kind(), ErrorKind::Validation);
    assert_eq!(error.message(), message);
}
pub async fn ids(tx: &mut Transaction, rec: &Records, field: &str) -> Vec<i64> {
    match rec.cell(tx, field).await.unwrap() {
        Cell::Ids(ids) => ids.iter().map(|id| id.get()).collect(),
        other => panic!("{field} must be ids, got {other:?}"),
    }
}
pub async fn tax(tx: &mut Transaction, company: i64, country: i64, name: &str) -> Records {
    let group = model(tx, company, "account.tax.group")
        .create(
            tx,
            vec![
                RawVals::new()
                    .set_cell("name", text(name))
                    .set_cell("country_id", Cell::Id(country)),
            ],
        )
        .await
        .unwrap();
    model(tx, company, "account.tax")
        .create(
            tx,
            vec![
                RawVals::new()
                    .set_cell("name", text(name))
                    .set_cell("country_id", Cell::Id(country))
                    .set_cell("tax_group_id", Cell::Id(group.ids()[0])),
            ],
        )
        .await
        .unwrap()
}
