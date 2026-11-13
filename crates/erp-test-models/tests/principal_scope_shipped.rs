// SPDX-License-Identifier: MIT OR Apache-2.0
//! Company-bearing rules of shipped models and scoped elevation (and A6, slice
//! ), on registries composed from the shipped addons (no database): a lift
//! of a restriction reading `ir.job`'s `run_as_company` is refused, and the (model, operation)
//! pairs of the shipped `ir.access.csv` rows whose company confinement is only in permission
//! rules are pinned. A separate binary from `principal_policy.rs`: linking the base addon
//! there would clash with that fixture's own `res.company`.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

use std::collections::BTreeMap;
use std::sync::Arc;

use erp_fields::{Error, ErrorKind};
use erp_orm::{
    AccessPolicy, ElevationScope, GroupId, Operation, OperationSet, QualifiedXmlId, ScopeItem,
};
use erp_security::company_access::generate;
use erp_security::rows::{GroupRow, PolicyRows, RuleId, RuleRow};
use erp_security::snapshot::Compiler;

/// `base.group_user`.
const USER: i64 = 1;

fn gid(n: i64) -> GroupId {
    GroupId::new(n).unwrap()
}

fn xml(text: &str) -> QualifiedXmlId {
    QualifiedXmlId::parse(text).unwrap()
}

/// One rule row named by its external id: `(id, xmlid, model, operation, group, domain)`.
fn rule(
    id: i64,
    xmlid: &str,
    model: &str,
    ops: &str,
    group: Option<i64>,
    domain: Option<&str>,
) -> RuleRow {
    let mut row = RuleRow::new(RuleId::new(id).unwrap(), xmlid, model, ops)
        .unwrap()
        .with_xmlid(xml(xmlid));
    if let Some(group) = group {
        row = row.with_group(gid(group));
    }
    if let Some(domain) = domain {
        row = row.with_domain(domain);
    }
    row
}

fn scope(items: impl IntoIterator<Item = ScopeItem>) -> ElevationScope {
    ElevationScope::new(items).unwrap()
}

#[track_caller]
fn assert_error(error: &Error, kind: ErrorKind, message: &str) {
    assert_eq!(error.kind(), kind, "{error:?}");
    assert_eq!(error.message(), message);
}

/// restriction reading `run_as_company`, a many2one to `res.company` not named `company_id`, is
/// refused as a lift with the rule and the reason.
#[test]
fn a_restriction_on_the_job_company_cannot_be_lifted() {
    const JOB_RULE: &str = "prin_scope.rule_job_company";
    let registry = Arc::new(
        erp_registry::RegistryBuilder::new([&erp_addon_base::ADDON])
            .build()
            .unwrap(),
    );
    let rows = PolicyRows::builder(1)
        .group(
            GroupRow::new(gid(USER), "Internal User")
                .unwrap()
                .with_xmlid(xml("base.group_user")),
        )
        .rule(rule(
            1,
            JOB_RULE,
            "ir.job",
            "r",
            None,
            Some("[('run_as_company', 'in', [1])]"),
        ))
        .build()
        .unwrap();
    let policy = Compiler::new(Arc::clone(&registry)).compile(&rows).unwrap();
    assert!(policy.report().rule_errors().is_empty());
    let job = registry.model_id("ir.job").unwrap();
    let item = ScopeItem::new(job, OperationSet::of([Operation::Read]))
        .unwrap()
        .lift([xml(JOB_RULE)])
        .unwrap();
    let error = policy.check_scope(&scope([item])).unwrap_err();
    assert_error(
        &error,
        ErrorKind::Programming,
        &format!(
            "cannot lift restriction rule '{JOB_RULE}' ({JOB_RULE}) on ir.job: it bears the \
             company (it reads ir.job.run_as_company, a relation to res.company), and a scope \
             never lifts a company restriction"
        ),
    );
}

/// The `ir.access.csv` files of the addons linked in `registry`, as the installer reads them: for
/// every linked addon, the CSV files under `security/` its manifest lists in `data`. Every
/// linked addon has a directory under `addons/` except `base_data`, the addon of
/// `ir.model.data` (`crates/erp-data/src/model_data.rs`), which ships no files.
fn shipped_access_csvs(registry: &erp_registry::Registry) -> Vec<(&'static str, String)> {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../addons");
    let mut out = Vec::new();
    for addon in registry.addons() {
        if addon.name == erp_data::model_data::ADDON.name {
            continue;
        }
        let dir = root.join(addon.name);
        let manifest = std::fs::read_to_string(dir.join("manifest.toml"))
            .unwrap_or_else(|e| panic!("the manifest of linked addon '{}': {e}", addon.name));
        let manifest = erp_modules::Manifest::from_toml(addon.name, &manifest).unwrap();
        for file in manifest.data() {
            if file.starts_with("security/") && file.ends_with(".csv") {
                let csv = std::fs::read_to_string(dir.join(file))
                    .unwrap_or_else(|e| panic!("{}/{file}: {e}", addon.name));
                out.push((addon.name, csv));
            }
        }
    }
    out
}

/// The model each `ir.model` external id names, as the installer writes them: `<addon>.model_<name
/// with dots as underscores>`, the addon being the one whose declaration created the model (the
/// oldest declaration under the model's own name in its MRO), never an addon that only extends
/// it. Two models with one external id are a fixture error.
fn model_xmlids(registry: &erp_registry::Registry) -> BTreeMap<String, &'static str> {
    let mut out = BTreeMap::new();
    for model in registry.models() {
        let addon = model
            .mro
            .iter()
            .rev()
            .find(|d| d.name == model.name)
            .map(|d| d.addon)
            .unwrap_or_else(|| panic!("{} has no declaration under its name", model.name));
        let xmlid = format!("{addon}.model_{}", model.name.replace('.', "_"));
        if let Some(other) = out.insert(xmlid.clone(), model.name) {
            panic!("{xmlid} names both {other} and {}", model.name);
        }
    }
    out
}

/// The fields of one CSV record (RFC 4180: double quotes around a field that has commas).
fn csv_fields(line: &str) -> Vec<String> {
    let mut fields = Vec::new();
    let mut current = String::new();
    let mut quoted = false;
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        match (c, quoted) {
            ('"', true) if chars.peek() == Some(&'"') => {
                chars.next();
                current.push('"');
            }
            ('"', _) => quoted = !quoted,
            (',', false) => fields.push(std::mem::take(&mut current)),
            (other, _) => current.push(other),
        }
    }
    fields.push(current);
    fields
}

/// The (model, operation) pairs of the shipped rules that a scope cannot name because their
/// company confinement is only in permission rules (A6, the fail-closed rule of this
/// slice, accepted), measured with the compiled policy of the shipped rows
/// of base, mail, uom and product plus the generated company restrictions. Every row's model is
/// resolved by its `ir.model` external id, and a row naming no model of the registry fails the
/// test, so no shipped pair drops out of the pin. Only the generated company restriction keeps a
/// confinement: every pair whose model has one (`res.currency.rate`, `ir.dated.parameter`: their
/// permissions bear the company, their generated restriction stays) is admitted.
#[test]
fn shipped_pairs_whose_company_confinement_is_only_in_permissions() {
    let registry = Arc::new(
        erp_registry::RegistryBuilder::new([
            &erp_addon_base::ADDON,
            &erp_data::model_data::ADDON,
            &erp_addon_mail::ADDON,
            &erp_addon_uom::ADDON,
            &erp_addon_product::ADDON,
        ])
        // the models the installer adds to every registry (`ModuleSystem`'s composition in
        // `crates/erp-modules/src/system.rs`): addon base's module metadata and `ir.model.data`
        .with_decls(
            erp_modules::base_models::DECLS
                .iter()
                .copied()
                .chain([&erp_data::IR_MODEL_DATA]),
        )
        .build()
        .unwrap(),
    );
    let models = model_xmlids(&registry);
    let csvs = shipped_access_csvs(&registry);
    let mut addons: Vec<&str> = csvs.iter().map(|(addon, _)| *addon).collect();
    addons.sort_unstable();
    assert_eq!(
        addons,
        ["base", "mail", "product", "uom"],
        "the linked addons' CSVs"
    );
    let mut groups: BTreeMap<String, i64> = BTreeMap::new();
    let mut rules = Vec::new();
    let mut unknown = Vec::new();
    let mut next = 1;
    for (addon, csv) in &csvs {
        for line in csv.lines().skip(1).filter(|l| !l.trim().is_empty()) {
            let f = csv_fields(line);
            let [id, _name, model, group, ops, domain] = f.as_slice() else {
                panic!("a row of six columns: {line}");
            };
            // a bare external id is the CSV's own addon's (as the installer resolves it)
            let xmlid = if model.contains('.') {
                model.clone()
            } else {
                format!("{addon}.{model}")
            };
            let Some(&name) = models.get(&xmlid) else {
                unknown.push(format!("{addon}.{id} ({xmlid})"));
                continue;
            };
            let group = (!group.is_empty()).then(|| {
                let n = i64::try_from(groups.len()).unwrap() + 1;
                *groups.entry(group.clone()).or_insert(n)
            });
            let domain = (!domain.is_empty()).then_some(domain.as_str());
            rules.push(rule(
                next,
                &format!("{addon}.{id}"),
                name,
                ops,
                group,
                domain,
            ));
            next += 1;
        }
    }
    let mut builder = PolicyRows::builder(1);
    for (xmlid, n) in &groups {
        builder = builder.group(
            GroupRow::new(gid(*n), xmlid)
                .unwrap()
                .with_xmlid(xml(xmlid)),
        );
    }
    for row in rules {
        builder = builder.rule(row);
    }
    for g in generate(&registry).unwrap() {
        builder = builder.rule(
            RuleRow::new(RuleId::new(next).unwrap(), g.name(), g.model_name(), "crud")
                .unwrap()
                .with_domain(g.text())
                .with_generated(),
        );
        next += 1;
    }
    let policy = Compiler::new(Arc::clone(&registry))
        .compile(&builder.build().unwrap())
        .unwrap();
    let errors: Vec<String> = policy
        .report()
        .rule_errors()
        .iter()
        .map(|issue| format!("{issue:?}"))
        .collect();
    assert!(errors.is_empty(), "{errors:#?}");
    let mut refused = Vec::new();
    for model in registry.models() {
        for op in Operation::ALL {
            let item = ScopeItem::new(model.id, OperationSet::of([op])).unwrap();
            match policy.check_scope(&scope([item])) {
                Ok(()) => {}
                Err(error) if error.kind() == ErrorKind::Programming => {
                    refused.push(error.message().to_owned());
                }
                Err(error) => panic!("{} {op}: {error:?}", model.name),
            }
        }
    }
    // every shipped row names a model of this registry: none drops out of the pin
    assert!(
        unknown.is_empty(),
        "rows whose model external id names no model of this registry: {unknown:?}"
    );
    eprintln!("refused pairs: {refused:#?}");
    let pair = |op: &str, model: &str, rules: &str| {
        format!(
            "the scope names {op} on '{model}', which keeps no generated company restriction and \
             whose permission rules ({rules}) may bear the company: scoped elevation cannot open \
             it"
        )
    };
    let company_all = "base.access_company_system";
    let company_read = "base.access_company_system, base.access_company_internal";
    let mut want = vec![
        pair("read", "res.company", company_read),
        pair("write", "res.company", company_all),
        pair("create", "res.company", company_all),
        pair("unlink", "res.company", company_all),
        pair(
            "read",
            "mail.message",
            "mail.access_mail_message_user, mail.access_mail_message_portal, \
             mail.access_mail_message_public",
        ),
        pair(
            "read",
            "mail.activity",
            "mail.access_mail_activity_user_read",
        ),
        pair(
            "write",
            "mail.activity",
            "mail.access_mail_activity_user_doc_write",
        ),
        pair(
            "unlink",
            "mail.activity",
            "mail.access_mail_activity_user_doc_unlink",
        ),
    ];
    want.sort();
    refused.sort();
    assert_eq!(refused, want);
}
