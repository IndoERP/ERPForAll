// A GuardEvaluation cannot be built from parts: every field is private.
#![allow(unused)]

use erp_orm::GuardEvaluation;

fn main() {
    let _forged: GuardEvaluation<'static> = GuardEvaluation {
        tx: todo!(),
        plan: todo!(),
        node: todo!(),
        record: todo!(),
        fields: todo!(),
        registry_generation: todo!(),
        policy: todo!(),
        snapshot: todo!(),
        subject: todo!(),
        key: todo!(),
        field_ctx: todo!(),
    };
}
