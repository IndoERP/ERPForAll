// An evaluation borrows the transaction it was decided in: it cannot outlive it.
#![allow(unused)]

use erp_fields::Result;
use erp_orm::{Env, GuardEvaluation, GuardPlan, Transaction};
use std::sync::Arc;

async fn run(env: &Env, plan: Arc<GuardPlan>, make: fn() -> Transaction) -> Result<()> {
    let evaluation: GuardEvaluation<'_>;
    {
        let mut tx: Transaction = make();
        evaluation = tx.evaluate_guards(env, &plan).await?;
    }
    let _ = evaluation.subject();
    Ok(())
}

fn main() {}
