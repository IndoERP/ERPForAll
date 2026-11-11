//! Regression tests: every seed in `fuzz/corpus/ui_compile` runs through `analyze` and `compile`
//! without panicking, produces deterministic canonical output, and satisfies compiler invariants
//! and expected diagnostics.

use erp_ui_compiler::{
    AddonName, BuildMode, CompileOptions, DiagnosticKind, TemplatePath, TemplateSet,
    TemplateSource, analyze, compile,
};
use std::fs;
use std::path::Path;

enum ExpectedOutcome {
    Clean {
        expected_templates: &'static [&'static str],
    },
    Error {
        kind: DiagnosticKind,
        message: &'static str,
        directive: Option<&'static str>,
    },
}

fn expected_outcome_for_seed(filename: &str) -> Option<ExpectedOutcome> {
    match filename {
        "seed_01_minimal.xml" => Some(ExpectedOutcome::Clean {
            expected_templates: &["web.Minimal"],
        }),
        "seed_02_branching.xml" => Some(ExpectedOutcome::Clean {
            expected_templates: &["web.Branching"],
        }),
        "seed_03_loop.xml" => Some(ExpectedOutcome::Clean {
            expected_templates: &["web.Loop"],
        }),
        "seed_04_events.xml" => Some(ExpectedOutcome::Clean {
            expected_templates: &["web.Events"],
        }),
        "seed_05_slots.xml" => Some(ExpectedOutcome::Clean {
            expected_templates: &["web.Slots"],
        }),
        "seed_06_inherit_primary.xml" => Some(ExpectedOutcome::Error {
            kind: DiagnosticKind::MissingTemplate,
            message: "template parent 'web.Parent' not found (t-inherit of 'web.Child')",
            directive: Some("t-inherit"),
        }),
        "seed_07_inherit_ext.xml" => Some(ExpectedOutcome::Error {
            kind: DiagnosticKind::MissingTemplate,
            message: "template parent 'web.Base' not found (t-inherit of an extension)",
            directive: Some("t-inherit"),
        }),
        "seed_08_svg.xml" => Some(ExpectedOutcome::Clean {
            expected_templates: &["web.Svg"],
        }),
        "seed_09_multi.xml" => Some(ExpectedOutcome::Clean {
            expected_templates: &["web.A", "web.B"],
        }),
        "seed_10_cdata.xml" => Some(ExpectedOutcome::Clean {
            expected_templates: &["web.Cdata"],
        }),
        "seed_11_attrs.xml" => Some(ExpectedOutcome::Clean {
            expected_templates: &["web.Attrs"],
        }),
        "seed_12_deep_expr.xml" => Some(ExpectedOutcome::Clean {
            expected_templates: &["web.DeepExpr"],
        }),
        _ => None,
    }
}

#[test]
fn seed_corpus_compiles_deterministically() {
    let corpus_dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("fuzz/corpus/ui_compile");

    assert!(
        corpus_dir.is_dir(),
        "corpus dir must exist: {}",
        corpus_dir.display()
    );

    let mut entries: Vec<_> = fs::read_dir(&corpus_dir)
        .expect("read corpus dir")
        .map(|res| res.expect("read dir entry").path())
        .filter(|p| p.is_file() && !p.file_name().unwrap().to_str().unwrap().starts_with('.'))
        .collect();
    entries.sort();

    assert!(
        !entries.is_empty(),
        "seed corpus must contain at least one file"
    );

    let addon = AddonName::new("web").unwrap();
    let path = TemplatePath::new("seed.xml").unwrap();

    for entry in &entries {
        let filename = entry.file_name().unwrap().to_str().unwrap();
        let expected = expected_outcome_for_seed(filename);
        assert!(
            expected.is_some(),
            "unrecognized seed file in corpus: {filename}"
        );
        let expected = expected.unwrap();

        let content = fs::read_to_string(entry).expect("read seed file");

        for mode in [BuildMode::Dev, BuildMode::Prod] {
            let options = CompileOptions::new(mode);
            let mut set = TemplateSet::new();
            set.push(TemplateSource::addon_file(
                addon.clone(),
                path.clone(),
                content.as_str(),
            ));

            let a1 = analyze(&set, &options);
            let mut out1 = String::new();
            a1.write_canonical(&mut out1);

            let a2 = analyze(&set, &options);
            let mut out2 = String::new();
            a2.write_canonical(&mut out2);
            assert_eq!(
                out1,
                out2,
                "analyze must be deterministic for {}",
                entry.display()
            );

            match &expected {
                ExpectedOutcome::Clean { expected_templates } => {
                    assert!(
                        a1.is_clean(),
                        "analyze expected clean for {}, got rejected={}",
                        entry.display(),
                        a1.rejected()
                    );
                    assert_eq!(
                        a1.passed(),
                        expected_templates.len(),
                        "passed templates count mismatch for {}",
                        entry.display()
                    );
                }
                ExpectedOutcome::Error {
                    kind,
                    message,
                    directive,
                } => {
                    assert!(
                        !a1.is_clean(),
                        "analyze expected error for {}",
                        entry.display()
                    );
                    let found = a1
                        .set_diagnostics()
                        .iter()
                        .chain(a1.templates().iter().flat_map(|t| t.diagnostics()))
                        .any(|d| {
                            d.kind() == *kind
                                && d.message() == *message
                                && d.directive() == *directive
                        });
                    assert!(
                        found,
                        "analyze did not find expected diagnostic {:?} '{}' for {}",
                        kind,
                        message,
                        entry.display()
                    );
                }
            }

            let mut c1_str = String::new();
            let c1 = compile(&set, &options);
            match &c1 {
                Ok(c) => c.write_canonical(&mut c1_str),
                Err(e) => e.write_canonical(&mut c1_str),
            }

            let mut c2_str = String::new();
            let c2 = compile(&set, &options);
            match &c2 {
                Ok(c) => c.write_canonical(&mut c2_str),
                Err(e) => e.write_canonical(&mut c2_str),
            }
            assert_eq!(
                c1_str,
                c2_str,
                "compile must be deterministic for {}",
                entry.display()
            );

            match (&expected, c1) {
                (ExpectedOutcome::Clean { expected_templates }, Ok(compiled)) => {
                    assert_eq!(
                        compiled.modules().len(),
                        expected_templates.len(),
                        "compiled module count mismatch for {}",
                        entry.display()
                    );
                    for expected_name in *expected_templates {
                        let module = compiled
                            .modules()
                            .iter()
                            .find(|m| m.name().as_str() == *expected_name);
                        assert!(
                            module.is_some(),
                            "missing expected module {expected_name} in {}",
                            entry.display()
                        );
                        assert!(
                            !module.unwrap().code().is_empty(),
                            "module code must not be empty for {expected_name} in {}",
                            entry.display()
                        );
                    }
                }
                (
                    ExpectedOutcome::Error {
                        kind,
                        message,
                        directive,
                    },
                    Err(errors),
                ) => {
                    assert_eq!(
                        errors.errors().len(),
                        1,
                        "expected exactly 1 compile error for {}, got {:?}",
                        entry.display(),
                        errors.errors()
                    );
                    let err = &errors.errors()[0];
                    assert_eq!(
                        err.kind(),
                        *kind,
                        "diagnostic kind mismatch for {}",
                        entry.display()
                    );
                    assert_eq!(
                        err.message(),
                        *message,
                        "diagnostic message mismatch for {}",
                        entry.display()
                    );
                    assert_eq!(
                        err.directive(),
                        *directive,
                        "directive mismatch for {}",
                        entry.display()
                    );
                }
                (ExpectedOutcome::Clean { .. }, Err(e)) => {
                    unreachable!(
                        "expected clean compile for {}, got Err({e:?})",
                        entry.display()
                    );
                }
                (ExpectedOutcome::Error { .. }, Ok(_)) => {
                    unreachable!(
                        "expected error compile for {}, got Ok(compiled)",
                        entry.display()
                    );
                }
            }
        }
    }
}
