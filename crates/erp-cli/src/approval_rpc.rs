use erp_addon_base::approval_request::{Admission, Call, admit, assistant_visible};
use erp_addon_base::ir_approval_policy;
use erp_orm::{BoxFuture, Principal, PrincipalKind, Transaction, rpc::Effect};
use erp_rpc::{AttemptAdmission, AttemptGate, AttemptGateCall};

/// The approval admission used by the browser and JSON-2 RPC service.
pub struct ApprovalGate;

impl AttemptGate for ApprovalGate {
    fn prepare_managed_password<'a>(
        &'a self,
        tx: &'a mut Transaction,
    ) -> BoxFuture<'a, erp_fields::Result<()>> {
        Box::pin(async move {
            if tx.registry().model("ir.approval.policy").is_some() {
                ir_approval_policy::prepare_managed_password(tx).await?;
            }
            Ok(())
        })
    }

    fn check_managed_password<'a>(
        &'a self,
        tx: &'a mut Transaction,
        principal: &'a Principal,
        model: &'a str,
        method: &'a str,
        effect: Effect,
    ) -> BoxFuture<'a, erp_fields::Result<()>> {
        Box::pin(async move {
            if principal.kind() != PrincipalKind::User || principal.actor().is_some() {
                return Err(erp_fields::Error::access(
                    "An approval policy does not permit this password operation.",
                ));
            }
            if tx.registry().model("ir.approval.policy").is_none() {
                return Ok(());
            }
            ir_approval_policy::check_managed_password(
                tx,
                principal.subject(),
                model,
                method,
                effect,
            )
            .await
        })
    }

    fn assistant_visible<'a>(
        &'a self,
        tx: &'a mut Transaction,
        model: &'a str,
        method: &'a str,
        effect: Effect,
    ) -> BoxFuture<'a, erp_fields::Result<bool>> {
        Box::pin(async move {
            if tx.registry().model("ir.approval.policy").is_none() {
                return Ok(
                    tx.env().principal().kind() != PrincipalKind::Agent || effect == Effect::Read
                );
            }
            assistant_visible(tx, model, method, effect).await
        })
    }

    fn check<'a>(
        &'a self,
        tx: &'a mut Transaction,
        call: AttemptGateCall<'a>,
    ) -> BoxFuture<'a, erp_fields::Result<AttemptAdmission>> {
        Box::pin(async move {
            if tx.registry().model("ir.approval.policy").is_none() {
                if tx.env().principal().kind() == PrincipalKind::Agent
                    && call.effect != Effect::Read
                {
                    return Err(erp_fields::Error::access(
                        "This agent call requires an explicit approval policy.",
                    ));
                }
                return Ok(AttemptAdmission::Dispatch);
            }
            let result = admit(
                tx,
                Call {
                    model: call.model,
                    method: call.method,
                    effect: call.effect,
                    key: call.key.map(erp_rpc::IdempotencyKey::as_str),
                    payload: call.request_digest,
                    binding: call.binding_digest,
                },
            )
            .await?;
            Ok(match result {
                Admission::Dispatch => AttemptAdmission::Dispatch,
                Admission::KeyRequired => AttemptAdmission::KeyRequired,
                Admission::Defer(error) => AttemptAdmission::Defer(error),
                Admission::ReplayOnly(error) => AttemptAdmission::ReplayOnly(error),
            })
        })
    }
}
