// SPDX-License-Identifier: MIT OR Apache-2.0
//! Localization pack format v1: validation, dated records, origin and licence, loader.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

use std::fs;
use std::path::{Path, PathBuf};

use erp_modules::pack::{
    ALLOWED_LICENCES, Date, FieldSpec, FieldType, ModelSchema, PackError, PackLimits, PackRegistry,
    PackRule, SchemaSet, StageContext, StageOutcome, Value, check_licence,
};
use sha2::{Digest, Sha256};

fn fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/pack_sample")
}

fn f(name: &str, ty: FieldType, required: bool, nullable: bool) -> FieldSpec {
    FieldSpec {
        name: name.into(),
        ty,
        required,
        nullable,
    }
}

fn schemas() -> SchemaSet {
    let text = |n: &str| f(n, FieldType::Text, true, false);
    SchemaSet::new(vec![
        ModelSchema {
            model: "example.account".into(),
            fields: vec![
                text("name"),
                text("code"),
                f("note", FieldType::Text, false, true),
                f("active", FieldType::Bool, false, false),
            ],
            dated_scope: None,
            code_field: Some("code".into()),
            active_field: Some("active".into()),
        },
        ModelSchema {
            model: "example.tax".into(),
            fields: vec![
                text("name"),
                f(
                    "account",
                    FieldType::Ref("example.account".into()),
                    true,
                    false,
                ),
                f("rate", FieldType::Decimal, true, false),
                f("factor", FieldType::Ratio, false, true),
            ],
            dated_scope: None,
            code_field: None,
            active_field: None,
        },
        ModelSchema {
            model: "example.rate".into(),
            fields: vec![
                text("jurisdiction"),
                text("key"),
                f("value", FieldType::Decimal, true, false),
                f("valid_from", FieldType::Date, true, false),
                f("valid_until", FieldType::Date, false, true),
                text("source"),
                text("review_status"),
            ],
            dated_scope: Some(vec!["jurisdiction".into(), "key".into()]),
            code_field: None,
            active_field: None,
        },
    ])
    .unwrap()
}

fn copy_dir(from: &Path, to: &Path) {
    fs::create_dir_all(to).unwrap();
    for e in fs::read_dir(from).unwrap() {
        let e = e.unwrap();
        let target = to.join(e.file_name());
        if e.file_type().unwrap().is_dir() {
            copy_dir(&e.path(), &target);
        } else {
            fs::copy(e.path(), target).unwrap();
        }
    }
}

/// A scratch copy of the sample pack.
struct Scratch {
    _tmp: tempfile::TempDir,
    dir: PathBuf,
}

impl Scratch {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("pack");
        copy_dir(&fixture(), &dir);
        Self { _tmp: tmp, dir }
    }

    fn edit(&self, file: &str, change: impl FnOnce(String) -> String) -> &Self {
        let p = self.dir.join(file);
        let new = change(fs::read_to_string(&p).unwrap());
        fs::write(p, new).unwrap();
        self
    }

    fn write(&self, file: &str, text: &str) -> &Self {
        let p = self.dir.join(file);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, text).unwrap();
        self
    }

    /// Recompute the declared digests of `pack.toml` from the files on disk.
    fn rehash(&self) -> &Self {
        let mut path = String::new();
        let mut out = String::new();
        let text = fs::read_to_string(self.dir.join("pack.toml")).unwrap();
        for line in text.lines() {
            if line.starts_with('[') {
                path.clear();
            }
            if let Some(p) = line.strip_prefix("path = \"") {
                path = p.trim_end_matches('"').to_owned();
            }
            if line.starts_with("sha256 = \"") && !path.is_empty() {
                let bytes = fs::read(self.dir.join(&path)).unwrap_or_default();
                let hex: String = Sha256::digest(&bytes)
                    .iter()
                    .map(|b| format!("{b:02x}"))
                    .collect();
                out.push_str(&format!("sha256 = \"{hex}\"\n"));
                continue;
            }
            out.push_str(line);
            out.push('\n');
        }
        fs::write(self.dir.join("pack.toml"), out).unwrap();
        self
    }
}

struct Env {
    schemas: SchemaSet,
    limits: PackLimits,
}

impl Env {
    fn new() -> Self {
        Self {
            schemas: schemas(),
            limits: PackLimits::default(),
        }
    }

    fn ctx(&self) -> StageContext<'_> {
        StageContext {
            schemas: &self.schemas,
            limits: self.limits,
        }
    }

    fn stage(&self, reg: &mut PackRegistry, s: &Scratch) -> Result<StageOutcome, PackError> {
        reg.stage_dir(&s.dir, &self.ctx())
    }
}

fn refused(s: &Scratch) -> PackError {
    Env::new()
        .stage(&mut PackRegistry::default(), s)
        .unwrap_err()
}

fn assert_refused(s: &Scratch, rule: PackRule, message: &str) -> PackError {
    let e = refused(s);
    assert_eq!(e.rule, rule, "{e}");
    assert_eq!(e.message, message, "{e}");
    e
}

fn sample() -> (PackRegistry, Env) {
    let env = Env::new();
    let mut reg = PackRegistry::default();
    assert_eq!(
        reg.stage_dir(&fixture(), &env.ctx()).unwrap(),
        StageOutcome::New
    );
    (reg, env)
}

// ---- the sample pack ----

#[test]
fn sample_pack_stages_and_records_are_typed() {
    let (reg, _) = sample();
    let pack = reg.get("example.sample", "R1").unwrap();
    let key = |m: &str, k: &str| (m.to_owned(), k.to_owned());
    let root = &pack.templates["chart"];
    let cash = &root.records[&key("example.account", "cash")];
    assert_eq!(cash.fields["code"], Value::Text("1200".into()));
    assert_eq!(cash.template, "example.sample/chart");
    let tax = &root.records[&key("example.tax", "vat_std")];
    let rate = Value::Decimal {
        value: "10".into(),
        unit: "percent".into(),
    };
    assert_eq!(tax.fields["rate"], rate);
    let factor = Value::Ratio {
        numerator: "11".into(),
        denominator: "12".into(),
        unit: "factor".into(),
    };
    assert_eq!(tax.fields["factor"], factor);
    assert_eq!(
        tax.fields["account"],
        Value::Ref {
            pack: None,
            key: "sales".into()
        }
    );
}

#[test]
fn staging_the_same_pack_again_is_a_no_op() {
    let (mut reg, env) = sample();
    let before = reg.get("example.sample", "R1").unwrap().clone();
    assert_eq!(
        reg.stage_dir(&fixture(), &env.ctx()).unwrap(),
        StageOutcome::Unchanged
    );
    assert_eq!(reg.get("example.sample", "R1").unwrap(), &before);
}

#[test]
fn a_reused_release_id_with_other_bytes_is_refused() {
    let (mut reg, env) = sample();
    let s = Scratch::new();
    s.edit("data/accounts.toml", |t| t.replace("Cash", "Petty cash"))
        .rehash();
    let e = env.stage(&mut reg, &s).unwrap_err();
    assert_eq!(e.rule, PackRule::ReleaseImmutable);
    assert_eq!(
        e.message,
        "Rule release example.sample/R1 already exists with different content."
    );
    assert_eq!(
        reg.get("example.sample", "R1").unwrap().templates["chart"].records
            [&("example.account".into(), "cash".into())]
            .fields["name"],
        Value::Text("Cash".into())
    );
}

/// The refusal names the file, the path and the rule.
#[test]
fn every_refusal_names_file_path_and_rule() {
    let s = Scratch::new();
    s.edit("data/accounts.toml", |t| {
        t.replace("code = \"12\"", "mystery = \"12\"")
    })
    .rehash();
    let e = refused(&s);
    assert_eq!(
        e.to_string(),
        "data/accounts.toml: record[0].mystery: [unknown-field] Unknown pack field mystery."
    );
    assert_eq!(PackRule::Limit.as_str(), "resource-limit");
}

// ---- profile ----

#[test]
fn unsupported_format_version_names_found_and_supported() {
    let s = Scratch::new();
    s.edit("pack.toml", |t| {
        t.replace("format_version = 1", "format_version = 2")
    });
    let e = assert_refused(
        &s,
        PackRule::FormatVersion,
        "Pack example.sample format version 2 is unsupported; supported versions: 1.",
    );
    assert_eq!(e.file, "pack.toml");
}

#[test]
fn digests_and_schema_hashes_are_checked() {
    let s = Scratch::new();
    s.edit("data/taxes.toml", |t| t.replace("Standard rate", "Other"));
    let e = refused(&s);
    assert_eq!(
        (e.rule, e.file.as_str()),
        (PackRule::DigestMismatch, "data/taxes.toml")
    );
    let env = Env {
        schemas: SchemaSet::new(vec![ModelSchema {
            model: "example.account".into(),
            fields: vec![f("name", FieldType::Text, true, false)],
            dated_scope: None,
            code_field: None,
            active_field: None,
        }])
        .unwrap(),
        ..Env::new()
    };
    let e = env
        .stage(&mut PackRegistry::default(), &Scratch::new())
        .unwrap_err();
    assert_eq!(e.rule, PackRule::SchemaMismatch);
}

// ---- child templates ----

type Id = (String, String);

fn id(model: &str, key: &str) -> Id {
    (model.to_owned(), key.to_owned())
}

/// Declare a child template `key` of `parent` whose file `data/<key>.toml` holds `body`.
fn add_template(s: &Scratch, key: &str, parent: &str, body: &str) {
    s.write(&format!("data/{key}.toml"), body);
    s.edit("pack.toml", |t| {
        format!(
            "{t}\n[[files]]\npath = \"data/{key}.toml\"\nsha256 = \"{}\"\nkind = \"records\"\n\n[[template]]\nkey = \"{key}\"\nparent = \"{parent}\"\nfiles = [\"data/{key}.toml\"]\n",
            "0".repeat(64)
        )
    })
    .rehash();
}

const BANK_RENAME: &str = "[[record]]\nmodel = \"example.account\"\nkey = \"bank\"\n[record.fields]\nname = \"Renamed\"\n";

#[test]
fn a_child_template_overrides_clears_and_removes_inherited_records() {
    let (reg, _) = sample();
    let pack = reg.get("example.sample", "R1").unwrap();
    let (root, child) = (&pack.templates["chart"], &pack.templates["chart_child"]);
    fn account<'a>(
        t: &'a erp_modules::pack::TemplateData,
        k: &str,
    ) -> Option<&'a erp_modules::pack::Record> {
        t.records.get(&id("example.account", k))
    }
    assert_eq!((root.depth, child.depth), (0, 1));
    assert_eq!(child.code_digits, Some(4));
    let bank = account(child, "bank").unwrap();
    assert_eq!(bank.fields["name"], Value::Text("Current account".into()));
    assert_eq!(
        bank.fields["code"],
        account(root, "bank").unwrap().fields["code"]
    );
    assert_eq!(bank.template, "example.sample/chart_child");
    assert_eq!(
        account(root, "bank").unwrap().fields["name"],
        Value::Text("Bank".into())
    );
    assert_eq!(account(child, "cash").unwrap().fields["note"], Value::Null);
    assert_eq!(account(child, "sales"), account(root, "sales"));
    assert!(account(child, "misc").is_none() && account(root, "misc").is_some());
    assert_eq!(child.tombstones, [id("example.account", "misc")].into());
    assert!(root.tombstones.is_empty());
    assert!(child.records.contains_key(&id("example.tax", "vat_std")));
}

#[test]
fn typed_empty_text_is_not_a_clear() {
    let s = Scratch::new();
    s.edit("data/child.toml", |t| {
        t.replace("clear_fields = [\"note\"]", "[record.fields]\nnote = \"\"")
    })
    .rehash();
    let (mut reg, env) = (PackRegistry::default(), Env::new());
    env.stage(&mut reg, &s).unwrap();
    let t = &reg.get("example.sample", "R1").unwrap().templates["chart_child"];
    assert_eq!(
        t.records[&id("example.account", "cash")].fields["note"],
        Value::Text(String::new())
    );
}

#[test]
fn invalid_clear_fields_are_refused() {
    let cases = [
        (
            "clear_fields = [\"note\"]",
            "clear_fields = [\"note\"]\n[record.fields]\nnote = \"x\"",
            "Field note cannot have both clear_fields and a nonempty value.",
        ),
        (
            "clear_fields = [\"note\"]",
            "clear_fields = [\"name\"]",
            "Field name is not nullable and cannot be cleared.",
        ),
        (
            "clear_fields = [\"note\"]",
            "clear_fields = [\"mystery\"]",
            "Unknown pack field mystery.",
        ),
    ];
    for (from, to, msg) in cases {
        let s = Scratch::new();
        s.edit("data/child.toml", |t| t.replace(from, to)).rehash();
        let e = refused(&s);
        assert_eq!(e.message, msg, "{e}");
        assert_eq!(e.file, "data/child.toml");
        assert!(
            matches!(e.rule, PackRule::ClearFields | PackRule::UnknownField),
            "{e}"
        );
    }
    // a record of the template itself has nothing to clear
    let s = Scratch::new();
    s.edit("data/accounts.toml", |t| {
        t.replacen(
            "key = \"sales\"",
            "key = \"sales\"\nclear_fields = [\"note\"]",
            1,
        )
    })
    .rehash();
    let e = refused(&s);
    assert_eq!(e.rule, PackRule::ClearFields);
    assert!(e.message.contains("overrides an inherited one"), "{e}");
}

#[test]
fn tombstones_are_explicit_and_checked() {
    let tomb = |key: &str| {
        format!("[[record]]\nmodel = \"example.account\"\nkey = \"{key}\"\ntombstone = true\n")
    };
    // not inherited
    let s = Scratch::new();
    s.edit("data/child.toml", |t| t + &tomb("ghost")).rehash();
    let e = refused(&s);
    assert_eq!(e.rule, PackRule::Tombstone, "{e}");
    assert_eq!(
        e.message,
        "Tombstone ghost must remove an inherited record and carry no fields."
    );
    // carries fields
    let s = Scratch::new();
    s.edit("data/child.toml", |t| {
        t.replace(
            "tombstone = true",
            "tombstone = true\n[record.fields]\nname = \"x\"",
        )
    })
    .rehash();
    assert_eq!(refused(&s).rule, PackRule::Tombstone);
    // same key twice (tombstone and override)
    let s = Scratch::new();
    s.edit("data/child.toml", |t| t + &tomb("bank")).rehash();
    assert_eq!(refused(&s).rule, PackRule::DuplicateKey);
    // the taxes of the parent still point at the removed account
    let s = Scratch::new();
    s.edit("data/child.toml", |t| t + &tomb("sales")).rehash();
    let e = refused(&s);
    assert_eq!(e.rule, PackRule::Reference, "{e}");
    assert_eq!(e.message, "Unresolved or undeclared pack reference sales.");
    // a grandchild may add the removed key again; it is no longer a tombstone
    let s = Scratch::new();
    add_template(
        &s,
        "chart_grand",
        "chart_child",
        "[[record]]\nmodel = \"example.account\"\nkey = \"misc\"\n[record.fields]\nname = \"Back\"\ncode = \"9001\"\n",
    );
    let (mut reg, env) = (PackRegistry::default(), Env::new());
    env.stage(&mut reg, &s).unwrap();
    let g = &reg.get("example.sample", "R1").unwrap().templates["chart_grand"];
    assert!(g.tombstones.is_empty() && g.records.contains_key(&id("example.account", "misc")));
}

#[test]
fn parent_cycles_and_depth_are_refused() {
    let s = Scratch::new();
    s.edit("pack.toml", |t| {
        t.replace(
            "country = \"XX\"",
            "country = \"XX\"\nparent = \"chart_child\"",
        )
    });
    assert_refused(
        &s,
        PackRule::Cycle,
        "Template parent cycle: chart -> chart_child -> chart.",
    );
    let s = Scratch::new();
    s.edit("pack.toml", |t| {
        t.replace("country = \"XX\"", "country = \"XX\"\nparent = \"chart\"")
    });
    assert_refused(
        &s,
        PackRule::Cycle,
        "Template parent cycle: chart -> chart.",
    );

    // chart <- chart_child <- c2 <- c3: depth 3
    let s = Scratch::new();
    add_template(&s, "c2", "chart_child", BANK_RENAME);
    add_template(&s, "c3", "c2", BANK_RENAME);
    let mut env = Env::new();
    env.limits = limits_with("l10n.pack.max_parent_depth", 3);
    let mut reg = PackRegistry::default();
    env.stage(&mut reg, &s).unwrap();
    assert_eq!(
        reg.get("example.sample", "R1").unwrap().templates["c3"].depth,
        3
    );
    env.limits = limits_with("l10n.pack.max_parent_depth", 2);
    let e = env.stage(&mut PackRegistry::default(), &s).unwrap_err();
    assert_eq!(
        (e.rule, e.message.as_str()),
        (
            PackRule::Depth,
            "Template parent depth exceeds configured limit."
        )
    );
    assert_eq!(e.file, "pack.toml");
}

#[test]
fn a_long_parent_chain_is_cut_before_its_far_end() {
    let s = Scratch::new();
    // declared leaf first, so no parent is staged yet when the leaf is resolved
    for (key, parent) in [
        ("c6", "c5"),
        ("c5", "c4"),
        ("c4", "c3"),
        ("c3", "c2"),
        ("c2", "chart_child"),
    ] {
        add_template(&s, key, parent, BANK_RENAME);
    }
    let mut env = Env::new();
    env.limits = limits_with("l10n.pack.max_parent_depth", 2);
    let e = env.stage(&mut PackRegistry::default(), &s).unwrap_err();
    assert_eq!(e.rule, PackRule::Depth, "{e}");
    assert_eq!(e.path, "template[c3].parent", "{e}");
}

#[test]
fn template_count_and_code_digits_are_bounded() {
    let s = Scratch::new();
    let mut env = Env::new();
    env.limits = limits_with("l10n.pack.max_templates", 2);
    env.stage(&mut PackRegistry::default(), &s).unwrap();
    env.limits = limits_with("l10n.pack.max_templates", 1);
    let e = env.stage(&mut PackRegistry::default(), &s).unwrap_err();
    assert_eq!(
        (e.rule, e.file.as_str(), e.path.as_str()),
        (PackRule::Limit, "pack.toml", "template")
    );
    assert_eq!(e.message, "Pack input exceeds configured resource limit.");

    let s = Scratch::new();
    s.edit("pack.toml", |t| {
        t.replace("code_digits = 4", "code_digits = 8")
    });
    env.limits = limits_with("l10n.pack.max_code_digits", 8);
    let mut reg = PackRegistry::default();
    env.stage(&mut reg, &s).unwrap();
    let t = &reg.get("example.sample", "R1").unwrap().templates["chart"];
    assert_eq!(
        t.records[&id("example.account", "cash")].fields["code"],
        Value::Text("12000000".into())
    );
    env.limits = limits_with("l10n.pack.max_code_digits", 7);
    let e = env.stage(&mut PackRegistry::default(), &s).unwrap_err();
    assert_eq!(e.rule, PackRule::Padding, "{e}");
    assert!(e.message.contains("l10n.pack.max_code_digits"), "{e}");
    // never more than the string cap
    env.limits = PackLimits {
        max_string_bytes: 37,
        ..limits_with("l10n.pack.max_code_digits", 200)
    };
    s.edit("pack.toml", |t| {
        t.replace("code_digits = 8", "code_digits = 40")
    });
    assert_eq!(
        env.stage(&mut PackRegistry::default(), &s)
            .unwrap_err()
            .rule,
        PackRule::Padding
    );
}

#[test]
fn parents_resolve_within_the_pack_only() {
    let s = Scratch::new();
    s.edit("pack.toml", |t| {
        t.replace("parent = \"chart\"", "parent = \"example.sample/chart\"")
    });
    Env::new().stage(&mut PackRegistry::default(), &s).unwrap();
    let s = Scratch::new();
    s.edit("pack.toml", |t| {
        t.replace("parent = \"chart\"", "parent = \"nothing\"")
    });
    assert_refused(&s, PackRule::Reference, "Unknown parent template nothing.");
    // a parent of another pack needs a pinned dependency
    let s = Scratch::new();
    s.edit("pack.toml", |t| {
        t.replace("parent = \"chart\"", "parent = \"example.other/chart\"")
    });
    assert_refused(
        &s,
        PackRule::Reference,
        "Unresolved or undeclared pack reference example.other/chart.",
    );
}

#[test]
fn account_codes_are_padded_on_the_right_for_active_accounts_only() {
    let (reg, _) = sample();
    let t = &reg.get("example.sample", "R1").unwrap().templates["chart"];
    let cash = &t.records[&id("example.account", "cash")];
    assert_eq!(cash.fields["code"], Value::Text("1200".into()));
    assert_eq!((cash.key.as_str(), t.code_digits), ("cash", Some(4)));
    let code_of = |s: &Scratch| {
        let (mut reg, env) = (PackRegistry::default(), Env::new());
        env.stage(&mut reg, s).unwrap();
        let t = &reg.get("example.sample", "R1").unwrap().templates["chart"];
        t.records[&id("example.account", "cash")].fields["code"].clone()
    };
    let text = |v: &str| Value::Text(v.into());
    // the identity stays the key: a changed code is a changed value
    let s = Scratch::new();
    s.edit("data/accounts.toml", |t| {
        t.replace("code = \"12\"", "code = \"13\"")
    })
    .rehash();
    assert_eq!(code_of(&s), text("1300"));
    // a code longer than code_digits stays as written: never truncated, never refused
    let s = Scratch::new();
    s.edit("data/accounts.toml", |t| {
        t.replace("code = \"12\"", "code = \"12345\"")
    })
    .rehash();
    assert_eq!(code_of(&s), text("12345"));
    // an inactive account keeps its code, and does not meet the active account padded to the same digits
    let s = Scratch::new();
    s.edit("data/accounts.toml", |t| {
        t.replace("code = \"12\"", "code = \"12\"\nactive = false")
            .replace("code = \"1100\"", "code = \"1200\"")
    })
    .rehash();
    assert_eq!(code_of(&s), text("12"));

    // 12 pads to 1200 and meets the account that already has it
    let s = Scratch::new();
    s.edit("data/accounts.toml", |t| {
        t.replace("code = \"1100\"", "code = \"1200\"")
    })
    .rehash();
    let e = assert_refused(
        &s,
        PackRule::Padding,
        "Account code 1200 collides in root chart.",
    );
    assert_eq!(e.path, "template[chart].cash.code");

    // without code_digits nothing is padded or checked
    let s = Scratch::new();
    s.edit("pack.toml", |t| t.replace("code_digits = 4\n", ""));
    assert_eq!(code_of(&s), text("12"));

    let s = Scratch::new();
    s.edit("pack.toml", |t| {
        t.replace("code_digits = 4", "code_digits = 0")
    });
    assert_refused(&s, PackRule::Padding, "code_digits must be positive.");
}

#[test]
fn a_child_may_widen_code_digits() {
    let s = Scratch::new();
    s.edit("pack.toml", |t| {
        t.replace("parent = \"chart\"", "parent = \"chart\"\ncode_digits = 6")
    });
    let (mut reg, env) = (PackRegistry::default(), Env::new());
    env.stage(&mut reg, &s).unwrap();
    let t = &reg.get("example.sample", "R1").unwrap().templates["chart_child"];
    assert_eq!(
        t.records[&id("example.account", "cash")].fields["code"],
        Value::Text("120000".into())
    );
    let s = Scratch::new();
    s.edit("pack.toml", |t| {
        t.replace("parent = \"chart\"", "parent = \"chart\"\ncode_digits = 3")
    });
    // a narrower width never shortens or refuses an inherited code
    let mut reg = PackRegistry::default();
    env.stage(&mut reg, &s).unwrap();
    let t = &reg.get("example.sample", "R1").unwrap().templates["chart_child"];
    assert_eq!(
        t.records[&id("example.account", "cash")].fields["code"],
        Value::Text("1200".into())
    );
}

// ---- values ----

fn with_rate(value: &str) -> Scratch {
    let s = Scratch::new();
    s.edit("data/taxes.toml", |t| {
        t.replace("\"decimal:10.0 percent\"", value)
    })
    .rehash();
    s
}

#[test]
fn binary_floats_and_untagged_values_are_refused() {
    assert_refused(
        &with_rate("0.1"),
        PackRule::ExactValue,
        "Pack example.sample field rate requires an exact tagged value; binary float is not allowed.",
    );
    let e = refused(&with_rate("\"0.1\""));
    assert_eq!(e.rule, PackRule::ExactValue);
    assert_eq!(e.path, "record[0].rate");
}

#[test]
fn exact_values_beyond_the_arithmetic_bounds_are_refused() {
    let too_many = format!("\"decimal:{} percent\"", "9".repeat(30));
    assert_refused(
        &with_rate(&too_many),
        PackRule::ExactBounds,
        "Exact pack value exceeds supported arithmetic bounds.",
    );
    let scale = format!("\"decimal:0.{} percent\"", "1".repeat(29));
    assert_eq!(refused(&with_rate(&scale)).rule, PackRule::ExactBounds);
    // the largest coefficient, 2^96 - 1, still fits; one more does not
    let max = "\"decimal:79228162514264337593543950335 percent\"";
    Env::new()
        .stage(&mut PackRegistry::default(), &with_rate(max))
        .unwrap();
    let over = "\"decimal:79228162514264337593543950336 percent\"";
    assert_eq!(refused(&with_rate(over)).rule, PackRule::ExactBounds);
    let zero_den = Scratch::new();
    zero_den
        .edit("data/taxes.toml", |t| {
            t.replace("ratio:11/12", "ratio:11/0")
        })
        .rehash();
    assert_eq!(
        refused(&zero_den).message,
        "A ratio denominator must be positive."
    );
}

#[test]
fn decimals_are_stored_canonically() {
    let s = with_rate("\"decimal:-000.500 percent\"");
    let mut reg = PackRegistry::default();
    Env::new().stage(&mut reg, &s).unwrap();
    let v = &reg.get("example.sample", "R1").unwrap().templates["chart"].records
        [&("example.tax".into(), "vat_std".into())]
        .fields["rate"];
    assert_eq!(
        v,
        &Value::Decimal {
            value: "-0.5".into(),
            unit: "percent".into()
        }
    );
}

// ---- strict schema, CSV ----

#[test]
fn missing_files_unknown_fields_and_models_are_refused() {
    let s = Scratch::new();
    fs::remove_file(s.dir.join("data/accounts.toml")).unwrap();
    let e = assert_refused(
        &s,
        PackRule::MissingFile,
        "Declared pack file data/accounts.toml is missing.",
    );
    assert_eq!(e.file, "data/accounts.toml");

    let s = Scratch::new();
    s.edit("data/taxes.toml", |t| t.replace("rate = ", "mystery = "))
        .rehash();
    assert_refused(&s, PackRule::UnknownField, "Unknown pack field mystery.");

    let s = Scratch::new();
    s.edit("data/taxes.toml", |t| {
        t.replace("example.tax", "example.unknown")
    })
    .rehash();
    assert_refused(
        &s,
        PackRule::UnknownModel,
        "Unknown pack model example.unknown.",
    );
}

// ---- identity, extension ----

#[test]
fn extending_another_template_in_place_is_refused() {
    let s = Scratch::new();
    s.edit("data/accounts.toml", |t| {
        t.replacen(
            "model = ",
            "template = \"example.parent/chart\"\nmodel = ",
            1,
        )
    })
    .rehash();
    assert_refused(
        &s,
        PackRule::InPlaceExtension,
        "Pack example.sample cannot extend template example.parent/chart in place; declare a child template.",
    );
}

#[test]
fn duplicate_keys_are_refused() {
    let s = Scratch::new();
    s.edit("data/accounts.toml", |t| {
        t.clone() + "\n[[record]]\nmodel = \"example.account\"\nkey = \"cash\"\n[record.fields]\nname = \"x\"\ncode = \"1\"\n"
    })
    .rehash();
    assert_refused(
        &s,
        PackRule::DuplicateKey,
        "Duplicate key cash in template chart.",
    );
}

#[test]
fn unresolved_references_are_refused() {
    let s = Scratch::new();
    s.edit("data/taxes.toml", |t| {
        t.replacen("account = \"sales\"", "account = \"nowhere\"", 1)
    })
    .rehash();
    assert_refused(
        &s,
        PackRule::Reference,
        "Unresolved or undeclared pack reference nowhere.",
    );
}

/// A record file is .toml or .csv; another extension is refused by name, never ignored.
#[test]
fn record_files_of_another_kind_are_refused() {
    let s = Scratch::new();
    s.write("data/more.txt", "key,name\n")
        .edit("pack.toml", |t| {
            t.replace("data/rates.toml", "data/more.txt")
        })
        .rehash();
    fs::remove_file(s.dir.join("data/rates.toml")).unwrap();
    let e = refused(&s);
    assert_eq!((e.rule, e.file.as_str()), (PackRule::Type, "data/more.txt"));
}

// ---- upgrade rules ----

fn release(pack_version: u32, release_id: &str, date: &str) -> Scratch {
    let s = Scratch::new();
    s.edit("pack.toml", |t| {
        t.replace(
            "rule_release_id = \"R1\"",
            &format!("rule_release_id = \"{release_id}\""),
        )
        .replace(
            "pack_version = 1",
            &format!("pack_version = {pack_version}"),
        )
        .replace(
            "release_date = 2026-01-15",
            &format!("release_date = {date}"),
        )
    });
    s
}

#[test]
fn a_newer_release_needs_a_greater_version_and_a_date_not_earlier() {
    let (mut reg, env) = sample();
    assert_eq!(
        env.stage(&mut reg, &release(2, "R2", "2026-03-01"))
            .unwrap(),
        StageOutcome::New
    );
    assert!(reg.get("example.sample", "R1").is_some() && reg.get("example.sample", "R2").is_some());
    for (v, id, d, why) in [
        (2, "R3", "2026-04-01", "must be greater"),
        (3, "R3", "2026-02-01", "earlier"),
    ] {
        let e = env.stage(&mut reg, &release(v, id, d)).unwrap_err();
        assert_eq!(e.rule, PackRule::Upgrade);
        assert!(e.message.contains(why), "{e}");
    }
    assert!(reg.get("example.sample", "R3").is_none());
}

// ---- dated records ----

#[test]
fn overlapping_and_empty_dated_ranges_are_refused() {
    let s = Scratch::new();
    s.edit("data/rates.toml", |t| {
        t.replace("valid_until = 2026-01-01", "valid_until = 2026-02-01")
    })
    .rehash();
    let e = refused(&s);
    assert_eq!(e.rule, PackRule::DatedRange);
    assert!(e.message.contains("overlap"), "{e}");
    let s = Scratch::new();
    s.edit("data/rates.toml", |t| {
        t.replace("valid_until = 2026-01-01", "valid_until = 2025-01-01")
    })
    .rehash();
    assert!(refused(&s).message.contains("not after valid_from"));
}

#[test]
fn dates_are_strict() {
    assert!(Date::parse("2024-02-29").is_some());
    for bad in [
        "2023-02-29",
        "2026-13-01",
        "2026-1-01",
        "26-01-01",
        "2026-01-32",
        "2026/01/01",
        "0999-01-01",
    ] {
        assert!(Date::parse(bad).is_none(), "{bad}");
    }
    assert!(Date::parse("2026-01-01") < Date::parse("2026-01-02"));
}

// ---- CSV records, translations, other packs ----

/// Replace the CSV of `file` and rehash.
fn csv(s: &Scratch, file: &str, text: &str) {
    s.write(file, text).rehash();
}

#[test]
fn csv_rows_are_typed_keyed_and_padded() {
    let (reg, _) = sample();
    let t = &reg.get("example.sample", "R1").unwrap().templates["chart"];
    let petty = &t.records[&id("example.account", "petty")];
    assert_eq!(petty.fields["code"], Value::Text("1500".into()));
    // CSV empty text is an empty string only for a text field
    assert_eq!(petty.fields["note"], Value::Text(String::new()));
    assert_eq!(
        t.records[&id("example.account", "card")].fields["note"],
        Value::Text("Card payments".into())
    );
    assert_eq!(petty.template, "example.sample/chart");
}

#[test]
fn malformed_csv_is_refused() {
    let head = "key,name,code,note\n";
    let cases = [
        (
            "key,name,name,note\nx,A,1,\n",
            PackRule::DuplicateHeader,
            "Duplicate pack CSV header name.",
        ),
        (
            "key,name,,note\nx,A,1,\n",
            PackRule::DuplicateHeader,
            "Empty pack CSV header name.",
        ),
        (
            "key,name,code,mystery\nx,A,1,y\n",
            PackRule::UnknownField,
            "Unknown pack field mystery.",
        ),
        (
            "key,name,code,note\nx,A,1\n",
            PackRule::Syntax,
            "Row has 3 fields, the header has 4.",
        ),
    ];
    for (text, rule, msg) in cases {
        let s = Scratch::new();
        csv(&s, "data/extra_accounts.csv", text);
        let e = assert_refused(&s, rule, msg);
        assert_eq!(e.file, "data/extra_accounts.csv");
    }
    let s = Scratch::new();
    csv(
        &s,
        "data/extra_accounts.csv",
        &format!("{head}cash,Again,13,\n"),
    );
    assert_refused(
        &s,
        PackRule::DuplicateKey,
        "Duplicate key cash in template chart.",
    );
    // tombstone is true, false or empty
    let s = Scratch::new();
    csv(
        &s,
        "data/extra_accounts.csv",
        "key,name,code,tombstone\nx,A,1,yes\n",
    );
    assert_eq!(refused(&s).rule, PackRule::Type);
    // a required cell that is not text may not be empty
    let s = Scratch::new();
    let rows = "key,name,account,rate,factor\nvat_x,Tax X,cash,,\n";
    s.write("data/taxes_more.csv", rows).edit("pack.toml", |t| {
        t.replace("data/taxes.toml", "data/taxes_more.csv")
    });
    s.edit("pack.toml", |t| {
        t.replace(
            "path = \"data/taxes_more.csv\"\n",
            "path = \"data/taxes_more.csv\"\nmodel = \"example.tax\"\n",
        )
    })
    .rehash();
    assert_refused(
        &s,
        PackRule::EmptyRequired,
        "Required pack field rate is empty.",
    );
}

#[test]
fn source_csv_grammar_is_not_a_runtime_format() {
    let msg =
        "Source CSV grammar is not accepted at runtime; use the offline localization converter.";
    for text in [
        "key,name,code,account_id/id\nx,A,1,y\n",
        "key,name,code,name@fr\nx,A,1,y\n",
        "key,name,code,note\npetty,Petty,15,\n,Continued,,more\n",
    ] {
        let s = Scratch::new();
        csv(&s, "data/extra_accounts.csv", text);
        assert_refused(&s, PackRule::SourceGrammar, msg);
    }
    for value in ["(6,0,[1])", "a,b"] {
        let s = Scratch::new();
        s.edit("data/taxes.toml", |t| {
            t.replace("account = \"sales\"", &format!("account = \"{value}\""))
        })
        .rehash();
        assert_refused(&s, PackRule::SourceGrammar, msg);
    }
}

#[test]
fn an_empty_cell_of_an_override_row_is_refused() {
    let s = Scratch::new();
    csv(
        &s,
        "data/extra_accounts.csv",
        "key,name,code,note\nbank,Current,,\n",
    );
    // bank is defined in accounts.toml of the same template: that is a duplicate, not an override
    assert_eq!(refused(&s).rule, PackRule::DuplicateKey);
    let s = Scratch::new();
    s.write("data/child.csv", "key,name,note\nbank,Current,\n")
        .edit("pack.toml", |t| {
            t.replace("data/child.toml", "data/child.csv").replace(
                "path = \"data/child.csv\"\n",
                "path = \"data/child.csv\"\nmodel = \"example.account\"\n",
            )
        })
        .rehash();
    assert_refused(
        &s,
        PackRule::EmptyOverride,
        "Empty override cell note requires clear_fields or an explicit typed value.",
    );
    // a filled override row and a tombstone column work
    let s = Scratch::new();
    s.write(
        "data/child.csv",
        "key,name,tombstone\nbank,Current,false\nmisc,,true\n",
    )
    .edit("pack.toml", |t| {
        t.replace("data/child.toml", "data/child.csv").replace(
            "path = \"data/child.csv\"\n",
            "path = \"data/child.csv\"\nmodel = \"example.account\"\n",
        )
    })
    .rehash();
    // the empty cells of a tombstone row are not overrides
    let (mut reg, env) = (PackRegistry::default(), Env::new());
    env.stage(&mut reg, &s).unwrap();
    let c = &reg.get("example.sample", "R1").unwrap().templates["chart_child"];
    assert_eq!(
        c.records[&id("example.account", "bank")].fields["name"],
        Value::Text("Current".into())
    );
    assert!(c.tombstones.contains(&id("example.account", "misc")));
}

#[test]
fn translations_are_keyed_rows_inherited_by_children() {
    let (reg, _) = sample();
    let pack = reg.get("example.sample", "R1").unwrap();
    let tr = |t: &str, k: &str, l: &str| {
        pack.templates[t]
            .translations
            .get(&("example.account".into(), k.into(), "name".into(), l.into()))
            .cloned()
    };
    assert_eq!(tr("chart", "cash", "fr").as_deref(), Some("Caisse"));
    assert_eq!(tr("chart", "cash", "pt_BR").as_deref(), Some("Caixa"));
    assert_eq!(tr("chart", "cash", "de"), None);
    // the child inherits them; the translation of the record it removed goes with the record
    assert_eq!(tr("chart_child", "cash", "fr").as_deref(), Some("Caisse"));
    assert_eq!(tr("chart", "misc", "fr").as_deref(), Some("Divers"));
    assert_eq!(tr("chart_child", "misc", "fr"), None);
    // a child file may translate again; the label is never a key
    let s = Scratch::new();
    s.write(
        "data/child_tr.csv",
        "model,key,field,lang,text\nexample.account,cash,name,fr,Caisse centrale\n",
    );
    s.edit("pack.toml", |t| {
        t.replace(
            "files = [\"data/child.toml\"]",
            "files = [\"data/child.toml\", \"data/child_tr.csv\"]",
        )
        .replacen("[[template]]", "[[files]]\npath = \"data/child_tr.csv\"\nsha256 = \"x\"\nkind = \"translations\"\n\n[[template]]", 1)
    })
    .rehash();
    let (mut reg, env) = (PackRegistry::default(), Env::new());
    env.stage(&mut reg, &s).unwrap();
    let c = &reg.get("example.sample", "R1").unwrap().templates["chart_child"];
    let key = (
        "example.account".into(),
        "cash".into(),
        "name".into(),
        "fr".into(),
    );
    assert_eq!(c.translations[&key], "Caisse centrale");
}

#[test]
fn translation_rows_are_checked() {
    let head = "model,key,field,lang,text\n";
    let cases = [
        (
            "example.account,cash,name,fr,A\nexample.account,cash,name,fr,B\n",
            PackRule::DuplicateKey,
        ),
        ("example.account,ghost,name,fr,A\n", PackRule::Reference),
        (
            "example.account,cash,mystery,fr,A\n",
            PackRule::UnknownField,
        ),
        ("example.tax,vat_std,rate,fr,A\n", PackRule::Translation),
        ("example.account,cash,name,FR,A\n", PackRule::Translation),
        ("example.account,cash,name,fr,\n", PackRule::EmptyRequired),
        ("example.nothing,cash,name,fr,A\n", PackRule::UnknownModel),
    ];
    for (rows, rule) in cases {
        let s = Scratch::new();
        csv(&s, "data/translations.csv", &format!("{head}{rows}"));
        let e = refused(&s);
        assert_eq!(
            (e.rule, e.file.as_str()),
            (rule, "data/translations.csv"),
            "{rows}: {e}"
        );
    }
    for text in [
        "model,key,field,text\nexample.account,cash,name,A\n",
        "model,key,field,lang@x,text\nexample.account,cash,name,fr,A\n",
    ] {
        let s = Scratch::new();
        csv(&s, "data/translations.csv", text);
        let e = refused(&s);
        assert!(
            matches!(e.rule, PackRule::Translation | PackRule::SourceGrammar),
            "{e}"
        );
    }
}

const LOCAL_PACK: &str = r#"format_version = 1
pack_id = "example.local"
profile_id = "example.local.generic"
rule_release_id = "L1"
pack_version = 1
release_date = 2026-02-01
jurisdictions = ["XX"]

[schemas]
SCHEMAS

[origin]
kind = "authored"
publisher = "Example Publisher"
licence = "Apache-2.0"
source_document = "Example Tax Act"
source_version = "2026"
verified_on = 2026-01-10
reviewer = "Example Reviewer"
review_status = "pending"

[dependencies."example.sample"]
release = "R1"
sha256 = "PARENT"

[[files]]
path = "data/local.toml"
sha256 = "x"
kind = "records"

[[template]]
key = "local"
parent = "example.sample/chart"
files = ["data/local.toml"]
"#;

const LOCAL_DATA: &str = r#"[[record]]
model = "example.account"
key = "extra"
[record.fields]
name = "Extra"
code = "5"

[[record]]
model = "example.tax"
key = "vat_local"
[record.fields]
name = "Local rate"
account = "example.sample/sales"
rate = "decimal:5 percent"
"#;

/// A pack that builds on `example.sample/R1`, pinned by digest. The schema hashes are the sample's own.
fn local_pack(parent_digest: &str) -> Scratch {
    let s = Scratch::new();
    let sample = fs::read_to_string(fixture().join("pack.toml")).unwrap();
    let schemas: Vec<&str> = sample
        .lines()
        .skip_while(|l| *l != "[schemas]")
        .skip(1)
        .take_while(|l| !l.is_empty())
        .collect();
    s.write(
        "pack.toml",
        &LOCAL_PACK
            .replace("SCHEMAS", &schemas.join("\n"))
            .replace("PARENT", parent_digest),
    );
    s.write("data/local.toml", LOCAL_DATA);
    s.rehash();
    s
}

#[test]
fn a_child_pack_pins_its_parent_release_and_digest() {
    let (mut reg, env) = sample();
    let digest = |reg: &PackRegistry, r: &str| reg.get("example.sample", r).unwrap().digest.clone();
    // R2 of the parent exists too, with another name for the cash account
    let r2 = release(2, "R2", "2026-03-01");
    r2.edit("data/accounts.toml", |t| {
        t.replace("\"Cash\"", "\"Cash R2\"")
    })
    .rehash();
    assert_eq!(env.stage(&mut reg, &r2).unwrap(), StageOutcome::New);
    let s = local_pack(&digest(&reg, "R1"));
    assert_eq!(env.stage(&mut reg, &s).unwrap(), StageOutcome::New);
    let t = &reg.get("example.local", "L1").unwrap().templates["local"];
    assert_eq!(t.depth, 1);
    assert_eq!(t.root, "chart");
    // R1 is resolved, never the newer R2; its records come padded with the inherited code_digits
    let cash = &t.records[&id("example.account", "cash")];
    assert_eq!(cash.fields["name"], Value::Text("Cash".into()));
    assert_eq!(cash.template, "example.sample/chart");
    assert_eq!(
        t.records[&id("example.account", "extra")].fields["code"],
        Value::Text("5000".into())
    );
    assert_eq!(t.translations.len(), 3);
    assert_eq!(
        t.records[&id("example.tax", "vat_local")].fields["account"],
        Value::Ref {
            pack: Some("example.sample".into()),
            key: "sales".into()
        }
    );

    // a wrong digest, and a release that was never staged
    let wrong = local_pack(&"0".repeat(64));
    let e = env.stage(&mut reg.clone(), &wrong).unwrap_err();
    assert_eq!(
        (e.rule, e.path.as_str()),
        (PackRule::DigestMismatch, "dependencies.example.sample")
    );
    let e = env.stage(&mut PackRegistry::default(), &wrong).unwrap_err();
    assert_eq!(e.rule, PackRule::Reference, "{e}");
}

#[test]
fn undeclared_and_missing_references_are_refused_by_name() {
    let (mut reg, env) = sample();
    let digest = reg.get("example.sample", "R1").unwrap().digest.clone();
    // a qualified reference to a pack that is not a declared dependency
    let s = Scratch::new();
    s.edit("data/taxes.toml", |t| {
        t.replace("account = \"sales\"", "account = \"example.other/tax.a\"")
    })
    .rehash();
    assert_refused(
        &s,
        PackRule::Reference,
        "Unresolved or undeclared pack reference example.other/tax.a.",
    );
    // declared dependency, absent record
    let s = local_pack(&digest);
    s.edit("data/local.toml", |t| {
        t.replace("example.sample/sales", "example.sample/nowhere")
    })
    .rehash();
    let e = env.stage(&mut reg, &s).unwrap_err();
    assert_eq!(
        e.message,
        "Unresolved or undeclared pack reference example.sample/nowhere."
    );
    // a reference to this pack by its own id is a local one
    let s = Scratch::new();
    s.edit("data/taxes.toml", |t| {
        t.replace("account = \"sales\"", "account = \"example.sample/sales\"")
    })
    .rehash();
    Env::new().stage(&mut PackRegistry::default(), &s).unwrap();
}

#[test]
fn a_padding_collision_in_a_child_pack_names_the_root_of_the_chain() {
    let (mut reg, env) = sample();
    let digest = reg.get("example.sample", "R1").unwrap().digest.clone();
    let s = local_pack(&digest);
    s.edit("data/local.toml", |t| {
        t.replace("code = \"5\"", "code = \"12\"")
    })
    .rehash();
    let e = env.stage(&mut reg, &s).unwrap_err();
    assert_eq!(e.message, "Account code 1200 collides in root chart.");
}

#[test]
fn the_dependency_count_is_a_bounded_setting() {
    let (mut reg, mut env) = sample();
    let digest = reg.get("example.sample", "R1").unwrap().digest.clone();
    let s = local_pack(&digest);
    env.limits = limits_with("l10n.pack.max_dependencies", 1);
    env.stage(&mut reg.clone(), &s).unwrap();
    s.edit("pack.toml", |t| {
        t.replace("[[files]]", "[dependencies.\"example.third\"]\nrelease = \"R1\"\nsha256 = \"0000000000000000000000000000000000000000000000000000000000000000\"\n\n[[files]]")
    })
    .rehash();
    let e = env.stage(&mut reg, &s).unwrap_err();
    assert_eq!((e.rule, e.path.as_str()), (PackRule::Limit, "dependencies"));
}
// ---- bounds ----

fn limits_with(key: &str, value: u64) -> PackLimits {
    PackLimits::from_params(&|k| (k == key).then(|| value.to_string())).unwrap()
}

#[test]
fn input_bounds_hold_at_the_limit_and_fail_one_above() {
    let msg = "Pack input exceeds configured resource limit.";
    let bytes = fs::metadata(fixture().join("data/rates.toml"))
        .unwrap()
        .len();
    // pack.toml is the largest file here; the file cap is its size and the next byte over fails
    let size = fs::metadata(fixture().join("pack.toml")).unwrap().len();
    assert!(size > bytes);
    // the 16 raw rows, and the records and translations the child template inherits from the parent
    let (reg, _) = sample();
    let root = &reg.get("example.sample", "R1").unwrap().templates["chart"];
    let inherited = (root.records.len() + root.translations.len()) as u64;
    let cases = [
        ("l10n.pack.max_file_bytes", size),
        ("l10n.pack.max_rows", 16 + inherited),
        ("l10n.pack.max_files", 6),
        ("l10n.pack.max_fields_per_row", 7),
        ("l10n.pack.max_string_bytes", 37),
        ("l10n.pack.max_ref_edges", 2),
    ];
    for (key, at) in cases {
        let s = Scratch::new();
        let mut env = Env::new();
        env.limits = limits_with(key, at);
        let ok = env.stage(&mut PackRegistry::default(), &s);
        env.limits = limits_with(
            key,
            at.saturating_sub(1)
                .max(if key == "l10n.pack.max_file_bytes" {
                    1024
                } else {
                    1
                }),
        );
        let over = env.stage(&mut PackRegistry::default(), &s);
        if let Err(e) = &ok {
            panic!("{key} = {at} should pass: {e}");
        }
        let e = over.unwrap_err();
        assert_eq!(
            (e.rule, e.message.as_str()),
            (PackRule::Limit, msg),
            "{key}"
        );
    }
}

#[test]
fn the_total_byte_cap_covers_all_files() {
    let total: u64 = [
        "pack.toml",
        "data/accounts.toml",
        "data/taxes.toml",
        "data/rates.toml",
        "data/child.toml",
        "data/extra_accounts.csv",
        "data/translations.csv",
    ]
    .iter()
    .map(|p| fs::metadata(fixture().join(p)).unwrap().len())
    .sum();
    let mut env = Env::new();
    env.limits = PackLimits {
        max_total_bytes: total,
        ..PackLimits::default()
    };
    env.stage(&mut PackRegistry::default(), &Scratch::new())
        .unwrap();
    env.limits = PackLimits {
        max_total_bytes: total - 1,
        ..PackLimits::default()
    };
    assert_eq!(
        env.stage(&mut PackRegistry::default(), &Scratch::new())
            .unwrap_err()
            .rule,
        PackRule::Limit
    );
}

#[test]
fn paths_cannot_escape_and_schemas_are_never_fetched() {
    let msg = "Pack path escapes package root.";
    for bad in ["../outside.toml", "/etc/passwd", "data/../../x.toml"] {
        let s = Scratch::new();
        s.edit("pack.toml", |t| t.replace("data/rates.toml", bad));
        let e = refused(&s);
        assert_eq!(
            (e.rule, e.message.as_str()),
            (PackRule::PathEscape, msg),
            "{bad}"
        );
    }
    let s = Scratch::new();
    s.edit("pack.toml", |t| {
        t.replace("data/rates.toml", "https://example.com/x.toml")
    });
    assert_refused(
        &s,
        PackRule::RemoteSchema,
        "Pack schemas must resolve offline within declared dependencies.",
    );
    let s = Scratch::new();
    s.edit("pack.toml", |t| {
        t.replace(
            "\"example.tax\" = \"",
            "\"example.tax\" = \"https://example.com/schema#",
        )
    });
    assert_eq!(refused(&s).rule, PackRule::RemoteSchema);
}

#[cfg(unix)]
#[test]
fn a_symbolic_link_inside_the_pack_is_refused() {
    let s = Scratch::new();
    let outside = s.dir.parent().unwrap().join("outside.toml");
    fs::write(&outside, "").unwrap();
    fs::remove_file(s.dir.join("data/rates.toml")).unwrap();
    std::os::unix::fs::symlink(&outside, s.dir.join("data/rates.toml")).unwrap();
    assert_refused(&s, PackRule::PathEscape, "Pack path escapes package root.");
}

/// Settings: default, floor and hard bound; zero never means unlimited.
#[test]
fn limit_settings_are_bounded_and_named() {
    assert_eq!(
        PackLimits::from_params(&|_| None).unwrap(),
        PackLimits::default()
    );
    for (key, bad) in [
        ("l10n.pack.max_rows", "0"),
        ("l10n.pack.max_rows", "1000001"),
        ("l10n.pack.max_total_bytes", "100"),
        ("l10n.pack.max_files", "many"),
        ("l10n.pack.max_string_bytes", "70000"),
        ("l10n.pack.max_parent_depth", "0"),
        ("l10n.pack.max_parent_depth", "33"),
        ("l10n.pack.max_code_digits", "257"),
        ("l10n.pack.max_templates", "0"),
        ("l10n.pack.max_dependencies", "65"),
    ] {
        let e = PackLimits::from_params(&|k| (k == key).then(|| bad.to_owned())).unwrap_err();
        assert_eq!(
            (e.rule, e.file.as_str()),
            (PackRule::Settings, key),
            "{key}={bad}"
        );
    }
    assert_eq!(
        limits_with("l10n.pack.max_rows", 1_000_000).max_rows,
        1_000_000
    );
}

// ---- origin and licence ----

/// Authored origin fields are required.
#[test]
fn authored_origin_needs_source_reviewer_and_status() {
    for (from, to, path) in [
        ("reviewer = \"Example Reviewer\"\n", "", "reviewer"),
        (
            "source_document = \"Example Tax Act\"\n",
            "",
            "source_document",
        ),
        (
            "review_status = \"reviewed\"\n",
            "review_status = \"maybe\"\n",
            "review_status",
        ),
        (
            "verified_on = 2026-01-10\n",
            "verified_on = \"soon\"\n",
            "verified_on",
        ),
        (
            "publisher = \"Example Publisher\"\n",
            "publisher = \" \"\n",
            "publisher",
        ),
    ] {
        let s = Scratch::new();
        s.edit("pack.toml", |t| t.replace(from, to));
        let e = refused(&s);
        assert_eq!(
            (e.rule, e.path.as_str()),
            (PackRule::Origin, format!("[origin].{path}").as_str()),
            "{e}"
        );
    }
}

/// The licence of a pack must be permissive.
#[test]
fn a_non_permissive_licence_is_refused() {
    for lic in [
        "GPL-3.0-only",
        "LGPL-3.0-only",
        "AGPL-3.0-or-later",
        "MIT AND GPL-2.0-only",
        "Proprietary",
        "",
    ] {
        let s = Scratch::new();
        s.edit("pack.toml", |t| {
            t.replace(
                "licence = \"MIT OR Apache-2.0\"",
                &format!("licence = \"{lic}\""),
            )
        });
        let e = refused(&s);
        assert_eq!(
            (e.rule, e.file.as_str(), e.path.as_str()),
            (PackRule::Licence, "pack.toml", "[origin].licence"),
            "{lic}"
        );
    }
    assert!(
        check_licence("MIT OR GPL-3.0-only").is_ok(),
        "one permissive alternative is enough"
    );
    assert!(check_licence("Apache-2.0 WITH LLVM-exception").is_ok());
    assert!(check_licence("(MIT)").is_err());
}

/// The policy list is the licence list of `deny.toml`.
#[test]
fn the_licence_policy_matches_deny_toml() {
    let deny =
        fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../deny.toml")).unwrap();
    let doc: toml::Table = deny.parse().unwrap();
    let allow: Vec<&str> = doc["licenses"]["allow"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert_eq!(allow, ALLOWED_LICENCES);
}

/// Converted and mixed origins are a follow-up: refused by name, never admitted without their checks.
#[test]
fn converted_and_mixed_origins_are_refused() {
    for kind in ["converted", "mixed"] {
        let s = Scratch::new();
        s.edit("pack.toml", |t| {
            t.replace("kind = \"authored\"", &format!("kind = \"{kind}\""))
        });
        let e = refused(&s);
        assert_eq!(
            (e.rule, e.file.as_str()),
            (PackRule::Syntax, "pack.toml"),
            "{e}"
        );
        assert!(e.message.contains(kind), "{e}");
    }
}

// ---- cycle 11 review fixes (CYCLE11-FIX-PACK) ----

#[test]
fn an_empty_required_text_cell_is_refused() {
    for (col, row, msg) in [
        ("name", "x,,5,\n", "Required pack field name is empty."),
        ("code", "x,A,,\n", "Required pack field code is empty."),
    ] {
        let s = Scratch::new();
        csv(
            &s,
            "data/extra_accounts.csv",
            &format!("key,name,code,note\n{row}"),
        );
        let e = assert_refused(&s, PackRule::EmptyRequired, msg);
        assert_eq!(
            (e.file.as_str(), e.path.as_str()),
            ("data/extra_accounts.csv", format!("row 2.{col}").as_str())
        );
    }
}

#[test]
fn inherited_records_count_against_the_row_limit() {
    let (reg, _) = sample();
    let root = &reg.get("example.sample", "R1").unwrap().templates["chart"];
    let inherited = (root.records.len() + root.translations.len()) as u64;
    // 16 raw rows (see input_bounds_hold_at_the_limit_and_fail_one_above) plus what the child inherits
    let at = 16 + inherited;
    let mut env = Env::new();
    let stage = |env: &Env| env.stage(&mut PackRegistry::default(), &Scratch::new());
    env.limits = limits_with("l10n.pack.max_rows", at);
    stage(&env).unwrap();
    env.limits = limits_with("l10n.pack.max_rows", at - 1);
    assert_eq!(stage(&env).unwrap_err().rule, PackRule::Limit);
    // the 13 raw rows of the parent leave room for less than the child inherits: refused at the clone, by name
    env.limits = limits_with("l10n.pack.max_rows", 13 + inherited - 1);
    let e = stage(&env).unwrap_err();
    assert_eq!(
        (e.rule, e.path.as_str()),
        (PackRule::Limit, "template[chart_child].parent"),
        "{e}"
    );
    assert_eq!(
        e.message, "Pack input exceeds configured resource limit.",
        "{e}"
    );
}

#[test]
fn the_csv_row_cap_refuses_inside_the_reading_loop() {
    let s = Scratch::new();
    // accounts.toml has 4 rows, so a cap of 7 leaves 3 CSV body rows; the fourth is over it, and a row wider
    // than the field cap follows: the cap is the refusal, the wide row is never looked at
    let wide = ",".repeat(8);
    let ok = "a,b,c,d\n".repeat(3);
    csv(
        &s,
        "data/extra_accounts.csv",
        &format!("key,name,code,note\n{ok}a,b,c,d\n{wide}\n"),
    );
    let mut env = Env::new();
    env.limits = PackLimits {
        max_rows: 7,
        max_fields_per_row: 4,
        ..PackLimits::default()
    };
    let e = env.stage(&mut PackRegistry::default(), &s).unwrap_err();
    assert_eq!(
        (e.rule, e.file.as_str(), e.path.as_str()),
        (PackRule::Limit, "data/extra_accounts.csv", "rows"),
        "{e}"
    );
    assert_eq!(e.message, "Pack input exceeds configured resource limit.");
}

#[test]
fn toml_row_values_obey_the_string_and_field_caps() {
    let long = "k".repeat(40);
    let cases: [(&str, String, String); 7] = [
        (
            "data/accounts.toml",
            "key = \"misc\"".into(),
            format!("key = \"{long}\""),
        ),
        (
            "data/accounts.toml",
            "key = \"misc\"".into(),
            format!("key = \"misc\"\ntemplate = \"{long}\""),
        ),
        (
            "data/accounts.toml",
            "model = \"example.account\"\nkey = \"misc\"".into(),
            format!("model = \"{long}\"\nkey = \"misc\""),
        ),
        (
            "data/child.toml",
            "clear_fields = [\"note\"]".into(),
            format!("clear_fields = [\"{long}\"]"),
        ),
        (
            "data/taxes.toml",
            "account = \"sales\"\nrate = \"decimal:10.0 percent\"".into(),
            format!("account = \"{long}\"\nrate = \"decimal:10.0 percent\""),
        ),
        (
            "data/taxes.toml",
            "rate = \"decimal:10.0 percent\"".into(),
            format!("rate = \"decimal:10.{} percent\"", "0".repeat(40)),
        ),
        (
            "data/child.toml",
            "clear_fields = [\"note\"]".into(),
            format!("clear_fields = [{}]", ["\"note\""; 8].join(", ")),
        ),
    ];
    for (i, (file, from, to)) in cases.iter().enumerate() {
        let s = Scratch::new();
        s.edit(file, |t| {
            assert!(t.contains(from.as_str()), "case {i}: {from}");
            t.replacen(from.as_str(), to, 1)
        })
        .rehash();
        let mut env = Env::new();
        env.limits = PackLimits {
            max_string_bytes: 37,
            max_fields_per_row: 7,
            ..PackLimits::default()
        };
        let e = env.stage(&mut PackRegistry::default(), &s).unwrap_err();
        assert_eq!(
            (e.rule, e.message.as_str()),
            (
                PackRule::Limit,
                "Pack input exceeds configured resource limit."
            ),
            "case {i}: {e}"
        );
    }
}

/// A template's `country` is a code of the pack's jurisdictions.
#[test]
fn a_template_country_must_be_a_jurisdiction_of_the_pack() {
    for bad in ["ZZ", "not-a-code", ""] {
        let s = Scratch::new();
        s.edit("pack.toml", |t| {
            t.replace("country = \"XX\"", &format!("country = \"{bad}\""))
        });
        let e = refused(&s);
        assert_eq!(
            (e.rule, e.file.as_str(), e.path.as_str()),
            (PackRule::Jurisdiction, "pack.toml", "template[0].country"),
            "{bad}: {e}"
        );
    }
    // accepted: the sample's own value, and no country at all
    Env::new()
        .stage(&mut PackRegistry::default(), &Scratch::new())
        .unwrap();
    let s = Scratch::new();
    s.edit("pack.toml", |t| t.replace("country = \"XX\"\n", ""));
    Env::new().stage(&mut PackRegistry::default(), &s).unwrap();
}

#[test]
fn a_pack_path_with_an_empty_component_is_refused() {
    let s = Scratch::new();
    s.edit("pack.toml", |t| {
        t.replace("data/rates.toml", "data//rates.toml")
    });
    let e = assert_refused(&s, PackRule::PathEscape, "Pack path escapes package root.");
    assert_eq!(e.file, "pack.toml");
    // a `.` component is the same file under another spelling
    let s = Scratch::new();
    s.edit("pack.toml", |t| {
        t.replace("data/rates.toml", "data/./rates.toml")
    });
    let e = assert_refused(&s, PackRule::PathEscape, "Pack path escapes package root.");
    assert_eq!(e.file, "pack.toml");
}

#[test]
fn an_empty_required_text_value_in_a_toml_row_is_refused() {
    let s = Scratch::new();
    s.edit("data/accounts.toml", |t| {
        t.replacen("name = \"Cash\"", "name = \"\"", 1)
    })
    .rehash();
    let e = assert_refused(
        &s,
        PackRule::EmptyRequired,
        "Required pack field name is missing.",
    );
    assert_eq!(e.file, "data/accounts.toml");
}

/// The profile id is the pack id or the pack id and a dot and more.
#[test]
fn a_profile_id_starts_at_a_dot_boundary_of_the_pack_id() {
    let s = Scratch::new();
    s.edit("pack.toml", |t| {
        t.replace(
            "profile_id = \"example.sample.generic\"",
            "profile_id = \"example.samplex\"",
        )
    });
    assert_eq!(refused(&s).rule, PackRule::Identity);
    let s = Scratch::new();
    s.edit("pack.toml", |t| {
        t.replace(
            "profile_id = \"example.sample.generic\"",
            "profile_id = \"example.sample\"",
        )
    });
    Env::new().stage(&mut PackRegistry::default(), &s).unwrap();
}
