// SPDX-License-Identifier: MIT OR Apache-2.0
//! `hr.employee`: a person employed by one company.

use erp_sdk::prelude::*;

/// `hr.employee`: an employee of exactly one company, optionally linked to the user who signs
/// in as them (at most one employee per user and company), archivable.
#[model(
    name = "hr.employee",
    description = "Employee",
    order = "name, id",
    company_access = direct(field = "company_id", branch = "exact", empty = "deny"),
    constraints = [(
        "_user_company_uniq",
        "UNIQUE (company_id, user_id)",
        "This user is already linked to another employee of this company."
    )]
)]
pub struct HrEmployee {
    /// Employee name.
    #[field(char, required = true, index = "trigram", string = "Employee Name")]
    pub name: Option<Arc<str>>,
    /// Company that employs the person.
    #[field(
        many2one,
        comodel = "res.company",
        required = true,
        ondelete = "restrict",
        index = true,
        string = "Company"
    )]
    pub company_id: Option<RecId>,
    /// User who signs in as this employee.
    #[field(
        many2one,
        comodel = "res.users",
        ondelete = "set null",
        index = true,
        string = "User"
    )]
    pub user_id: Option<RecId>,
    /// Archived employees are hidden from searches.
    #[field(boolean, default = true, string = "Active")]
    pub active: bool,
    /// Work email address.
    #[field(char, string = "Work Email")]
    pub work_email: Option<Arc<str>>,
    /// Work phone number.
    #[field(char, string = "Work Phone")]
    pub work_phone: Option<Arc<str>>,
}
