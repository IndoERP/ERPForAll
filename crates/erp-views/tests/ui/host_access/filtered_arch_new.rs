// A FilteredArch has no constructor.
#![allow(unused)]

use erp_views::GuardedArch;
use erp_views::host_access::FilteredArch;

fn forge(guarded: &GuardedArch) {
    let _forged = FilteredArch::new(guarded, Vec::new());
}

fn main() {}
