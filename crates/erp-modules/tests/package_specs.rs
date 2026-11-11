#![allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]

mod common;
use common::*;

use std::io::{Cursor, Write};

use erp_modules::error::Error;
use erp_modules::manifest::{Manifest, ModuleName};
use erp_modules::package::{
    AdminConsent, ErpAddonPackage, ErpAddonPackageWriter, MAX_ARCHIVE_BYTES, MAX_ENTRIES,
    MAX_ENTRY_BYTES, MAX_LIST_PACKAGES, MAX_TOTAL_BYTES, PackageLimits, WasmCapability,
    create_package_table_sql, delete_package, get_package_by_name, get_package_by_name_version,
    get_package_by_sha256, init_package_table, list_packages, sha256_hex, store_package,
};
use erp_modules::system::InitOptions;
use erp_orm::SUPERUSER_ID;
use erp_testkit::TestDb;
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipArchive, ZipWriter};

const DUMMY_WASM: &[u8] = b"\x00asm\x01\x00\x00\x00";

fn create_test_manifest(name: &str, capabilities: &[&str]) -> String {
    manifest_text(name, "1.0.0", Some(capabilities))
}

/// A manifest with a `[wasm]` section when `capabilities` is `Some`.
fn manifest_text(name: &str, version: &str, capabilities: Option<&[&str]>) -> String {
    let wasm = match capabilities {
        Some(caps) => {
            let caps_str = caps
                .iter()
                .map(|c| format!("\"{c}\""))
                .collect::<Vec<_>>()
                .join(", ");
            format!("\n[wasm]\nsdk_version = \"1.0\"\ncapabilities = [{caps_str}]\n")
        }
        None => String::new(),
    };
    format!("name = \"{name}\"\nlicense = \"MIT OR Apache-2.0\"\nversion = \"{version}\"\n{wasm}")
}

/// A valid package with a `[wasm]` section and its component.
fn wasm_package_bytes(name: &str, version: &str, capabilities: &[&str]) -> Vec<u8> {
    let mut writer = ErpAddonPackageWriter::new();
    writer.add_file(
        "manifest.toml",
        manifest_text(name, version, Some(capabilities)).as_bytes(),
    );
    writer.add_file(format!("wasm/{name}.wasm"), DUMMY_WASM);
    writer.build_zip().unwrap()
}

fn sums_text(entries: &[(&str, &[u8])]) -> String {
    entries
        .iter()
        .map(|(n, c)| format!("{}  {n}\n", sha256_hex(c)))
        .collect()
}

/// Build an archive directly with the zip crate. `sums` overrides the generated `SHA256SUMS`.
fn forge(entries: &[(&str, &[u8])], method: CompressionMethod, sums: Option<&str>) -> Vec<u8> {
    let mut cursor = Cursor::new(Vec::new());
    {
        let mut zip = ZipWriter::new(&mut cursor);
        let options = SimpleFileOptions::default().compression_method(method);
        for (name, content) in entries {
            zip.start_file(*name, options).unwrap();
            zip.write_all(content).unwrap();
        }
        let generated = sums_text(entries);
        zip.start_file("SHA256SUMS", options).unwrap();
        zip.write_all(sums.unwrap_or(&generated).as_bytes())
            .unwrap();
        zip.finish().unwrap();
    }
    cursor.into_inner()
}

/// Replace every occurrence of `from` by `to` (same length) in the archive bytes: lets a test
/// give entries names the zip writer would refuse (duplicates, odd paths).
fn patch_names(bytes: &mut [u8], from: &str, to: &[u8]) {
    assert_eq!(from.len(), to.len(), "patch must keep the length");
    let needle = from.as_bytes();
    let mut hits = 0;
    let mut i = 0;
    while i + needle.len() <= bytes.len() {
        if &bytes[i..i + needle.len()] == needle {
            bytes[i..i + needle.len()].copy_from_slice(to);
            hits += 1;
            i += needle.len();
        } else {
            i += 1;
        }
    }
    assert!(hits >= 2, "name found in local and central headers");
}

fn entry_count(bytes: &[u8]) -> usize {
    ZipArchive::new(Cursor::new(bytes)).unwrap().len()
}

fn consented(bytes: &[u8], name: &str, caps: &[&str]) -> erp_modules::package::ConsentedPackage {
    let package = ErpAddonPackage::from_bytes(bytes).unwrap();
    let mut consent = AdminConsent::new(ModuleName::new(name).unwrap());
    for cap in caps {
        consent.approve_capability(*cap);
    }
    package.consent(&consent).unwrap()
}

#[test]
fn spec_wasm_017_package_archive_parsing_and_sha256_integrity() {
    let manifest_content = create_test_manifest("test_pkg_a", &["migrations-sql"]);
    let mut writer = ErpAddonPackageWriter::new();
    writer.add_file("manifest.toml", manifest_content.as_bytes());
    writer.add_file("wasm/test_pkg_a.wasm", DUMMY_WASM);
    writer.add_file("data/init.xml", b"<odoo></odoo>");

    let zip_bytes = writer.build_zip().unwrap();
    assert!(!zip_bytes.is_empty());

    // 1. Valid package load
    let package = ErpAddonPackage::from_bytes(&zip_bytes).unwrap();
    assert_eq!(package.name().as_str(), "test_pkg_a");
    assert_eq!(package.version().to_string(), "1.0.0");
    assert_eq!(package.wasm_binary(), Some(DUMMY_WASM));
    assert_eq!(
        package.get_file("data/init.xml"),
        Some(b"<odoo></odoo>".as_slice())
    );
    assert_eq!(package.archive_sha256(), sha256_hex(&zip_bytes));
    assert_eq!(package.archive_bytes(), zip_bytes.as_slice());

    // 2. Failure: tampered file content (checksum mismatch with SHA256SUMS)
    let tampered = forge(
        &[
            ("manifest.toml", manifest_content.as_bytes()),
            (
                "wasm/test_pkg_a.wasm",
                b"\x00asm\x01\x00\x00\x00_tampered_payload",
            ),
        ],
        CompressionMethod::Deflated,
        Some(&sums_text(&[
            ("manifest.toml", manifest_content.as_bytes()),
            ("wasm/test_pkg_a.wasm", DUMMY_WASM),
        ])),
    );
    let err = ErpAddonPackage::from_bytes(&tampered).unwrap_err();
    match &err {
        Error::PackageIntegrity { message } => assert!(
            message.contains("checksum mismatch for file 'wasm/test_pkg_a.wasm'"),
            "unexpected error message: {message}"
        ),
        other => panic!("expected PackageIntegrity, got: {other:?}"),
    }

    // 2b. Failure: damaged end-of-central-directory record
    let mut corrupted = zip_bytes.clone();
    let eocd = corrupted
        .windows(4)
        .rposition(|w| w == [0x50, 0x4b, 0x05, 0x06])
        .expect("end of central directory present");
    corrupted[eocd] ^= 0xff;
    let err = ErpAddonPackage::from_bytes(&corrupted).unwrap_err();
    match &err {
        Error::PackageIntegrity { message } => assert!(
            message.contains("invalid zip archive"),
            "unexpected error message: {message}"
        ),
        other => panic!("expected PackageIntegrity, got: {other:?}"),
    }

    // 2c. Failure: bit flip inside stored file data (zip CRC rejects it while reading)
    let payload = b"a distinctive payload that appears once in the archive";
    let mut stored = forge(
        &[
            ("manifest.toml", manifest_content.as_bytes()),
            ("wasm/test_pkg_a.wasm", DUMMY_WASM),
            ("data/blob.txt", payload),
        ],
        CompressionMethod::Stored,
        None,
    );
    let at = stored
        .windows(payload.len())
        .position(|w| w == payload)
        .expect("stored payload present");
    stored[at] ^= 0x01;
    let err = ErpAddonPackage::from_bytes(&stored).unwrap_err();
    match &err {
        Error::PackageIntegrity { message } => assert!(
            message.contains("failed to read file \"data/blob.txt\""),
            "unexpected error message: {message}"
        ),
        other => panic!("expected PackageIntegrity, got: {other:?}"),
    }

    // 3. Failure: file not recorded in SHA256SUMS
    let unrecorded = forge(
        &[("manifest.toml", manifest_content.as_bytes())],
        CompressionMethod::Deflated,
        Some("e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855  other.txt\n"),
    );
    let err = ErpAddonPackage::from_bytes(&unrecorded).unwrap_err();
    match &err {
        Error::PackageIntegrity { message } => assert!(
            message.contains("file 'manifest.toml' in archive is not recorded in SHA256SUMS"),
            "unexpected error message: {message}"
        ),
        other => panic!("expected PackageIntegrity, got: {other:?}"),
    }

    // 3b. Failure: SHA256SUMS lists a file the archive lacks
    let missing = forge(
        &[("manifest.toml", manifest_content.as_bytes())],
        CompressionMethod::Deflated,
        Some(&format!(
            "{}{}",
            sums_text(&[("manifest.toml", manifest_content.as_bytes())]),
            sums_text(&[("other.txt", b"")])
        )),
    );
    let err = ErpAddonPackage::from_bytes(&missing).unwrap_err();
    match &err {
        Error::PackageIntegrity { message } => assert!(
            message.contains("file 'other.txt' listed in SHA256SUMS is missing from archive"),
            "unexpected error message: {message}"
        ),
        other => panic!("expected PackageIntegrity, got: {other:?}"),
    }

    // 4. Failure: missing SHA256SUMS file
    let mut no_sums_writer = ErpAddonPackageWriter::new();
    no_sums_writer.add_file("manifest.toml", manifest_content.as_bytes());
    let no_sums_bytes = no_sums_writer.build_raw_zip().unwrap();
    let err = ErpAddonPackage::from_bytes(&no_sums_bytes).unwrap_err();
    match &err {
        Error::PackageIntegrity { message } => assert!(
            message.contains("missing SHA256SUMS"),
            "unexpected message: {message}"
        ),
        other => panic!("expected PackageIntegrity, got: {other:?}"),
    }
}

#[test]
fn spec_wasm_017_archive_path_rules_reject_traversal_and_ambiguity() {
    let manifest_ok = manifest_text("path_pkg", "1.0.0", None);
    let cases: &[(&str, &str)] = &[
        ("../evil.txt", "traversal"),
        ("a/../b", "traversal"),
        ("a/..", "traversal"),
        ("/abs/file", "absolute path"),
        ("C:/windows/x", "`:`"),
        ("a/./b", "`.` path segment"),
        ("./a", "`.` path segment"),
        ("a\\b", "backslash"),
        ("a\\..\\b", "backslash"),
        ("a//b", "empty path segment"),
    ];
    for (bad, reason) in cases {
        let placeholder = "Z".repeat(bad.len());
        let mut bytes = forge(
            &[
                ("manifest.toml", manifest_ok.as_bytes()),
                (placeholder.as_str(), b"x"),
            ],
            CompressionMethod::Deflated,
            None,
        );
        patch_names(&mut bytes, &placeholder, bad.as_bytes());
        let err = ErpAddonPackage::from_bytes(&bytes).unwrap_err();
        match &err {
            Error::PackageFormat { message } => assert!(
                message.contains("illegal path") && message.contains(reason),
                "case {bad:?}: unexpected message: {message}"
            ),
            other => panic!("case {bad:?}: expected PackageFormat, got: {other:?}"),
        }
    }
}

#[test]
fn spec_wasm_017_duplicate_colliding_and_symlink_entries_are_refused() {
    let manifest = manifest_text("dup_pkg", "1.0.0", None);

    // Duplicate entry name (two entries, one name): the writer refuses, so rename after writing.
    let mut dup = forge(
        &[
            ("manifest.toml", manifest.as_bytes()),
            ("a.txt", b"first"),
            ("b.txt", b"second"),
        ],
        CompressionMethod::Deflated,
        None,
    );
    patch_names(&mut dup, "b.txt", b"a.txt");
    let err = ErpAddonPackage::from_bytes(&dup).unwrap_err();
    match &err {
        Error::PackageFormat { message } => assert!(
            message.contains("duplicate entry names"),
            "unexpected message: {message}"
        ),
        other => panic!("expected PackageFormat, got: {other:?}"),
    }

    // Case-colliding names
    let case = forge(
        &[
            ("manifest.toml", manifest.as_bytes()),
            ("static/dist/A.js", b"1"),
            ("static/dist/a.js", b"2"),
        ],
        CompressionMethod::Deflated,
        None,
    );
    let err = ErpAddonPackage::from_bytes(&case).unwrap_err();
    match &err {
        Error::PackageFormat { message } => assert!(
            message.contains("collide on case-insensitive filesystems"),
            "unexpected message: {message}"
        ),
        other => panic!("expected PackageFormat, got: {other:?}"),
    }

    // A file that is also the directory of another file
    let both = forge(
        &[
            ("manifest.toml", manifest.as_bytes()),
            ("data", b"file"),
            ("data/x.xml", b"<odoo/>"),
        ],
        CompressionMethod::Deflated,
        None,
    );
    let err = ErpAddonPackage::from_bytes(&both).unwrap_err();
    match &err {
        Error::PackageFormat { message } => assert!(
            message.contains("is both a file and a directory"),
            "unexpected message: {message}"
        ),
        other => panic!("expected PackageFormat, got: {other:?}"),
    }

    // Symlink entry
    let mut cursor = Cursor::new(Vec::new());
    {
        let mut zip = ZipWriter::new(&mut cursor);
        let options = SimpleFileOptions::default().compression_method(CompressionMethod::Stored);
        zip.start_file("manifest.toml", options).unwrap();
        zip.write_all(manifest.as_bytes()).unwrap();
        zip.add_symlink("link", "/etc/passwd", options).unwrap();
        zip.start_file("SHA256SUMS", options).unwrap();
        zip.write_all(
            sums_text(&[
                ("manifest.toml", manifest.as_bytes()),
                ("link", b"/etc/passwd"),
            ])
            .as_bytes(),
        )
        .unwrap();
        zip.finish().unwrap();
    }
    let err = ErpAddonPackage::from_bytes(&cursor.into_inner()).unwrap_err();
    match &err {
        Error::PackageFormat { message } => assert!(
            message.contains("symlink entry \"link\""),
            "unexpected message: {message}"
        ),
        other => panic!("expected PackageFormat, got: {other:?}"),
    }

    // The writer refuses what the reader refuses
    let mut writer = ErpAddonPackageWriter::new();
    writer.add_file("manifest.toml", manifest.as_bytes());
    writer.add_file("../evil", b"x");
    let err = writer.build_zip().unwrap_err();
    assert!(
        matches!(&err, Error::PackageFormat { message } if message.contains("traversal")),
        "unexpected: {err:?}"
    );
}

/// total size, each at the bound and one past it)
#[test]
fn spec_wasm_017_archive_bounds_hold_at_the_limit_and_fail_one_past() {
    let manifest = manifest_text("bound_pkg", "1.0.0", None);
    let big = vec![0u8; 4096];
    let entries: &[(&str, &[u8])] = &[
        ("manifest.toml", manifest.as_bytes()),
        ("data/big.bin", &big),
        ("data/small.txt", b"small"),
    ];
    let bytes = forge(entries, CompressionMethod::Deflated, None);
    let sums = sums_text(entries);

    let ok = |limits: &PackageLimits| ErpAddonPackage::from_bytes_with(&bytes, None, limits);
    let expect_limit = |err: Error, name: &str, max: u64, observed: u64| match err {
        Error::PackageLimitExceeded {
            limit,
            max: m,
            observed_at_least,
            ..
        } => {
            assert_eq!(limit, name);
            assert_eq!(m, max);
            assert_eq!(observed_at_least, observed);
        }
        other => panic!("expected PackageLimitExceeded({name}), got: {other:?}"),
    };

    // Archive bytes
    let mut limits = PackageLimits::default();
    limits.max_archive_bytes = bytes.len();
    ok(&limits).unwrap();
    limits.max_archive_bytes = bytes.len() - 1;
    expect_limit(
        ok(&limits).unwrap_err(),
        "archive bytes",
        (bytes.len() - 1) as u64,
        bytes.len() as u64,
    );

    // Entry count (SHA256SUMS counts)
    let count = entry_count(&bytes);
    let mut limits = PackageLimits::default();
    limits.max_entries = count;
    ok(&limits).unwrap();
    limits.max_entries = count - 1;
    expect_limit(
        ok(&limits).unwrap_err(),
        "entry count",
        (count - 1) as u64,
        count as u64,
    );

    // Per-entry uncompressed bytes: the largest entry is `big`
    let mut limits = PackageLimits::default();
    limits.max_entry_bytes = big.len() as u64;
    ok(&limits).unwrap();
    limits.max_entry_bytes = big.len() as u64 - 1;
    expect_limit(
        ok(&limits).unwrap_err(),
        "entry uncompressed bytes",
        big.len() as u64 - 1,
        big.len() as u64,
    );

    // Total uncompressed bytes (all entries plus SHA256SUMS)
    let total = (manifest.len() + big.len() + b"small".len() + sums.len()) as u64;
    let mut limits = PackageLimits::default();
    limits.max_total_bytes = total;
    ok(&limits).unwrap();
    limits.max_total_bytes = total - 1;
    match ok(&limits).unwrap_err() {
        Error::PackageLimitExceeded {
            limit,
            max,
            observed_at_least,
            ..
        } => {
            assert_eq!(limit, "total uncompressed bytes");
            assert_eq!(max, total - 1);
            assert_eq!(observed_at_least, total);
        }
        other => panic!("expected PackageLimitExceeded, got: {other:?}"),
    }

    // The default bounds are the documented constants
    let defaults = PackageLimits::default();
    assert_eq!(defaults.max_archive_bytes, MAX_ARCHIVE_BYTES);
    assert_eq!(defaults.max_entries, MAX_ENTRIES);
    assert_eq!(defaults.max_entry_bytes, MAX_ENTRY_BYTES);
    assert_eq!(defaults.max_total_bytes, MAX_TOTAL_BYTES);
}

#[test]
fn spec_wasm_017_default_entry_count_bound() {
    let build = |files: usize| {
        let mut writer = ErpAddonPackageWriter::new();
        writer.add_file("manifest.toml", manifest_text("many_pkg", "1.0.0", None));
        for i in 0..files {
            writer.add_file(format!("data/f{i}.txt"), b"x");
        }
        writer.build_zip().unwrap()
    };
    // manifest + files + SHA256SUMS
    let at_bound = build(MAX_ENTRIES - 2);
    assert_eq!(entry_count(&at_bound), MAX_ENTRIES);
    ErpAddonPackage::from_bytes(&at_bound).unwrap();

    let past = build(MAX_ENTRIES - 1);
    assert_eq!(entry_count(&past), MAX_ENTRIES + 1);
    let err = ErpAddonPackage::from_bytes(&past).unwrap_err();
    assert!(
        matches!(
            err,
            Error::PackageLimitExceeded { limit: "entry count", max, observed_at_least, .. }
                if max == MAX_ENTRIES as u64 && observed_at_least == MAX_ENTRIES as u64 + 1
        ),
        "unexpected: {err:?}"
    );
}

/// Locate the central-directory header of `name` and the local header, and rewrite their
/// uncompressed-size fields to `size`.
fn forge_declared_size(bytes: &mut [u8], name: &str, size: u32) {
    let name = name.as_bytes();
    let mut patched = 0;
    for i in 0..bytes.len().saturating_sub(46) {
        let central = bytes[i..i + 4] == [0x50, 0x4b, 0x01, 0x02]
            && bytes.get(i + 46..i + 46 + name.len()) == Some(name);
        let local = bytes[i..i + 4] == [0x50, 0x4b, 0x03, 0x04]
            && bytes.get(i + 30..i + 30 + name.len()) == Some(name);
        if central {
            bytes[i + 24..i + 28].copy_from_slice(&size.to_le_bytes());
            patched += 1;
        } else if local {
            bytes[i + 22..i + 26].copy_from_slice(&size.to_le_bytes());
            patched += 1;
        }
    }
    assert_eq!(patched, 2, "local and central headers patched");
}

#[test]
fn spec_wasm_017_forged_size_headers_cannot_force_allocation_or_inflation() {
    let manifest = manifest_text("bomb_pkg", "1.0.0", None);
    // 1 MiB of zeros deflates to about a kilobyte.
    let bomb = vec![0u8; 1024 * 1024];
    let entries: &[(&str, &[u8])] = &[
        ("manifest.toml", manifest.as_bytes()),
        ("data/bomb.bin", &bomb),
    ];
    let mut limits = PackageLimits::default();
    limits.max_entry_bytes = 64 * 1024;

    // Honest header: refused from the declared size, before any inflation.
    let honest = forge(entries, CompressionMethod::Deflated, None);
    assert!(honest.len() < 8 * 1024, "the bomb is small on the wire");
    let err = ErpAddonPackage::from_bytes_with(&honest, None, &limits).unwrap_err();
    assert!(
        matches!(
            err,
            Error::PackageLimitExceeded { limit: "entry uncompressed bytes", max, observed_at_least, .. }
                if max == 64 * 1024 && observed_at_least == bomb.len() as u64
        ),
        "unexpected: {err:?}"
    );

    // Header lies low (claims 10 bytes): the bounded read, not the header, decides.
    let mut lying = honest.clone();
    forge_declared_size(&mut lying, "data/bomb.bin", 10);
    let err = ErpAddonPackage::from_bytes_with(&lying, None, &limits).unwrap_err();
    assert!(
        matches!(
            err,
            Error::PackageLimitExceeded { .. } | Error::PackageIntegrity { .. }
        ),
        "unexpected: {err:?}"
    );

    // Header lies high (claims 4 GiB - 16): refused without allocating that much.
    let mut huge = forge(
        &[
            ("manifest.toml", manifest.as_bytes()),
            ("data/tiny.bin", b"tiny"),
        ],
        CompressionMethod::Deflated,
        None,
    );
    forge_declared_size(&mut huge, "data/tiny.bin", u32::MAX - 15);
    let err = ErpAddonPackage::from_bytes(&huge).unwrap_err();
    assert!(
        matches!(
            err,
            Error::PackageLimitExceeded { limit: "entry uncompressed bytes", observed_at_least, .. }
                if observed_at_least == u64::from(u32::MAX - 15)
        ),
        "unexpected: {err:?}"
    );
}

#[test]
fn spec_wasm_017_sha256sums_grammar_is_strict() {
    let manifest = manifest_text("sums_pkg", "1.0.0", None);
    let manifest_hash = sha256_hex(manifest.as_bytes());
    let load = |sums: &str, extra: &[(&str, &[u8])]| {
        let mut entries: Vec<(&str, &[u8])> = vec![("manifest.toml", manifest.as_bytes())];
        entries.extend_from_slice(extra);
        ErpAddonPackage::from_bytes(&forge(&entries, CompressionMethod::Deflated, Some(sums)))
    };
    let line = format!("{manifest_hash}  manifest.toml\n");

    // Well-formed baseline, and the binary-mode `*` marker
    load(&line, &[]).unwrap();
    load(&format!("{manifest_hash} *manifest.toml\n"), &[]).unwrap();

    let malformed: &[(&str, String, &str)] = &[
        (
            "duplicate",
            format!("{line}{line}"),
            "duplicate entry for \"manifest.toml\"",
        ),
        (
            "non-hex",
            format!("{}  manifest.toml\n", "g".repeat(64)),
            "64 lowercase hex",
        ),
        (
            "uppercase hex",
            format!("{}  manifest.toml\n", manifest_hash.to_uppercase()),
            "64 lowercase hex",
        ),
        (
            "one space",
            format!("{manifest_hash} manifest.toml\n"),
            "separated by two characters",
        ),
        (
            "blank line",
            format!("{line}\n{line}"),
            "malformed SHA256SUMS line 2",
        ),
        (
            "short hash",
            format!("{}  manifest.toml\n", &manifest_hash[..63]),
            "malformed SHA256SUMS line 1",
        ),
        (
            "self reference",
            format!("{line}{manifest_hash}  SHA256SUMS\n"),
            "must not list itself",
        ),
        (
            "carriage return",
            format!("{manifest_hash}  manifest.toml\r\n"),
            "control character",
        ),
    ];
    for (what, sums, expect) in malformed {
        let err = load(sums, &[]).unwrap_err();
        match &err {
            Error::PackageIntegrity { message } | Error::PackageFormat { message } => assert!(
                message.contains(expect),
                "{what}: unexpected message: {message}"
            ),
            other => panic!("{what}: unexpected error kind: {other:?}"),
        }
    }

    // Traversal in SHA256SUMS is refused with the same rule as the archive side
    let err = load(&format!("{line}{manifest_hash}  ../x\n"), &[]).unwrap_err();
    assert!(
        matches!(&err, Error::PackageFormat { message } if message.contains("traversal")),
        "unexpected: {err:?}"
    );

    // Legitimate names are not mistaken for traversal; internal whitespace is kept exactly.
    let dots: &[u8] = b"dots";
    let spaced: &[u8] = b"spaced";
    let ok_entries: &[(&str, &[u8])] = &[
        ("manifest.toml", manifest.as_bytes()),
        ("data/a..b.txt", dots),
        ("data/two  spaces.txt", spaced),
    ];
    let package =
        ErpAddonPackage::from_bytes(&forge(ok_entries, CompressionMethod::Deflated, None)).unwrap();
    assert_eq!(package.get_file("data/a..b.txt"), Some(dots));
    assert_eq!(package.get_file("data/two  spaces.txt"), Some(spaced));
}

#[test]
fn spec_wasm_015_capability_values_are_validated_and_unknown_names_refused() {
    for ok in [
        "migrations-sql",
        "network:outbound:https://api.stripe.com/v1/*",
        "network:outbound:http://localhost:8080/hook",
        "network:outbound:https://api.example.com",
        "filesystem:temp:50mb",
        "filesystem:temp:1gb",
        "filesystem:temp:512kb",
    ] {
        let cap = WasmCapability::parse(ok).unwrap();
        assert_eq!(cap.canonical_str(), ok, "round trip");
    }
    // The stored form is normalised, so the consent text and the enforcer see one host.
    let cap = WasmCapability::parse("network:outbound:HTTPS://API.Stripe.COM:443/v1/*");
    assert!(cap.is_err(), "scheme is matched exactly");
    let cap = WasmCapability::parse("network:outbound:https://API.Stripe.COM:443/v1/*").unwrap();
    assert_eq!(
        cap,
        WasmCapability::NetworkOutbound("https://api.stripe.com:443/v1/*".to_string())
    );
    assert_eq!(
        WasmCapability::parse("network:outbound:http://[::1]:8080/hook")
            .unwrap()
            .canonical_str(),
        "network:outbound:http://[::1]:8080/hook"
    );
    assert_eq!(
        WasmCapability::parse("network:outbound:http://10.0.0.5").unwrap(),
        WasmCapability::NetworkOutbound("http://10.0.0.5".to_string())
    );
    for (bad, why) in [
        ("", "cannot be empty"),
        ("network:outbound:", "http:// or https://"),
        ("network:outbound:*", "http:// or https://"),
        ("network:outbound:ftp://x", "http:// or https://"),
        ("network:outbound:https://", "needs a host"),
        (
            "network:outbound:https://*.evil.com",
            "wildcard in the host",
        ),
        ("network:outbound:https://a.com*", "wildcard in the host"),
        (
            "network:outbound:https://a.com/*/b",
            "whole last path segment",
        ),
        (
            "network:outbound:https://a.com/v1*",
            "whole last path segment",
        ),
        ("network:outbound:https://a b.com", "printable ASCII"),
        ("network:outbound:https://user@a.com/x", "userinfo"),
        ("network:outbound:https://a.com@evil.com/x", "userinfo"),
        ("network:outbound:https://a.com\\@evil.com", "backslash"),
        ("network:outbound:https://a%2ecom/x", "percent-encoded"),
        ("network:outbound:https://a.com/x?q=1", "`?`, `#` or `%`"),
        ("network:outbound:https://a.com:0/x", "invalid port"),
        ("network:outbound:https://a.com:99999/x", "invalid port"),
        ("network:outbound:https://a.com:8o/x", "invalid port"),
        ("network:outbound:https://a..com/x", "invalid DNS name"),
        ("network:outbound:https://-a.com/x", "invalid DNS name"),
        ("network:outbound:https://999.1.1.1/x", "non-canonical IPv4"),
        ("network:outbound:https://127.1/x", "non-canonical IPv4"),
        (
            "network:outbound:https://2130706433/x",
            "non-canonical IPv4",
        ),
        ("network:outbound:https://0x7f.1/x", "non-canonical IPv4"),
        (
            "network:outbound:https://0x7f000001/x",
            "non-canonical IPv4",
        ),
        ("network:outbound:https://1e100/x", "non-canonical IPv4"),
        ("network:outbound:https://a.1e100/x", "non-canonical IPv4"),
        ("network:outbound:https://01.1.1.1/x", "non-canonical IPv4"),
        ("network:outbound:https://1.2.3.4.5/x", "non-canonical IPv4"),
        ("network:outbound:https://a.com/a/../b", "canonical"),
        ("network:outbound:https://[::1/x", "unterminated"),
        ("filesystem:temp:", "requires a size"),
        ("filesystem:temp:abc", "requires a size"),
        ("filesystem:temp:50", "unit must be kb, mb or gb"),
        ("filesystem:temp:50tb", "unit must be kb, mb or gb"),
        ("filesystem:temp:0mb", "between 1 and"),
        ("filesystem:temp:99999mb", "between 1 and"),
        ("Network:Outbound:https://x.com", "unknown capability"),
        ("migrations-sql ", "unknown capability"),
        ("my_vendor:thing", "unknown capability"),
    ] {
        let err = WasmCapability::parse(bad).unwrap_err();
        match &err {
            Error::InvalidCapability { capability, reason } => {
                assert_eq!(capability, bad);
                assert!(reason.contains(why), "{bad:?}: unexpected reason: {reason}");
            }
            other => panic!("{bad:?}: expected InvalidCapability, got: {other:?}"),
        }
    }

    // The manifest refuses an unknown capability when it is loaded.
    let err = Manifest::from_toml(
        "cap_bad",
        &manifest_text("cap_bad", "1.0.0", Some(&["my_vendor:thing"])),
    )
    .unwrap_err();
    assert!(matches!(err, Error::InvalidCapability { .. }), "{err:?}");
}

#[test]
fn spec_wasm_017_manifest_must_match_the_archive() {
    // [wasm] without its component
    let manifest = manifest_text("wasm_missing", "1.0.0", Some(&[]));
    let mut writer = ErpAddonPackageWriter::new();
    writer.add_file("manifest.toml", manifest.as_bytes());
    let err = ErpAddonPackage::from_bytes(&writer.build_zip().unwrap()).unwrap_err();
    assert!(
        matches!(&err, Error::PackageFormat { message }
            if message.contains("no 'wasm/wasm_missing.wasm'")),
        "unexpected: {err:?}"
    );

    // Invalid technical name in the manifest
    let mut writer = ErpAddonPackageWriter::new();
    writer.add_file(
        "manifest.toml",
        "name = \"bad-name\"\nlicense = \"MIT\"\nversion = \"1.0.0\"\n".as_bytes(),
    );
    let err = ErpAddonPackage::from_bytes(&writer.build_zip().unwrap()).unwrap_err();
    assert!(
        matches!(&err, Error::PackageFormat { message }
            if message.contains("not a valid technical module name")),
        "unexpected: {err:?}"
    );

    // Name longer than the store column
    let long = "n".repeat(129);
    let mut writer = ErpAddonPackageWriter::new();
    writer.add_file("manifest.toml", manifest_text(&long, "1.0.0", None));
    let err = ErpAddonPackage::from_bytes(&writer.build_zip().unwrap()).unwrap_err();
    assert!(
        matches!(&err, Error::PackageFormat { message }
            if message.contains("module name has 129 characters")),
        "unexpected: {err:?}"
    );

    // An explicit name is validated too
    let mut writer = ErpAddonPackageWriter::new();
    writer.add_file("manifest.toml", manifest_text("explicit_ok", "1.0.0", None));
    let bytes = writer.build_zip().unwrap();
    let package = ErpAddonPackage::from_bytes_with_name(&bytes, Some("explicit_ok")).unwrap();
    assert_eq!(package.name().as_str(), "explicit_ok");
    let err = ErpAddonPackage::from_bytes_with_name(&bytes, Some("not valid")).unwrap_err();
    assert!(matches!(err, Error::InvalidModuleName { .. }), "{err:?}");
}

#[tokio::test]
async fn spec_wasm_018_content_addressed_database_storage() {
    let addons = Addons::new();
    let db = TestDb::create().await.unwrap();
    let system = builder(&db, &addons).build().unwrap();
    let init = system
        .initialize(InitOptions::new().with_skip_auto_install(true))
        .await
        .unwrap();
    let mut tx = init.orm.begin(SUPERUSER_ID).await.unwrap();

    init_package_table(&mut tx).await.unwrap();

    let zip_bytes = wasm_package_bytes("stored_addon", "1.0.0", &["migrations-sql"]);
    let package = consented(&zip_bytes, "stored_addon", &["migrations-sql"]);
    let sha256 = package.archive_sha256().to_string();

    // 1. Store package in ir_module_package: the stored blob is the verified archive itself
    let record = store_package(&mut tx, &package).await.unwrap();
    assert_eq!(record.name, "stored_addon");
    assert_eq!(record.version, "1.0.0");
    assert_eq!(record.sha256, sha256);
    assert_eq!(record.package_data, zip_bytes);
    assert_eq!(sha256_hex(&record.package_data), record.sha256);
    assert_eq!(record.capabilities, serde_json::json!(["migrations-sql"]));
    assert_eq!(record.client_dist_digest, None);

    // 2. Retrieve package by sha256
    let fetched = get_package_by_sha256(&mut tx, &sha256)
        .await
        .unwrap()
        .expect("package found");
    assert_eq!(fetched, record);

    // 3. Retrieve package by name and by (name, version)
    let fetched_by_name = get_package_by_name(&mut tx, "stored_addon")
        .await
        .unwrap()
        .expect("found by name");
    assert_eq!(fetched_by_name.sha256, sha256);
    let exact = get_package_by_name_version(&mut tx, "stored_addon", "1.0.0")
        .await
        .unwrap()
        .expect("found by name and version");
    assert_eq!(exact.id, record.id);
    assert!(
        get_package_by_name_version(&mut tx, "stored_addon", "9.9.9")
            .await
            .unwrap()
            .is_none()
    );

    // 4. Listing packages (bounded, paged)
    let list = list_packages(&mut tx, 10, 0).await.unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].name, "stored_addon");
    assert_eq!(list[0].sha256, sha256);
    assert!(list_packages(&mut tx, 10, 1).await.unwrap().is_empty());
    assert!(list_packages(&mut tx, 0, 0).await.is_err());
    assert!(
        list_packages(&mut tx, MAX_LIST_PACKAGES + 1, 0)
            .await
            .is_err()
    );

    // 5. Re-storing the same package changes nothing (no DO UPDATE, no timestamp bump)
    let record2 = store_package(&mut tx, &package).await.unwrap();
    assert_eq!(record2, record);

    // 6. "Latest" is by insertion order and does not move when an older archive is re-stored
    let v2_bytes = wasm_package_bytes("stored_addon", "1.1.0", &["migrations-sql"]);
    let v2 = consented(&v2_bytes, "stored_addon", &["migrations-sql"]);
    let v2_record = store_package(&mut tx, &v2).await.unwrap();
    assert_ne!(v2_record.id, record.id);
    store_package(&mut tx, &package).await.unwrap();
    let latest = get_package_by_name(&mut tx, "stored_addon")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(latest.version, "1.1.0");
    let list = list_packages(&mut tx, 10, 0).await.unwrap();
    assert_eq!(
        list.iter().map(|p| p.version.as_str()).collect::<Vec<_>>(),
        ["1.0.0", "1.1.0"]
    );

    // 7. A different archive under an existing (name, version) is refused, the row is untouched
    let mut other = ErpAddonPackageWriter::new();
    other.add_file(
        "manifest.toml",
        manifest_text("stored_addon", "1.0.0", Some(&["migrations-sql"])),
    );
    other.add_file("wasm/stored_addon.wasm", DUMMY_WASM);
    other.add_file("data/extra.xml", b"<odoo/>");
    let other_bytes = other.build_zip().unwrap();
    let other_pkg = consented(&other_bytes, "stored_addon", &["migrations-sql"]);
    let err = store_package(&mut tx, &other_pkg).await.unwrap_err();
    assert!(
        err.to_string().contains("refusing a different archive"),
        "unexpected: {err}"
    );
    let still = get_package_by_sha256(&mut tx, &sha256)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(still, record);

    // 8. A blob that no longer matches its address is refused on read
    tx.execute(
        "UPDATE ir_module_package SET package_data = $1 WHERE sha256 = $2",
        &[&b"damaged".as_slice(), &sha256],
    )
    .await
    .unwrap();
    let err = get_package_by_sha256(&mut tx, &sha256).await.unwrap_err();
    assert!(err.to_string().contains("is corrupt"), "unexpected: {err}");

    // 9. Delete package
    let deleted = delete_package(&mut tx, &sha256).await.unwrap();
    assert!(deleted);
    let after_delete = get_package_by_sha256(&mut tx, &sha256).await.unwrap();
    assert!(after_delete.is_none());

    // 10. The table enforces the shapes the reader checks
    assert!(
        create_package_table_sql().contains(&MAX_ARCHIVE_BYTES.to_string()),
        "the size CHECK is derived from MAX_ARCHIVE_BYTES"
    );
    let bad = tx
        .execute(
            "INSERT INTO ir_module_package (name, version, sha256, package_data, capabilities) \
             VALUES ('x', '1.0.0', 'NOT-A-HEX-DIGEST', '\\x00', '[]')",
            &[],
        )
        .await;
    assert!(bad.is_err(), "non-hex sha256 must violate the CHECK");

    tx.rollback().await.unwrap();
}

/// install-path witness is required by storage; recording the acknowledgement in the audit trail
/// and the upgrade-with-new-digest comparison are)
#[test]
fn spec_wasm_023_client_code_dist_digest_acknowledgement_gate() {
    // 1. Package with static/dist client code
    let manifest_content = manifest_text("client_addon", "1.0.0", None);
    let mut writer = ErpAddonPackageWriter::new();
    writer.add_file("manifest.toml", manifest_content.as_bytes());
    writer.add_file("static/dist/index.js", b"console.log('client code');");
    writer.add_file("static/dist/assets/style.css", b"body { margin: 0; }");

    let zip_bytes = writer.build_zip().unwrap();
    let package = ErpAddonPackage::from_bytes(&zip_bytes).unwrap();

    let digest = package
        .client_dist_digest()
        .expect("must compute dist digest");
    assert_eq!(digest.len(), 64);
    assert_eq!(
        package.client_files(),
        vec!["static/dist/assets/style.css", "static/dist/index.js"]
    );

    // Gate: without acknowledgement, verify_admin_consent fails loudly
    let mut consent = AdminConsent::new(ModuleName::new("client_addon").unwrap());
    let err = package.verify_admin_consent(&consent).unwrap_err();
    match &err {
        Error::UnacknowledgedClientCode {
            addon,
            client_files,
            expected_digest,
            provided_digest,
        } => {
            assert_eq!(addon.as_str(), "client_addon");
            assert_eq!(
                client_files,
                &["static/dist/assets/style.css", "static/dist/index.js"]
            );
            assert_eq!(expected_digest, digest);
            assert_eq!(provided_digest, &None);
        }
        other => panic!("expected UnacknowledgedClientCode, got: {other:?}"),
    }
    // ... and the consuming path that storage requires fails the same way
    let err = package.clone().consent(&consent).unwrap_err();
    assert!(
        matches!(err, Error::UnacknowledgedClientCode { .. }),
        "{err:?}"
    );

    // Gate: with mismatched acknowledgement digest, fails loudly
    consent.set_client_dist_digest_ack(
        "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
    );
    let err = package.verify_admin_consent(&consent).unwrap_err();
    match &err {
        Error::UnacknowledgedClientCode {
            expected_digest,
            provided_digest,
            ..
        } => {
            assert_eq!(expected_digest, digest);
            assert_eq!(
                provided_digest.as_deref(),
                Some("0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef")
            );
        }
        other => panic!("expected UnacknowledgedClientCode, got: {other:?}"),
    }

    // Gate: with correct acknowledgement digest, succeeds and yields the witness
    consent.set_client_dist_digest_ack(digest);
    package.verify_admin_consent(&consent).unwrap();
    let witness = package.clone().consent(&consent).unwrap();
    assert_eq!(witness.archive_sha256(), package.archive_sha256());

    // 2. Package without static/dist client code: no acknowledgement required
    let mut writer_no_client = ErpAddonPackageWriter::new();
    writer_no_client.add_file("manifest.toml", manifest_content.as_bytes());
    writer_no_client.add_file("data/data.xml", b"<odoo/>");
    let no_client_bytes = writer_no_client.build_zip().unwrap();
    let no_client_pkg = ErpAddonPackage::from_bytes(&no_client_bytes).unwrap();
    assert_eq!(no_client_pkg.client_dist_digest(), None);

    let mut empty_consent = AdminConsent::new(ModuleName::new("client_addon").unwrap());
    no_client_pkg.verify_admin_consent(&empty_consent).unwrap();

    // A stale acknowledgement for client code the package does not carry is refused
    empty_consent.set_client_dist_digest_ack(digest);
    let err = no_client_pkg
        .verify_admin_consent(&empty_consent)
        .unwrap_err();
    assert!(
        matches!(&err, Error::ConsentMismatch { message, .. }
            if message.contains("no static/dist/")),
        "{err:?}"
    );
}

#[test]
fn test_wasm_capabilities_parsing_and_consent_gate() {
    let manifest_toml = r#"name = "cap_pkg"
license = "MIT"
version = "1.0.0"

[wasm]
sdk_version = "1.0"
capabilities = [
    "network:outbound:https://api.stripe.com/v1/*",
    "filesystem:temp:50mb",
    "migrations-sql",
]
"#;
    let manifest = Manifest::from_toml("cap_pkg", manifest_toml).unwrap();
    let wasm = manifest.wasm().expect("[wasm] section present");
    assert_eq!(wasm.sdk_version, "1.0");
    assert_eq!(
        wasm.capabilities,
        vec![
            WasmCapability::NetworkOutbound("https://api.stripe.com/v1/*".to_string()),
            WasmCapability::FilesystemTemp("50mb".to_string()),
            WasmCapability::MigrationsSql,
        ]
    );

    let mut writer = ErpAddonPackageWriter::new();
    writer.add_file("manifest.toml", manifest_toml.as_bytes());
    writer.add_file("wasm/cap_pkg.wasm", DUMMY_WASM);
    let zip_bytes = writer.build_zip().unwrap();
    let package = ErpAddonPackage::from_bytes(&zip_bytes).unwrap();

    // Unapproved capability fails consent gate
    let mut consent = AdminConsent::new(ModuleName::new("cap_pkg").unwrap());
    consent.approve_capability("migrations-sql");
    consent.approve_capability("filesystem:temp:50mb");
    // Missing "network:outbound:https://api.stripe.com/v1/*"
    let err = package.verify_admin_consent(&consent).unwrap_err();
    match &err {
        Error::UnapprovedCapabilities { addon, unapproved } => {
            assert_eq!(addon.as_str(), "cap_pkg");
            assert_eq!(
                unapproved,
                &["network:outbound:https://api.stripe.com/v1/*"]
            );
        }
        other => panic!("expected UnapprovedCapabilities, got: {other:?}"),
    }

    // All approved capabilities pass
    consent.approve_capability("network:outbound:https://api.stripe.com/v1/*");
    package.verify_admin_consent(&consent).unwrap();

    // Stale approval of a capability the package does not request is refused
    consent.approve_capability("filesystem:temp:1gb");
    let err = package.verify_admin_consent(&consent).unwrap_err();
    assert!(
        matches!(&err, Error::ConsentMismatch { message, .. }
            if message.contains("filesystem:temp:1gb")),
        "{err:?}"
    );

    // Consent for another addon is a consent mismatch, not an invalid manifest
    let other = AdminConsent::new(ModuleName::new("someone_else").unwrap());
    let err = package.verify_admin_consent(&other).unwrap_err();
    assert!(
        matches!(&err, Error::ConsentMismatch { message, .. }
            if message.contains("someone_else")),
        "{err:?}"
    );
}

/// with other letter case)
#[test]
fn spec_wasm_023_client_tree_spelling_cannot_bypass_the_gate() {
    let manifest = manifest_text("case_pkg", "1.0.0", None);
    for bad in [
        "Static/dist/app.js",
        "static/DIST/app.js",
        "STATIC/DIST/app.js",
    ] {
        let bytes = forge(
            &[("manifest.toml", manifest.as_bytes()), (bad, b"evil()")],
            CompressionMethod::Deflated,
            None,
        );
        let err = ErpAddonPackage::from_bytes(&bytes).unwrap_err();
        assert!(
            matches!(&err, Error::PackageFormat { message }
                if message.contains("must be spelled exactly `static/dist/`")),
            "{bad}: {err:?}"
        );
    }
    // The exact spelling is the one the gate digests
    let bytes = forge(
        &[
            ("manifest.toml", manifest.as_bytes()),
            ("static/dist/app.js", b"ok()"),
        ],
        CompressionMethod::Deflated,
        None,
    );
    let package = ErpAddonPackage::from_bytes(&bytes).unwrap();
    assert!(package.client_dist_digest().is_some());
}

#[test]
fn spec_wasm_017_directory_prefix_collisions_are_refused() {
    let manifest = manifest_text("prefix_pkg", "1.0.0", None);
    let cases: &[(&[&str], &str)] = &[
        (
            &["Data/x.xml", "data/y.xml"],
            "collide on case-insensitive filesystems",
        ),
        (
            &["data/Sub/x.xml", "data/sub/y.xml"],
            "collide on case-insensitive filesystems",
        ),
        (
            &["Data", "data/y.xml"],
            "collide on case-insensitive filesystems",
        ),
        (&["data", "data/y.xml"], "is both a file and a directory"),
    ];
    for (names, expect) in cases {
        let mut entries: Vec<(&str, &[u8])> = vec![("manifest.toml", manifest.as_bytes())];
        entries.extend(names.iter().map(|n| (*n, b"x".as_slice())));
        let err = ErpAddonPackage::from_bytes(&forge(&entries, CompressionMethod::Deflated, None))
            .unwrap_err();
        assert!(
            matches!(&err, Error::PackageFormat { message } if message.contains(expect)),
            "{names:?}: {err:?}"
        );
    }
}

#[test]
fn technical_name_in_a_package_manifest_is_read_and_checked() {
    let manifest = "technical_name = \"shop_wasm\"\nname = \"Shop (WASM)\"\nversion = \"1.0.0\"\n\n[wasm]\nsdk_version = \"1.0\"\n";
    let mut writer = ErpAddonPackageWriter::new();
    writer.add_file("manifest.toml", manifest.as_bytes());
    writer.add_file("wasm/shop_wasm.wasm", DUMMY_WASM);
    let package = ErpAddonPackage::from_bytes(&writer.build_zip().unwrap()).unwrap();
    assert_eq!(package.name().as_str(), "shop_wasm");
    assert_eq!(package.manifest().shortdesc(), "Shop (WASM)");
    // the same text for another module is refused
    let err = Manifest::from_toml("other_wasm", manifest).unwrap_err();
    assert!(
        matches!(&err, Error::InvalidManifest { message, .. } if message.contains("technical_name")),
        "got {err:?}"
    );
}
