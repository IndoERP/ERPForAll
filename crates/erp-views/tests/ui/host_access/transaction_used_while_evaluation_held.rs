// The transaction cannot be used again, not even to commit, while an evaluation of it lives
// .
#![allow(unused)]

use erp_fields::Result;
use erp_orm::{Env, GuardPlan, Transaction};
use std::sync::Arc;

async fn run(env: &Env, plan: Arc<GuardPlan>, mut tx: Transaction) -> Result<()> {
    let evaluation = tx.evaluate_guards(env, &plan).await?;
    tx.commit().await?;
    let _ = evaluation.subject();
    Ok(())
}

fn main() {}
