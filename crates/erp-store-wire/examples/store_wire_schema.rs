//! Build tooling: emit TypeScript declarations for the wire documents into a directory.
//! `cargo run -p erp-store-wire --features schema-gen --example store_wire_schema -- OUTPUT_DIR`.
//! Production builds never enable `schema-gen`.
use std::{env, error::Error, io, path::PathBuf};

use erp_store_wire::doc::{
    AsofDoc, ErrorCodeDoc, FixtureFileDoc, LimitDoc, PayloadDoc, RefusalDoc, SchemaDoc,
};
use ts_rs::TS;

fn main() -> Result<(), Box<dyn Error>> {
    let usage = || {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "usage: store_wire_schema OUTPUT_DIRECTORY",
        )
    };
    let mut args = env::args_os().skip(1);
    let output = PathBuf::from(args.next().ok_or_else(usage)?);
    if args.next().is_some() {
        return Err(usage().into());
    }
    // The fixture file reaches every document type; the others are the roots the decoder consumes
    // directly, listed so that removing one from the fixtures never drops its declaration.
    FixtureFileDoc::export_all_to(&output)?;
    PayloadDoc::export_all_to(&output)?;
    SchemaDoc::export_all_to(&output)?;
    AsofDoc::export_all_to(&output)?;
    RefusalDoc::export_all_to(&output)?;
    ErrorCodeDoc::export_all_to(&output)?;
    LimitDoc::export_all_to(&output)?;
    Ok(())
}
