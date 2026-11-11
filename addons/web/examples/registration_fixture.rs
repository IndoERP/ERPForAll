//! Test-only producer for
//! It writes synthetic modules into an explicitly supplied empty fixture directory; no Node or
//! application activation occurs here. Ordinary Rust CI can compile this example without Node.
use erp_addon_web::assets::assembly::{AddonAssets, AssemblyOptions, AssetInput, assemble};
use erp_sdk::ui_compiler::{BuildMode, CompileOptions};
use std::{error::Error, path::PathBuf};

fn main() -> Result<(), Box<dyn Error>> {
    let directory = PathBuf::from(
        std::env::args_os()
            .nth(1)
            .ok_or("supply an empty fixture directory")?,
    );
    if directory.exists() && std::fs::read_dir(&directory)?.next().is_some() {
        return Err("fixture directory must be empty".into());
    }
    std::fs::create_dir_all(&directory)?;
    let source = b"export function register(ctx) { globalThis.registrationOrder.push(ctx.addon); }";
    let mut addons = Vec::new();
    for name in ["base", "web"] {
        let specifier = format!("@erpforall/{name}");
        let mut addon = AddonAssets::new(name, "1", "runtime-1")?;
        addon.push(AssetInput::module(
            "static/dist/main.js",
            &specifier,
            source.to_vec(),
        )?)?;
        addon.set_entry(&specifier)?;
        addons.push(addon);
        let package = directory.join("node_modules").join("@erpforall").join(name);
        std::fs::create_dir_all(&package)?;
        std::fs::write(
            package.join("package.json"),
            br#"{"type":"module","exports":"./index.js"}"#,
        )?;
        std::fs::write(package.join("index.js"), source)?;
    }
    let options = AssemblyOptions::new(
        "compiler-1",
        "runtime-1",
        "en_US",
        "main",
        CompileOptions::new(BuildMode::Prod),
    )?;
    let bundle = assemble(&addons, &options)?;
    std::fs::write(
        directory.join("registrations.mjs"),
        bundle.register_module().bytes(),
    )?;
    println!("{}", directory.display());
    Ok(())
}
