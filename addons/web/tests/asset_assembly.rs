//! Pure UI3 assembly contracts; durable publication is tested separately.
#![allow(clippy::unwrap_used, clippy::expect_used, reason = "test assertions")]
use erp_addon_web::assets::assembly::{
    AddonAssets, AssemblyError, AssemblyOptions, AssetInput, assemble,
};
use erp_sdk::ui_compiler::{BuildMode, CompileOptions, LimitKind};
use std::io::Read;

fn options() -> AssemblyOptions {
    AssemblyOptions::new(
        "compiler-1",
        "runtime-1",
        "en_US",
        "main",
        CompileOptions::new(BuildMode::Prod),
    )
    .unwrap()
}
fn addon(name: &str) -> AddonAssets {
    let mut a = AddonAssets::new(name, "1.0", "runtime-1").unwrap();
    a.push(
        AssetInput::module(
            "static/dist/main.js",
            &format!("@erpforall/{name}"),
            b"/*! Fixture MIT notice */\nexport function register(ctx) { return ctx; }".to_vec(),
        )
        .unwrap(),
    )
    .unwrap();
    a.set_entry(&format!("@erpforall/{name}")).unwrap();
    a
}

#[test]
fn deterministic_bundle_key_compression_and_notices() {
    let mut a = addon("web");
    a.push(
        AssetInput::template(
            "static/src/view.xml",
            "<templates><t t-name=\"web.X\"><div/></t></templates>",
        )
        .unwrap(),
    )
    .unwrap();
    a.push(AssetInput::static_file("static/icon.svg", b"<svg/>".to_vec()).unwrap())
        .unwrap();
    a.push(
        AssetInput::module(
            "static/dist/abi.js",
            "@erpforall/elang/abi",
            b"export {};".to_vec(),
        )
        .unwrap(),
    )
    .unwrap();
    let first = assemble(&[a.clone()], &options()).unwrap();
    let second = assemble(&[a], &options()).unwrap();
    assert_eq!(first, second);
    assert_eq!(first.key().len(), 64);
    let notice = first
        .blobs()
        .iter()
        .find(|b| b.bytes().starts_with(b"/*! Fixture MIT notice */"))
        .unwrap();
    let mut gzip_bytes = Vec::new();
    flate2::read::GzDecoder::new(notice.gzip())
        .read_to_end(&mut gzip_bytes)
        .unwrap();
    assert_eq!(gzip_bytes, notice.bytes());
    let mut brotli_bytes = Vec::new();
    brotli::Decompressor::new(notice.brotli(), 4096)
        .read_to_end(&mut brotli_bytes)
        .unwrap();
    assert_eq!(brotli_bytes, notice.bytes());
    assert_eq!(
        first.static_files()["/web/static/icon.svg"],
        format!(
            "/erp/assets/{}.svg",
            first
                .blobs()
                .iter()
                .find(|b| b.bytes() == b"<svg/>")
                .unwrap()
                .sha256()
        )
    );
    assert!(first.imports().contains_key("web.X"));
    assert!(first.imports().contains_key("@erpforall/web"));
}

#[test]
fn cache_key_includes_each_boundary_and_compiler_setting() {
    let baseline = assemble(&[addon("web")], &options()).unwrap();
    let changed_options = [
        AssemblyOptions::new(
            "compiler-2",
            "runtime-1",
            "en_US",
            "main",
            CompileOptions::new(BuildMode::Prod),
        )
        .unwrap(),
        AssemblyOptions::new(
            "compiler-1",
            "runtime-1",
            "id_ID",
            "main",
            CompileOptions::new(BuildMode::Prod),
        )
        .unwrap(),
        AssemblyOptions::new(
            "compiler-1",
            "runtime-1",
            "en_US",
            "main",
            CompileOptions::new(BuildMode::Dev),
        )
        .unwrap(),
        AssemblyOptions::new(
            "compiler-1",
            "runtime-1",
            "en_US",
            "main",
            CompileOptions::new(BuildMode::Prod).with_limits(
                erp_sdk::ui_compiler::CompilerLimits::DEFAULT
                    .lowered(LimitKind::FileBytes, 1024)
                    .unwrap(),
            ),
        )
        .unwrap(),
        AssemblyOptions::new(
            "compiler-1",
            "runtime-1",
            "en_US",
            "main",
            CompileOptions::new(BuildMode::Prod)
                .with_custom_directive("sample")
                .unwrap(),
        )
        .unwrap(),
    ];
    for o in changed_options {
        assert_ne!(baseline.key(), assemble(&[addon("web")], &o).unwrap().key());
    }
    let mut a = addon("web");
    a.push(AssetInput::static_file("static/data.txt", b"changed".to_vec()).unwrap())
        .unwrap();
    assert_ne!(baseline.key(), assemble(&[a], &options()).unwrap().key());
    let first = assemble(&[addon("a"), addon("bc")], &options()).unwrap();
    let second = assemble(&[addon("ab"), addon("c")], &options()).unwrap();
    assert_ne!(first.key(), second.key());
    let reversed = assemble(&[addon("bc"), addon("a")], &options()).unwrap();
    assert_ne!(first.key(), reversed.key());
    let changed_version = AddonAssets::new("web", "1.1", "runtime-1").unwrap();
    assert_ne!(
        baseline.key(),
        assemble(&[changed_version], &options()).unwrap().key()
    );
}

#[test]
fn rejects_unimplemented_group_with_exact_diagnostic() {
    let error = AssemblyOptions::new(
        "compiler-1",
        "runtime-1",
        "en_US",
        "graph",
        CompileOptions::new(BuildMode::Prod),
    )
    .unwrap_err();
    assert!(matches!(&error, AssemblyError::UnsupportedGroup { group } if group == "graph"));
    assert_eq!(
        error.to_string(),
        "lazy group 'graph' is not supported until a later release"
    );
}

#[test]
fn rejects_runtime_mismatch_before_compilation() {
    let error = assemble(&[AddonAssets::new("web", "1", "old").unwrap()], &options()).unwrap_err();
    assert!(
        matches!(&error, AssemblyError::RuntimeAbi { addon, expected, actual } if addon == "web" && expected == "runtime-1" && actual == "old")
    );
    assert_eq!(
        error.to_string(),
        "addon 'web' runtime ABI 'old' does not match 'runtime-1'"
    );
}

#[test]
fn rejects_traversal_and_non_declared_media_types() {
    for path in [
        "static/../secret.js",
        "/static/file.js",
        "static//file.js",
        "static/%2e%2e/file.js",
        "static/file\\name.js",
    ] {
        let error = AssetInput::static_file(path, vec![]).unwrap_err();
        assert!(matches!(&error, AssemblyError::InvalidInput { field, .. } if *field == "path"));
        assert_eq!(
            error.to_string(),
            format!("invalid path '{path}': expected a normalized relative static path")
        );
    }
    let error = AssetInput::static_file("static/payload.html", vec![]).unwrap_err();
    assert!(
        matches!(&error, AssemblyError::UnsupportedMedia { path } if path == "static/payload.html")
    );
    assert_eq!(
        error.to_string(),
        "unsupported static media type for 'static/payload.html'"
    );
}

#[test]
fn rejects_duplicate_addons_paths_and_imports() {
    let error = assemble(&[addon("web"), addon("web")], &options()).unwrap_err();
    assert!(
        matches!(&error, AssemblyError::Duplicate { kind, value } if *kind == "addon" && value == "web")
    );
    assert_eq!(error.to_string(), "duplicate addon 'web'");
    let mut a = addon("web");
    let error = a
        .push(AssetInput::module("static/dist/main.js", "other", vec![]).unwrap())
        .unwrap_err();
    assert!(
        matches!(&error, AssemblyError::Duplicate { kind, value } if *kind == "path" && value == "static/dist/main.js")
    );
    assert_eq!(error.to_string(), "duplicate path 'static/dist/main.js'");
    let mut b = AddonAssets::new("other", "1", "runtime-1").unwrap();
    b.push(AssetInput::module("static/dist/entry.js", "@erpforall/web", vec![]).unwrap())
        .unwrap();
    let error = assemble(&[addon("web"), b], &options()).unwrap_err();
    assert!(
        matches!(&error, AssemblyError::Duplicate { kind, value } if *kind == "specifier" && value == "@erpforall/web")
    );
    assert_eq!(error.to_string(), "duplicate specifier '@erpforall/web'");
}

#[test]
fn broken_templates_are_not_returned_as_an_assembled_bundle() {
    let mut a = addon("web");
    a.push(
        AssetInput::template(
            "static/src/broken.xml",
            "<templates><t t-name=\"web.Child\" t-inherit=\"web.Missing\"/></templates>",
        )
        .unwrap(),
    )
    .unwrap();
    let error = assemble(&[a], &options()).unwrap_err();
    assert!(matches!(&error, AssemblyError::Compile { .. }));
    // Compiler diagnostics include location prose and are covered by compiler-specific exact assertions.
    assert!(error.to_string().contains("web.Missing"), "{error}");
    assert!(error.to_string().contains("broken.xml"), "{error}");
}

#[test]
fn register_sequence_follows_addon_load_order() {
    let bundle = assemble(&[addon("base"), addon("web")], &options()).unwrap();
    let code = std::str::from_utf8(bundle.register_module().bytes()).unwrap();
    assert!(code.find("@erpforall/base").unwrap() < code.find("@erpforall/web").unwrap());
    assert!(code.contains("export const registrations"), "{code}");
    assert!(!code.contains("async function register"), "{code}");
    assert!(code.find("register:r0").unwrap() < code.find("register:r1").unwrap());
}

#[test]
fn refuses_unbounded_or_injected_identifiers_and_source_maps() {
    let error = AssetInput::module(
        "static/dist/main.js",
        "https://outside.invalid/x.js",
        vec![],
    )
    .unwrap_err();
    assert!(matches!(&error, AssemblyError::InvalidInput { field, .. } if *field == "specifier"));
    assert_eq!(
        error.to_string(),
        "invalid specifier 'https://outside.invalid/x.js': expected a bounded bare module specifier"
    );
    let mut a = AddonAssets::new("web", "1", "runtime-1").unwrap();
    a.push(
        AssetInput::module(
            "static/dist/main.js",
            "@erpforall/web",
            b"export {};\n//# sourceMappingURL=secret.map".to_vec(),
        )
        .unwrap(),
    )
    .unwrap();
    let error = assemble(&[a], &options()).unwrap_err();
    assert!(
        matches!(&error, AssemblyError::ProductionSourceMap { addon, path } if addon == "web" && path == "static/dist/main.js")
    );
    assert_eq!(
        error.to_string(),
        "production module 'web/static/dist/main.js' contains sourceMappingURL"
    );
}

#[test]
fn stored_bundle_admission_rejects_tampered_bytes() {
    let bundle = assemble(&[addon("web")], &options()).unwrap();
    let encoded = serde_json::to_vec(&bundle).unwrap();
    assert_eq!(
        bundle,
        erp_addon_web::assets::assembly::AssembledBundle::decode(&encoded).unwrap()
    );
    let mut raw: serde_json::Value = serde_json::from_slice(&encoded).unwrap();
    raw["blobs"][0]["bytes"][0] = serde_json::json!(0);
    let error = erp_addon_web::assets::assembly::AssembledBundle::decode(
        &serde_json::to_vec(&raw).unwrap(),
    )
    .unwrap_err();
    assert!(matches!(&error, AssemblyError::CorruptBundle { .. }));
    assert_eq!(
        error.to_string(),
        "stored UI bundle is corrupt: blob SHA-256 mismatch"
    );
}

#[test]
fn metadata_manifest_rehydrates_only_verified_supplied_blobs() {
    let bundle = assemble(&[addon("web")], &options()).unwrap();
    let manifest = bundle.encode_manifest().unwrap();
    let raw: serde_json::Value = serde_json::from_slice(&manifest).unwrap();
    assert!(raw.get("blobs").is_none());
    let restored = erp_addon_web::assets::assembly::AssembledBundle::decode_manifest(
        &manifest,
        bundle.blobs().to_vec(),
    )
    .unwrap();
    assert_eq!(restored, bundle);
    let error =
        erp_addon_web::assets::assembly::AssembledBundle::decode_manifest(&manifest, vec![])
            .unwrap_err();
    assert!(matches!(&error, AssemblyError::CorruptBundle { .. }));
    assert_eq!(
        error.to_string(),
        "stored UI bundle is corrupt: missing referenced blob"
    );
}

#[test]
fn entry_must_resolve_to_a_declared_module() {
    let mut a = AddonAssets::new("web", "1", "runtime-1").unwrap();
    a.set_entry("@erpforall/missing").unwrap();
    let error = assemble(&[a], &options()).unwrap_err();
    assert!(
        matches!(&error, AssemblyError::MissingEntry { addon, specifier } if addon == "web" && specifier == "@erpforall/missing")
    );
    assert_eq!(
        error.to_string(),
        "addon 'web' entry '@erpforall/missing' is not a declared module"
    );
}

#[test]
fn runtime_identity_changes_bundle_key_when_addon_abi_matches() {
    let first = assemble(
        &[AddonAssets::new("web", "1", "runtime-1").unwrap()],
        &options(),
    )
    .unwrap();
    let second = assemble(
        &[AddonAssets::new("web", "1", "runtime-2").unwrap()],
        &AssemblyOptions::new(
            "compiler-1",
            "runtime-2",
            "en_US",
            "main",
            CompileOptions::new(BuildMode::Prod),
        )
        .unwrap(),
    )
    .unwrap();
    assert_ne!(first.key(), second.key());
}

#[test]
fn oversized_declared_files_fail_before_assembly() {
    let limit = erp_addon_web::assets::assembly::MAX_ASSET_BYTES;
    let error = AssetInput::static_file("static/large.txt", vec![0; limit + 1]).unwrap_err();
    assert!(
        matches!(&error, AssemblyError::Limit { name, max } if *name == "file bytes" && *max == limit)
    );
    assert_eq!(
        error.to_string(),
        format!("UI asset file bytes exceeds hard limit {limit}")
    );
}

#[test]
fn compiled_templates_require_declared_runtime_module() {
    let mut a = addon("web");
    a.push(
        AssetInput::template(
            "static/src/view.xml",
            "<templates><t t-name=\"web.X\"><div/></t></templates>",
        )
        .unwrap(),
    )
    .unwrap();
    let error = assemble(&[a], &options()).unwrap_err();
    assert!(matches!(&error, AssemblyError::MissingRuntime { .. }));
    assert_eq!(
        error.to_string(),
        "compiled templates require declared runtime module '@erpforall/elang/abi'"
    );
}

#[test]
fn stored_manifest_retains_build_mode() {
    let production = assemble(&[addon("web")], &options()).unwrap();
    assert_eq!(
        serde_json::to_value(&production).unwrap()["mode"],
        serde_json::json!("prod")
    );
    let development = assemble(
        &[addon("web")],
        &AssemblyOptions::new(
            "compiler-1",
            "runtime-1",
            "en_US",
            "main",
            CompileOptions::new(BuildMode::Dev),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(
        serde_json::to_value(&development).unwrap()["mode"],
        serde_json::json!("dev")
    );
    let restored = erp_addon_web::assets::assembly::AssembledBundle::decode_manifest(
        &development.encode_manifest().unwrap(),
        development.blobs().to_vec(),
    )
    .unwrap();
    assert_eq!(
        serde_json::to_value(&restored).unwrap()["mode"],
        serde_json::json!("dev")
    );
    assert_eq!(restored, development);
}

fn readmitted_blob(
    blob: &erp_addon_web::assets::assembly::Blob,
    gzip: Vec<u8>,
    brotli: Vec<u8>,
) -> Result<erp_addon_web::assets::assembly::Blob, AssemblyError> {
    erp_addon_web::assets::assembly::Blob::from_stored(
        blob.sha256().to_owned(),
        blob.bytes().to_vec(),
        gzip,
        brotli,
        blob.media_type().to_owned(),
        blob.extension().to_owned(),
    )
}

#[test]
fn alternate_valid_compressed_representations_remain_admissible() {
    use std::io::Write;
    let bundle = assemble(&[addon("web")], &options()).unwrap();
    let blob = &bundle.blobs()[0];
    let mut encoder = flate2::GzBuilder::new()
        .mtime(123)
        .filename("previous-codec.js")
        .write(Vec::new(), flate2::Compression::best());
    encoder.write_all(blob.bytes()).unwrap();
    let alternate_gzip = encoder.finish().unwrap();
    assert_ne!(alternate_gzip, blob.gzip());
    let mut alternate_brotli = Vec::new();
    brotli::BrotliCompress(
        &mut std::io::Cursor::new(blob.bytes()),
        &mut alternate_brotli,
        &brotli::enc::BrotliEncoderParams {
            quality: 1,
            lgwin: 18,
            ..Default::default()
        },
    )
    .unwrap();
    let admitted = readmitted_blob(blob, alternate_gzip.clone(), alternate_brotli.clone()).unwrap();
    assert_eq!(admitted.bytes(), blob.bytes());
    assert_eq!(admitted.gzip(), alternate_gzip);
    assert_eq!(admitted.brotli(), alternate_brotli);
}

#[test]
fn compressed_representations_reject_trailing_bytes() {
    let bundle = assemble(&[addon("web")], &options()).unwrap();
    let blob = &bundle.blobs()[0];
    let mut trailing_gzip = blob.gzip().to_vec();
    trailing_gzip.push(0);
    let error = readmitted_blob(blob, trailing_gzip, blob.brotli().to_vec()).unwrap_err();
    assert!(matches!(&error, AssemblyError::CorruptBundle { .. }));
    assert_eq!(
        error.to_string(),
        "stored UI bundle is corrupt: gzip trailing data"
    );
    let mut trailing_brotli = blob.brotli().to_vec();
    trailing_brotli.push(0);
    let error = readmitted_blob(blob, blob.gzip().to_vec(), trailing_brotli).unwrap_err();
    assert!(matches!(&error, AssemblyError::CorruptBundle { .. }));
    assert_eq!(
        error.to_string(),
        "stored UI bundle is corrupt: Brotli trailing data"
    );
}

#[test]
fn compressed_inflation_is_bounded_by_identity_bytes() {
    use std::io::Write;
    let bundle = assemble(&[addon("web")], &options()).unwrap();
    let blob = &bundle.blobs()[0];
    let expansion = vec![0; 1024 * 1024];
    let mut encoder = flate2::GzBuilder::new().write(Vec::new(), flate2::Compression::best());
    encoder.write_all(&expansion).unwrap();
    let error =
        readmitted_blob(blob, encoder.finish().unwrap(), blob.brotli().to_vec()).unwrap_err();
    assert!(matches!(&error, AssemblyError::CorruptBundle { .. }));
    assert_eq!(
        error.to_string(),
        "stored UI bundle is corrupt: blob compressed bytes mismatch"
    );
    let mut compressed = Vec::new();
    brotli::BrotliCompress(
        &mut std::io::Cursor::new(expansion),
        &mut compressed,
        &brotli::enc::BrotliEncoderParams::default(),
    )
    .unwrap();
    let error = readmitted_blob(blob, blob.gzip().to_vec(), compressed).unwrap_err();
    assert!(matches!(&error, AssemblyError::CorruptBundle { .. }));
    assert_eq!(
        error.to_string(),
        "stored UI bundle is corrupt: blob compressed bytes mismatch"
    );
}

#[test]
fn assembly_attempts_are_observable_for_no_rebuild_request_regressions() {
    let before = erp_addon_web::assets::assembly::assembly_calls();
    assemble(&[addon("web")], &options()).unwrap();
    let after = erp_addon_web::assets::assembly::assembly_calls();
    // Other tests in this binary may assemble concurrently; isolated HTTP fixtures assert equality
    // across the request phase, after their own startup/install has completed.
    assert!(after >= before.saturating_add(1));
}

#[test]
fn prebuilt_modules_keep_their_supplied_legal_notices_and_source_bytes() {
    let mut a = AddonAssets::new("web", "1", "runtime-1").unwrap();
    a.push(AssetInput::module("static/dist/main.js", "@erpforall/web", b"/*! Fixture license one */\n// ordinary prebuilt comment retained\nexport function register(ctx) { /*! Fixture license two */ ctx.value = 1 + 1; }\n/*! Fixture license three */".to_vec()).unwrap()).unwrap();
    a.set_entry("@erpforall/web").unwrap();
    let bundle = assemble(&[a], &options()).unwrap();
    let url = &bundle.imports()["@erpforall/web"];
    let output = std::str::from_utf8(
        bundle
            .blobs()
            .iter()
            .find(|b| b.url() == *url)
            .unwrap()
            .bytes(),
    )
    .unwrap();
    assert!(
        output.contains("ordinary prebuilt comment retained"),
        "{output}"
    );
    for notice in [
        "/*! Fixture license one */",
        "/*! Fixture license two */",
        "/*! Fixture license three */",
    ] {
        assert!(output.contains(notice), "missing {notice}: {output}");
    }
}

#[test]
fn compiled_template_modules_use_production_minification_and_keep_their_license() {
    use erp_sdk::ui_compiler::{AddonName, TemplatePath, TemplateSet, TemplateSource};
    let xml = "<templates><t t-name=\"web.X\"><div><span t-out=\"this.props.name\"/></div></t></templates>";
    let mut a = AddonAssets::new("web", "1", "runtime-1").unwrap();
    a.push(
        AssetInput::module(
            "static/dist/abi.js",
            "@erpforall/elang/abi",
            b"export {};".to_vec(),
        )
        .unwrap(),
    )
    .unwrap();
    a.push(AssetInput::template("static/src/view.xml", xml).unwrap())
        .unwrap();
    let bundle = assemble(&[a], &options()).unwrap();
    let mut set = TemplateSet::new();
    set.push(TemplateSource::addon_file(
        AddonName::new("web").unwrap(),
        TemplatePath::new("static/src/view.xml").unwrap(),
        xml,
    ));
    let emitted =
        erp_sdk::ui_compiler::compile(&set, &CompileOptions::new(BuildMode::Prod)).unwrap();
    let url = &bundle.imports()["web.X"];
    let output = std::str::from_utf8(
        bundle
            .blobs()
            .iter()
            .find(|b| b.url() == *url)
            .unwrap()
            .bytes(),
    )
    .unwrap();
    assert_ne!(output, emitted.modules()[0].code());
    assert!(
        output.contains("/*! SPDX-License-Identifier: MIT OR Apache-2.0 */"),
        "{output}"
    );
}

#[test]
fn one_content_hash_can_have_distinct_declared_media_and_rehydrate_from_one_row() {
    use std::collections::BTreeMap;
    let identity = b"/* valid as both JavaScript and CSS */".to_vec();
    let mut a = AddonAssets::new("web", "1", "runtime-1").unwrap();
    a.push(
        AssetInput::module("static/dist/empty.js", "@erpforall/empty", identity.clone()).unwrap(),
    )
    .unwrap();
    a.push(AssetInput::static_file("static/empty.css", identity.clone()).unwrap())
        .unwrap();
    let bundle = assemble(&[a], &options()).unwrap();
    let artifacts: Vec<_> = bundle
        .blobs()
        .iter()
        .filter(|b| b.bytes() == identity)
        .collect();
    assert_eq!(artifacts.len(), 2);
    assert_eq!(artifacts[0].sha256(), artifacts[1].sha256());
    assert_ne!(artifacts[0].media_type(), artifacts[1].media_type());
    assert_ne!(artifacts[0].url(), artifacts[1].url());
    let mut rows = BTreeMap::new();
    for blob in bundle.blobs() {
        rows.entry(blob.sha256().to_owned())
            .or_insert_with(|| blob.clone());
    }
    let restored = erp_addon_web::assets::assembly::AssembledBundle::decode_manifest(
        &bundle.encode_manifest().unwrap(),
        rows.into_values().collect(),
    )
    .unwrap();
    assert_eq!(restored, bundle);
    assert!(restored.static_files()["/web/static/empty.css"].ends_with(".css"));
    assert!(restored.imports()["@erpforall/empty"].ends_with(".js"));
}

#[test]
fn registration_reference_uses_url_when_css_has_the_same_raw_hash() {
    use std::collections::BTreeMap;
    let baseline = assemble(
        &[AddonAssets::new("web", "1", "runtime-1").unwrap()],
        &options(),
    )
    .unwrap();
    let mut addon = AddonAssets::new("web", "1", "runtime-1").unwrap();
    addon
        .push(
            AssetInput::static_file(
                "static/same.css",
                baseline.register_module().bytes().to_vec(),
            )
            .unwrap(),
        )
        .unwrap();
    let bundle = assemble(&[addon], &options()).unwrap();
    let mut rows = BTreeMap::new();
    for blob in bundle.blobs() {
        rows.entry(blob.sha256().to_owned())
            .or_insert_with(|| blob.clone());
    }
    let restored = erp_addon_web::assets::assembly::AssembledBundle::decode_manifest(
        &bundle.encode_manifest().unwrap(),
        rows.into_values().collect(),
    )
    .unwrap();
    assert_eq!(restored.register_module().extension(), "js");
    assert_eq!(
        restored.register_module().media_type(),
        "text/javascript; charset=utf-8"
    );
    assert_eq!(restored, bundle);
}

#[test]
fn manifest_artifact_media_is_validated_independently_of_raw_row_metadata() {
    use std::collections::BTreeMap;
    let identity = b"/* media alias */".to_vec();
    let mut addon = AddonAssets::new("web", "1", "runtime-1").unwrap();
    addon
        .push(
            AssetInput::module(
                "static/dist/shared.js",
                "@erpforall/shared",
                identity.clone(),
            )
            .unwrap(),
        )
        .unwrap();
    addon
        .push(AssetInput::static_file("static/shared.css", identity).unwrap())
        .unwrap();
    let bundle = assemble(&[addon], &options()).unwrap();
    let mut rows = BTreeMap::new();
    for blob in bundle.blobs() {
        rows.entry(blob.sha256().to_owned())
            .or_insert_with(|| blob.clone());
    }
    let mut metadata: serde_json::Value =
        serde_json::from_slice(&bundle.encode_manifest().unwrap()).unwrap();
    let artifact = metadata["artifacts"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .find(|a| a["extension"] == "css")
        .unwrap();
    artifact["media_type"] = serde_json::json!("text/html");
    let error = erp_addon_web::assets::assembly::AssembledBundle::decode_manifest(
        &serde_json::to_vec(&metadata).unwrap(),
        rows.into_values().collect(),
    )
    .unwrap_err();
    assert!(matches!(&error, AssemblyError::CorruptBundle { .. }));
    assert_eq!(
        error.to_string(),
        "stored UI bundle is corrupt: artifact media type mismatch"
    );
}

#[test]
fn raw_row_admission_refuses_duplicates_instead_of_dropping_them() {
    let bundle = assemble(&[addon("web")], &options()).unwrap();
    // This fixture has the declared dist module and a distinct generated registration module.
    assert_eq!(bundle.blobs().len(), 2);
    let duplicate = vec![bundle.blobs()[0].clone(), bundle.blobs()[0].clone()];
    let error = erp_addon_web::assets::assembly::AssembledBundle::decode_manifest(
        &bundle.encode_manifest().unwrap(),
        duplicate,
    )
    .unwrap_err();
    assert!(matches!(&error, AssemblyError::CorruptBundle { .. }));
    assert_eq!(
        error.to_string(),
        "stored UI bundle is corrupt: duplicate raw content row"
    );
}

#[test]
fn generated_registration_module_exports_original_entries_in_runtime_load_order() {
    use oxc_ast::ast::{
        ArrayExpressionElement, BindingPattern, Declaration, Expression, ObjectPropertyKind,
        PropertyKey, Statement,
    };
    let bundle = assemble(
        &[addon("base"), addon("web")],
        &AssemblyOptions::new(
            "compiler-1",
            "runtime-1",
            "en_US",
            "main",
            CompileOptions::new(BuildMode::Dev),
        )
        .unwrap(),
    )
    .unwrap();
    let source = std::str::from_utf8(bundle.register_module().bytes()).unwrap();
    let allocator = oxc_allocator::Allocator::default();
    let parsed = oxc_parser::Parser::new(&allocator, source, oxc_span::SourceType::mjs()).parse();
    assert!(parsed.diagnostics.is_empty(), "{:?}", parsed.diagnostics);
    let exports: Vec<_> = parsed
        .program
        .body
        .iter()
        .filter_map(|statement| match statement {
            Statement::ExportDeclaration(export) => Some(&export.declaration),
            _ => None,
        })
        .collect();
    assert_eq!(exports.len(), 1);
    let Declaration::VariableDeclaration(declaration) = exports[0] else {
        panic!("registration module must export data, not a callback wrapper")
    };
    assert_eq!(declaration.declarations.len(), 1);
    let binding = &declaration.declarations[0];
    let BindingPattern::BindingIdentifier(name) = &binding.id else {
        panic!("registration export must have one name")
    };
    assert_eq!(name.name.as_str(), "registrations");
    let Some(Expression::ArrayExpression(entries)) = &binding.init else {
        panic!("registration export must be an array")
    };
    let mut actual = Vec::new();
    for entry in &entries.elements {
        let ArrayExpressionElement::ObjectExpression(entry) = entry else {
            panic!("registration must be an object")
        };
        let mut id = None;
        let mut function = None;
        for property in &entry.properties {
            let ObjectPropertyKind::ObjectProperty(property) = property else {
                panic!("registration cannot spread unknown fields")
            };
            let PropertyKey::StaticIdentifier(key) = &property.key else {
                panic!("registration key must be static")
            };
            match key.name.as_str() {
                "id" => {
                    let Expression::StringLiteral(value) = &property.value else {
                        panic!("addon id must be a string")
                    };
                    id = Some(value.value.as_str());
                }
                "register" => {
                    let Expression::Identifier(value) = &property.value else {
                        panic!(
                            "register must be the imported original function, never an async wrapper"
                        )
                    };
                    function = Some(value.name.as_str());
                }
                other => panic!("unexpected registration field {other}"),
            }
        }
        actual.push((id.unwrap(), function.unwrap()));
    }
    assert_eq!(actual, [("base", "r0"), ("web", "r1")]);
}

#[test]
fn new_blobs_use_declared_gzip9_brotli11_forms_and_keep_old_stored_variants() {
    use erp_addon_web::assets::assembly::Blob;
    use std::io::{Cursor, Write};

    let data = "Encoded asset contract: č漢字 repeated content.\n"
        .repeat(64)
        .into_bytes();
    let mut a = addon("web");
    a.push(AssetInput::static_file("static/encoding.txt", data.clone()).unwrap())
        .unwrap();
    let bundle = assemble(&[a], &options()).unwrap();
    let blob = bundle
        .blobs()
        .iter()
        .find(|blob| blob.bytes() == data)
        .unwrap();
    let mut encoder = flate2::GzBuilder::new()
        .mtime(0)
        .write(Vec::new(), flate2::Compression::best());
    encoder.write_all(&data).unwrap();
    assert_eq!(blob.gzip(), encoder.finish().unwrap());
    let mut expected_brotli = Vec::new();
    let parameters = brotli::enc::BrotliEncoderParams {
        quality: 11,
        lgwin: 22,
        ..Default::default()
    };
    brotli::BrotliCompress(&mut Cursor::new(&data), &mut expected_brotli, &parameters).unwrap();
    assert_eq!(blob.brotli(), expected_brotli);

    let mut older_gzip = flate2::GzBuilder::new()
        .mtime(123)
        .write(Vec::new(), flate2::Compression::fast());
    older_gzip.write_all(&data).unwrap();
    let older_gzip = older_gzip.finish().unwrap();
    let mut older_brotli = Vec::new();
    let older_parameters = brotli::enc::BrotliEncoderParams {
        quality: 5,
        lgwin: 22,
        ..Default::default()
    };
    brotli::BrotliCompress(
        &mut Cursor::new(&data),
        &mut older_brotli,
        &older_parameters,
    )
    .unwrap();
    let admitted = Blob::from_stored(
        blob.sha256().to_owned(),
        data,
        older_gzip.clone(),
        older_brotli.clone(),
        blob.media_type().to_owned(),
        blob.extension().to_owned(),
    )
    .unwrap();
    assert_eq!(admitted.gzip(), older_gzip);
    assert_eq!(admitted.brotli(), older_brotli);
}
