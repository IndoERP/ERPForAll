// SPDX-License-Identifier: MIT OR Apache-2.0
//! `hr.job`: a job position of a company.

use erp_sdk::prelude::*;

/// `hr.job`: a named job position, optionally tied to a company, archivable.
#[model(
    name = "hr.job",
    description = "Job Position",
    order = "name, id",
    company_access = direct(field = "company_id", branch = "exact", empty = "allow")
)]
pub struct HrJob {
    /// Job position name.
    #[field(char, required = true, translate = true, string = "Job Position")]
    pub name: Option<Arc<str>>,
    /// Company the position belongs to.
    #[field(
        many2one,
        comodel = "res.company",
        ondelete = "restrict",
        string = "Company"
    )]
    pub company_id: Option<RecId>,
    /// Archived positions are hidden from searches.
    #[field(boolean, default = true, string = "Active")]
    pub active: bool,
}
