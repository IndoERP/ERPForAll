// A GuardedArch cannot be built from an arch and a table of the caller's own.
#![allow(unused)]

use erp_views::{Arch, GuardedArch};

fn main() {
    let arch: Arch = todo!();
    let _forged = GuardedArch {
        arch,
        table: todo!(),
    };
}
