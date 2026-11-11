//! `erp addon build --target wasm` and `erp addon package`.
//!
//! The component build test is `#[ignore]`d in the workspace run: `scripts/ci.sh` (step
//! `wasm-cli-package`) builds and packages `test_wasm_dual_b` through the real binary, and the test
//! below runs the same through the library when `ERP_CLI_WASM_ADDONS` names the fixtures directory.

#![allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]

use clap::Parser;
use erp_cli::addon::{
    INSTALL_WASM_TARGET, SystemToolchain, ToolchainEnv, WASM_TARGET, build_wasm,
    check_wasm_toolchain, package, verify_described,
};
use erp_cli::args::{AddonAction, BuildTarget, Cli, Commands};
use erp_cli::{CliError, run};
use erp_modules::ModuleName;
use erp_modules::package::{ErpAddonPackage, sha256_hex};
use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use tempfile::tempdir;

fn parse(args: &[&str]) -> Result<Cli, clap::Error> {
    Cli::try_parse_from(std::iter::once("erp").chain(args.iter().copied()))
}

/// A toolchain whose sysroot is `sysroot`, or that cannot be asked at all.
struct FakeToolchain(Result<PathBuf, String>);

impl ToolchainEnv for FakeToolchain {
    fn rustc_sysroot(&self, _cwd: &Path) -> Result<PathBuf, String> {
        self.0.clone()
    }
}

/// An addon directory: `[wasm]` manifest with a display name, a data file, client code, a
/// translation, and (when `wasm` is given) the component.
fn write_addon(root: &Path, name: &str, wasm: Option<&[u8]>) -> PathBuf {
    let dir = root.join(name);
    fs::create_dir_all(dir.join("data")).unwrap();
    fs::create_dir_all(dir.join("static/dist")).unwrap();
    fs::create_dir_all(dir.join("i18n")).unwrap();
    fs::write(
        dir.join("manifest.toml"),
        "name = \"Shop (WASM)\"\nversion = \"1.2.3\"\ndepends = []\ndata = [\"data/shop.xml\"]\n\n[wasm]\nsdk_version = \"1.0\"\ncapabilities = [\"migrations-sql\"]\n",
    )
    .unwrap();
    fs::write(dir.join("Cargo.toml"), "[package]\nname = \"x\"\n").unwrap();
    fs::write(dir.join("data/shop.xml"), "<elang/>").unwrap();
    fs::write(dir.join("static/dist/app.js"), "export {}").unwrap();
    fs::write(dir.join("i18n/id.po"), "msgid \"\"\n").unwrap();
    if let Some(bytes) = wasm {
        fs::create_dir_all(dir.join("wasm")).unwrap();
        fs::write(dir.join(format!("wasm/{name}.wasm")), bytes).unwrap();
    }
    dir
}

fn empty_component() -> Vec<u8> {
    wat::parse_str("(component)").unwrap()
}

/// A component that exports `addon-guest` with a `describe` returning `text` and imports one host
/// function it never calls (the shape of an addon component).
fn describing(text: &str) -> Vec<u8> {
    let len = text.len();
    let escaped = text.replace('\\', "\\\\").replace('"', "\\\"");
    wat::parse_str(format!(
        r#"(component
  (import "erpforall:addon/unused-host-fn" (func))
  (core module $m
    (memory (export "mem") 1)
    (data (i32.const 8) "{escaped}")
    (func (export "describe") (result i32)
      (i32.store (i32.const 0) (i32.const 8))
      (i32.store (i32.const 4) (i32.const {len}))
      i32.const 0)
    (func (export "realloc") (param i32 i32 i32 i32) (result i32) i32.const 0))
  (core instance $i (instantiate $m))
  (func $d (result string)
    (canon lift (core func $i "describe") (memory (core memory $i "mem")) (realloc (core func $i "realloc"))))
  (instance $g (export "describe" (func $d)))
  (export "erpforall:addon/addon-guest@1.0.0" (instance $g)))"#
    ))
    .unwrap()
}

fn description(addons: &[&str]) -> String {
    let entries: Vec<String> = addons
        .iter()
        .map(|a| {
            format!(
                "{{\"addon\":\"{a}\",\"kind\":\"addon\",\"model\":\"\",\"name\":\"{a}\",\"detail\":{{}}}}"
            )
        })
        .collect();
    format!(
        "{{\"schema\":{},\"contributions\":[{}]}}",
        erp_sdk::DESCRIBE_SCHEMA,
        entries.join(",")
    )
}

#[test]
fn the_arguments_of_build_and_package_parse() {
    let cli = parse(&[
        "addon",
        "build",
        "shop",
        "--target",
        "wasm",
        "--target-dir",
        "t",
    ])
    .unwrap();
    let Commands::Addon(cmd) = cli.command else {
        panic!("expected the addon command");
    };
    let AddonAction::Build(args) = cmd.action else {
        panic!("expected build");
    };
    assert_eq!(args.addon, "shop");
    assert_eq!(args.target, BuildTarget::Wasm);
    assert_eq!(args.target_dir, Some(PathBuf::from("t")));
    assert_eq!(args.addons_path, [PathBuf::from("./addons")]);

    let cli = parse(&[
        "addon",
        "package",
        "./shop",
        "-o",
        "out.erpaddon",
        "--wasm",
        "c.wasm",
    ])
    .unwrap();
    let Commands::Addon(cmd) = cli.command else {
        panic!("expected the addon command");
    };
    let AddonAction::Package(args) = cmd.action else {
        panic!("expected package");
    };
    assert_eq!(args.output, Some(PathBuf::from("out.erpaddon")));
    assert_eq!(args.wasm, Some(PathBuf::from("c.wasm")));
}

#[test]
fn build_needs_an_explicit_known_target() {
    let missing = parse(&["addon", "build", "shop"]).unwrap_err();
    assert_eq!(
        missing.kind(),
        clap::error::ErrorKind::MissingRequiredArgument
    );
    let unknown = parse(&["addon", "build", "shop", "--target", "native"]).unwrap_err();
    assert_eq!(unknown.kind(), clap::error::ErrorKind::InvalidValue);
    assert_eq!(
        run(["erp", "addon", "build", "shop", "--target", "native"]),
        ExitCode::from(1)
    );
}

#[test]
fn a_toolchain_without_the_wasm_target_is_refused_with_the_install_command() {
    let sysroot = tempdir().unwrap();
    fs::create_dir_all(sysroot.path().join("lib/rustlib/x86_64-unknown-linux-gnu")).unwrap();
    let env = FakeToolchain(Ok(sysroot.path().to_path_buf()));
    let err = check_wasm_toolchain(&env, Path::new(".")).unwrap_err();
    assert!(
        matches!(&err, CliError::ToolchainMissing { tool, install, .. }
            if tool.contains(WASM_TARGET) && install == INSTALL_WASM_TARGET),
        "got {err:?}"
    );
    assert!(err.to_string().contains("rustup target add wasm32-wasip2"));
    assert_eq!(err.exit_code(), ExitCode::from(1));

    fs::create_dir_all(
        sysroot
            .path()
            .join("lib/rustlib")
            .join(WASM_TARGET)
            .join("lib"),
    )
    .unwrap();
    check_wasm_toolchain(&env, Path::new(".")).unwrap();
}

#[test]
fn a_rustc_that_cannot_be_asked_is_a_typed_error() {
    let env = FakeToolchain(Err("cannot run rustc: not found".to_owned()));
    let err = check_wasm_toolchain(&env, Path::new(".")).unwrap_err();
    assert!(
        matches!(&err, CliError::ToolchainMissing { tool, .. } if tool == "rustc"),
        "got {err:?}"
    );
}

#[test]
fn build_refuses_before_running_cargo() {
    let tmp = tempdir().unwrap();
    let addon = write_addon(tmp.path(), "shop_wasm", None);
    let args = |dir: &Path| {
        let cli = parse(&["addon", "build", dir.to_str().unwrap(), "--target", "wasm"]).unwrap();
        let Commands::Addon(cmd) = cli.command else {
            panic!("expected the addon command");
        };
        let AddonAction::Build(args) = cmd.action else {
            panic!("expected build");
        };
        args
    };
    // no target installed: the fake sysroot is empty; cargo never runs (there is no component)
    let empty = tempdir().unwrap();
    let env = FakeToolchain(Ok(empty.path().to_path_buf()));
    let err = build_wasm(&args(&addon), &env).unwrap_err();
    assert!(
        matches!(err, CliError::ToolchainMissing { .. }),
        "got {err:?}"
    );
    assert!(!addon.join("wasm").exists());

    // no [wasm] section
    let native = tmp.path().join("native_addon");
    fs::create_dir_all(&native).unwrap();
    fs::write(native.join("manifest.toml"), "name = \"Native\"\n").unwrap();
    let err = build_wasm(&args(&native), &env).unwrap_err();
    assert!(
        matches!(&err, CliError::NotWasmAddon { addon, .. } if addon.as_str() == "native_addon"),
        "got {err:?}"
    );

    // a [wasm] addon that is not a crate
    fs::remove_file(addon.join("Cargo.toml")).unwrap();
    let err = build_wasm(&args(&addon), &env).unwrap_err();
    assert!(matches!(&err, CliError::InvalidInput { reason, .. } if reason.contains("Cargo.toml")));
}

#[test]
fn describe_must_list_every_expected_addon() {
    let name = ModuleName::new("leaf_wasm").unwrap();
    let expected: BTreeSet<String> = ["base_wasm", "leaf_wasm"].map(str::to_owned).into();

    let both = describing(&description(&["base_wasm", "leaf_wasm"]));
    let described = verify_described(&name, &both, &expected).unwrap();
    assert_eq!(described, expected);

    let leaf_only = describing(&description(&["leaf_wasm"]));
    let err = verify_described(&name, &leaf_only, &expected).unwrap_err();
    assert!(
        matches!(&err, CliError::DescribeMismatch { detail, .. } if detail.contains("base_wasm") && detail.contains("fat LTO")),
        "got {err:?}"
    );

    let not_json = describing("not json");
    let err = verify_described(&name, &not_json, &expected).unwrap_err();
    assert!(
        matches!(err, CliError::DescribeMismatch { .. }),
        "got {err:?}"
    );

    // a component without the addon-guest export is a host error, not a pass
    let err = verify_described(&name, &empty_component(), &expected).unwrap_err();
    assert!(matches!(err, CliError::WasmHost { .. }), "got {err:?}");
}

#[test]
fn a_package_round_trips_through_the_reader() {
    let tmp = tempdir().unwrap();
    let component = empty_component();
    let addon = write_addon(tmp.path(), "shop_wasm", Some(&component));
    let out = tmp.path().join("shop.erpaddon");
    let cli = parse(&[
        "addon",
        "package",
        addon.to_str().unwrap(),
        "-o",
        out.to_str().unwrap(),
    ])
    .unwrap();
    let Commands::Addon(cmd) = cli.command else {
        panic!("expected the addon command");
    };
    let AddonAction::Package(args) = cmd.action else {
        panic!("expected package");
    };
    let report = package(&args).unwrap();

    let on_disk = fs::read(&out).unwrap();
    assert_eq!(report.archive_sha256, sha256_hex(&on_disk));
    assert_eq!(report.capabilities, ["migrations-sql"]);
    let package = ErpAddonPackage::open(&out).unwrap();
    assert_eq!(package.name().as_str(), "shop_wasm");
    assert_eq!(package.manifest().shortdesc(), "Shop (WASM)");
    assert_eq!(package.version().to_string(), report.version);
    assert_eq!(package.wasm_binary(), Some(component.as_slice()));
    assert_eq!(package.get_file("data/shop.xml"), Some(&b"<elang/>"[..]));
    assert_eq!(package.client_files(), ["static/dist/app.js"]);
    assert_eq!(
        report.client_dist_digest.as_deref(),
        package.client_dist_digest()
    );
    assert!(package.client_dist_digest().is_some());
    assert!(package.get_file("i18n/id.po").is_some());
    let mut paths: Vec<&str> = package.file_paths().collect();
    paths.sort_unstable();
    assert_eq!(
        paths,
        [
            "SHA256SUMS",
            "data/shop.xml",
            "i18n/id.po",
            "manifest.toml",
            "static/dist/app.js",
            "wasm/shop_wasm.wasm"
        ]
    );
    let text = report.to_string();
    assert!(text.contains(&report.archive_sha256) && text.contains("migrations-sql"));
}

#[test]
fn package_refuses_what_it_cannot_pack_faithfully() {
    let tmp = tempdir().unwrap();
    let packed = |dir: &Path| {
        let out = dir.join("out.erpaddon");
        let cli = parse(&[
            "addon",
            "package",
            dir.to_str().unwrap(),
            "-o",
            out.to_str().unwrap(),
        ])
        .unwrap();
        let Commands::Addon(cmd) = cli.command else {
            panic!("expected the addon command");
        };
        let AddonAction::Package(args) = cmd.action else {
            panic!("expected package");
        };
        (package(&args), out)
    };

    let no_wasm = write_addon(tmp.path(), "no_wasm", None);
    let (result, out) = packed(&no_wasm);
    let err = result.unwrap_err();
    assert!(
        matches!(&err, CliError::WasmBinaryMissing { .. })
            && err.to_string().contains("erp addon build"),
        "got {err:?}"
    );
    assert!(!out.exists());

    let garbage = write_addon(tmp.path(), "garbage", Some(b"not a component"));
    let (result, out) = packed(&garbage);
    assert!(matches!(result.unwrap_err(), CliError::WasmHost { .. }));
    assert!(!out.exists());

    let component = empty_component();
    let no_data = write_addon(tmp.path(), "no_data", Some(&component));
    fs::remove_file(no_data.join("data/shop.xml")).unwrap();
    let (result, _) = packed(&no_data);
    assert!(matches!(result.unwrap_err(), CliError::Io { .. }));

    #[cfg(unix)]
    {
        let linked = write_addon(tmp.path(), "linked", Some(&component));
        fs::remove_file(linked.join("data/shop.xml")).unwrap();
        std::os::unix::fs::symlink("/etc/hosts", linked.join("data/shop.xml")).unwrap();
        let (result, _) = packed(&linked);
        assert!(
            matches!(&result.unwrap_err(), CliError::InvalidInput { reason, .. } if reason.contains("symlink"))
        );
    }
}

#[test]
#[ignore = "needs the wasm32-wasip2 target; scripts/ci.sh step wasm-cli-package runs the binary, ERP_CLI_WASM_ADDONS runs this"]
fn the_cli_builds_checks_and_packages_the_dual_fixture() {
    let addons = std::env::var("ERP_CLI_WASM_ADDONS")
        .unwrap_or_else(|_| panic!("ERP_CLI_WASM_ADDONS must name crates/erp-test-models/addons"));
    let addons = PathBuf::from(addons);
    let target_dir = std::env::var("ERP_CLI_WASM_TARGET_DIR").ok();
    let mut build = vec![
        "addon".to_owned(),
        "build".to_owned(),
        "test_wasm_dual_b".to_owned(),
        "--target".to_owned(),
        "wasm".to_owned(),
        "--addons-path".to_owned(),
        addons.display().to_string(),
    ];
    if let Some(dir) = &target_dir {
        build.extend(["--target-dir".to_owned(), dir.clone()]);
    }
    let cli = Cli::try_parse_from(std::iter::once("erp".to_owned()).chain(build)).unwrap();
    let Commands::Addon(cmd) = cli.command else {
        panic!("expected the addon command");
    };
    let AddonAction::Build(args) = cmd.action else {
        panic!("expected build");
    };
    let report = build_wasm(&args, &SystemToolchain).unwrap();
    let expected: BTreeSet<String> = ["test_wasm_dual_a", "test_wasm_dual_b"]
        .map(str::to_owned)
        .into();
    assert_eq!(report.described, expected);
    assert!(report.wasm_path.ends_with("wasm/test_wasm_dual_b.wasm"));
    assert_eq!(fs::read(&report.wasm_path).unwrap().len(), report.bytes);

    let out = tempdir().unwrap();
    let output = out.path().join("b.erpaddon");
    let cli = parse(&[
        "addon",
        "package",
        "test_wasm_dual_b",
        "--addons-path",
        addons.to_str().unwrap(),
        "-o",
        output.to_str().unwrap(),
    ])
    .unwrap();
    let Commands::Addon(cmd) = cli.command else {
        panic!("expected the addon command");
    };
    let AddonAction::Package(args) = cmd.action else {
        panic!("expected package");
    };
    let packaged = package(&args).unwrap();
    let read = ErpAddonPackage::open(&output).unwrap();
    assert_eq!(read.archive_sha256(), packaged.archive_sha256);
    assert_eq!(
        read.wasm_binary(),
        Some(fs::read(&report.wasm_path).unwrap().as_slice())
    );
}
