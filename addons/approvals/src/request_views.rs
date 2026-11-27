//! Refresh base's read-only receipt projections along with the native action and its ACLs,
//! including when an installation upgrades the Approvals addon on its own.
use erp_sdk::prelude::*;

erp_sdk::model_ref!(Request = "approval.request.view");
erp_sdk::model_ref!(Stage = "approval.request.stage");
erp_sdk::model_ref!(Scope = "approval.request.scope");

#[extend(Request, addon = "approvals")]
struct RequestWindow {}
#[extend(Stage, addon = "approvals")]
struct StageWindow {}
#[extend(Scope, addon = "approvals")]
struct RequestScope {}
