use erp_orm::{Env, GuardEvaluation};
use erp_views::{CheckedViewInput, GuardedArch, host_access};

fn escape<'a>(
    env: &'a Env,
    guarded: &'a GuardedArch,
    evaluation: &GuardEvaluation<'_>,
) -> CheckedViewInput<'a> {
    host_access::checked_view_input(env, guarded, evaluation).unwrap()
}

fn main() {}
