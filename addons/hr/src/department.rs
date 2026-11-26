// SPDX-License-Identifier: MIT OR Apache-2.0
//! `hr.department`: hierarchical organizational units with managers, colors, and chart RPCs.

use std::collections::{BTreeMap, BTreeSet};

use erp_addon_mail::{MailActivityMixin, MailThread};
use erp_sdk::ValueMap;
use erp_sdk::prelude::*;

use crate::counts::{as_field, count_by};
use crate::{HIERARCHY_DEPTH_KEY, ORG_CHART_ROWS_KEY, count_parameter};

fn too_deep(depth: usize) -> Error {
    Error::validation(format!(
        "The department tree is deeper than {HIERARCHY_DEPTH_KEY} ({depth}) levels."
    ))
}

fn recursive_departments() -> Error {
    Error::validation("You cannot create recursive departments.")
}

/// `hr.department`: a company department organized in a parent/child tree.
#[model(
    name = "hr.department",
    description = "Department",
    inherit = [MailThread, MailActivityMixin],
    parent_store = true,
    parent_name = "parent_id",
    rec_name = "complete_name",
    rec_names_search = ["complete_name", "name"],
    order = "complete_name, id",
    company_access = direct(field = "company_id", branch = "exact", empty = "allow")
)]
pub struct HrDepartment {
    /// Short department name.
    #[field(
        char,
        required = true,
        index = "trigram",
        tracking = true,
        string = "Department Name"
    )]
    pub name: Option<Arc<str>>,
    /// Full hierarchical name ('Parent / Child').
    #[field(
        char,
        compute = "_compute_complete_name",
        store = true,
        string = "Complete Name"
    )]
    pub complete_name: Option<Arc<str>>,
    /// Archivable flag.
    #[field(boolean, default = true, string = "Active")]
    pub active: bool,
    /// Owning company (optional for shared departments).
    #[field(
        many2one,
        comodel = "res.company",
        ondelete = "restrict",
        index = true,
        string = "Company"
    )]
    pub company_id: Option<RecId>,
    /// Parent department.
    #[field(
        many2one,
        comodel = "hr.department",
        ondelete = "set null",
        index = true,
        tracking = true,
        string = "Parent Department"
    )]
    pub parent_id: Option<RecId>,
    /// Direct sub-departments.
    #[field(
        one2many,
        comodel = "hr.department",
        inverse_name = "parent_id",
        string = "Child Departments"
    )]
    pub child_ids: Vec<RecId>,
    /// Materialized parent path maintained by `parent_store`.
    #[field(char, index = true)]
    pub parent_path: Option<Arc<str>>,
    /// Employee managing this department.
    #[field(
        many2one,
        comodel = "hr.employee",
        check_company = false,
        ondelete = "set null",
        tracking = true,
        string = "Manager"
    )]
    pub manager_id: Option<RecId>,
    /// Color palette index (0..11) for cards and hierarchy headers.
    #[field(integer, default = 0, string = "Color Index")]
    pub color: i32,
    /// Internal notes.
    #[field(text, string = "Notes")]
    pub note: Option<Arc<str>>,
    /// Count of active employees in this department (including descendants).
    #[field(integer, compute = "_compute_counts", string = "Employees")]
    pub total_employee: i32,
    /// Count of direct sub-departments.
    #[field(integer, compute = "_compute_counts", string = "Sub-departments")]
    pub child_count: i32,
}

#[methods(HrDepartment, addon = "hr")]
impl HrDepartment {
    #[method(override)]
    async fn default_get(
        this: &RecordSet<HrDepartment>,
        tx: &mut Transaction,
        names: Vec<String>,
        sup: Super,
    ) -> Result<ValueMap> {
        let mut values: ValueMap = sup.call(tx, this.as_records(), (names.clone(),)).await?;
        if names.iter().any(|n| n == "company_id")
            && !values.contains_key("company_id")
            && let Some(company) = this.env().company_id()?
        {
            values.insert("company_id".into(), Value::Int(company));
        }
        Ok(values)
    }

    #[method(override)]
    async fn create(
        this: &RecordSet<HrDepartment>,
        tx: &mut Transaction,
        mut vals_list: Vec<RawVals>,
        sup: Super,
    ) -> Result<Records> {
        let env = this.env().clone();
        let active_company = env.company_id()?;
        let depts = env.records("hr.department")?;
        for vals in &mut vals_list {
            if !vals.contains("company_id") {
                let inherited = match vals.get("parent_id") {
                    Some(Cell::Id(pid)) => {
                        match depts.browse([*pid])?.cell(tx, "company_id").await? {
                            Cell::Id(cid) => Some(cid),
                            _ => None,
                        }
                    }
                    _ => None,
                };
                if let Some(cid) = inherited.or(active_company) {
                    vals.insert("company_id".into(), Cell::Id(cid));
                }
            }
        }
        sup.call(tx, this.as_records(), (vals_list,)).await
    }

    #[compute(depends = ["name", "parent_id", "parent_id.complete_name"])]
    async fn _compute_complete_name(
        this: &RecordSet<HrDepartment>,
        tx: &mut Transaction,
        out: &mut Assign<'_, HrDepartment>,
    ) -> Result<()> {
        let names = this.mapped(tx, HrDepartment::NAME).await?;
        let parents = this.mapped(tx, HrDepartment::PARENT_ID).await?;
        let depts = this.env().records("hr.department")?;
        for (index, (name_opt, parent_opt)) in names.into_iter().zip(parents).enumerate() {
            let name = name_opt.as_deref().unwrap_or("");
            let full = if let Some(parent_id) = parent_opt {
                let parent = depts.browse([parent_id.get()])?;
                let p_name = match parent.cell(tx, "complete_name").await? {
                    Cell::Str(s) if !s.is_empty() => s.to_string(),
                    _ => match parent.cell(tx, "name").await? {
                        Cell::Str(s) => s.to_string(),
                        _ => String::new(),
                    },
                };
                if p_name.is_empty() {
                    name.to_string()
                } else {
                    format!("{p_name} / {name}")
                }
            } else {
                name.to_string()
            };
            out.set(index, HrDepartment::COMPLETE_NAME, Some(full.into()))?;
        }
        Ok(())
    }

    #[compute(depends = ["child_ids"], depends_context = ["uid", "allowed_company_ids"])]
    async fn _compute_counts(
        this: &RecordSet<HrDepartment>,
        tx: &mut Transaction,
        out: &mut Assign<'_, HrDepartment>,
    ) -> Result<()> {
        let ids: Vec<i64> = this
            .as_records()
            .rec_ids()
            .iter()
            .map(|id| id.get())
            .collect();
        if ids.is_empty() {
            return Ok(());
        }
        let env = this.env();
        // every department under the asked ones, archived included, with its materialized path
        let tree = env
            .records("hr.department")?
            .search_with(
                tx,
                &Domain::field("id")?.child_of(ids.clone()),
                &SearchOptions {
                    active_test: false,
                    ..SearchOptions::default()
                },
            )
            .await?
            .read(tx, &["parent_path"])
            .await?;
        let mut paths = BTreeMap::new();
        for row in &tree {
            let (Some(Value::Int(id)), Some(Value::Str(path))) =
                (row.get("id"), row.get("parent_path"))
            else {
                return Err(Error::internal(format!(
                    "a department of the tree has no parent path: {row:?}"
                )));
            };
            paths.insert(*id, path.clone());
        }
        let tree_ids: Vec<i64> = paths.keys().copied().collect();
        let employees = count_by(tx, env, "hr.employee", "department_id", &tree_ids).await?;
        let children = count_by(tx, env, "hr.department", "parent_id", &ids).await?;
        for (index, id) in ids.iter().enumerate() {
            // an unsaved department is in no stored path: it has no employees yet
            let total: i64 = paths.get(id).map_or(0, |own| {
                paths
                    .iter()
                    .filter(|(_, path)| path.starts_with(own.as_ref()))
                    .map(|(dept, _)| employees.get(dept).copied().unwrap_or(0))
                    .sum()
            });
            out.set(
                index,
                HrDepartment::TOTAL_EMPLOYEE,
                as_field(total, "department employee")?,
            )?;
            out.set(
                index,
                HrDepartment::CHILD_COUNT,
                as_field(children.get(id).copied().unwrap_or(0), "child department")?,
            )?;
        }
        Ok(())
    }

    #[constrains("parent_id")]
    async fn _check_parent_recursion(
        this: &RecordSet<HrDepartment>,
        tx: &mut Transaction,
    ) -> Result<()> {
        let depth = count_parameter(tx, HIERARCHY_DEPTH_KEY).await?;
        let depts = this.env().records("hr.department")?;
        for id in this.as_records().rec_ids() {
            let start = id.get();
            let mut seen = BTreeSet::from([start]);
            let mut cursor = start;
            let mut hops = 0;
            loop {
                let row = depts.browse([cursor])?;
                let next = match row.cell(tx, "parent_id").await? {
                    Cell::Id(pid) => pid,
                    _ => break,
                };
                if !seen.insert(next) {
                    return Err(recursive_departments());
                }
                hops += 1;
                if hops > depth {
                    return Err(too_deep(depth));
                }
                cursor = next;
            }
        }
        Ok(())
    }

    #[method]
    #[rpc(effect = read)]
    async fn action_open_employees(
        this: &RecordSet<HrDepartment>,
        tx: &mut Transaction,
    ) -> Result<Value> {
        let id = this.as_records().ensure_one()?.int()?;
        let label = match this.as_records().cell(tx, "name").await? {
            Cell::Str(s) => format!("{s} — Employees"),
            _ => "Employees".to_string(),
        };
        Ok(Value::Map(Arc::new(ValueMap::from([
            ("type".into(), Value::str("ir.actions.act_window")),
            ("target".into(), Value::str("current")),
            ("name".into(), Value::str(label)),
            ("res_model".into(), Value::str("hr.employee")),
            (
                "views".into(),
                Value::list([
                    Value::list([Value::Bool(false), Value::str("list")]),
                    Value::list([Value::Bool(false), Value::str("form")]),
                ]),
            ),
            (
                "domain".into(),
                Value::list([Value::list([
                    Value::str("department_id"),
                    Value::str("child_of"),
                    Value::Int(id),
                ])]),
            ),
            (
                "context".into(),
                Value::Map(Arc::new(ValueMap::from([(
                    "default_department_id".into(),
                    Value::Int(id),
                )]))),
            ),
        ]))))
    }

    #[method]
    #[rpc(effect = read)]
    async fn action_open_child_departments(
        this: &RecordSet<HrDepartment>,
        tx: &mut Transaction,
    ) -> Result<Value> {
        let id = this.as_records().ensure_one()?.int()?;
        let label = match this.as_records().cell(tx, "name").await? {
            Cell::Str(s) => format!("{s} — Sub-departments"),
            _ => "Sub-departments".to_string(),
        };
        Ok(Value::Map(Arc::new(ValueMap::from([
            ("type".into(), Value::str("ir.actions.act_window")),
            ("target".into(), Value::str("current")),
            ("name".into(), Value::str(label)),
            ("res_model".into(), Value::str("hr.department")),
            (
                "views".into(),
                Value::list([
                    Value::list([Value::Bool(false), Value::str("list")]),
                    Value::list([Value::Bool(false), Value::str("form")]),
                ]),
            ),
            (
                "domain".into(),
                Value::list([Value::list([
                    Value::str("parent_id"),
                    Value::str("="),
                    Value::Int(id),
                ])]),
            ),
            (
                "context".into(),
                Value::Map(Arc::new(ValueMap::from([(
                    "default_parent_id".into(),
                    Value::Int(id),
                )]))),
            ),
        ]))))
    }

    /// Returns `{ self, ancestors, children, children_total, children_truncated }` for the inline
    /// `hr.department_chart` widget. At most `hr.max_org_chart_rows` sub-departments are listed;
    /// a parent chain longer than `hr.max_hierarchy_depth` or a cycle is an error.
    #[method]
    #[rpc(effect = read)]
    async fn get_department_chart(
        this: &RecordSet<HrDepartment>,
        tx: &mut Transaction,
    ) -> Result<Value> {
        let current_id = this.as_records().ensure_one()?.int()?;
        let depth = count_parameter(tx, HIERARCHY_DEPTH_KEY).await?;
        let rows = count_parameter(tx, ORG_CHART_ROWS_KEY).await?;
        let depts = this.env().records("hr.department")?;
        let employees = this.env().records("hr.employee")?;

        let (self_val, mut parent_cursor) =
            read_department_node(&depts, &employees, current_id, tx).await?;
        let mut ancestors = Vec::new();
        let mut seen = BTreeSet::from([current_id]);
        while let Some(pid) = parent_cursor {
            if !seen.insert(pid) {
                return Err(recursive_departments());
            }
            if ancestors.len() == depth {
                return Err(too_deep(depth));
            }
            let (val, next_parent) = read_department_node(&depts, &employees, pid, tx).await?;
            ancestors.push(val);
            parent_cursor = next_parent;
        }
        ancestors.reverse();

        let child_domain = Domain::condition("parent_id", Operator::Eq, current_id)?;
        let total = depts.search_count(tx, &child_domain, None).await?;
        let limit = u64::try_from(rows)
            .map_err(|_| Error::programming(format!("{ORG_CHART_ROWS_KEY} {rows} exceeds u64")))?;
        let child_recs = depts
            .search_with(tx, &child_domain, &SearchOptions::new().limit(limit))
            .await?;
        let mut children = Vec::with_capacity(child_recs.len());
        for cid in child_recs.ids().iter().copied() {
            let (val, _) = read_department_node(&depts, &employees, cid, tx).await?;
            children.push(val);
        }
        let shown = u64::try_from(children.len())
            .map_err(|_| Error::programming("sub-department rows exceed u64"))?;
        let total_val = i64::try_from(total)
            .map_err(|_| Error::programming(format!("sub-department count {total} exceeds i64")))?;

        Ok(Value::Map(Arc::new(ValueMap::from([
            ("self".into(), self_val),
            ("ancestors".into(), Value::list(ancestors)),
            ("children".into(), Value::list(children)),
            ("children_total".into(), Value::Int(total_val)),
            ("children_truncated".into(), Value::Bool(total > shown)),
        ]))))
    }
}

async fn read_department_node(
    depts: &Records,
    employees: &Records,
    id: i64,
    tx: &mut Transaction,
) -> Result<(Value, Option<i64>)> {
    let rec = depts.browse([id])?;
    let name = match rec.cell(tx, "name").await? {
        Cell::Str(s) => s.to_string(),
        _ => format!("#{id}"),
    };
    let color: i64 = match rec.cell(tx, "color").await? {
        Cell::Int(n) => i64::from(n),
        _ => 0,
    };
    let total_employee: i64 = match rec.cell(tx, "total_employee").await? {
        Cell::Int(n) => i64::from(n),
        _ => 0,
    };
    let manager_name = match rec.cell(tx, "manager_id").await? {
        Cell::Id(mid) => match employees.browse([mid])?.cell(tx, "name").await? {
            Cell::Str(s) => s.to_string(),
            _ => String::new(),
        },
        _ => String::new(),
    };
    let parent_id = match rec.cell(tx, "parent_id").await? {
        Cell::Id(pid) => Some(pid),
        _ => None,
    };
    let map = ValueMap::from([
        ("id".into(), Value::Int(id)),
        ("name".into(), Value::str(name)),
        ("color".into(), Value::Int(color)),
        ("total_employee".into(), Value::Int(total_employee)),
        ("manager_name".into(), Value::str(manager_name)),
    ]);
    Ok((Value::Map(Arc::new(map)), parent_id))
}
