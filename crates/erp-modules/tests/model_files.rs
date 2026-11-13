//! `models/*.toml` of data-only addons: parsing, strictness, limits, and the
//! registry they produce. Pure: no database (the tables are exercised in erp-test-models
//! `cross_addon`).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

use std::fs;
use std::path::{Path, PathBuf};

use erp_fields::{ErrorKind, FieldType};
use erp_modules::{
    Error, MAX_MODEL_FILE_BYTES, MAX_MODEL_FILES, MAX_MODEL_FILES_TOTAL_BYTES,
    MAX_MODELS_PER_ADDON, MAX_MODELS_PER_FILE, ModuleName, addon_descriptor, parse_model_file,
    read_manifest_from_dir, read_model_files,
};
use erp_registry::{AddonDescriptor, DefaultValue, KindDef, ModelKind, Overlay, RegistryBuilder};
use tempfile::tempdir;

const FILE: &str = "models/test.toml";

fn my_addon() -> ModuleName {
    ModuleName::new("my_addon").unwrap()
}

fn fixture_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/test_data_only")
}

/// The display of a refused model file, from our error type.
fn refused(content: &str) -> String {
    let err = parse_model_file(&my_addon(), FILE, content).unwrap_err();
    assert!(
        matches!(err, Error::ModelFile { .. }),
        "expected Error::ModelFile, got {err:?}"
    );
    err.to_string()
}

/// `models/*.toml` declares a new model and extends another addon's model, and the registry
/// built with it has both. The `installed` state and data records arrive with)
#[test]
fn spec_mod_045_data_only_addon_models() {
    let dir = fixture_dir();
    let name = ModuleName::new("test_data_only").unwrap();
    let manifest = read_manifest_from_dir(&dir, &name).unwrap().unwrap();
    assert!(
        !dir.join("Cargo.toml").exists(),
        "a data-only addon has no crate"
    );
    let addon = addon_descriptor(&manifest).unwrap();
    assert_eq!(
        (addon.name, addon.depends),
        ("test_data_only", &["base"][..])
    );
    // equal manifests intern to one descriptor
    assert!(std::ptr::eq(addon, addon_descriptor(&manifest).unwrap()));

    let defs = read_model_files(&dir, &name).unwrap();
    // file-name order: 10_tag.toml, then 20_users.toml
    let summary: Vec<(&str, bool, &str, u32)> = defs
        .iter()
        .map(|d| (d.name.as_str(), d.is_extension(), d.file.as_str(), d.line))
        .collect();
    assert_eq!(
        summary,
        vec![
            ("test_data_only.tag", false, "models/10_tag.toml", 2),
            ("res.users", true, "models/20_users.toml", 2),
        ]
    );

    let mut overlay = Overlay::new();
    overlay.add_addon_defs(defs).unwrap();
    let registry = RegistryBuilder::new([addon])
        .linked(false)
        .overlay(&overlay)
        .build()
        .unwrap();
    let tag = registry.model("test_data_only.tag").unwrap();
    assert!(tag.has_table() && !tag.custom);
    assert_eq!(tag.description, "Tag");
    let tag_name = tag.field("name").unwrap();
    assert!(tag_name.required);
    assert_eq!(tag_name.string, "Tag Name");
    assert_eq!(tag_name.modules, vec!["test_data_only"]);
    assert_eq!(tag.field_by_id(tag.rec_name.unwrap()).name, "name");
    let users = registry.model("res.users").unwrap();
    let tag_id = users.field("tag_id").unwrap();
    assert!(matches!(
        &tag_id.ty,
        FieldType::Many2one { comodel, .. } if comodel == "test_data_only.tag"
    ));
    assert_eq!(tag_id.modules, vec!["test_data_only"]);
    let related = users.field("tag_name").unwrap();
    assert_eq!(related.related, Some("tag_id.name"));
    assert!(!related.store);
    // `res.users` keeps the fields `base` declares
    assert!(users.field("login").is_some());
}

#[test]
fn every_declaration_attribute_is_read() {
    let defs = parse_model_file(
        &my_addon(),
        FILE,
        r#"
[[model]]
name = "my.wizard"
inherit = ["my.mixin"]
description = "Wizard"
transient = true
order = "id desc"
rec_name = "label"

[[model.field]]
name = "label"
type = "char"
size = 20
help = "Shown in lists"
readonly = true
copy = false
index = "trigram"
groups = "base.group_user"

[[model.field]]
name = "amount"
type = "float"
digits = [16, 2]
default = 1.5
store = true
index = false

[[model.field]]
name = "line_ids"
type = "one2many"
comodel = "my.line"
inverse = "wizard_id"

[[model.field]]
name = "tag_ids"
type = "many2many"
comodel = "my.tag"
relation = "my_wizard_tag_rel"
column1 = "wizard_id"
column2 = "tag_id"
domain = "[[\"active\", \"=\", true]]"
context = "{\"active_test\": false}"
check_company = true

[[model.field]]
name = "state"
type = "selection"
selection_add = [["done", "Done"], ["draft"]]
"#,
    )
    .unwrap();
    let [wizard] = defs.as_slice() else {
        panic!("one model expected, got {defs:?}")
    };
    assert_eq!(wizard.kind, Some(ModelKind::Transient));
    assert_eq!(wizard.inherit, vec!["my.mixin"]);
    assert!(!wizard.is_extension());
    assert_eq!(wizard.order.as_deref(), Some("id desc"));
    assert_eq!(wizard.rec_name.as_deref(), Some("label"));
    let fields: Vec<&str> = wizard.fields.iter().map(|f| f.name.as_str()).collect();
    assert_eq!(fields, ["label", "amount", "line_ids", "tag_ids", "state"]);
    let label = &wizard.fields[0];
    assert_eq!(label.kind, KindDef::Char { size: Some(20) });
    assert_eq!(
        (label.readonly, label.copy, label.index),
        (
            Some(true),
            Some(false),
            Some(Some(erp_fields::IndexKind::Trigram))
        )
    );
    let amount = &wizard.fields[1];
    assert_eq!(amount.default, Some(DefaultValue::Float(1.5)));
    assert_eq!(amount.index, Some(None), "index = false is explicit");
    let tags = &wizard.fields[3];
    match &tags.kind {
        KindDef::Many2many {
            comodel,
            table: Some(table),
            ondelete: None,
        } => {
            assert_eq!(comodel, "my.tag");
            assert_eq!(
                (
                    table.relation.as_str(),
                    table.column1.as_str(),
                    table.column2.as_str()
                ),
                ("my_wizard_tag_rel", "wizard_id", "tag_id")
            );
        }
        other => panic!("unexpected kind {other:?}"),
    }
    assert_eq!(tags.check_company, Some(true));
    assert_eq!(
        wizard.fields[4].kind,
        KindDef::Selection {
            values: None,
            add: vec![
                ("done".to_owned(), Some("Done".to_owned())),
                ("draft".to_owned(), None)
            ],
            ondelete: Vec::new(),
        }
    );
}

#[test]
fn empty_file_crlf_and_non_ascii() {
    assert!(parse_model_file(&my_addon(), FILE, "").unwrap().is_empty());
    let defs = parse_model_file(
        &my_addon(),
        FILE,
        "[[model]]\r\nname = \"my.tag\"\r\ndescription = \"Étiquette 標籤\"\r\n",
    )
    .unwrap();
    assert_eq!(defs[0].description.as_deref(), Some("Étiquette 標籤"));
}

#[test]
fn unknown_keys_are_refused_with_file_and_line() {
    assert_eq!(
        refused("[[model]]\nname = \"my.tag\"\ncolour = \"red\"\n"),
        "Model file 'models/test.toml:3': unknown field `colour`, expected one of `name`, \
         `extend`, `inherit`, `description`, `table`, `order`, `rec_name`, `check_company_auto`, `company_match`, `company_access`, `eventful`, `audit_reads`, `transient`, \
         `abstract`, `transient_max_age_seconds`, `transient_max_count`, `field`, `index`, `partition`, `exclude`, `key`, `sql_function`, `sql_trigger`, `sql_condition`, `sql_grant`, `persistence`"
    );
    assert_eq!(
        refused(
            "[[model]]\nname = \"my.tag\"\n\n[[model.field]]\nname = \"a\"\ntype = \"char\"\nwidget = \"x\"\n"
        ),
        "Model file 'models/test.toml:7': unknown field `widget`, expected one of `name`, `type`, \
         `string`, `help`, `required`, `readonly`, `store`, `copy`, `index`, `default`, \
         `related`, `groups`, `domain`, `context`, `check_company`, `digits`, `size`, \
         `currency_field`, `selection`, `selection_add`, `comodel`, `inverse`, `relation`, \
         `column1`, `column2`, `ondelete`, `bounds`, `generated`, `generated_storage`, `compute`, `depends`"
    );
    assert_eq!(
        refused("[unknown]\n"),
        "Model file 'models/test.toml:1': unknown field `unknown`, expected one of `model`, \
         `sql_function`, `sql_trigger`, `sql_condition`, `sql_grant`, `persistence`"
    );
}

#[test]
fn wrong_value_types_are_refused() {
    assert_eq!(
        refused(
            "[[model]]\nname = \"my.tag\"\n\n[[model.field]]\nname = \"a\"\ntype = \"char\"\nrequired = \"yes\"\n"
        ),
        "Model file 'models/test.toml:7': invalid type: string \"yes\", expected a boolean"
    );
    assert_eq!(
        refused(
            "[[model]]\nname = \"my.tag\"\n\n[[model.field]]\nname = \"a\"\ntype = \"char\"\ndefault = [1]\n"
        ),
        "Model file 'models/test.toml:4': field my.tag.a: `default` is a boolean, number or \
         string, not array"
    );
    assert_eq!(
        refused(
            "[[model]]\nname = \"my.tag\"\n\n[[model.field]]\nname = \"a\"\ntype = \"char\"\nindex = 3\n"
        ),
        "Model file 'models/test.toml:4': field my.tag.a: `index` is true, false or an index \
         kind, not integer"
    );
    assert_eq!(
        refused(
            "[[model]]\nname = \"my.tag\"\n\n[[model.field]]\nname = \"a\"\ntype = \"char\"\nindex = \"hash\"\n"
        ),
        "Model file 'models/test.toml:4': field my.tag.a: unknown index kind 'hash'"
    );
}

#[test]
fn model_header_rules() {
    let cases: [(&str, &str); 6] = [
        (
            "[[model]]\ndescription = \"x\"\n",
            "Model file 'models/test.toml:1': a model needs `name` (new model) or `extend` \
             (extension)",
        ),
        (
            "[[model]]\nname = \"a.b\"\nextend = \"a.b\"\n",
            "Model file 'models/test.toml:1': a model sets either `name` (new model) or `extend` \
             (extension), not both",
        ),
        (
            "[[model]]\nname = \"a.b\"\ninherit = [\"a.b\"]\n",
            "Model file 'models/test.toml:1': model 'a.b': use `extend = \"a.b\"` to extend a \
             model in place",
        ),
        (
            "[[model]]\nname = \"a.b\"\ninherit = [\"c.d\", \"c.d\"]\n",
            "Model file 'models/test.toml:1': model 'a.b': parent 'c.d' is listed twice",
        ),
        (
            "[[model]]\nextend = \"a.b\"\ntable = \"t\"\n",
            "Model file 'models/test.toml:1': model 'a.b': an extension cannot set `table` (it \
             belongs to the model's definition)",
        ),
        (
            "[[model]]\nname = \"a.b\"\ntransient = true\nabstract = true\n",
            "Model file 'models/test.toml:1': model 'a.b': `transient` and `abstract` exclude \
             each other",
        ),
    ];
    for (content, message) in cases {
        assert_eq!(refused(content), message, "{content}");
    }
    assert_eq!(
        refused("[[model]]\nname = \"My.Tag\"\n"),
        "Model file 'models/test.toml:1': The model name can only contain lowercase characters, \
         digits, underscores and dots. (got 'My.Tag')"
    );
}

#[test]
fn field_rules() {
    let field = |body: &str| {
        format!("[[model]]\nname = \"my.tag\"\n\n[[model.field]]\nname = \"f\"\n{body}")
    };
    let cases: [(&str, &str); 12] = [
        (
            "type = \"char\"\ncompute = \"_compute_f\"\n",
            "field my.tag.f: `compute`/`depends` are not supported in model files: a computed \
             field there is `related`; Luau compute formulas in model files are not scheduled yet",
        ),
        (
            "type = \"char\"\ndepends = [\"a\"]\n",
            "field my.tag.f: `compute`/`depends` are not supported in model files: a computed \
             field there is `related`; Luau compute formulas in model files are not scheduled yet",
        ),
        (
            "type = \"integer\"\nsize = 3\n",
            "field my.tag.f: `size` does not apply to type 'integer'",
        ),
        (
            "type = \"char\"\ndomain = \"[]\"\n",
            "field my.tag.f: `domain`/`context` do not apply to type 'char'",
        ),
        (
            "type = \"many2one\"\n",
            "field my.tag.f: a many2one field needs `comodel`",
        ),
        (
            "type = \"one2many\"\ncomodel = \"a.b\"\n",
            "field my.tag.f: a one2many field needs `inverse`",
        ),
        (
            "type = \"many2many\"\ncomodel = \"a.b\"\nrelation = \"r\"\n",
            "field my.tag.f: `relation`, `column1` and `column2` go together",
        ),
        (
            "type = \"many2one\"\ncomodel = \"a.b\"\nondelete = \"set default\"\n",
            "field my.tag.f: unknown ondelete 'set default' (expected \"set null\", \"restrict\" \
             or \"cascade\")",
        ),
        (
            "type = \"reference\"\n",
            "field my.tag.f: type 'reference' is not supported in model files (declare it in a \
             native addon)",
        ),
        (
            "type = \"blob\"\n",
            "field my.tag.f: unknown field type 'blob'",
        ),
        (
            "type = \"float\"\ndigits = [16]\n",
            "field my.tag.f: `digits` is [precision, scale]",
        ),
        (
            "type = \"selection\"\nselection = [[\"a\"]]\n",
            "field my.tag.f: `selection` entries are [value, label]",
        ),
    ];
    for (body, message) in cases {
        assert_eq!(
            refused(&field(body)),
            format!("Model file 'models/test.toml:4': {message}"),
            "{body}"
        );
    }
    assert_eq!(
        refused(
            "[[model]]\nname = \"my.tag\"\n\n[[model.field]]\nname = \"a\"\ntype = \"char\"\n\n\
             [[model.field]]\nname = \"a\"\ntype = \"text\"\n"
        ),
        "Model file 'models/test.toml:8': model 'my.tag': field 'a' is declared twice"
    );
}

#[test]
fn the_line_is_the_model_that_failed() {
    let content = "[[model]]\nname = \"a.b\"\n\n[[model]]\nname = \"c.d\"\n\n[[model]]\n";
    assert_eq!(
        refused(content),
        "Model file 'models/test.toml:7': a model needs `name` (new model) or `extend` \
         (extension)"
    );
}

#[test]
fn file_limits() {
    let too_big = "#".repeat(usize::try_from(MAX_MODEL_FILE_BYTES).unwrap() + 1);
    assert_eq!(
        refused(&too_big),
        format!("Model file 'models/test.toml': larger than {MAX_MODEL_FILE_BYTES} bytes")
    );
    let many = "[[model]]\nname = \"a.b\"\n".repeat(MAX_MODELS_PER_FILE + 1);
    assert_eq!(
        refused(&many),
        format!(
            "Model file 'models/test.toml': more than {MAX_MODELS_PER_FILE} models in one file"
        )
    );
    let mut wide = String::from("[[model]]\nname = \"a.b\"\n");
    for i in 0..=erp_registry::MAX_FIELDS_PER_MODEL {
        wide.push_str(&format!(
            "[[model.field]]\nname = \"f{i}\"\ntype = \"integer\"\n"
        ));
    }
    assert_eq!(
        refused(&wide),
        format!(
            "Model file 'models/test.toml:1': model 'a.b': {} fields, more than the limit of {}",
            erp_registry::MAX_FIELDS_PER_MODEL + 1,
            erp_registry::MAX_FIELDS_PER_MODEL
        )
    );
    let long = format!(
        "[[model]]\nname = \"a.b\"\ndescription = \"{}\"\n",
        "d".repeat(erp_registry::MAX_TEXT_BYTES + 1)
    );
    assert_eq!(
        refused(&long),
        format!(
            "Model file 'models/test.toml:1': model 'a.b': `description` is {} bytes long, more \
             than the limit of {}",
            erp_registry::MAX_TEXT_BYTES + 1,
            erp_registry::MAX_TEXT_BYTES
        )
    );
    // review L7: the registry's error stays reachable as the source, with its kind
    let err = parse_model_file(&my_addon(), FILE, &long).unwrap_err();
    let source = std::error::Error::source(&err)
        .and_then(|s| s.downcast_ref::<Box<erp_fields::Error>>())
        .expect("the registry error is the source");
    assert_eq!(source.kind(), erp_fields::ErrorKind::Validation);
    assert_eq!(
        source.message(),
        format!(
            "model 'a.b': `description` is {} bytes long, more than the limit of {}",
            erp_registry::MAX_TEXT_BYTES + 1,
            erp_registry::MAX_TEXT_BYTES
        )
    );
    // deep nesting inside a free-form value stops at the parser's recursion limit
    let deep = format!(
        "[[model]]\nname = \"a.b\"\n[[model.field]]\nname = \"f\"\ntype = \"char\"\ndefault = {}{}\n",
        "[".repeat(10_000),
        "]".repeat(10_000)
    );
    assert!(refused(&deep).starts_with("Model file 'models/test.toml:"));
}

#[test]
fn directory_rules() {
    let tmp = tempdir().unwrap();
    let addon = tmp.path();
    // no models directory: no models
    assert!(read_model_files(addon, &my_addon()).unwrap().is_empty());
    // a models file instead of a directory
    fs::write(addon.join("models"), "").unwrap();
    let err = read_model_files(addon, &my_addon()).unwrap_err();
    assert_eq!(
        err.to_string(),
        format!(
            "Model file '{}': not a directory",
            addon.join("models").display()
        )
    );
    fs::remove_file(addon.join("models")).unwrap();
    let models = addon.join("models");
    fs::create_dir(&models).unwrap();
    // file-name order, regardless of creation order
    fs::write(models.join("b.toml"), "[[model]]\nname = \"b.b\"\n").unwrap();
    fs::write(models.join("a.toml"), "[[model]]\nname = \"a.a\"\n").unwrap();
    let defs = read_model_files(addon, &my_addon()).unwrap();
    let order: Vec<(&str, &str)> = defs
        .iter()
        .map(|d| (d.name.as_str(), d.file.as_str()))
        .collect();
    assert_eq!(order, [("a.a", "models/a.toml"), ("b.b", "models/b.toml")]);
    // an error in a file names its full path
    fs::write(models.join("c.toml"), "[[model]]\n").unwrap();
    let err = read_model_files(addon, &my_addon()).unwrap_err();
    assert_eq!(
        err.to_string(),
        format!(
            "Model file '{}:1': a model needs `name` (new model) or `extend` (extension)",
            models.join("c.toml").display()
        )
    );
    fs::remove_file(models.join("c.toml")).unwrap();
    // anything else in models/ is refused
    fs::write(models.join("notes.txt"), "").unwrap();
    let err = read_model_files(addon, &my_addon()).unwrap_err();
    assert_eq!(
        err.to_string(),
        format!(
            "Model file '{}': unexpected entry in models/: only regular files named *.toml \
             belong here",
            models.join("notes.txt").display()
        )
    );
    fs::remove_file(models.join("notes.txt")).unwrap();
    fs::create_dir(models.join("sub.toml")).unwrap();
    let err = read_model_files(addon, &my_addon()).unwrap_err();
    assert_eq!(
        err.to_string(),
        format!(
            "Model file '{}': unexpected entry in models/: only regular files named *.toml \
             belong here",
            models.join("sub.toml").display()
        )
    );
    fs::remove_dir(models.join("sub.toml")).unwrap();
    // an oversized file on disk is refused without reading it all
    fs::write(
        models.join("big.toml"),
        vec![b'#'; usize::try_from(MAX_MODEL_FILE_BYTES).unwrap() + 1],
    )
    .unwrap();
    let err = read_model_files(addon, &my_addon()).unwrap_err();
    assert_eq!(
        err.to_string(),
        format!(
            "Model file '{}': larger than {MAX_MODEL_FILE_BYTES} bytes",
            models.join("big.toml").display()
        )
    );
    fs::remove_file(models.join("big.toml")).unwrap();
    // too many files
    for i in 0..MAX_MODEL_FILES {
        fs::write(models.join(format!("m{i:04}.toml")), "").unwrap();
    }
    let err = read_model_files(addon, &my_addon()).unwrap_err();
    assert_eq!(
        err.to_string(),
        format!(
            "Model file '{}': more than {MAX_MODEL_FILES} model files",
            models.display()
        )
    );
}

#[cfg(unix)]
#[test]
fn symlinks_are_refused() {
    let tmp = tempdir().unwrap();
    let addon = tmp.path().join("addon");
    let elsewhere = tmp.path().join("elsewhere");
    fs::create_dir_all(&addon).unwrap();
    fs::create_dir_all(&elsewhere).unwrap();
    fs::write(elsewhere.join("x.toml"), "[[model]]\nname = \"x.x\"\n").unwrap();
    std::os::unix::fs::symlink(&elsewhere, addon.join("models")).unwrap();
    let err = read_model_files(&addon, &my_addon()).unwrap_err();
    assert_eq!(
        err.to_string(),
        format!(
            "Model file '{}': not a directory",
            addon.join("models").display()
        )
    );
    fs::remove_file(addon.join("models")).unwrap();
    fs::create_dir(addon.join("models")).unwrap();
    std::os::unix::fs::symlink(elsewhere.join("x.toml"), addon.join("models/x.toml")).unwrap();
    let err = read_model_files(&addon, &my_addon()).unwrap_err();
    assert_eq!(
        err.to_string(),
        format!(
            "Model file '{}': unexpected entry in models/: only regular files named *.toml \
             belong here",
            addon.join("models/x.toml").display()
        )
    );
}

#[test]
fn defaults_digits_and_size_are_checked() {
    let field = |body: &str| {
        format!("[[model]]\nname = \"my.tag\"\n\n[[model.field]]\nname = \"f\"\n{body}")
    };
    let cases: [(&str, &str); 6] = [
        (
            "type = \"integer\"\ndefault = \"abc\"\n",
            "field 'f' on model 'my.tag': a string is not a valid default for a field of type \
             'integer'",
        ),
        (
            "type = \"float\"\ndefault = nan\n",
            "field 'f' on model 'my.tag': a non-finite float is not a valid default for a field \
             of type 'float'",
        ),
        (
            "type = \"many2one\"\ncomodel = \"a.b\"\ndefault = true\n",
            "field 'f' on model 'my.tag': a boolean is not a valid default for a field of type \
             'many2one'",
        ),
        (
            "type = \"float\"\ndigits = [16, 1001]\n",
            "field 'f' on model 'my.tag': `digits` scale 1001 is larger than 1000 (PostgreSQL \
             numeric)",
        ),
        (
            "type = \"char\"\nsize = 0\n",
            "field 'f' on model 'my.tag': `size` must be at least 1 (leave it out for no limit)",
        ),
        (
            "type = \"monetary\"\ndefault = inf\n",
            "field 'f' on model 'my.tag': a non-finite float is not a valid default for a field \
             of type 'monetary'",
        ),
    ];
    for (body, message) in cases {
        assert_eq!(
            refused(&field(body)),
            format!("Model file 'models/test.toml:4': {message}"),
            "{body}"
        );
    }
    // a field over a size limit is reported at its own line
    let long = field(&format!(
        "type = \"char\"\nhelp = \"{}\"\n",
        "h".repeat(erp_registry::MAX_TEXT_BYTES + 1)
    ));
    assert_eq!(
        refused(&long),
        format!(
            "Model file 'models/test.toml:4': field 'f' on model 'my.tag': `help` is {} bytes \
             long, more than the limit of {}",
            erp_registry::MAX_TEXT_BYTES + 1,
            erp_registry::MAX_TEXT_BYTES
        )
    );
    // echoed values are clipped
    let odd = field(&format!("type = \"{}\"\n", "z".repeat(1000)));
    assert_eq!(
        refused(&odd),
        format!(
            "Model file 'models/test.toml:4': field my.tag.f: unknown field type '{}…'",
            "z".repeat(64)
        )
    );
}

#[test]
fn addon_totals_and_encoding() {
    let tmp = tempdir().unwrap();
    let addon = tmp.path();
    let models = addon.join("models");
    fs::create_dir(&models).unwrap();
    // not UTF-8
    fs::write(models.join("bad.toml"), b"[[model]]\nname = \"\xff\"\n").unwrap();
    let err = read_model_files(addon, &my_addon()).unwrap_err();
    assert_eq!(
        err.to_string(),
        format!(
            "Model file '{}': not valid UTF-8 (at byte 18)",
            models.join("bad.toml").display()
        )
    );
    fs::remove_file(models.join("bad.toml")).unwrap();
    // more models in one addon than an overlay holds
    let per_file = "[[model]]\nname = \"a.b\"\n".repeat(MAX_MODELS_PER_FILE);
    let files = MAX_MODELS_PER_ADDON / MAX_MODELS_PER_FILE + 1;
    for i in 0..files {
        fs::write(models.join(format!("m{i:02}.toml")), &per_file).unwrap();
    }
    let err = read_model_files(addon, &my_addon()).unwrap_err();
    assert_eq!(
        err.to_string(),
        format!(
            "Model file '{}': more than {MAX_MODELS_PER_ADDON} models in one addon",
            models.display()
        )
    );
    for i in 0..files {
        fs::remove_file(models.join(format!("m{i:02}.toml"))).unwrap();
    }
    // more bytes in total than allowed (comments: valid, empty files)
    let big = "#".repeat(usize::try_from(MAX_MODEL_FILE_BYTES).unwrap());
    let count = MAX_MODEL_FILES_TOTAL_BYTES / MAX_MODEL_FILE_BYTES + 1;
    for i in 0..count {
        fs::write(models.join(format!("c{i:02}.toml")), &big).unwrap();
    }
    let err = read_model_files(addon, &my_addon()).unwrap_err();
    assert_eq!(
        err.to_string(),
        format!(
            "Model file '{}': model files larger than {MAX_MODEL_FILES_TOTAL_BYTES} bytes in total",
            models.display()
        )
    );
}

/// Compose the registry of data-only addons `(name, depends, model file)`, in that order.
fn compose(addons: &[(&str, &[&str], &str)]) -> erp_fields::Result<erp_registry::Registry> {
    let mut overlay = Overlay::new();
    let mut descriptors = Vec::new();
    for (name, depends, content) in addons {
        descriptors.push(erp_registry::intern_addon(name, depends).unwrap());
        let module = ModuleName::new(*name).unwrap();
        overlay
            .add_addon_defs(parse_model_file(&module, FILE, content).unwrap())
            .unwrap();
    }
    RegistryBuilder::new(descriptors)
        .linked(false)
        .overlay(&overlay)
        .build()
}

const KINDS_BASE: &str = r#"
[[model]]
name = "kb.mixin"
abstract = true

[[model]]
name = "kb.wizard"
transient = true
"#;

#[test]
fn extensions_take_the_kind_of_their_model() {
    let ext = r#"
[[model]]
extend = "kb.mixin"
[[model.field]]
name = "note"
type = "char"

[[model]]
extend = "kb.wizard"
[[model.field]]
name = "step"
type = "integer"
"#;
    let defs = parse_model_file(&ModuleName::new("kb_ext").unwrap(), FILE, ext).unwrap();
    assert!(defs.iter().all(|d| d.kind.is_none()));
    let registry = compose(&[
        ("kb_base", &["base"], KINDS_BASE),
        ("kb_ext", &["kb_base"], ext),
    ])
    .unwrap();
    let mixin = registry.model("kb.mixin").unwrap();
    assert_eq!(mixin.kind, ModelKind::Abstract);
    assert!(mixin.field("note").is_some());
    let wizard = registry.model("kb.wizard").unwrap();
    assert_eq!(wizard.kind, ModelKind::Transient);
    assert!(wizard.field("step").is_some());

    let err = compose(&[
        ("kb_base", &["base"], KINDS_BASE),
        (
            "kb_ext",
            &["kb_base"],
            "[[model]]\nextend = \"kb.wizard\"\ntransient = false\n",
        ),
    ])
    .unwrap_err();
    assert_eq!(err.kind(), erp_fields::ErrorKind::Programming);
    assert_eq!(
        err.message(),
        "model definition 'kb.wizard' of addon 'kb_ext' (models/test.toml:1) transforms the \
         transient model 'kb.wizard' into a non-transient model. That class should either \
         inherit from TransientModel, or set a different '_name'."
    );
    let err = compose(&[
        ("kb_base", &["base"], KINDS_BASE),
        (
            "kb_ext",
            &["kb_base"],
            "[[model]]\nextend = \"kb.mixin\"\nabstract = false\n",
        ),
    ])
    .unwrap_err();
    assert_eq!(err.kind(), erp_fields::ErrorKind::Programming);
    assert_eq!(
        err.message(),
        "model definition 'kb.mixin' of addon 'kb_ext' (models/test.toml:1) transforms the \
         abstract model 'kb.mixin' into a non-abstract model. That class should either inherit \
         from AbstractModel, or set a different '_name'."
    );
}

/// Review item 8: one addon declaring the same new model twice (in one file or two) fails the
/// registry build, naming the second definition.
#[test]
fn a_model_declared_twice_in_an_addon_is_refused() {
    let twice = "[[model]]\nname = \"dup.tag\"\n\n[[model]]\nname = \"dup.tag\"\n";
    let err = compose(&[("dup_addon", &["base"], twice)]).unwrap_err();
    assert_eq!(err.kind(), erp_fields::ErrorKind::Programming);
    assert_eq!(
        err.message(),
        "model definition 'dup.tag' of addon 'dup_addon' (models/test.toml:4) defines model \
         'dup.tag', which already exists; extend it with inherit = ['dup.tag']"
    );
}

/// Review item 8: a new selection field needs `selection` (or `selection_add` on an inherited
/// one); `rec_name` must name a field. Both are checked by the registry, never passed to SQL.
#[test]
fn selection_and_rec_name_are_checked_by_the_registry() {
    let sel = "[[model]]\nname = \"chk.tag\"\n\n[[model.field]]\nname = \"state\"\ntype = \"selection\"\n";
    let err = compose(&[("chk_addon", &["base"], sel)]).unwrap_err();
    assert_eq!(err.kind(), erp_fields::ErrorKind::Programming);
    assert_eq!(
        err.message(),
        "chk.tag.state: selection is mandatory unless the field is related or extended"
    );
    let rec = "[[model]]\nname = \"chk.tag\"\nrec_name = \"name; DROP TABLE chk_tag\"\n";
    let err = compose(&[("chk_addon", &["base"], rec)]).unwrap_err();
    assert_eq!(err.kind(), erp_fields::ErrorKind::Programming);
    assert_eq!(
        err.message(),
        "Invalid _rec_name='name; DROP TABLE chk_tag' for model 'chk.tag'"
    );
}

/// `default = true` on a many2one is still refused (see
/// `defaults_digits_and_size_are_checked`). Before the fix the many2one and char cases were read
/// as `Bool(false)` and refused by the registry.
#[test]
fn default_false_is_reference_false_on_any_field() {
    let content = r#"
[[model]]
name = "df.thing"

[[model.field]]
name = "partner_id"
type = "many2one"
comodel = "df.thing"
default = false

[[model.field]]
name = "label"
type = "char"
default = false

[[model.field]]
name = "active"
type = "boolean"
default = false
"#;
    let defs = parse_model_file(&my_addon(), FILE, content).unwrap();
    let defaults: Vec<_> = defs[0].fields.iter().map(|f| f.default.clone()).collect();
    assert_eq!(
        defaults,
        [
            Some(DefaultValue::False),
            Some(DefaultValue::False),
            Some(DefaultValue::Bool(false))
        ]
    );
    let registry = compose(&[("df_addon", &[], content)]).unwrap();
    let thing = registry.model("df.thing").unwrap();
    for (field, boolean) in [("partner_id", false), ("label", false), ("active", true)] {
        let default = thing.field(field).unwrap().default;
        assert!(
            match default {
                Some(erp_registry::FieldDefault::Value(erp_registry::StaticValue::False)) =>
                    !boolean,
                Some(erp_registry::FieldDefault::Value(erp_registry::StaticValue::Bool(false))) =>
                    boolean,
                _ => false,
            },
            "{field}: {default:?}"
        );
    }
}

/// Partitioning is declared by native addons only (design §7.8): a model file that
/// sets `partition`, on a new model or an extension, in any form, is refused with the line.
#[test]
fn partitioning_is_refused_in_model_files() {
    for (content, line, model) in [
        (
            "[[model]]\nname = \"scale.event\"\npartition = { interval = \"day\", premake = 7, \
             retain = 30 }\n",
            1,
            "scale.event",
        ),
        (
            "[[model]]\nname = \"a.b\"\n\n[[model]]\nextend = \"res.partner\"\npartition = \
             \"day\"\n",
            4,
            "res.partner",
        ),
    ] {
        assert_eq!(
            refused(content),
            format!(
                "Model file 'models/test.toml:{line}': model '{model}': partitioning is declared \
                 by native addons only ({})",
                erp_registry::PARTITION_ADR
            ),
            "{content}"
        );
    }
}

// ----- typed indexes (`[[model.index]]`, design, §9) ----------------

const INDEXED: &str = r#"
[[model]]
name = "ix.booking"

[[model.field]]
name = "state"
type = "char"

[[model.field]]
name = "start"
type = "datetime"

[[model.field]]
name = "resource"
type = "integer"

[[model.index]]
name = "_open_by_date"
fields = ["state", "start"]
include = ["resource"]

[[model.index]]
name = "_by_start"
fields = ["start"]
method = "brin"

[[model.index]]
name = "_open"
fields = ["start"]
filter = "[('state', '=', 'open')]"
"#;

/// the registry like native ones)
#[test]
fn spec_ormx_021_model_file_typed_indexes() {
    let defs = parse_model_file(&my_addon(), FILE, INDEXED).unwrap();
    assert_eq!(
        defs[0].indexes,
        [
            erp_registry::IndexDef::new("_open_by_date", ["state", "start"]).include(["resource"]),
            erp_registry::IndexDef::new("_by_start", ["start"])
                .method(erp_registry::IndexMethod::Brin),
            erp_registry::IndexDef::new("_open", ["start"]).filter("[('state', '=', 'open')]"),
        ]
    );
    let registry = compose(&[("ix_addon", &[], INDEXED)]).unwrap();
    let booking = registry.model("ix.booking").unwrap();
    let objects: Vec<_> = booking
        .table_objects
        .iter()
        .map(|o| (o.full_name.as_str(), o.name, o.kind))
        .collect();
    let [
        (open_name, "_open_by_date", erp_registry::TableObjectKind::TypedIndex(open)),
        (start_name, "_by_start", erp_registry::TableObjectKind::TypedIndex(start)),
        (filtered_name, "_open", erp_registry::TableObjectKind::TypedIndex(filtered)),
    ] = objects.as_slice()
    else {
        panic!("unexpected table objects {objects:?}")
    };
    assert_eq!(
        (*open_name, *start_name),
        ("ix_booking_open_by_date", "ix_booking_by_start")
    );
    assert_eq!(
        (open.fields, open.method, open.include),
        (
            &["state", "start"][..],
            erp_registry::IndexMethod::Btree,
            &["resource"][..]
        )
    );
    assert_eq!(
        (start.fields, start.method, start.include),
        (&["start"][..], erp_registry::IndexMethod::Brin, &[][..])
    );
    assert_eq!(
        (*filtered_name, filtered.filter),
        ("ix_booking_open", Some("[('state', '=', 'open')]"))
    );
    // an extension in another data-only addon adds an index to a model it does not own
    let ext = r#"
[[model]]
extend = "ix.booking"

[[model.index]]
name = "_by_resource"
fields = ["resource"]
method = "brin"
"#;
    let registry = compose(&[("ix_addon", &[], INDEXED), ("ix_ext", &["ix_addon"], ext)]).unwrap();
    let names: Vec<_> = registry
        .model("ix.booking")
        .unwrap()
        .table_objects
        .iter()
        .map(|o| o.name)
        .collect();
    assert_eq!(
        names,
        ["_open_by_date", "_by_start", "_open", "_by_resource"]
    );
    // and the registry checks the fields as for native declarations
    let wrong = r#"
[[model]]
extend = "ix.booking"

[[model.index]]
name = "_by_state"
fields = ["state"]
method = "brin"
"#;
    let err = compose(&[("ix_addon", &[], INDEXED), ("ix_ext", &["ix_addon"], wrong)]).unwrap_err();
    assert_eq!(err.kind(), erp_fields::ErrorKind::Programming);
    assert_eq!(
        err.message(),
        "index '_by_state' of 'ix.booking': method = \"brin\" is for date, datetime and integer \
         fields ('state' is char)"
    );
}

#[test]
fn spec_ormx_021_model_file_index_refusals() {
    let head = "[[model]]\nname = \"ix.t\"\n\n[[model.field]]\nname = \"a\"\ntype = \"char\"\n\n";
    let cases = [
        (
            "[[model.index]]\nname = \"_a\"\nfields = [\"a\"]\nfilter = 1\n",
            "Model file 'models/test.toml:11': invalid type: integer `1`, expected a string",
        ),
        (
            "[[model.index]]\nname = \"_a\"\nfields = [\"a\"]\nmethod = \"hash\"\n",
            "Model file 'models/test.toml:8': model 'ix.t': index '_a': unknown method 'hash' \
             (expected btree, brin, gin, gist or trigram)",
        ),
        (
            "[[model.index]]\nname = \"a\"\nfields = [\"a\"]\n",
            "Model file 'models/test.toml:8': model 'ix.t': index 'a': names of SQL objects in a \
             model start with '_'",
        ),
        (
            "[[model.index]]\nname = \"_a\"\nfields = [\"a\"]\n\n[[model.index]]\nname = \
             \"_a\"\nfields = [\"a\"]\n",
            "Model file 'models/test.toml:12': model 'ix.t': index '_a' is declared twice",
        ),
        (
            "[[model.index]]\nname = \"_a\"\nfields = [\"a\"]\nunique = true\n",
            "Model file 'models/test.toml:11': unknown field `unique`, expected one of `name`, \
             `fields`, `method`, `include`, `filter`",
        ),
        (
            "[[model.index]]\nname = \"_a\"\nfields = \"a\"\n",
            "Model file 'models/test.toml:10': invalid type: string \"a\", expected a sequence",
        ),
    ];
    for (tail, message) in cases {
        assert_eq!(refused(&format!("{head}{tail}")), message, "{tail}");
    }
    // the column limit is the model's (reported at its line)
    let many: Vec<String> = (0..=erp_registry::MAX_INDEX_COLUMNS)
        .map(|i| format!("\"f{i}\""))
        .collect();
    assert_eq!(
        refused(&format!(
            "{head}[[model.index]]\nname = \"_wide\"\nfields = [{}]\n",
            many.join(", ")
        )),
        format!(
            "Model file 'models/test.toml:1': model 'ix.t': index '_wide' has {} columns; \
             PostgreSQL allows at most {}",
            erp_registry::MAX_INDEX_COLUMNS + 1,
            erp_registry::MAX_INDEX_COLUMNS
        )
    );
    // a wrong field-level kind is reported at the field's line
    assert_eq!(
        refused(
            "[[model]]\nname = \"ix.t\"\n\n[[model.field]]\nname = \"a\"\ntype = \"char\"\nindex = \"brin\"\n"
        ),
        "Model file 'models/test.toml:4': field ix.t.a: index = \"brin\" is for date, datetime \
         and integer fields (this field is char)"
    );
    // brin and gin need a stored field, like the registry says
    assert_eq!(
        refused(
            "[[model]]\nname = \"ix.t\"\n\n[[model.field]]\nname = \"a\"\ntype = \"json\"\nstore = false\nrelated = \"x.y\"\nindex = \"gin\"\n"
        ),
        "Model file 'models/test.toml:4': field ix.t.a: index = \"gin\" needs a stored field \
         (this field has no column)"
    );
}

const EXCLUDE: &str = "[[model]]\nextend = \"scale.booking\"\n\n[[model.exclude]]\n\
name = \"_no_double_booking\"\nequal = [\"resource_id\"]\n\
period = { from = \"start\", to = \"stop\", bounds = \"[)\" }\n\
filter = \"[('state', '!=', 'cancel')]\"\nmessage = \"Booked.\"\n";

/// `WITHOUT OVERLAPS` keys parse into their definitions; refused files name the file and line)
#[test]
fn spec_ormx_021_typed_objects_parse_strictly() {
    let defs = parse_model_file(&my_addon(), FILE, EXCLUDE).unwrap();
    let excl = &defs[0].exclusions[0];
    assert_eq!(excl.name, "_no_double_booking");
    assert_eq!(excl.equal, ["resource_id"]);
    assert_eq!(excl.period, ("start".to_owned(), "stop".to_owned()));
    assert_eq!(excl.bounds, erp_fields::RangeBounds::HalfOpen);
    assert_eq!(excl.filter.as_deref(), Some("[('state', '!=', 'cancel')]"));
    let defs = parse_model_file(
        &my_addon(),
        FILE,
        "[[model]]\nname = \"my.rate\"\n\n[[model.field]]\nname = \"validity\"\n\
         type = \"date_range\"\nbounds = \"[]\"\n\n[[model.key]]\nname = \"_rate_key\"\n\
         fields = [\"product\"]\nwithout_overlaps = \"validity\"\nmessage = \"One rate.\"\n",
    )
    .unwrap();
    assert_eq!(
        defs[0].fields[0].kind,
        KindDef::DateRange {
            bounds: erp_fields::RangeBounds::Closed
        }
    );
    assert_eq!(defs[0].temporal_keys[0].range, "validity");
    assert_eq!(defs[0].temporal_keys[0].fields, ["product"]);
    // CRLF line ends and non-ASCII messages
    let crlf = EXCLUDE
        .replace(
            "message = \"Booked.\"",
            "message = \"Déjà réservé — 予約済み\"",
        )
        .replace('\n', "\r\n");
    let defs = parse_model_file(&my_addon(), FILE, &crlf).unwrap();
    assert_eq!(defs[0].exclusions[0].message, "Déjà réservé — 予約済み");

    let cases = [
        // raw SQL has no place in a typed object
        (
            &EXCLUDE.replace(
                "message = \"Booked.\"\n",
                "message = \"Booked.\"\ndefinition = \"EXCLUDE USING gist (x WITH &&)\"\n",
            ),
            "Model file 'models/test.toml:10': unknown field `definition`, expected one of \
             `name`, `equal`, `period`, `filter`, `message`",
        ),
        // an unknown option
        (
            &EXCLUDE.replace("bounds = \"[)\" }", "bounds = \"[)\", deferrable = true }"),
            "Model file 'models/test.toml:7': unknown field `deferrable`, expected one of \
             `from`, `to`, `bounds`",
        ),
        (
            &EXCLUDE.replace("bounds = \"[)\"", "bounds = \"()\""),
            "Model file 'models/test.toml:4': exclusion '_no_double_booking': unknown bounds \
             '()' (expected \"[)\" or \"[]\")",
        ),
        (
            &format!(
                "{EXCLUDE}\n{}",
                &EXCLUDE[EXCLUDE.find("[[model.exclude]]").unwrap()..]
            ),
            "Model file 'models/test.toml:11': model 'scale.booking': '_no_double_booking' is \
             declared twice",
        ),
        (
            &"[[model]]\nname = \"my.rate\"\n\n[[model.key]]\nname = \"_code\"\nfields = \
             [\"code\"]\nmessage = \"Unique.\"\n"
                .to_owned(),
            "Model file 'models/test.toml:4': key '_code': business keys in model files arrive \
             with a later release; this build declares keys with `without_overlaps = \"<range \
             field>\"` only",
        ),
        (
            &"[[model]]\nname = \"my.rate\"\n\n[[model.field]]\nname = \"validity\"\n\
             type = \"date_range\"\n"
                .to_owned(),
            "Model file 'models/test.toml:4': field my.rate.validity: a date_range field needs \
             `bounds` (\"[)\" or \"[]\"; no default)",
        ),
        (
            &"[[model]]\nname = \"my.rate\"\n\n[[model.field]]\nname = \"code\"\n\
             type = \"char\"\nbounds = \"[)\"\n"
                .to_owned(),
            "Model file 'models/test.toml:4': field my.rate.code: `bounds` does not apply to \
             type 'char'",
        ),
    ];
    for (content, message) in cases {
        assert_eq!(refused(content), message, "{content}");
    }
}

/// The two company keys of a model file (and 2).
#[test]
fn company_keys_parse_and_refuse_unknown_matches() {
    let defs = parse_model_file(
        &my_addon(),
        FILE,
        "[[model]]\nname = \"my.catalog\"\ncheck_company_auto = false\ncompany_match = \"parent_of\"\n",
    )
    .unwrap();
    assert_eq!(defs[0].check_company_auto, Some(false));
    assert_eq!(
        defs[0].company_match,
        Some(erp_registry::CompanyMatch::ParentOf)
    );
    let defs = parse_model_file(&my_addon(), FILE, "[[model]]\nname = \"my.catalog\"\n").unwrap();
    assert_eq!(defs[0].check_company_auto, None);
    assert_eq!(defs[0].company_match, None);
    assert_eq!(
        refused("[[model]]\nname = \"my.catalog\"\ncompany_match = \"Exact\"\n"),
        "Model file 'models/test.toml:1': model 'my.catalog': company_match must be one of \
         \"exact\", \"parent_of\", \"any_of\", \"parent_of_any\" (got \"Exact\")"
    );
    assert_eq!(
        refused("[[model]]\nname = \"my.catalog\"\ncheck_company_auto = \"no\"\n"),
        "Model file 'models/test.toml:3': invalid type: string \"no\", expected a boolean"
    );
}

/// `[model.company_access]` (A1-D2, design): each
/// form, the registry spelling of what was read, and every refusal with its file and line.
///
/// (blank or 513-byte reason, empty field or edge name) fail with the model file's line before the
/// registry is built; the registry variants through a runtime definition are
/// `runtime_overlays_use_the_same_validation`; no install or update)
#[test]
fn company_access_in_model_files() {
    use erp_registry::{CompanyAccessDecl, CompanyBranch, EmptyCompany};
    let read = |access: &str| {
        let defs = parse_model_file(
            &my_addon(),
            FILE,
            &format!("[[model]]\nname = \"my.doc\"\n\n[model.company_access]\n{access}"),
        )
        .unwrap();
        // what the registry composes: the interned declaration
        defs[0].to_decl().unwrap().company_access
    };
    assert_eq!(
        read("direct = { field = \"company_id\", branch = \"ancestor\", empty = \"allow\" }\n"),
        Some(CompanyAccessDecl::direct(
            "company_id",
            CompanyBranch::Ancestor,
            EmptyCompany::Allow
        ))
    );
    assert_eq!(
        read("parent = \"doc_id\"\n"),
        Some(CompanyAccessDecl::parent("doc_id"))
    );
    assert_eq!(
        read(
            "direct = { field = \"company_ids\", branch = \"exact\", empty = \"deny\" }\n\
             parent = \"tax_id\"\n"
        ),
        Some(CompanyAccessDecl::direct_and_parent(
            "company_ids",
            CompanyBranch::Exact,
            EmptyCompany::Deny,
            "tax_id"
        ))
    );
    assert_eq!(
        read(
            "direct = { field = \"company_id\", branch = \"ancestor\", empty = \"allow\", escape = \
             \"share\" }\n"
        ),
        Some(CompanyAccessDecl::direct_escaping(
            "company_id",
            CompanyBranch::Ancestor,
            EmptyCompany::Allow,
            "share"
        ))
    );
    assert_eq!(
        read("unrestricted = \"Société: visible to every company\"\n"),
        Some(CompanyAccessDecl::unrestricted(
            "Société: visible to every company"
        ))
    );
    let defs = parse_model_file(&my_addon(), FILE, "[[model]]\nname = \"my.doc\"\n").unwrap();
    assert_eq!(defs[0].company_access, None);

    let header = "[[model]]\nname = \"my.doc\"\n\n[model.company_access]\n";
    assert_eq!(
        refused(header),
        "Model file 'models/test.toml:1': model 'my.doc': company_access needs direct, parent or \
         unrestricted"
    );
    assert_eq!(
        refused(&format!(
            "{header}unrestricted = \"x\"\nparent = \"doc_id\"\n"
        )),
        "Model file 'models/test.toml:1': model 'my.doc': company_access: unrestricted cannot be \
         combined with direct or parent"
    );
    assert_eq!(
        refused(&format!(
            "{header}direct = {{ field = \"company_id\", branch = \"exact\", empty = \"deny\" }}\n\
             unrestricted = \"x\"\n"
        )),
        "Model file 'models/test.toml:1': model 'my.doc': company_access: unrestricted cannot be \
         combined with direct or parent"
    );
    assert_eq!(
        refused(&format!(
            "{header}direct = {{ field = \"company_id\", branch = \"parent_of\", empty = \"deny\" }}\n"
        )),
        "Model file 'models/test.toml:1': model 'my.doc': company_access.direct: branch must be \
         \"exact\" or \"ancestor\" (got \"parent_of\")"
    );
    assert_eq!(
        refused(&format!(
            "{header}direct = {{ field = \"company_id\", branch = \"exact\", empty = \"False\" }}\n"
        )),
        "Model file 'models/test.toml:1': model 'my.doc': company_access.direct: empty must be \
         \"allow\" or \"deny\" (got \"False\")"
    );
    assert_eq!(
        refused(&format!(
            "{header}parent = \"doc_id\"\ndirect = {{ field = \"company_id\", branch = \"exact\", \
             empty = \"deny\", escape = \"share\" }}\n"
        )),
        "Model file 'models/test.toml:1': model 'my.doc': company_access.direct: escape cannot be \
         combined with parent"
    );
    assert_eq!(
        refused(&format!(
            "{header}direct = {{ field = \"company_id\", branch = \"exact\", empty = \"deny\", \
             escape = \"\" }}\n"
        )),
        "Model file 'models/test.toml:1': model 'my.doc': company_access: the escape name is empty"
    );
    // unknown keys and missing parts are serde's errors, with the line
    assert_eq!(
        refused(&format!("{header}owner = \"doc_id\"\n")),
        "Model file 'models/test.toml:5': unknown field `owner`, expected one of `direct`, \
         `parent`, `unrestricted`"
    );
    assert_eq!(
        refused(&format!(
            "{header}direct = {{ field = \"company_id\", branch = \"exact\", empty = \"deny\", \
             parent = \"doc_id\" }}\n"
        )),
        "Model file 'models/test.toml:5': unknown field `parent`, expected one of `field`, \
         `branch`, `empty`, `escape`"
    );
    assert_eq!(
        refused(&format!("{header}direct = {{ field = \"company_id\" }}\n")),
        "Model file 'models/test.toml:5': missing field `branch`"
    );
    assert_eq!(
        refused(&format!("{header}parent = 3\n")),
        "Model file 'models/test.toml:5': invalid type: integer `3`, expected a string"
    );
    // the declaration alone is checked before the registry, with the macro's words and the
    // model's line (review F1)
    let early = |access: &str, message: &str| {
        assert_eq!(
            refused(&format!("{header}{access}")),
            format!("Model file 'models/test.toml:1': model 'my.doc': {message}")
        );
    };
    early(
        &format!("unrestricted = \"{}\"\n", "r".repeat(513)),
        "company_access: unrestricted(…) needs a reason that is not blank and at most 512 bytes \
         long, but it is 513 bytes long",
    );
    // empty or only whitespace (review M1)
    for blank in ["\"\"", "\"   \"", "\"\\t\\r\\n\"", "\"\\u00A0\""] {
        early(
            &format!("unrestricted = {blank}\n"),
            "company_access: unrestricted(…) needs a reason that is not blank and at most 512 bytes \
             long, but it is blank",
        );
    }
    early(
        "direct = { field = \"\", branch = \"exact\", empty = \"deny\" }\n",
        "company_access: the field name is empty",
    );
    early("parent = \"\"\n", "company_access: the edge name is empty");
    early(
        "direct = { field = \"company_id\", branch = \"exact\", empty = \"deny\" }\nparent = \"\"\n",
        "company_access: the edge name is empty",
    );
}

#[test]
fn spec_sqlfield_001_model_file_generated_columns() {
    let toml = r#"
[[model]]
name = "my.doc"

[[model.field]]
name = "a"
type = "integer"

[[model.field]]
name = "total"
type = "integer"
generated = "a + 1"

[[model.field]]
name = "s_total"
type = "integer"
generated = "a + 2"
generated_storage = "stored"

[[model.field]]
name = "v_total"
type = "integer"
generated = "a + 3"
generated_storage = "virtual"
"#;
    let defs = parse_model_file(&my_addon(), FILE, toml).unwrap();
    assert_eq!(defs.len(), 1);
    let m = &defs[0];
    let total = m.fields.iter().find(|f| f.name == "total").unwrap();
    assert_eq!(
        total.generated.as_ref().and_then(|o| o.as_deref()),
        Some("a + 1")
    );
    assert_eq!(total.generated_storage, None);

    let s_total = m.fields.iter().find(|f| f.name == "s_total").unwrap();
    assert_eq!(
        s_total.generated.as_ref().and_then(|o| o.as_deref()),
        Some("a + 2")
    );
    assert_eq!(
        s_total.generated_storage,
        Some(erp_registry::GeneratedStorage::Stored)
    );

    let v_total = m.fields.iter().find(|f| f.name == "v_total").unwrap();
    assert_eq!(
        v_total.generated.as_ref().and_then(|o| o.as_deref()),
        Some("a + 3")
    );
    assert_eq!(
        v_total.generated_storage,
        Some(erp_registry::GeneratedStorage::Virtual)
    );

    // Validation refusals
    let bad_storage = r#"
[[model]]
name = "my.doc"

[[model.field]]
name = "f"
type = "integer"
generated = "1"
generated_storage = "invalid"
"#;
    let err = refused(bad_storage);
    assert!(err.contains("unknown generated_storage 'invalid', expected 'virtual' or 'stored'"));

    let empty_storage = bad_storage.replace("\"invalid\"", "\"\"");
    let err = refused(&empty_storage);
    assert!(err.contains("unknown generated_storage '', expected 'virtual' or 'stored'"));

    // TOML's diagnostic includes source locations; assert its stable type reason and file path.
    // `refused` also asserts the public Error::ModelFile variant.
    for value in ["true", "42", "[]", "{}"] {
        let wrong_type = bad_storage.replace("\"invalid\"", value);
        let err = refused(&wrong_type);
        assert!(err.contains("Model file 'models/test.toml:"), "{err}");
        assert!(err.contains("invalid type:"), "{err}");
        assert!(err.contains("expected a string"), "{err}");
    }

    let storage_without_expr = r#"
[[model]]
name = "my.doc"

[[model.field]]
name = "f"
type = "integer"
generated_storage = "virtual"
"#;
    let err = refused(storage_without_expr);
    assert!(err.contains("attribute 'generated_storage' conflicts with generation"));
}

/// `transient_max_count` of a model file reach the composed registry; a zero disables a threshold.
#[test]
fn transient_policy_attributes_reach_the_registry() {
    let defs = parse_model_file(
        &my_addon(),
        FILE,
        "[[model]]\nname = \"x_wiz\"\ntransient = true\ntransient_max_age_seconds = 7200\n\
         transient_max_count = 50\n\
         [[model]]\nname = \"x_off\"\ntransient = true\ntransient_max_age_seconds = 0\n",
    )
    .unwrap();
    assert_eq!(defs[0].transient_max_age_seconds, Some(7200));
    assert_eq!(defs[0].transient_max_count, Some(50));
    assert_eq!(defs[1].transient_max_count, None);
    let manifest = erp_modules::Manifest::from_toml("my_addon", "name = 'M'\n").unwrap();
    let addon = addon_descriptor(&manifest).unwrap();
    let mut overlay = Overlay::new();
    overlay.add_addon_defs(defs).unwrap();
    let registry = RegistryBuilder::new([addon])
        .linked(false)
        .overlay(&overlay)
        .build()
        .unwrap();
    let wiz = registry.model("x_wiz").unwrap().transient_policy.unwrap();
    assert_eq!(
        (wiz.max_age_seconds(), wiz.max_count()),
        (Some(7200), Some(50))
    );
    let off = registry.model("x_off").unwrap().transient_policy.unwrap();
    assert_eq!((off.max_age_seconds(), off.max_count()), (None, None));
    assert!(off.is_cleanup_disabled());
}

/// Wrong type, negative or overflowing policy attributes are refused naming the attribute; a
/// regular model declaring one is refused when the registry is built.
#[test]
fn transient_policy_attributes_are_validated() {
    for (attr, value, hint) in [
        ("transient_max_age_seconds", "-1", "must not be negative"),
        ("transient_max_count", "-5", "must not be negative"),
        ("transient_max_age_seconds", "1.5", "must be an integer"),
        ("transient_max_count", "\"10\"", "must be an integer"),
        ("transient_max_count", "true", "must be an integer"),
        (
            "transient_max_age_seconds",
            "315360001",
            "at most 315360000",
        ),
    ] {
        let message = refused(&format!(
            "[[model]]\nname = \"a.b\"\ntransient = true\n{attr} = {value}\n"
        ));
        assert!(
            message.contains(attr) && message.contains(hint),
            "{attr} = {value}: {message}"
        );
    }
    // beyond i64 the TOML parser itself refuses the number, naming the line
    assert!(
        parse_model_file(
            &my_addon(),
            FILE,
            "[[model]]\nname = \"a.b\"\ntransient = true\ntransient_max_count = 9223372036854775808\n"
        )
        .is_err()
    );
    // i64::MAX is the largest accepted count
    let defs = parse_model_file(
        &my_addon(),
        FILE,
        "[[model]]\nname = \"a.b\"\ntransient = true\ntransient_max_count = 9223372036854775807\n",
    )
    .unwrap();
    assert_eq!(defs[0].transient_max_count, Some(i64::MAX.cast_unsigned()));

    let defs = parse_model_file(
        &my_addon(),
        FILE,
        "[[model]]\nname = \"x_plain\"\ntransient_max_count = 3\n",
    )
    .unwrap();
    let manifest = erp_modules::Manifest::from_toml("my_addon", "name = 'M'\n").unwrap();
    let addon = addon_descriptor(&manifest).unwrap();
    let mut overlay = Overlay::new();
    overlay.add_addon_defs(defs).unwrap();
    let err = RegistryBuilder::new([addon])
        .linked(false)
        .overlay(&overlay)
        .build()
        .unwrap_err();
    assert!(
        err.message().contains("x_plain") && err.message().contains("transient_max_count"),
        "{err}"
    );
}

#[test]
fn native_fields_model_files_keep_explicit_native_types() {
    let definitions = parse_model_file(
        &my_addon(),
        FILE,
        r#"
[[model]]
name = "my.native"
[[model.field]]
name = "u"
type = "uuid"
[[model.field]]
name = "x"
type = "xid8"
[[model.field]]
name = "t"
type = "timestamptz"
"#,
    )
    .unwrap();
    assert_eq!(definitions[0].fields[0].kind, KindDef::Uuid);
    assert_eq!(definitions[0].fields[1].kind, KindDef::Xid8);
    assert_eq!(definitions[0].fields[2].kind, KindDef::TimestampTz);
    assert!(
        refused("[[model]]\nname='my.native'\n[[model.field]]\nname='u'\ntype='uuid'\nsize=12\n")
            .contains("`size` does not apply to type 'uuid'")
    );
}

#[test]
fn eventful_opt_in_is_read_from_model_files() {
    let defs = parse_model_file(&my_addon(), FILE, "[[model]]\nname = 'test.eventful'\neventful = true\n\n[[model]]\nextend = 'test.eventful'\neventful = false\n").unwrap();
    assert!(defs[0].eventful);
    assert!(!defs[1].eventful);
    assert!(
        !parse_model_file(&my_addon(), FILE, "[[model]]\nname = 'test.plain'\n").unwrap()[0]
            .eventful
    );
}

#[test]
fn eventful_wrong_types_and_tableless_attributes_are_refused() {
    assert_eq!(
        refused("[[model]]\nname = 'test.eventful'\neventful = 'true'\n"),
        "Model file 'models/test.toml:3': invalid type: string \"true\", expected a boolean"
    );
    for key in ["auto", "view"] {
        assert_eq!(
            refused(&format!(
                "[[model]]\nname = 'test.eventful'\n{key} = false\n"
            )),
            format!(
                "Model file 'models/test.toml:3': unknown field `{key}`, expected one of `name`, `extend`, `inherit`, `description`, `table`, `order`, `rec_name`, `check_company_auto`, `company_match`, `company_access`, `eventful`, `audit_reads`, `transient`, `abstract`, `transient_max_age_seconds`, `transient_max_count`, `field`, `index`, `partition`, `exclude`, `key`, `sql_function`, `sql_trigger`, `sql_condition`, `sql_grant`, `persistence`"
            )
        );
    }
}

#[test]
fn audit_reads_opt_in_is_read_from_model_files() {
    let defs = parse_model_file(&my_addon(), FILE, "[[model]]\nname = 'test.audit_reads'\naudit_reads = true\n\n[[model]]\nextend = 'test.audit_reads'\naudit_reads = false\n").unwrap();
    assert!(defs[0].audit_reads);
    assert!(!defs[1].audit_reads);
    assert!(
        !parse_model_file(&my_addon(), FILE, "[[model]]\nname = 'test.plain'\n").unwrap()[0]
            .audit_reads
    );
}

#[test]
fn audit_reads_wrong_type_is_refused() {
    assert_eq!(
        refused("[[model]]\nname = 'test.audited'\naudit_reads = 'true'\n"),
        "Model file 'models/test.toml:3': invalid type: string \"true\", expected a boolean"
    );
}

static MY_ADDON: AddonDescriptor = AddonDescriptor {
    name: "my_addon",
    depends: &["base"],
    exact_only: false,
};

/// A model file that tries a function body, trigger, condition registration, grant or table
/// persistence, at model or file level, as a value or a table, is refused Programming naming the
/// key and the native-only surface; an unknown look-alike key is refused, not ignored; an
/// ordinary runtime declaration still parses and composes. Overlays are built from typed
/// `ModelDef`s and have no key through which SQL could be given.
#[test]
fn spec_ormx_039_runtime_inputs_cannot_declare_sql_objects() {
    for key in [
        "sql_function",
        "sql_trigger",
        "sql_condition",
        "sql_grant",
        "persistence",
    ] {
        let expected = format!("SQL object key '{key}' is native-only in a later release");
        for (content, at) in [
            // model level: the line of the model
            (
                format!("\n[[model]]\nname = \"m.t\"\n{key} = \"x\"\n"),
                Some(2),
            ),
            (
                format!("\n[[model]]\nname = \"m.t\"\n[[model.{key}]]\nname = \"x\"\n"),
                Some(2),
            ),
            // file level: no line
            (format!("{key} = \"x\"\n[[model]]\nname = \"m.t\"\n"), None),
            (
                format!("[[model]]\nname = \"m.t\"\n[[{key}]]\nname = \"x\"\n"),
                None,
            ),
        ] {
            let err = parse_model_file(&my_addon(), FILE, &content).unwrap_err();
            let Error::ModelFile {
                line,
                message,
                source,
                ..
            } = &err
            else {
                panic!("expected Error::ModelFile, got {err:?}");
            };
            assert_eq!(message, &expected, "{content}");
            assert_eq!(
                source.as_deref().map(erp_fields::Error::kind),
                Some(ErrorKind::Programming),
                "{content}"
            );
            assert_eq!(*line, at, "{content}");
            let location = at.map_or(FILE.to_owned(), |at| format!("{FILE}:{at}"));
            assert_eq!(
                err.to_string(),
                format!("Model file '{location}': {expected}")
            );
        }
    }
    assert!(
        refused("[[model]]\nname = \"my.tag\"\nsql_view = \"x\"\n").starts_with(
            "Model file 'models/test.toml:3': unknown field `sql_view`, expected one of"
        )
    );

    let defs = parse_model_file(
        &my_addon(),
        FILE,
        "[[model]]\nname = \"my.tag\"\n\n[[model.field]]\nname = \"label\"\ntype = \"char\"\n",
    )
    .unwrap();
    let mut overlay = Overlay::new();
    overlay.add_addon_defs(defs).unwrap();
    let registry = RegistryBuilder::new([&MY_ADDON])
        .linked(false)
        .overlay(&overlay)
        .build()
        .unwrap();
    assert!(registry.model("my.tag").unwrap().field("label").is_some());
}
