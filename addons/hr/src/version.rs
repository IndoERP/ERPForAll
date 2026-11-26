// SPDX-License-Identifier: MIT OR Apache-2.0
//! `hr.employee.version`: contract and employment history records for an employee.

use erp_sdk::prelude::*;

/// `hr.employee.version`: a dated employment / contract record for an employee.
#[model(
    name = "hr.employee.version",
    description = "Employment Contract History",
    order = "date_start desc, id desc",
    company_access = direct(field = "company_id", branch = "exact", empty = "deny")
)]
pub struct HrEmployeeVersion {
    /// Contract or version reference.
    #[field(char, required = true, string = "Contract Reference")]
    pub name: Option<Arc<str>>,
    /// Owning employee.
    #[field(
        many2one,
        comodel = "hr.employee",
        required = true,
        ondelete = "cascade",
        index = true,
        string = "Employee"
    )]
    pub employee_id: Option<RecId>,
    /// Company of the employee: always the employee's, so a wage row is never visible to an
    /// officer of another company.
    #[field(
        many2one,
        comodel = "res.company",
        compute = "_compute_company_id",
        store = true,
        index = true,
        string = "Company"
    )]
    pub company_id: Option<RecId>,
    /// Department during this contract period.
    #[field(
        many2one,
        comodel = "hr.department",
        ondelete = "set null",
        string = "Department"
    )]
    pub department_id: Option<RecId>,
    /// Job position during this contract period.
    #[field(
        many2one,
        comodel = "hr.job",
        ondelete = "set null",
        string = "Job Position"
    )]
    pub job_id: Option<RecId>,
    /// Employment type during this contract period.
    #[field(
        many2one,
        comodel = "hr.employee.type",
        ondelete = "set null",
        string = "Employee Type"
    )]
    pub employee_type_id: Option<RecId>,
    /// Working schedule.
    #[field(
        many2one,
        comodel = "hr.work.schedule",
        ondelete = "set null",
        string = "Working Schedule"
    )]
    pub work_schedule_id: Option<RecId>,
    /// Contract start date.
    #[field(date, required = true, string = "Start Date")]
    pub date_start: Option<Date>,
    /// Contract end date (blank = ongoing / open-ended).
    #[field(date, string = "End Date")]
    pub date_end: Option<Date>,
    /// Company currency.
    #[field(
        many2one,
        comodel = "res.currency",
        related = "company_id.currency_id",
        store = true,
        string = "Currency"
    )]
    pub currency_id: Option<RecId>,
    /// Monthly gross wage.
    #[field(
        monetary,
        currency_field = "currency_id",
        groups = "hr.group_hr_user",
        string = "Monthly Wage"
    )]
    pub wage: Decimal,
    /// Lifecycle status of the contract record.
    #[field(
        selection,
        selection = [
            ("draft", "New"),
            ("running", "Running"),
            ("expired", "Expired"),
            ("cancelled", "Cancelled"),
        ],
        default = "running",
        required = true,
        string = "Status"
    )]
    pub state: Option<Arc<str>>,
    /// Additional contract notes.
    #[field(text, string = "Notes")]
    pub notes: Option<Arc<str>>,
}

#[methods(HrEmployeeVersion, addon = "hr")]
impl HrEmployeeVersion {
    #[compute(depends = ["employee_id", "employee_id.company_id"])]
    async fn _compute_company_id(
        this: &RecordSet<HrEmployeeVersion>,
        tx: &mut Transaction,
        out: &mut Assign<'_, HrEmployeeVersion>,
    ) -> Result<()> {
        let employees = this.env().records("hr.employee")?;
        let emp_ids = this.mapped(tx, HrEmployeeVersion::EMPLOYEE_ID).await?;
        for (index, emp_opt) in emp_ids.into_iter().enumerate() {
            let company = if let Some(eid) = emp_opt {
                match employees
                    .browse([eid.get()])?
                    .cell(tx, "company_id")
                    .await?
                {
                    Cell::Id(cid) => RecId::from_real(cid),
                    _ => None,
                }
            } else {
                None
            };
            out.set(index, HrEmployeeVersion::COMPANY_ID, company)?;
        }
        Ok(())
    }

    #[constrains("date_start", "date_end")]
    async fn _check_dates(this: &RecordSet<HrEmployeeVersion>, tx: &mut Transaction) -> Result<()> {
        let starts = this.mapped(tx, HrEmployeeVersion::DATE_START).await?;
        let ends = this.mapped(tx, HrEmployeeVersion::DATE_END).await?;
        for (start_opt, end_opt) in starts.into_iter().zip(ends) {
            if let (Some(start), Some(end)) = (start_opt, end_opt)
                && end < start
            {
                return Err(Error::validation(
                    "Contract end date cannot be earlier than start date.",
                ));
            }
        }
        Ok(())
    }
}
