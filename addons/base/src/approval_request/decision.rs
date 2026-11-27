//! Write-once stage decisions. The request lifecycle inserts a decision and advances its
//! locked request in the same transaction. There is no generic ORM mutation path.
use erp_sdk::prelude::*;
use erp_sdk::{Operation, TimestampTz};

#[model(
    name = "ir.approval.decision",
    description = "Approval Stage Decision",
    company_access = unrestricted(reason = "Private audit rows are exposed only by the authorized approval receipt"),
    unique_indexes = [("_request_stage_uniq", "(request_id, position)", "This stage already has a decision.")],
    indexes = [("_reviewer_request_idx", "(reviewer_id, request_id)")],
    constraints = [("_stage", "CHECK (position BETWEEN 1 AND 16)", "Invalid approval stage.")]
)]
pub struct IrApprovalDecision {
    #[field(
        many2one,
        comodel = "ir.approval.request",
        required,
        ondelete = "restrict"
    )]
    pub request_id: Option<RecId>,
    #[field(integer, required)]
    pub position: i32,
    #[field(many2one, comodel = "res.users", required, ondelete = "restrict")]
    pub reviewer_id: Option<RecId>,
    #[field(boolean, required)]
    pub approved: bool,
    #[field(timestamptz, required)]
    pub decided_at: Option<TimestampTz>,
}

#[methods(IrApprovalDecision, addon = "base")]
impl IrApprovalDecision {
    #[method(override)]
    async fn create(
        this: &RecordSet<IrApprovalDecision>,
        tx: &mut Transaction,
        _values: Vec<RawVals>,
        _sup: Super,
    ) -> Result<Records> {
        this.as_records()
            .check_access(tx, Operation::Create)
            .await?;
        Err(Error::access(
            "Approval decisions can only be recorded through their request.",
        ))
    }
    #[method(override)]
    async fn write(
        this: &RecordSet<IrApprovalDecision>,
        tx: &mut Transaction,
        _values: RawVals,
        _sup: Super,
    ) -> Result<()> {
        this.as_records().check_access(tx, Operation::Write).await?;
        Err(Error::access("Approval decisions cannot be changed."))
    }
    #[method(override)]
    async fn unlink(
        this: &RecordSet<IrApprovalDecision>,
        tx: &mut Transaction,
        _sup: Super,
    ) -> Result<()> {
        this.as_records()
            .check_access(tx, Operation::Unlink)
            .await?;
        Err(Error::access("Approval decisions cannot be deleted."))
    }
}
