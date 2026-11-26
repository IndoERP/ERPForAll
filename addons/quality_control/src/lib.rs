// SPDX-License-Identifier: MIT OR Apache-2.0
//! Addon `quality_control` for ERPForAll: quality teams, quality control points,
//! automatic transfer quality check creation on confirm, validation gate blocking transfers
//! with incomplete checks, exact-decimal measurement tolerance evaluation, and quality alerts.

use std::collections::BTreeMap;

use erp_addon_stock::StockPicking;
use erp_sdk::sequence::draw_by_code;
use erp_sdk::{CompanyId, ErrorKind, GroupValue, Operation, ReadGroup, ValueMap, prelude::*};

erp_sdk::addon!(
    name = "quality_control",
    exact_only = true,
    depends = ["base", "product", "stock"]
);

fn db_err(e: impl std::error::Error + Send + Sync + 'static) -> Error {
    Error::db("Quality control data could not be read.").with_source(e)
}

/// Pure exact-decimal tolerance check: returns `true` when `measure` lies inside
/// `[tolerance_min, tolerance_max]` (inclusive).
pub fn is_measure_within_tolerance(
    measure: Decimal,
    tolerance_min: Decimal,
    tolerance_max: Decimal,
) -> Result<bool> {
    if tolerance_max < tolerance_min {
        return Err(Error::validation(
            "Tolerance maximum cannot be less than tolerance minimum.",
        ));
    }
    Ok(measure >= tolerance_min && measure <= tolerance_max)
}

async fn next_seq(env: &Env, tx: &mut Transaction, code: &str, company_id: i64) -> Result<String> {
    let today = env.context_today()?;
    let company = CompanyId::new(company_id)?;
    draw_by_code(tx, &env.sudo(true), code, company, Some(today)).await
}

/// `quality.team`: team responsible for quality inspections and alerts.
#[model(
    name = "quality.team",
    description = "Quality Team",
    order = "name, id",
    company_access = direct(field = "company_id", branch = "exact", empty = "allow")
)]
pub struct QualityTeam {
    /// Team name.
    #[field(char, required = true, string = "Team Name")]
    pub name: Option<Arc<str>>,
    /// Optional owning company.
    #[field(many2one, comodel = "res.company", string = "Company")]
    pub company_id: Option<RecId>,
    /// Active flag.
    #[field(boolean, default = true)]
    pub active: bool,
}

/// `quality.point`: control point triggering quality checks when matching transfers confirm.
#[model(
    name = "quality.point",
    description = "Quality Control Point",
    order = "id desc",
    company_access = direct(field = "company_id", branch = "exact", empty = "deny")
)]
pub struct QualityPoint {
    /// Reference (e.g. `QCP/00001`).
    #[field(
        char,
        required = true,
        default = "New",
        copy = false,
        string = "Reference"
    )]
    pub name: Option<Arc<str>>,
    /// Short title copied onto generated checks.
    #[field(char, required = true, string = "Title")]
    pub title: Option<Arc<str>>,
    /// Active flag.
    #[field(boolean, default = true)]
    pub active: bool,
    /// Owning company.
    #[field(
        many2one,
        comodel = "res.company",
        required = true,
        index = true,
        string = "Company"
    )]
    pub company_id: Option<RecId>,
    /// Responsible quality team.
    #[field(
        many2one,
        comodel = "quality.team",
        check_company = true,
        string = "Team"
    )]
    pub team_id: Option<RecId>,
    /// Operation type where this control point triggers (empty = all operation types).
    #[field(
        many2one,
        comodel = "stock.picking.type",
        check_company = true,
        string = "Operation Type"
    )]
    pub picking_type_id: Option<RecId>,
    /// Specific product covered (empty = all products on matching transfers).
    #[field(
        many2one,
        comodel = "product.product",
        check_company = true,
        string = "Product"
    )]
    pub product_id: Option<RecId>,
    /// Inspection kind.
    #[field(
        selection,
        selection = [
            ("passfail", "Pass - Fail"),
            ("measure", "Measure"),
            ("instructions", "Instructions / Worksheet")
        ],
        default = "passfail",
        required = true,
        string = "Type"
    )]
    pub test_type: Option<Arc<str>>,
    /// Target nominal value for `measure` checks.
    #[field(float, digits = 0, default = 0, string = "Norm")]
    pub norm: Decimal,
    /// Minimum acceptable measurement.
    #[field(float, digits = 0, default = 0, string = "Tolerance From")]
    pub tolerance_min: Decimal,
    /// Maximum acceptable measurement.
    #[field(float, digits = 0, default = 0, string = "Tolerance To")]
    pub tolerance_max: Decimal,
    /// Unit label for measurement (e.g. `g`, `mm`).
    #[field(char, string = "Unit of Measure")]
    pub norm_unit: Option<Arc<str>>,
    /// Inspection instructions / worksheet guide.
    #[field(text, string = "Instructions")]
    pub note: Option<Arc<str>>,
}

/// `quality.check`: one inspection check attached to a transfer (and optionally a product/lot).
#[model(
    name = "quality.check",
    description = "Quality Check",
    order = "id desc",
    company_access = direct(field = "company_id", branch = "exact", empty = "deny")
)]
pub struct QualityCheck {
    /// Reference (e.g. `QC/00001`).
    #[field(
        char,
        required = true,
        default = "New",
        copy = false,
        string = "Reference"
    )]
    pub name: Option<Arc<str>>,
    /// Inspection title.
    #[field(char, required = true, string = "Title")]
    pub title: Option<Arc<str>>,
    /// Source control point (optional for ad-hoc checks).
    #[field(
        many2one,
        comodel = "quality.point",
        ondelete = "set null",
        check_company = true,
        string = "Control Point"
    )]
    pub point_id: Option<RecId>,
    /// Quality team.
    #[field(
        many2one,
        comodel = "quality.team",
        check_company = true,
        string = "Team"
    )]
    pub team_id: Option<RecId>,
    /// Owning company.
    #[field(
        many2one,
        comodel = "res.company",
        required = true,
        index = true,
        string = "Company"
    )]
    pub company_id: Option<RecId>,
    /// Linked stock transfer.
    #[field(
        many2one,
        comodel = "stock.picking",
        ondelete = "cascade",
        index = true,
        check_company = true,
        string = "Transfer"
    )]
    pub picking_id: Option<RecId>,
    /// Inspected product.
    #[field(
        many2one,
        comodel = "product.product",
        check_company = true,
        string = "Product"
    )]
    pub product_id: Option<RecId>,
    /// Inspected lot/serial.
    #[field(
        many2one,
        comodel = "stock.lot",
        check_company = true,
        string = "Lot/Serial"
    )]
    pub lot_id: Option<RecId>,
    /// Inspection type.
    #[field(
        selection,
        selection = [
            ("passfail", "Pass - Fail"),
            ("measure", "Measure"),
            ("instructions", "Instructions / Worksheet")
        ],
        default = "passfail",
        required = true,
        string = "Type"
    )]
    pub test_type: Option<Arc<str>>,
    /// Recorded measurement value.
    #[field(float, digits = 0, default = 0, string = "Measure")]
    pub measure: Decimal,
    /// Target nominal value.
    #[field(float, digits = 0, default = 0, string = "Norm")]
    pub norm: Decimal,
    /// Minimum acceptable measurement.
    #[field(float, digits = 0, default = 0, string = "Tolerance From")]
    pub tolerance_min: Decimal,
    /// Maximum acceptable measurement.
    #[field(float, digits = 0, default = 0, string = "Tolerance To")]
    pub tolerance_max: Decimal,
    /// Inspector worksheet notes.
    #[field(text, string = "Worksheet Notes")]
    pub worksheet_notes: Option<Arc<str>>,
    /// Check outcome (`none` = To Do, `pass` = Passed, `fail` = Failed).
    #[field(
        selection,
        selection = [
            ("none", "To Do"),
            ("pass", "Passed"),
            ("fail", "Failed")
        ],
        default = "none",
        required = true,
        readonly = true,
        copy = false,
        string = "Status"
    )]
    pub quality_state: Option<Arc<str>>,
    /// UTC timestamp when the check was completed.
    #[field(datetime, readonly = true, string = "Control Date")]
    pub control_date: Option<DateTime>,
    /// User who completed the check.
    #[field(
        many2one,
        comodel = "res.users",
        readonly = true,
        string = "Checked By"
    )]
    pub user_id: Option<RecId>,
}

/// `quality.alert`: corrective & preventive action ticket raised from a failed check or defect.
#[model(
    name = "quality.alert",
    description = "Quality Alert",
    order = "id desc",
    company_access = direct(field = "company_id", branch = "exact", empty = "deny")
)]
pub struct QualityAlert {
    /// Reference (e.g. `QA/00001`).
    #[field(
        char,
        required = true,
        default = "New",
        copy = false,
        string = "Reference"
    )]
    pub name: Option<Arc<str>>,
    /// Summary title.
    #[field(char, required = true, string = "Title")]
    pub title: Option<Arc<str>>,
    /// Responsible team.
    #[field(
        many2one,
        comodel = "quality.team",
        check_company = true,
        string = "Team"
    )]
    pub team_id: Option<RecId>,
    /// Owning company.
    #[field(
        many2one,
        comodel = "res.company",
        required = true,
        index = true,
        string = "Company"
    )]
    pub company_id: Option<RecId>,
    /// Related transfer.
    #[field(
        many2one,
        comodel = "stock.picking",
        ondelete = "set null",
        check_company = true,
        string = "Transfer"
    )]
    pub picking_id: Option<RecId>,
    /// Related failed check.
    #[field(
        many2one,
        comodel = "quality.check",
        ondelete = "set null",
        check_company = true,
        string = "Quality Check"
    )]
    pub check_id: Option<RecId>,
    /// Affected product.
    #[field(
        many2one,
        comodel = "product.product",
        check_company = true,
        string = "Product"
    )]
    pub product_id: Option<RecId>,
    /// Affected lot/serial.
    #[field(
        many2one,
        comodel = "stock.lot",
        check_company = true,
        string = "Lot/Serial"
    )]
    pub lot_id: Option<RecId>,
    /// Priority (`0` normal, `1` urgent).
    #[field(
        selection,
        selection = [("0", "Normal"), ("1", "High")],
        default = "0",
        required = true,
        string = "Priority"
    )]
    pub priority: Option<Arc<str>>,
    /// Lifecycle stage.
    #[field(
        selection,
        selection = [
            ("new", "New"),
            ("confirmed", "Confirmed"),
            ("action_proposed", "Action Proposed"),
            ("solved", "Solved")
        ],
        default = "new",
        required = true,
        readonly = true,
        string = "Stage"
    )]
    pub stage: Option<Arc<str>>,
    /// Defect description.
    #[field(text, string = "Description")]
    pub description: Option<Arc<str>>,
    /// Root cause analysis.
    #[field(text, string = "Root Cause")]
    pub root_cause: Option<Arc<str>>,
    /// Corrective actions taken.
    #[field(text, string = "Corrective Actions")]
    pub corrective_actions: Option<Arc<str>>,
    /// Preventive actions planned.
    #[field(text, string = "Preventive Actions")]
    pub preventive_actions: Option<Arc<str>>,
}

#[methods(QualityPoint, addon = "quality_control")]
impl QualityPoint {
    #[method(override)]
    async fn default_get(
        this: &RecordSet<QualityPoint>,
        tx: &mut Transaction,
        fields: Vec<String>,
        sup: Super,
    ) -> Result<ValueMap> {
        let mut out: ValueMap = sup.call(tx, this.as_records(), (fields.clone(),)).await?;
        if fields.iter().any(|f| f == "company_id")
            && !out.contains_key("company_id")
            && let Some(cid) = this.env().company_id()?
        {
            out.insert("company_id".into(), Value::Int(cid));
        }
        Ok(out)
    }

    #[method(override)]
    async fn create(
        this: &RecordSet<QualityPoint>,
        tx: &mut Transaction,
        mut vals_list: Vec<RawVals>,
        sup: Super,
    ) -> Result<Records> {
        this.as_records()
            .check_access(tx, Operation::Create)
            .await?;
        let env = this.env();
        let active_company = env.company_id()?;
        for vals in &mut vals_list {
            if matches!(vals.get("company_id"), None | Some(Cell::Null))
                && let Some(cid) = active_company
            {
                vals.insert("company_id".into(), Cell::Id(cid));
            }
            let company_id = match vals.get("company_id") {
                Some(Cell::Id(cid)) => *cid,
                Some(Cell::Int(cid)) => i64::from(*cid),
                _ => {
                    return Err(Error::validation(
                        "A quality control point requires a company.",
                    ));
                }
            };
            let needs_name = match vals.get("name") {
                None | Some(Cell::Null) => true,
                Some(Cell::Str(s)) => s.trim().is_empty() || s.as_ref() == "New",
                _ => false,
            };
            if needs_name {
                let name = next_seq(env, tx, "quality.point", company_id).await?;
                vals.insert("name".into(), Cell::Str(name.into()));
            }
        }
        sup.call(tx, this.as_records(), (vals_list,)).await
    }
}

#[derive(Default)]
struct InspectionWriteGuard(Option<i64>);

async fn complete_inspection_state(
    records: &Records,
    tx: &mut Transaction,
    id: i64,
    state: &str,
    uid: i64,
) -> Result<()> {
    let mut vals = RawVals::new();
    vals.insert("quality_state".into(), Cell::Str(state.into()));
    vals.insert("control_date".into(), Cell::DateTime(DateTime::now()));
    vals.insert("user_id".into(), Cell::Id(uid));
    let previous = tx.extension::<InspectionWriteGuard>().0.replace(id);
    let result = records.write(tx, vals).await;
    tx.extension::<InspectionWriteGuard>().0 = previous;
    result?;
    tx.flush().await
}

#[methods(QualityCheck, addon = "quality_control")]
impl QualityCheck {
    #[method(override)]
    async fn default_get(
        this: &RecordSet<QualityCheck>,
        tx: &mut Transaction,
        fields: Vec<String>,
        sup: Super,
    ) -> Result<ValueMap> {
        let mut out: ValueMap = sup.call(tx, this.as_records(), (fields.clone(),)).await?;
        if fields.iter().any(|f| f == "company_id")
            && !out.contains_key("company_id")
            && let Some(cid) = this.env().company_id()?
        {
            out.insert("company_id".into(), Value::Int(cid));
        }
        Ok(out)
    }

    #[method(override)]
    async fn create(
        this: &RecordSet<QualityCheck>,
        tx: &mut Transaction,
        mut vals_list: Vec<RawVals>,
        sup: Super,
    ) -> Result<Records> {
        this.as_records()
            .check_access(tx, Operation::Create)
            .await?;
        let env = this.env();
        let active_company = env.company_id()?;
        for vals in &mut vals_list {
            if vals.contains("quality_state")
                || vals.contains("control_date")
                || vals.contains("user_id")
            {
                return Err(Error::validation(
                    "Quality check outcome fields are managed by inspection actions.",
                ));
            }
            if matches!(vals.get("company_id"), None | Some(Cell::Null))
                && let Some(cid) = active_company
            {
                vals.insert("company_id".into(), Cell::Id(cid));
            }
            let company_id = match vals.get("company_id") {
                Some(Cell::Id(cid)) => *cid,
                Some(Cell::Int(cid)) => i64::from(*cid),
                _ => {
                    return Err(Error::validation("A quality check requires a company."));
                }
            };
            let needs_name = match vals.get("name") {
                None | Some(Cell::Null) => true,
                Some(Cell::Str(s)) => s.trim().is_empty() || s.as_ref() == "New",
                _ => false,
            };
            if needs_name {
                let name = next_seq(env, tx, "quality.check", company_id).await?;
                vals.insert("name".into(), Cell::Str(name.into()));
            }
        }
        sup.call(tx, this.as_records(), (vals_list,)).await
    }

    #[method(override)]
    async fn write(
        this: &RecordSet<QualityCheck>,
        tx: &mut Transaction,
        vals: RawVals,
        sup: Super,
    ) -> Result<()> {
        let ids = this.ids();
        let guarded = tx
            .extension::<InspectionWriteGuard>()
            .0
            .is_some_and(|allowed| ids.as_slice() == [allowed]);
        if !guarded
            && (vals.contains("quality_state")
                || vals.contains("control_date")
                || vals.contains("user_id"))
        {
            return Err(Error::validation(
                "Quality check outcome fields are managed by inspection actions.",
            ));
        }
        if !ids.is_empty()
            && !this.env().su()
            && !tx
                .has_group(this.env(), "stock.group_stock_manager")
                .await?
        {
            tx.flush().await?;
            let rows = tx
                .query(
                    "SELECT count(*)::int8 FROM quality_check \
                     WHERE id = ANY($1) AND quality_state <> 'none'",
                    &[&ids],
                )
                .await?;
            let done_cnt: i64 = rows
                .first()
                .ok_or_else(|| Error::internal("Quality check count returned no row."))?
                .try_get(0)
                .map_err(db_err)?;
            if done_cnt > 0 {
                return Err(Error::user(
                    "Completed quality checks can only be modified by an inventory manager.",
                ));
            }
        }
        sup.call(tx, this.as_records(), (vals,)).await
    }

    #[method(override)]
    async fn unlink(
        this: &RecordSet<QualityCheck>,
        tx: &mut Transaction,
        sup: Super,
    ) -> Result<()> {
        if !this.env().su()
            && !tx
                .has_group(this.env(), "stock.group_stock_manager")
                .await?
        {
            return Err(Error::access(
                "Only inventory managers can delete quality checks.",
            ));
        }
        sup.call(tx, this.as_records(), ()).await
    }

    /// Mark a pending quality check as Passed.
    #[method]
    #[rpc(effect = write)]
    pub async fn do_pass(this: &RecordSet<QualityCheck>, tx: &mut Transaction) -> Result<()> {
        let records = this.as_records().clone();
        records.check_access(tx, Operation::Write).await?;
        let id = records.ensure_one()?.int()?;
        let uid = records.env().uid();
        tx.flush().await?;
        let rows = tx
            .query(
                "SELECT quality_state FROM quality_check WHERE id = $1 FOR UPDATE",
                &[&id],
            )
            .await?;
        let Some(row) = rows.first() else {
            return Err(Error::missing("Quality check not found."));
        };
        let cur: String = row.try_get(0).map_err(db_err)?;
        if cur != "none" {
            return Err(Error::user(
                "This quality check has already been completed.",
            ));
        }
        complete_inspection_state(&records, tx, id, "pass", uid).await
    }

    /// Mark a pending quality check as Failed.
    #[method]
    #[rpc(effect = write)]
    pub async fn do_fail(this: &RecordSet<QualityCheck>, tx: &mut Transaction) -> Result<()> {
        let records = this.as_records().clone();
        records.check_access(tx, Operation::Write).await?;
        let id = records.ensure_one()?.int()?;
        let uid = records.env().uid();
        tx.flush().await?;
        let rows = tx
            .query(
                "SELECT quality_state FROM quality_check WHERE id = $1 FOR UPDATE",
                &[&id],
            )
            .await?;
        let Some(row) = rows.first() else {
            return Err(Error::missing("Quality check not found."));
        };
        let cur: String = row.try_get(0).map_err(db_err)?;
        if cur != "none" {
            return Err(Error::user(
                "This quality check has already been completed.",
            ));
        }
        complete_inspection_state(&records, tx, id, "fail", uid).await
    }

    /// Evaluate `measure` against `[tolerance_min, tolerance_max]` and mark Pass or Fail.
    #[method]
    #[rpc(effect = write)]
    pub async fn do_measure(this: &RecordSet<QualityCheck>, tx: &mut Transaction) -> Result<()> {
        let records = this.as_records().clone();
        records.check_access(tx, Operation::Write).await?;
        let id = records.ensure_one()?.int()?;
        tx.flush().await?;
        let rows = tx
            .query(
                "SELECT quality_state, test_type, measure, tolerance_min, tolerance_max \
                 FROM quality_check WHERE id = $1 FOR UPDATE",
                &[&id],
            )
            .await?;
        let Some(row) = rows.first() else {
            return Err(Error::missing("Quality check not found."));
        };
        let cur: String = row.try_get(0).map_err(db_err)?;
        if cur != "none" {
            return Err(Error::user(
                "This quality check has already been completed.",
            ));
        }
        let test_type: String = row.try_get(1).map_err(db_err)?;
        if test_type != "measure" {
            return Err(Error::user(
                "Only measurement quality checks can be evaluated with do_measure.",
            ));
        }
        let measure: Decimal = row.try_get(2).map_err(db_err)?;
        let min_v: Decimal = row.try_get(3).map_err(db_err)?;
        let max_v: Decimal = row.try_get(4).map_err(db_err)?;
        let passed = is_measure_within_tolerance(measure, min_v, max_v)?;
        let state = if passed { "pass" } else { "fail" };
        let uid = records.env().uid();
        complete_inspection_state(&records, tx, id, state, uid).await
    }

    /// Create and open a `quality.alert` linked to this failed check.
    #[method]
    #[rpc(effect = write)]
    pub async fn action_create_alert(
        this: &RecordSet<QualityCheck>,
        tx: &mut Transaction,
    ) -> Result<Value> {
        let records = this.as_records();
        records.check_access(tx, Operation::Read).await?;
        let id = records.ensure_one()?.int()?;
        let rows = tx
            .query(
                "SELECT title, team_id::int8, company_id::int8, picking_id::int8, \
                        product_id::int8, lot_id::int8, worksheet_notes \
                 FROM quality_check WHERE id = $1",
                &[&id],
            )
            .await?;
        let Some(row) = rows.first() else {
            return Err(Error::missing("Quality check not found."));
        };
        let title: String = row.try_get(0).map_err(db_err)?;
        let team_id: Option<i64> = row.try_get(1).map_err(db_err)?;
        let company_id: i64 = row.try_get(2).map_err(db_err)?;
        let picking_id: Option<i64> = row.try_get(3).map_err(db_err)?;
        let product_id: Option<i64> = row.try_get(4).map_err(db_err)?;
        let lot_id: Option<i64> = row.try_get(5).map_err(db_err)?;
        let notes: Option<String> = row.try_get(6).map_err(db_err)?;

        let mut vals = RawVals::new();
        vals.insert("title".into(), Cell::Str(format!("Alert: {title}").into()));
        vals.insert("company_id".into(), Cell::Id(company_id));
        vals.insert("check_id".into(), Cell::Id(id));
        if let Some(tid) = team_id {
            vals.insert("team_id".into(), Cell::Id(tid));
        }
        if let Some(pid) = picking_id {
            vals.insert("picking_id".into(), Cell::Id(pid));
        }
        if let Some(prod) = product_id {
            vals.insert("product_id".into(), Cell::Id(prod));
        }
        if let Some(lid) = lot_id {
            vals.insert("lot_id".into(), Cell::Id(lid));
        }
        if let Some(desc) = notes {
            vals.insert("description".into(), Cell::Str(desc.into()));
        }
        let created = records
            .env()
            .records("quality.alert")?
            .create(tx, vec![vals])
            .await?;
        let alert_id = created
            .ids()
            .into_iter()
            .next()
            .ok_or_else(|| Error::invariant("Created quality.alert returned no id."))?;

        let mut action = ValueMap::new();
        action.insert("type".into(), Value::str("ir.actions.act_window"));
        action.insert("name".into(), Value::str("Quality Alert"));
        action.insert("res_model".into(), Value::str("quality.alert"));
        action.insert("res_id".into(), Value::Int(alert_id));
        action.insert(
            "views".into(),
            Value::list([Value::list([Value::False, Value::str("form")])]),
        );
        action.insert("target".into(), Value::str("current"));
        Ok(Value::Map(action.into()))
    }
}

#[methods(QualityAlert, addon = "quality_control")]
impl QualityAlert {
    #[method(override)]
    async fn default_get(
        this: &RecordSet<QualityAlert>,
        tx: &mut Transaction,
        fields: Vec<String>,
        sup: Super,
    ) -> Result<ValueMap> {
        let mut out: ValueMap = sup.call(tx, this.as_records(), (fields.clone(),)).await?;
        if fields.iter().any(|f| f == "company_id")
            && !out.contains_key("company_id")
            && let Some(cid) = this.env().company_id()?
        {
            out.insert("company_id".into(), Value::Int(cid));
        }
        Ok(out)
    }

    #[method(override)]
    async fn create(
        this: &RecordSet<QualityAlert>,
        tx: &mut Transaction,
        mut vals_list: Vec<RawVals>,
        sup: Super,
    ) -> Result<Records> {
        this.as_records()
            .check_access(tx, Operation::Create)
            .await?;
        let env = this.env();
        let active_company = env.company_id()?;
        for vals in &mut vals_list {
            if vals.contains("stage") {
                return Err(Error::validation(
                    "Quality alert stage is managed by workflow actions.",
                ));
            }
            if matches!(vals.get("company_id"), None | Some(Cell::Null))
                && let Some(cid) = active_company
            {
                vals.insert("company_id".into(), Cell::Id(cid));
            }
            let company_id = match vals.get("company_id") {
                Some(Cell::Id(cid)) => *cid,
                Some(Cell::Int(cid)) => i64::from(*cid),
                _ => {
                    return Err(Error::validation("A quality alert requires a company."));
                }
            };
            let needs_name = match vals.get("name") {
                None | Some(Cell::Null) => true,
                Some(Cell::Str(s)) => s.trim().is_empty() || s.as_ref() == "New",
                _ => false,
            };
            if needs_name {
                let name = next_seq(env, tx, "quality.alert", company_id).await?;
                vals.insert("name".into(), Cell::Str(name.into()));
            }
        }
        sup.call(tx, this.as_records(), (vals_list,)).await
    }

    #[method(override)]
    async fn write(
        this: &RecordSet<QualityAlert>,
        tx: &mut Transaction,
        vals: RawVals,
        sup: Super,
    ) -> Result<()> {
        if vals.contains("stage") {
            return Err(Error::validation(
                "Quality alert stage is managed by workflow actions.",
            ));
        }
        sup.call(tx, this.as_records(), (vals,)).await
    }

    #[method]
    #[rpc(effect = write)]
    pub async fn action_confirm(
        this: &RecordSet<QualityAlert>,
        tx: &mut Transaction,
    ) -> Result<()> {
        let records = this.as_records().clone();
        records.check_access(tx, Operation::Write).await?;
        let id = records.ensure_one()?.int()?;
        let uid = records.env().uid();
        let rows = tx
            .query(
                "SELECT stage FROM quality_alert WHERE id = $1 FOR UPDATE",
                &[&id],
            )
            .await?;
        let Some(row) = rows.first() else {
            return Err(Error::missing("Quality alert not found."));
        };
        let stage: String = row.try_get(0).map_err(db_err)?;
        if stage != "new" {
            return Err(Error::user("Only new quality alerts can be confirmed."));
        }
        tx.execute(
            "UPDATE quality_alert \
             SET stage = 'confirmed', write_uid = $2, write_date = (now() AT TIME ZONE 'UTC') \
             WHERE id = $1",
            &[&id, &uid],
        )
        .await?;
        records.modified_external(tx, &["stage"]).await?;
        Ok(())
    }

    #[method]
    #[rpc(effect = write)]
    pub async fn action_solve(this: &RecordSet<QualityAlert>, tx: &mut Transaction) -> Result<()> {
        let records = this.as_records().clone();
        records.check_access(tx, Operation::Write).await?;
        let id = records.ensure_one()?.int()?;
        let uid = records.env().uid();
        let rows = tx
            .query(
                "SELECT stage FROM quality_alert WHERE id = $1 FOR UPDATE",
                &[&id],
            )
            .await?;
        let Some(row) = rows.first() else {
            return Err(Error::missing("Quality alert not found."));
        };
        let stage: String = row.try_get(0).map_err(db_err)?;
        if stage == "solved" {
            return Err(Error::user("This quality alert is already solved."));
        }
        tx.execute(
            "UPDATE quality_alert \
             SET stage = 'solved', write_uid = $2, write_date = (now() AT TIME ZONE 'UTC') \
             WHERE id = $1",
            &[&id, &uid],
        )
        .await?;
        records.modified_external(tx, &["stage"]).await?;
        Ok(())
    }
}

#[extend(StockPicking, addon = "quality_control")]
pub struct StockPickingQuality {
    /// Count of quality checks linked to this transfer.
    #[field(
        integer,
        compute = "_compute_quality_checks",
        string = "Quality Checks"
    )]
    pub quality_check_count: i32,
}

async fn generate_quality_checks_for_pickings(
    env: &Env,
    tx: &mut Transaction,
    ids: &[i64],
) -> Result<()> {
    if ids.is_empty() {
        return Ok(());
    }
    tx.flush().await?;
    let pickings = tx
        .query(
            "SELECT id::int8, company_id::int8, picking_type_id::int8 \
             FROM stock_picking WHERE id = ANY($1) AND state NOT IN ('done', 'cancel')",
            &[&ids],
        )
        .await?;
    let checks = env.sudo(true).records("quality.check")?;
    for prow in pickings {
        let picking_id: i64 = prow.try_get(0).map_err(db_err)?;
        let company_id: i64 = prow.try_get(1).map_err(db_err)?;
        let ptype_id: Option<i64> = prow.try_get(2).map_err(db_err)?;
        let Some(ptype_id) = ptype_id else {
            continue;
        };
        let move_products: Vec<i64> = tx
            .query(
                "SELECT DISTINCT product_id::int8 FROM stock_move \
                 WHERE picking_id = $1 AND state NOT IN ('done', 'cancel') ORDER BY product_id",
                &[&picking_id],
            )
            .await?
            .into_iter()
            .map(|r| r.try_get(0))
            .collect::<std::result::Result<Vec<i64>, _>>()
            .map_err(db_err)?;
        let points = tx
            .query(
                "SELECT qp.id::int8, qp.title, qp.team_id::int8, qp.test_type, \
                        qp.norm, qp.tolerance_min, qp.tolerance_max, qp.note, qp.product_id::int8 \
                 FROM quality_point qp \
                 WHERE qp.active IS TRUE AND qp.company_id = $1 \
                   AND (qp.picking_type_id IS NULL OR qp.picking_type_id = $2) \
                 ORDER BY qp.id",
                &[&company_id, &ptype_id],
            )
            .await?;
        for qrow in points {
            let point_id: i64 = qrow.try_get(0).map_err(db_err)?;
            let title: String = qrow.try_get(1).map_err(db_err)?;
            let team_id: Option<i64> = qrow.try_get(2).map_err(db_err)?;
            let test_type: String = qrow.try_get(3).map_err(db_err)?;
            let norm: Decimal = qrow.try_get(4).map_err(db_err)?;
            let tol_min: Decimal = qrow.try_get(5).map_err(db_err)?;
            let tol_max: Decimal = qrow.try_get(6).map_err(db_err)?;
            let note: Option<String> = qrow.try_get(7).map_err(db_err)?;
            let point_product: Option<i64> = qrow.try_get(8).map_err(db_err)?;

            let matched_products: Vec<Option<i64>> = match point_product {
                None => {
                    if move_products.is_empty() {
                        vec![None]
                    } else {
                        move_products.iter().copied().map(Some).collect()
                    }
                }
                Some(req_pid) => move_products
                    .iter()
                    .copied()
                    .filter(|pid| *pid == req_pid)
                    .map(Some)
                    .collect(),
            };

            for prod_opt in matched_products {
                let exists_rows = tx
                    .query(
                        "SELECT count(*)::int8 FROM quality_check \
                         WHERE picking_id = $1 AND point_id = $2 \
                           AND product_id IS NOT DISTINCT FROM $3",
                        &[&picking_id, &point_id, &prod_opt],
                    )
                    .await?;
                let exists: i64 = exists_rows
                    .first()
                    .ok_or_else(|| {
                        Error::internal("Quality check existence query returned no row.")
                    })?
                    .try_get(0)
                    .map_err(db_err)?;
                if exists > 0 {
                    continue;
                }
                let mut cvals = RawVals::new();
                cvals.insert("title".into(), Cell::Str(title.as_str().into()));
                cvals.insert("point_id".into(), Cell::Id(point_id));
                if let Some(tid) = team_id {
                    cvals.insert("team_id".into(), Cell::Id(tid));
                }
                cvals.insert("company_id".into(), Cell::Id(company_id));
                cvals.insert("picking_id".into(), Cell::Id(picking_id));
                if let Some(pid) = prod_opt {
                    cvals.insert("product_id".into(), Cell::Id(pid));
                }
                cvals.insert("test_type".into(), Cell::Str(test_type.as_str().into()));
                cvals.insert("norm".into(), Cell::Decimal(norm));
                cvals.insert("tolerance_min".into(), Cell::Decimal(tol_min));
                cvals.insert("tolerance_max".into(), Cell::Decimal(tol_max));
                if let Some(n) = &note {
                    cvals.insert("worksheet_notes".into(), Cell::Str(n.as_str().into()));
                }
                checks.create(tx, vec![cvals]).await?;
            }
        }
    }
    Ok(())
}

async fn ensure_no_pending_quality_checks(tx: &mut Transaction, ids: &[i64]) -> Result<()> {
    if ids.is_empty() {
        return Ok(());
    }
    tx.flush().await?;
    let rows = tx
        .query(
            "SELECT count(*)::int8 FROM quality_check \
             WHERE picking_id = ANY($1) AND quality_state = 'none'",
            &[&ids],
        )
        .await?;
    let todo_count: i64 = rows
        .first()
        .ok_or_else(|| Error::internal("Pending quality check count returned no row."))?
        .try_get(0)
        .map_err(db_err)?;
    if todo_count > 0 {
        return Err(Error::user(format!(
            "Complete all {todo_count} pending Quality Check(s) on this transfer before validating."
        )));
    }
    Ok(())
}

#[methods(StockPicking, addon = "quality_control")]
impl StockPickingQuality {
    #[compute(depends = [], depends_context = ["uid", "allowed_company_ids"])]
    async fn _compute_quality_checks(
        this: &RecordSet<StockPicking>,
        tx: &mut Transaction,
        out: &mut Assign<'_, StockPicking>,
    ) -> Result<()> {
        let rec_ids = this.as_records().rec_ids();
        let real_ids: Vec<i64> = rec_ids
            .iter()
            .filter_map(|id| id.as_real().map(|r| r.get()))
            .collect();
        let mut counts: BTreeMap<i64, i32> = BTreeMap::new();
        if !real_ids.is_empty() {
            tx.flush().await?;
            let domain = Domain::field("picking_id")?.in_(real_ids);
            let read = this
                .env()
                .records("quality.check")?
                ._read_group(
                    tx,
                    ReadGroup::new(domain)
                        .groupby(["picking_id"])
                        .aggregates(["__count"]),
                )
                .await;
            match read {
                Ok(groups) => {
                    for row in groups.rows() {
                        if let (Some(pid), Some(GroupValue::Int(cnt))) = (
                            row.first().and_then(|k| k.record_id().map(|id| id.get())),
                            row.last(),
                        ) {
                            let cnt_i32 = i32::try_from(*cnt).map_err(|_| {
                                Error::internal("Quality check count exceeds i32 range.")
                            })?;
                            counts.insert(pid, cnt_i32);
                        }
                    }
                }
                Err(error) if error.kind() == ErrorKind::Access => {}
                Err(error) => return Err(error),
            }
        }
        for (idx, rec_id) in rec_ids.iter().enumerate() {
            let cnt = rec_id
                .as_real()
                .and_then(|id| counts.get(&id.get()).copied())
                .unwrap_or(0);
            out.set(idx, StockPickingQuality::QUALITY_CHECK_COUNT, cnt)?;
        }
        Ok(())
    }

    /// Open quality checks linked to this transfer.
    #[method]
    #[rpc(effect = read)]
    pub async fn action_view_quality_checks(
        this: &RecordSet<StockPicking>,
        tx: &mut Transaction,
    ) -> Result<Value> {
        let records = this.as_records();
        records.check_access(tx, Operation::Read).await?;
        let id = records.ensure_one()?.int()?;
        let mut action = ValueMap::new();
        action.insert("type".into(), Value::str("ir.actions.act_window"));
        action.insert("name".into(), Value::str("Quality Checks"));
        action.insert("res_model".into(), Value::str("quality.check"));
        action.insert(
            "views".into(),
            Value::list([
                Value::list([Value::False, Value::str("list")]),
                Value::list([Value::False, Value::str("form")]),
            ]),
        );
        action.insert("target".into(), Value::str("current"));
        action.insert(
            "domain".into(),
            Value::list([Value::list([
                Value::str("picking_id"),
                Value::str("="),
                Value::Int(id),
            ])]),
        );
        Ok(Value::Map(action.into()))
    }

    /// Generate matching `quality.check` records whenever a transfer is confirmed.
    #[method(override)]
    async fn action_confirm(
        this: &RecordSet<StockPicking>,
        tx: &mut Transaction,
        sup: Super,
    ) -> Result<()> {
        sup.call(tx, this.as_records(), ()).await?;
        let ids = this.ids();
        generate_quality_checks_for_pickings(this.env(), tx, &ids).await
    }

    /// Block transfer validation on all validation paths (direct, draft, batch, backorder, RPC)
    /// while any matching or linked `quality.check` is still pending (`none`).
    #[method(override)]
    async fn button_validate(
        this: &RecordSet<StockPicking>,
        tx: &mut Transaction,
        sup: Super,
    ) -> Result<()> {
        let ids = this.ids();
        generate_quality_checks_for_pickings(this.env(), tx, &ids).await?;
        ensure_no_pending_quality_checks(tx, &ids).await?;
        sup.call(tx, this.as_records(), ()).await
    }

    /// Block UI transfer validation before backorder wizard when checks are still pending.
    #[method(override)]
    async fn action_validate_transfer(
        this: &RecordSet<StockPicking>,
        tx: &mut Transaction,
        sup: Super,
    ) -> Result<Value> {
        let ids = this.ids();
        generate_quality_checks_for_pickings(this.env(), tx, &ids).await?;
        ensure_no_pending_quality_checks(tx, &ids).await?;
        sup.call(tx, this.as_records(), ()).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    fn dec(s: &str) -> Decimal {
        Decimal::from_str(s).expect("decimal")
    }

    #[test]
    fn evaluates_exact_decimal_measurement_tolerances() {
        assert!(is_measure_within_tolerance(dec("250.00"), dec("245"), dec("255")).unwrap());
        assert!(is_measure_within_tolerance(dec("245.00"), dec("245"), dec("255")).unwrap());
        assert!(is_measure_within_tolerance(dec("255.00"), dec("245"), dec("255")).unwrap());
        assert!(!is_measure_within_tolerance(dec("244.99"), dec("245"), dec("255")).unwrap());
        assert!(!is_measure_within_tolerance(dec("255.01"), dec("245"), dec("255")).unwrap());
        assert!(is_measure_within_tolerance(dec("250"), dec("260"), dec("240")).is_err());
    }
}
