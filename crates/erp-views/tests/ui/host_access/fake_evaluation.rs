// filter takes the concrete GuardEvaluation; a type of the caller's own cannot stand in for it
// .
#![allow(unused)]

use erp_orm::Env;
use erp_views::GuardedArch;
use erp_views::host_access::filter;

struct Fake;

fn forge(env: &Env, guarded: &GuardedArch) {
    let _ = filter(env, guarded, &Fake);
}

fn main() {}
