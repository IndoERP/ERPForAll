//! Shared real-model workloads for BASE PERF-01. Setup/verification are outside cost windows.
#![allow(
    dead_code,
    reason = "shared by allocation tests and the standalone measurement example"
)]
use std::error::Error;

use erp_fields::{Cell, Value};
use erp_orm::{RawVals, Transaction};
use erp_test_models::base_users;
use erp_testkit::Harness;

pub type Result<T> = std::result::Result<T, Box<dyn Error>>;

#[derive(Clone, Copy, Debug)]
pub enum Case {
    Default,
    CompanyCreate,
    AssignInactive,
    AssignActive,
    LanguageCreate,
    LanguageDeactivate,
}

impl Case {
    pub const ALL: [Self; 6] = [
        Self::Default,
        Self::CompanyCreate,
        Self::AssignInactive,
        Self::AssignActive,
        Self::LanguageCreate,
        Self::LanguageDeactivate,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Self::Default => "company_default",
            Self::CompanyCreate => "company_create_default",
            Self::AssignInactive => "company_assign_inactive",
            Self::AssignActive => "company_assign_active",
            Self::LanguageCreate => "language_create",
            Self::LanguageDeactivate => "language_deactivate",
        }
    }

    pub fn parse(name: &str) -> Result<Self> {
        Self::ALL
            .into_iter()
            .find(|case| case.name() == name)
            .ok_or_else(|| format!("unknown BASE cost case: {name}").into())
    }
}

pub struct Fixture {
    pub harness: Harness,
    pub language_rows: usize,
    home_currency: i64,
    inactive_currency: i64,
    active_currency: i64,
    company: i64,
    /// Partner handed to the created company: creating a company that names no partner also
    /// creates one, which is a different operation from the one measured here.
    created_partner: i64,
    language: i64,
}

fn language_values(name: String) -> RawVals {
    RawVals::new()
        .set_cell("name", Cell::Str(name.clone().into()))
        .set_cell("code", Cell::Str(name.clone().into()))
        .set_cell("url_code", Cell::Str(name.into()))
        .set_cell("active", Cell::Bool(true))
}

async fn create(tx: &mut Transaction, model: &str, rows: Vec<RawVals>) -> Result<Vec<i64>> {
    let records = tx.env().records(model)?.create(tx, rows).await?;
    Ok(records.rec_ids().iter().map(|id| id.get()).collect())
}

fn single(ids: &[i64]) -> Result<i64> {
    match ids {
        [id] => Ok(*id),
        _ => Err(format!("expected one created row, got {}", ids.len()).into()),
    }
}

impl Fixture {
    pub async fn new(language_rows: usize) -> Result<Self> {
        if !(2..=10_000).contains(&language_rows) {
            return Err("language fixture size must be 2..=10000".into());
        }
        let harness = Harness::builder([&base_users::ADDON, &erp_addon_base::ADDON])
            .build()
            .await?;
        let mut tx = harness.begin().await?;
        let mut currencies = Vec::new();
        for (name, active) in [("HOM", true), ("INA", false), ("ACT", true)] {
            currencies.push(single(
                &create(
                    &mut tx,
                    "res.currency",
                    vec![
                        RawVals::new()
                            .set_cell("name", Cell::Str(name.into()))
                            .set_cell("symbol", Cell::Str("$".into()))
                            .set_cell("rounding", Cell::Decimal("0.01".parse()?))
                            .set_cell("active", Cell::Bool(active)),
                    ],
                )
                .await?,
            )?);
        }
        let [home_currency, inactive_currency, active_currency] = currencies.as_slice() else {
            return Err("currency fixture must contain three rows".into());
        };
        let created_partner = single(
            &create(
                &mut tx,
                "res.partner",
                vec![RawVals::new().set_cell("name", Cell::Str("Cost created partner".into()))],
            )
            .await?,
        )?;
        let home = single(
            &create(
                &mut tx,
                "res.company",
                vec![
                    RawVals::new()
                        .set_cell("name", Cell::Str("Cost home".into()))
                        .set_cell("currency_id", Cell::Id(*home_currency)),
                ],
            )
            .await?,
        )?;
        let company = single(
            &create(
                &mut tx,
                "res.company",
                vec![
                    RawVals::new()
                        .set_cell("name", Cell::Str("Cost target".into()))
                        .set_cell("currency_id", Cell::Id(*home_currency)),
                ],
            )
            .await?,
        )?;
        tx.execute("UPDATE res_users SET company_id=$1 WHERE id=1", &[&home])
            .await?;
        let languages = create(
            &mut tx,
            "res.lang",
            (0..language_rows)
                .map(|n| language_values(format!("cost_{n}")))
                .collect(),
        )
        .await?;
        if languages.len() != language_rows {
            return Err("language fixture row count mismatch".into());
        }
        let language = *languages.first().ok_or("language fixture is empty")?;
        tx.commit().await?;
        Ok(Self {
            harness,
            language_rows,
            home_currency: *home_currency,
            inactive_currency: *inactive_currency,
            active_currency: *active_currency,
            company,
            created_partner,
            language,
        })
    }

    /// Full ORM operation and its flush. Transaction opening/commit are separately excluded.
    pub async fn orm(&self, tx: &mut Transaction, case: Case) -> Result<i64> {
        let id = match case {
            Case::Default => {
                let values = tx
                    .env()
                    .records("res.company")?
                    .default_get(tx, &["currency_id"])
                    .await?;
                match values.get("currency_id") {
                    Some(Value::Int(id)) => *id,
                    other => {
                        return Err(format!("expected currency default id, got {other:?}").into());
                    }
                }
            }
            Case::CompanyCreate => single(
                &create(
                    tx,
                    "res.company",
                    vec![
                        RawVals::new()
                            .set_cell("name", Cell::Str("Cost created".into()))
                            .set_cell("partner_id", Cell::Id(self.created_partner)),
                    ],
                )
                .await?,
            )?,
            Case::AssignInactive | Case::AssignActive => {
                let target = self.assignment(case)?;
                tx.env()
                    .records("res.company")?
                    .browse([self.company])?
                    .write(tx, RawVals::new().set_cell("currency_id", Cell::Id(target)))
                    .await?;
                self.company
            }
            Case::LanguageCreate => single(
                &create(tx, "res.lang", vec![language_values("cost_new".to_owned())]).await?,
            )?,
            Case::LanguageDeactivate => {
                tx.env()
                    .records("res.lang")?
                    .browse([self.language])?
                    .write(tx, RawVals::new().set_cell("active", Cell::Bool(false)))
                    .await?;
                self.language
            }
        };
        tx.flush().await?;
        Ok(id)
    }

    fn assignment(&self, case: Case) -> Result<i64> {
        match case {
            Case::AssignInactive => Ok(self.inactive_currency),
            Case::AssignActive => Ok(self.active_currency),
            _ => Err("assignment requires an assignment case".into()),
        }
    }

    /// Direct SQL floor through the same transaction/driver. It preserves fixture-visible
    /// defaults, writes, audit fields, and currency/language locking; it is not a generic ORM.
    pub async fn floor(&self, tx: &mut Transaction, case: Case) -> Result<i64> {
        match case {
            Case::Default | Case::CompanyCreate => {
                let rows = tx.query("SELECT c.currency_id FROM res_users u JOIN res_company c ON c.id=u.company_id WHERE u.id=1", &[]).await?;
                let [row] = rows.as_slice() else {
                    return Err("home company lookup must return one row".into());
                };
                let currency: i64 = row.try_get(0)?;
                if matches!(case, Case::Default) {
                    return Ok(currency);
                }
                let rows = tx.query("INSERT INTO res_company (name,currency_id,create_uid,write_uid,create_date,write_date) VALUES ('Cost created',$1,1,1,CURRENT_TIMESTAMP,CURRENT_TIMESTAMP) RETURNING id", &[&currency]).await?;
                let [row] = rows.as_slice() else {
                    return Err("company insert must return one row".into());
                };
                let id = row.try_get(0)?;
                Self::activate(tx, currency).await?;
                Ok(id)
            }
            Case::AssignInactive | Case::AssignActive => {
                let currency = self.assignment(case)?;
                let affected = tx.execute("UPDATE res_company SET currency_id=$1,write_uid=1,write_date=CURRENT_TIMESTAMP WHERE id=$2", &[&currency, &self.company]).await?;
                if affected != 1 {
                    return Err("company assignment must update one row".into());
                }
                Self::activate(tx, currency).await?;
                Ok(self.company)
            }
            Case::LanguageCreate => {
                let rows = tx.query("INSERT INTO res_lang (name,code,url_code,active,direction,date_format,time_format,week_start,grouping,decimal_point,thousands_sep,create_uid,write_uid,create_date,write_date) VALUES ('cost_new','cost_new','cost_new',true,'ltr','%m/%d/%Y','%H:%M:%S','7','[3,0]','.',',',1,1,CURRENT_TIMESTAMP,CURRENT_TIMESTAMP) RETURNING id", &[]).await?;
                let [row] = rows.as_slice() else {
                    return Err("language insert must return one row".into());
                };
                let id = row.try_get(0)?;
                Self::language_guard(tx).await?;
                Ok(id)
            }
            Case::LanguageDeactivate => {
                let affected = tx.execute("UPDATE res_lang SET active=false,write_uid=1,write_date=CURRENT_TIMESTAMP WHERE id=$1", &[&self.language]).await?;
                if affected != 1 {
                    return Err("language write must update one row".into());
                }
                Self::language_guard(tx).await?;
                Ok(self.language)
            }
        }
    }

    async fn activate(tx: &mut Transaction, currency: i64) -> Result<()> {
        let affected = tx.execute("UPDATE res_currency SET active=true,write_uid=1,write_date=CURRENT_TIMESTAMP WHERE id=$1", &[&currency]).await?;
        if affected != 1 {
            return Err("currency activation must update one row".into());
        }
        Ok(())
    }

    async fn language_guard(tx: &mut Transaction) -> Result<()> {
        let rows = tx.query("SELECT count(*) FILTER (WHERE active) FROM (SELECT active FROM res_lang ORDER BY id FOR UPDATE) AS locked_languages", &[]).await?;
        let [row] = rows.as_slice() else {
            return Err("language guard must return one count".into());
        };
        if row.try_get::<_, i64>(0)? < 1 {
            return Err("language guard found no active language".into());
        }
        Ok(())
    }

    /// Read back database effects, outside timing/allocation/query windows; no golden self-check.
    pub async fn verify(&self, tx: &mut Transaction, case: Case, id: i64) -> Result<()> {
        match case {
            Case::Default => {
                if id != self.home_currency {
                    return Err("wrong home-company currency default".into());
                }
            }
            Case::CompanyCreate | Case::AssignInactive | Case::AssignActive => {
                let expected = if matches!(case, Case::CompanyCreate) {
                    self.home_currency
                } else {
                    self.assignment(case)?
                };
                let rows = tx.query("SELECT c.currency_id,r.active FROM res_company c JOIN res_currency r ON r.id=c.currency_id WHERE c.id=$1", &[&id]).await?;
                let [row] = rows.as_slice() else {
                    return Err("company verification must return one row".into());
                };
                if row.try_get::<_, i64>(0)? != expected || !row.try_get::<_, bool>(1)? {
                    return Err("company currency assignment or activation mismatch".into());
                }
            }
            Case::LanguageCreate | Case::LanguageDeactivate => {
                let rows = tx
                    .query("SELECT active FROM res_lang WHERE id=$1", &[&id])
                    .await?;
                let [row] = rows.as_slice() else {
                    return Err("language verification must return one row".into());
                };
                if row.try_get::<_, bool>(0)? != matches!(case, Case::LanguageCreate) {
                    return Err("language activity mismatch".into());
                }
                let rows = tx
                    .query("SELECT count(*) FILTER (WHERE active) FROM res_lang", &[])
                    .await?;
                let [row] = rows.as_slice() else {
                    return Err("language count must return one row".into());
                };
                let count = i64::try_from(self.language_rows)?;
                let expected = if matches!(case, Case::LanguageCreate) {
                    count + 1
                } else {
                    count - 1
                };
                if row.try_get::<_, i64>(0)? != expected {
                    return Err("active language count mismatch".into());
                }
            }
        }
        Ok(())
    }
}
