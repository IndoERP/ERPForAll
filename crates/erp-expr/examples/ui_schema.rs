//! Build tooling: emit declarations from explicit Rust schema roots and their dependencies.
#[path = "support/schema.rs"]
pub mod schema;

use std::{env, error::Error, io, path::PathBuf};
use ts_rs::TS;

fn main() -> Result<(), Box<dyn Error>> {
    let mut args = env::args_os().skip(1);
    let output = PathBuf::from(args.next().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "usage: ui_schema OUTPUT_DIRECTORY",
        )
    })?);
    if args.next().is_some() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "usage: ui_schema OUTPUT_DIRECTORY",
        )
        .into());
    }
    // Future schema owners register their real roots here. Neither root performs I/O at runtime.
    erp_expr::wire::WireDocument::export_all_to(&output)?;
    schema::TestOnlyResponse::export_all_to(&output)?;
    Ok(())
}
