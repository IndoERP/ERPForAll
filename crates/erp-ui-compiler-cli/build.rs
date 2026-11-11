//! Links the `wasm32-wasip1` build of `erp-ui-compile` with an 8 MiB stack (design note §9): the
//! default 1 MiB leaves too little room for the deepest template the limits accept.

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    let target = std::env::var("TARGET").unwrap_or_default();
    if target.starts_with("wasm32-") {
        println!("cargo:rustc-link-arg-bins=-zstack-size=8388608");
    }
}
