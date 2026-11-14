// SPDX-License-Identifier: MIT OR Apache-2.0
//! Hand-written CRUD acceptance manifest and runner for the multi-user HTTP suite.
//! Expected outcomes are authored here from the product, uom and security
//! contracts; they are never read from `ir.access` at run time. Success bodies are never printed:
//! diagnostics carry the HTTP status, error name and error message only.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "integration assertions"
)]

use crate::multiuser_fixture::{MultiuserFixture, Subject};
use crate::native_http::{self, Reply};
use serde_json::{Value, json};
use std::{
    cell::OnceCell,
    collections::{BTreeMap, BTreeSet},
};

/// The five RPC operations of every cell.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Op {
    Search,
    Read,
    Create,
    Write,
    Unlink,
}

/// Every operation, in the order the runner exercises them (non-mutating first).
pub const OPS: [Op; 5] = [Op::Search, Op::Read, Op::Create, Op::Write, Op::Unlink];

impl Op {
    /// The JSON-2 method name.
    pub fn method(self) -> &'static str {
        match self {
            Op::Search => "search",
            Op::Read => "read",
            Op::Create => "create",
            Op::Write => "write",
            Op::Unlink => "unlink",
        }
    }

    /// The verb of the model-level access message (`explain::model_verb`).
    fn verb(self) -> &'static str {
        match self {
            Op::Search | Op::Read => "access",
            Op::Create => "create",
            Op::Write => "modify",
            Op::Unlink => "delete",
        }
    }

    /// The operation name of the record-level access message (`explain::operation_name`).
    fn record_name(self) -> &'static str {
        match self {
            Op::Search | Op::Read => "read",
            Op::Create => "create",
            Op::Write => "write",
            Op::Unlink => "delete",
        }
    }
}

/// The four test subjects.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Who {
    Admin,
    A,
    B,
    Anon,
}

impl Who {
    fn subject(self) -> Subject {
        match self {
            Who::Admin => Subject::Admin,
            Who::A => Subject::A,
            Who::B => Subject::B,
            Who::Anon => Subject::Anonymous,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Who::Admin => "admin",
            Who::A => "A",
            Who::B => "B",
            Who::Anon => "anonymous",
        }
    }
}

/// Which seeded records a search must return.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Visible {
    /// Every seeded record of the model.
    All,
    /// The principal's own company record and the shared (no company) record, and nothing else.
    OwnAndShared,
}

/// The hand-written expected outcome of one cell.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Expect {
    /// HTTP 200.
    Ok,
    /// A search that succeeds and returns exactly the named seeded set.
    Visible(Visible),
    /// A read of three records: own company and shared succeed, the other company is denied.
    ReadByCompany,
    /// 403 AccessError carrying the model-level message.
    ModelDenied,
    /// 403 AccessError carrying the record-level message.
    RecordDenied,
    /// 401 for an anonymous call.
    Unauthenticated,
    /// Not yet written; names the board task that will write it.
    Deferred(&'static str),
}

/// How a model's records relate to companies; decides what is seeded.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Scope {
    /// One record, visible to everyone who may read the model.
    Global,
    /// One record in company A, one in company B and one with no company.
    Company,
}

/// The contract shape of a model for ordinary internal users.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Policy {
    /// Internal users read and search; only the named managers create, write and unlink.
    InternalReadOnly { groups: &'static [&'static str] },
    /// As `InternalReadOnly`, plus a company rule on the records. `company_hint` says whether a
    /// record-level denial ends with the multi-company hint. `false` encodes a
    /// known defect, not the contract: through a parent rule (product.product) the access evidence
    /// carries no company, so the hint is missing (MU-C02, task,
    /// which flips it to `true`).
    CompanyScoped {
        groups: &'static [&'static str],
        company_hint: bool,
    },
    /// Matrix not written yet.
    Deferred(&'static str),
}

/// One installed model of the manifest.
pub struct ModelCase {
    pub model: &'static str,
    pub addon: &'static str,
    /// The model description shown in access messages (empty while deferred).
    pub description: &'static str,
    pub transient: bool,
    /// Spec or ADR ids that justify the expectations; never `ir.access` rows.
    pub basis: &'static [&'static str],
    pub scope: Scope,
    pub policy: Policy,
}

impl ModelCase {
    fn groups(&self) -> &'static [&'static str] {
        match self.policy {
            Policy::InternalReadOnly { groups } | Policy::CompanyScoped { groups, .. } => groups,
            Policy::Deferred(_) => &[],
        }
    }

    fn is_deferred(&self) -> bool {
        matches!(self.policy, Policy::Deferred(_))
    }
}

const TASK_RES: &str = "a later release";
const TASK_IR: &str = "a later release";
const TASK_MAIL: &str = "a later release";
const TASK_SERVICE: &str = "a later release";
const TASKS: [&str; 4] = [TASK_RES, TASK_IR, TASK_MAIL, TASK_SERVICE];

// The groups an access denial names: the product manager group, and Settings, which implies it
// (addons/product/security/product_security.xml, implied_by_ids), in the order of the message
// (focused run 3). The manager
// group belongs to the Products privilege, so its full name carries that prefix.
const PRODUCT_MANAGER: &[&str] = &[
    "Products / Product Administrator",
    "Settings Administration",
];
const UOM_MANAGERS: &[&str] = &[
    "Products / Product Administrator",
    "Settings Administration",
];
// Spec cases behind the expectations: PRODUCT-099 (managers write, users read), PRODUCT-100 (company
// isolation), SEC-001/-003 (no grant, grants OR), SEC-008/-009 (model and record message), SEC-068
// (direct company restriction), SEC-069 (company through the owner record), SEC-074 (a generated
// restriction never adds rows), UOM-016 (units: users read, managers write).
const ATTRIBUTE_BASIS: &[&str] = &["", "", "", ""];
const COMPANY_BASIS: &[&str] = &["", "", "", "", "", ""];
const VARIANT_BASIS: &[&str] = &["", "", "", "", ""];
const UOM_BASIS: &[&str] = &["", ""];

const fn deferred(
    model: &'static str,
    addon: &'static str,
    transient: bool,
    task: &'static str,
) -> ModelCase {
    ModelCase {
        model,
        addon,
        description: "",
        transient,
        basis: &[],
        scope: Scope::Global,
        policy: Policy::Deferred(task),
    }
}

const fn res(model: &'static str) -> ModelCase {
    deferred(model, "base", false, TASK_RES)
}

const fn ir(model: &'static str) -> ModelCase {
    deferred(model, "base", false, TASK_IR)
}

const fn mail(model: &'static str) -> ModelCase {
    deferred(model, "mail", false, TASK_MAIL)
}

const fn service(model: &'static str, addon: &'static str) -> ModelCase {
    deferred(model, addon, false, TASK_SERVICE)
}

const fn transient(model: &'static str) -> ModelCase {
    deferred(model, "base", true, TASK_SERVICE)
}

const fn read_only(
    model: &'static str,
    addon: &'static str,
    description: &'static str,
    basis: &'static [&'static str],
    groups: &'static [&'static str],
) -> ModelCase {
    ModelCase {
        model,
        addon,
        description,
        transient: false,
        basis,
        scope: Scope::Global,
        policy: Policy::InternalReadOnly { groups },
    }
}

const fn company(
    model: &'static str,
    description: &'static str,
    basis: &'static [&'static str],
) -> ModelCase {
    ModelCase {
        model,
        addon: "product",
        description,
        transient: false,
        basis,
        scope: Scope::Company,
        policy: Policy::CompanyScoped {
            groups: PRODUCT_MANAGER,
            company_hint: true,
        },
    }
}

/// A product model whose company comes from its parent record (parent form).
const fn company_via_parent(
    model: &'static str,
    description: &'static str,
    basis: &'static [&'static str],
) -> ModelCase {
    ModelCase {
        policy: Policy::CompanyScoped {
            groups: PRODUCT_MANAGER,
            company_hint: false,
        },
        ..company(model, description, basis)
    }
}

/// Models that exist only as mixins or service namespaces: allowed in or out of `ir.model`.
pub const ABSTRACT: &[(&str, &str)] = &[
    ("base", "the abstract root every model inherits from"),
    ("ir.actions.base", "abstract base of the action models"),
    ("mail.activity.mixin", "abstract activity mixin"),
    ("mail.store", "abstract store-fetch service, no table"),
    ("mail.thread", "abstract chatter mixin"),
];

/// Every installed concrete or transient model, written by hand.
pub const MANIFEST: &[ModelCase] = &[
    // Product and uom family: full matrix. Child models are seeded under a no-company parent, so
    // their visibility is "all"; parent-company visibility belongs to the BOUNDARIES task.
    read_only(
        "product.attribute",
        "product",
        "Product Attribute",
        ATTRIBUTE_BASIS,
        PRODUCT_MANAGER,
    ),
    read_only(
        "product.attribute.value",
        "product",
        "Attribute Value",
        ATTRIBUTE_BASIS,
        PRODUCT_MANAGER,
    ),
    read_only(
        "product.combo.item",
        "product",
        "Product Combo Item",
        ATTRIBUTE_BASIS,
        PRODUCT_MANAGER,
    ),
    read_only(
        "product.pricelist.item",
        "product",
        "Pricelist Rule",
        ATTRIBUTE_BASIS,
        PRODUCT_MANAGER,
    ),
    read_only(
        "product.template.attribute.line",
        "product",
        "Product Template Attribute Line",
        ATTRIBUTE_BASIS,
        PRODUCT_MANAGER,
    ),
    read_only(
        "product.template.attribute.value",
        "product",
        "Product Template Attribute Value",
        ATTRIBUTE_BASIS,
        PRODUCT_MANAGER,
    ),
    company("product.template", "Product", COMPANY_BASIS),
    company("product.category", "Product Category", COMPANY_BASIS),
    company("product.pricelist", "Pricelist", COMPANY_BASIS),
    company("product.supplierinfo", "Supplier Price List", COMPANY_BASIS),
    company("product.combo", "Product Combo", COMPANY_BASIS),
    company(
        "product.uom",
        "Product unit with its own barcode",
        COMPANY_BASIS,
    ),
    company_via_parent("product.product", "Product Variant", VARIANT_BASIS),
    read_only("uom.uom", "uom", "Unit of Measure", UOM_BASIS, UOM_MANAGERS),
    // Users read only the custom values they created (an access-row domain on create_uid); no
    // policy of this matrix seeds per-user records yet.
    deferred(
        "product.attribute.custom.value",
        "product",
        false,
        TASK_SERVICE,
    ),
    // res.*
    res("res.company"),
    res("res.country"),
    res("res.country.group"),
    res("res.country.state"),
    res("res.currency"),
    res("res.currency.rate"),
    res("res.currency.rate.type"),
    res("res.groups"),
    res("res.groups.privilege"),
    res("res.lang"),
    res("res.partner"),
    res("res.partner.category"),
    res("res.partner.industry"),
    res("res.users"),
    res("res.users.settings"),
    // ir.* with access rows
    ir("decimal.precision"),
    ir("ir.access"),
    ir("ir.actions.act_url"),
    ir("ir.actions.act_window"),
    ir("ir.actions.act_window.view"),
    ir("ir.actions.actions"),
    ir("ir.actions.client"),
    ir("ir.actions.server"),
    ir("ir.approval.policy"),
    ir("ir.attachment"),
    ir("ir.config.setting"),
    ir("ir.config.setting.audit"),
    ir("ir.dated.parameter"),
    ir("ir.default"),
    ir("ir.filters"),
    ir("ir.job"),
    ir("ir.job.run"),
    ir("ir.model"),
    ir("ir.model.data"),
    ir("ir.model.fields"),
    ir("ir.model.fields.selection"),
    ir("ir.model.relation"),
    ir("ir.config_parameter"),
    ir("ir.module.module"),
    ir("ir.sequence"),
    ir("ir.sequence.date_range"),
    ir("ir.ui.menu"),
    ir("ir.ui.view"),
    // mail.*
    mail("mail.activity"),
    mail("mail.activity.type"),
    mail("mail.followers"),
    mail("mail.message"),
    mail("mail.message.subtype"),
    mail("mail.scheduled.message"),
    // models without an access row, and transients
    service("ir.capability", "base"),
    service("ir.config.setting.scope", "base"),
    service("ir.config.setting.value", "base"),
    service("ir.cron", "base"),
    service("ir.cron.trigger", "base"),
    service("ir.module.category", "base"),
    service("ir.module.module.dependency", "base"),
    service("ir.module.module.exclusion", "base"),
    service("ir.module.post_commit", "base"),
    service("res.users.apikeys", "base"),
    service("res.users.log", "base"),
    service("product.barcode.claim", "product"),
    service("product.barcode.scope", "product"),
    service("uom.reference.fence", "uom"),
    service("uom.reference.use", "uom"),
    transient("change.password.own"),
    transient("change.password.user"),
    transient("change.password.wizard"),
    transient("res.users.identitycheck"),
];

/// The expected outcome of one cell, from the contract.
pub fn expect(case: &ModelCase, who: Who, op: Op) -> Expect {
    if who == Who::Anon {
        return Expect::Unauthenticated;
    }
    match case.policy {
        Policy::Deferred(task) => Expect::Deferred(task),
        Policy::InternalReadOnly { .. } => match (who, op) {
            (_, Op::Search) => Expect::Visible(Visible::All),
            (Who::Admin, _) | (_, Op::Read) => Expect::Ok,
            _ => Expect::ModelDenied,
        },
        Policy::CompanyScoped { .. } => match (who, op) {
            (Who::Admin, Op::Search) => Expect::Visible(Visible::All),
            (Who::Admin, _) => Expect::Ok,
            (_, Op::Search) => Expect::Visible(Visible::OwnAndShared),
            (_, Op::Read) => Expect::ReadByCompany,
            _ => Expect::ModelDenied,
        },
    }
}

/// Structural problems of the manifest itself (no server needed).
pub fn manifest_problems() -> Vec<String> {
    let mut problems = Vec::new();
    let mut seen = BTreeSet::new();
    for case in MANIFEST {
        if !seen.insert(case.model) {
            problems.push(format!("{} appears twice in MANIFEST", case.model));
        }
        if ABSTRACT.iter().any(|(name, _)| *name == case.model) {
            problems.push(format!("{} is in both MANIFEST and ABSTRACT", case.model));
        }
        if case.addon.is_empty() {
            problems.push(format!("{} has no addon", case.model));
        }
        match case.policy {
            Policy::Deferred(task) => {
                if !TASKS.contains(&task) {
                    problems.push(format!("{} defers to unknown task {task}", case.model));
                }
            }
            Policy::InternalReadOnly { groups } | Policy::CompanyScoped { groups, .. } => {
                if case.basis.is_empty() || case.description.is_empty() || groups.is_empty() {
                    problems.push(format!(
                        "{} lacks basis, description or granting groups",
                        case.model
                    ));
                }
                let company_policy = matches!(case.policy, Policy::CompanyScoped { .. });
                if company_policy != (case.scope == Scope::Company) {
                    problems.push(format!("{}: scope and policy disagree", case.model));
                }
            }
        }
    }
    for (name, reason) in ABSTRACT {
        if reason.is_empty() {
            problems.push(format!("abstract {name} has no reason"));
        }
    }
    problems
}

/// Differences between the installed `ir.model` rows and the manifest, empty when they agree.
pub fn manifest_diff(installed: &BTreeMap<String, bool>) -> Vec<String> {
    let abstract_names: BTreeSet<&str> = ABSTRACT.iter().map(|(name, _)| *name).collect();
    let manifest: BTreeMap<&str, bool> = MANIFEST.iter().map(|c| (c.model, c.transient)).collect();
    let missing: Vec<&str> = manifest
        .keys()
        .copied()
        .filter(|name| !installed.contains_key(*name))
        .collect();
    let extra: Vec<&str> = installed
        .keys()
        .map(String::as_str)
        .filter(|name| !manifest.contains_key(name) && !abstract_names.contains(name))
        .collect();
    let mut diff = Vec::new();
    if !missing.is_empty() {
        diff.push(format!("in the manifest but not installed: {missing:?}"));
    }
    if !extra.is_empty() {
        diff.push(format!("installed but not in the manifest: {extra:?}"));
    }
    for (name, transient) in &manifest {
        if let Some(actual) = installed.get(*name)
            && actual != transient
        {
            diff.push(format!(
                "{name}: manifest transient={transient}, installed transient={actual}"
            ));
        }
    }
    diff
}

/// A response reduced to what diagnostics may show. The body of a success stays private.
struct Outcome {
    status: u16,
    name: String,
    message: String,
    value: Option<Value>,
}

impl Outcome {
    fn brief(&self) -> String {
        if self.status == 200 {
            "HTTP 200".to_owned()
        } else {
            format!("HTTP {} {}: {}", self.status, self.name, self.message)
        }
    }

    fn ids(&self) -> Option<BTreeSet<i64>> {
        self.value
            .as_ref()?
            .as_array()?
            .iter()
            .map(Value::as_i64)
            .collect()
    }
}

fn classify(reply: &Reply) -> Outcome {
    let parsed: Option<Value> = serde_json::from_str(&reply.body).ok();
    if reply.status == 200 {
        return Outcome {
            status: 200,
            name: String::new(),
            message: String::new(),
            value: parsed,
        };
    }
    let text = |key: &str| {
        parsed
            .as_ref()
            .and_then(|body| body.get(key))
            .and_then(Value::as_str)
            .unwrap_or("<missing>")
            .to_owned()
    };
    Outcome {
        status: reply.status,
        name: text("name"),
        message: text("message"),
        value: None,
    }
}

/// A record id no fixture creates (int4 range, which every id column accepts).
const NO_SUCH_ID: i64 = 2_147_483_647;
const ACCESS_ERROR: &str = "erp.exceptions.AccessError";
const UNAUTHORIZED: &str = "werkzeug.exceptions.Unauthorized";
const UNAUTHENTICATED_MESSAGE: &str =
    "User not authenticated, use an API Key with a Bearer Authorization header.";
/// The SEC-008 model-level text (`crates/erp-security/src/explain.rs`, `model_message`), built from
/// the contract's granting groups.
fn model_message(case: &ModelCase, op: Op) -> String {
    let groups: Vec<String> = case.groups().iter().map(|g| format!("\t- {g}")).collect();
    format!(
        "Your access rights do not let you {} '{}' ({}) records.\n\nGroups that may do this:\n{}\n\nAsk an administrator if you need this access.",
        op.verb(),
        case.description,
        case.model,
        groups.join("\n")
    )
}

/// The company hint at the end of a record-level denial (`crates/erp-orm/src/access/explain.rs`,
/// `CompanyHint::text`).
enum Hint<'h> {
    /// The denied record's company is not one of the subject's companies.
    OutsideYourCompanies,
    /// The subject could switch to this company.
    Switch(&'h str),
    /// No hint (the blamed rule does not name the company).
    None,
}

/// The SEC-009 record-level text (non-debug) for subject `name` (`id`).
fn record_message(case: &ModelCase, op: Op, name: &str, id: i64, hint: &Hint<'_>) -> String {
    let hint = match hint {
        Hint::None => String::new(),
        Hint::OutsideYourCompanies => {
            "\n\nThe record belongs to a company outside your allowed companies.".to_owned()
        }
        Hint::Switch(company) => {
            format!("\n\nTo open the record, switch to its company: {company}.")
        }
    };
    format!(
        "Access rules protect some of these records.\n\n{name} (id={id}) has no '{}' access to:\n- {} ({})\n\nAsk an administrator if you need access to these records.{hint}",
        op.record_name(),
        case.description,
        case.model
    )
}

/// Compare one outcome with the expected one; `record` is the subject and hint a record-level
/// denial must name.
fn judge(
    case: &ModelCase,
    op: Op,
    expect: Expect,
    out: &Outcome,
    record: Option<(&str, i64, Hint<'_>)>,
) -> Option<String> {
    let denied = out.status == 403 && out.name == ACCESS_ERROR;
    let fine = match expect {
        Expect::Ok | Expect::Visible(_) | Expect::ReadByCompany => out.status == 200,
        Expect::ModelDenied => denied && out.message == model_message(case, op),
        Expect::RecordDenied => match &record {
            Some((name, id, hint)) => {
                let hint = match case.policy {
                    Policy::CompanyScoped {
                        company_hint: false,
                        ..
                    } => &Hint::None,
                    Policy::CompanyScoped { .. }
                    | Policy::InternalReadOnly { .. }
                    | Policy::Deferred(_) => hint,
                };
                denied && out.message == record_message(case, op, name, *id, hint)
            }
            None => false,
        },
        Expect::Unauthenticated => {
            out.status == 401 && out.name == UNAUTHORIZED && out.message == UNAUTHENTICATED_MESSAGE
        }
        // a deferred cell is skipped before it is judged; reaching here is a runner defect
        Expect::Deferred(task) => {
            return Some(format!("a cell deferred to {task} reached the judge"));
        }
    };
    (!fine).then(|| format!("expected {expect:?}, got {}", out.brief()))
}

/// The one simple field a write changes, and how its value is compared.
enum Marker {
    Char(&'static str),
    Int(&'static str),
    Bool(&'static str),
    Float(&'static str),
}

impl Marker {
    fn of(model: &str) -> Self {
        match model {
            "product.category" | "uom.uom" => Marker::Char("name"),
            "product.product" => Marker::Char("default_code"),
            "product.uom" => Marker::Char("barcode"),
            "product.pricelist.item" => Marker::Float("min_quantity"),
            "product.combo.item" => Marker::Float("extra_price"),
            "product.template.attribute.value" => Marker::Float("price_extra"),
            "product.template.attribute.line" => Marker::Bool("active"),
            "product.attribute"
            | "product.attribute.value"
            | "product.combo"
            | "product.pricelist"
            | "product.supplierinfo"
            | "product.template" => Marker::Int("sequence"),
            other => panic!("no write marker is chosen for {other}"),
        }
    }

    fn field(&self) -> &'static str {
        match self {
            Marker::Char(f) | Marker::Int(f) | Marker::Bool(f) | Marker::Float(f) => f,
        }
    }

    fn new_value(&self, tag: &str) -> Value {
        match self {
            Marker::Char(_) => json!(format!("{tag} written")),
            Marker::Int(_) => json!(77),
            Marker::Bool(_) => json!(false),
            Marker::Float(_) => json!(12.5),
        }
    }

    fn same(&self, left: &Value, right: &Value) -> bool {
        match self {
            Marker::Float(_) => match (number(left), number(right)) {
                (Some(l), Some(r)) => (l - r).abs() < 1e-9,
                _ => false,
            },
            _ => left == right,
        }
    }
}

fn number(value: &Value) -> Option<f64> {
    value
        .as_f64()
        .or_else(|| value.as_str().and_then(|s| s.parse().ok()))
}

/// Seeded record ids of one model.
struct Seeds {
    a: Option<i64>,
    b: Option<i64>,
    shared: i64,
}

impl Seeds {
    fn all(&self) -> Vec<i64> {
        [self.a, self.b, Some(self.shared)]
            .into_iter()
            .flatten()
            .collect()
    }

    fn own(&self, who: Who) -> Option<i64> {
        match who {
            Who::A => self.a,
            Who::B => self.b,
            Who::Admin | Who::Anon => None,
        }
    }

    fn other(&self, who: Who) -> Option<i64> {
        match who {
            Who::A => self.b,
            Who::B => self.a,
            Who::Admin | Who::Anon => None,
        }
    }

    fn visible(&self, who: Who, visible: Visible) -> BTreeSet<i64> {
        match visible {
            Visible::All => self.all().into_iter().collect(),
            Visible::OwnAndShared => self
                .own(who)
                .into_iter()
                .chain(std::iter::once(self.shared))
                .collect(),
        }
    }
}

/// The company a seeded record belongs to.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Co {
    A,
    B,
    Shared,
}

struct Base {
    uom: i64,
    currency: i64,
    partner: i64,
}

/// Runner over one started multi-user fixture.
pub struct Crud<'a, 'f> {
    fx: &'a MultiuserFixture<'f>,
    base: OnceCell<Base>,
}

fn ids_of(value: &Value) -> Option<Vec<i64>> {
    value.as_array()?.iter().map(Value::as_i64).collect()
}

impl<'a, 'f> Crud<'a, 'f> {
    pub fn new(fx: &'a MultiuserFixture<'f>) -> Self {
        Self {
            fx,
            base: OnceCell::new(),
        }
    }

    fn company(&self, who: Subject) -> i64 {
        self.fx.user(who).company
    }

    fn all_companies(&self) -> Vec<i64> {
        vec![
            self.company(Subject::Admin),
            self.company(Subject::A),
            self.company(Subject::B),
        ]
    }

    fn allowed(&self, who: Who) -> Vec<i64> {
        match who {
            Who::Admin => self.all_companies(),
            Who::A => vec![self.company(Subject::A)],
            Who::B => vec![self.company(Subject::B)],
            Who::Anon => Vec::new(),
        }
    }

    fn co(&self, co: Co) -> Value {
        match co {
            Co::A => json!(self.company(Subject::A)),
            Co::B => json!(self.company(Subject::B)),
            Co::Shared => json!(false),
        }
    }

    fn co_of(who: Who) -> Co {
        match who {
            Who::A => Co::A,
            Who::B => Co::B,
            Who::Admin | Who::Anon => Co::Shared,
        }
    }

    /// A call as a test subject, with the explicit company context.
    fn call_in(
        &self,
        who: Who,
        allowed: &[i64],
        model: &str,
        method: &str,
        mut body: Value,
    ) -> Outcome {
        if who != Who::Anon {
            body["context"] = json!({"allowed_company_ids": allowed});
        }
        classify(&self.fx.rpc(who.subject(), model, method, body))
    }

    fn call(&self, who: Who, model: &str, method: &str, body: Value) -> Outcome {
        self.call_in(who, &self.allowed(who), model, method, body)
    }

    /// A call of the provisioning session (not a test subject), used for seeding and readback.
    fn prov_with(
        &self,
        model: &str,
        method: &str,
        mut body: Value,
        extra: &[(&str, Value)],
    ) -> Result<Value, String> {
        let mut context = json!({"allowed_company_ids": self.all_companies()});
        for (key, value) in extra {
            context[*key] = value.clone();
        }
        body["context"] = context;
        let reply = native_http::json_post(
            self.fx.native.port,
            &self.fx.native.cookies,
            &format!("/erp/json/2/{model}/{method}"),
            &body.to_string(),
        );
        let out = classify(&reply);
        match out.value {
            Some(value) if out.status == 200 => Ok(value),
            _ => Err(format!("{model}/{method}: {}", out.brief())),
        }
    }

    fn prov(&self, model: &str, method: &str, body: Value) -> Result<Value, String> {
        self.prov_with(model, method, body, &[])
    }

    /// Installed models as `(name, transient)` from `ir.model`.
    pub fn installed_models(&self) -> BTreeMap<String, bool> {
        let rows = self
            .prov(
                "ir.model",
                "search_read",
                json!({"domain": [], "fields": ["model", "transient"]}),
            )
            .unwrap_or_else(|e| panic!("reading ir.model: {e}"));
        let rows = rows.as_array().expect("ir.model rows");
        // a server-side row cap would hide models from the comparison
        let total = self.count("ir.model", json!([]));
        assert_eq!(
            i64::try_from(rows.len()).expect("row count fits i64"),
            total,
            "ir.model search_read returned fewer rows than search_count"
        );
        rows.iter()
            .map(|row| {
                (
                    row["model"]
                        .as_str()
                        .expect("ir.model.model is a string")
                        .to_owned(),
                    row["transient"]
                        .as_bool()
                        .expect("ir.model.transient is a boolean"),
                )
            })
            .collect()
    }

    fn base(&self) -> &Base {
        self.base.get_or_init(|| {
            let first = |model: &str| {
                let found = self
                    .prov(model, "search", json!({"domain": [], "limit": 1}))
                    .unwrap_or_else(|e| panic!("base seed: {e}"));
                ids_of(&found)
                    .and_then(|ids| ids.first().copied())
                    .unwrap_or_else(|| panic!("base seed: no {model} record exists"))
            };
            Base {
                uom: first("uom.uom"),
                currency: first("res.currency"),
                partner: self.create_id("res.partner", json!({"name": "crud matrix vendor"}), &[]),
            }
        })
    }

    fn create_id(&self, model: &str, vals: Value, extra: &[(&str, Value)]) -> i64 {
        let created = self
            .prov_with(model, "create", json!({"vals_list": [vals]}), extra)
            .unwrap_or_else(|e| panic!("seed {e}"));
        ids_of(&created)
            .and_then(|ids| ids.first().copied())
            .unwrap_or_else(|| panic!("seed {model}: create returned no identity"))
    }

    fn one_id(&self, model: &str, domain: Value) -> i64 {
        let found = self
            .prov(model, "search", json!({"domain": domain}))
            .unwrap_or_else(|e| panic!("seed lookup {e}"));
        match ids_of(&found).as_deref() {
            Some([id]) => *id,
            _ => panic!("seed lookup {model}: expected exactly one record"),
        }
    }

    /// An attribute with `n` values, set not to generate variants.
    fn attribute(&self, tag: &str, n: usize) -> (i64, Vec<i64>) {
        let attribute = self.create_id(
            "product.attribute",
            json!({"name": format!("{tag} attr"), "create_variant": "no_variant"}),
            &[],
        );
        let values = (0..n)
            .map(|i| {
                self.create_id(
                    "product.attribute.value",
                    json!({"name": format!("{tag} v{i}"), "attribute_id": attribute}),
                    &[],
                )
            })
            .collect();
        (attribute, values)
    }

    /// A valid create payload with its prerequisites created by the provisioning session.
    fn payload(&self, model: &str, tag: &str, co: Co) -> Value {
        let base = self.base();
        let company = self.co(co);
        let parent = format!("{tag} parent");
        let mut vals = match model {
            "product.attribute" => json!({"name": tag}),
            "product.attribute.value" => {
                let (attribute, _) = self.attribute(&parent, 0);
                json!({"name": tag, "attribute_id": attribute})
            }
            "product.category" | "product.combo" => json!({"name": tag, "company_id": company}),
            "product.combo.item" => json!({
                "combo_id": self.make("product.combo", &parent, Co::Shared),
                "product_id": self.make("product.product", &parent, Co::Shared),
            }),
            "product.pricelist" => json!({
                "name": tag, "currency_id": base.currency, "company_id": company
            }),
            "product.pricelist.item" => {
                json!({"pricelist_id": self.make("product.pricelist", &parent, Co::Shared)})
            }
            "product.product" => {
                // a template without its automatic variant, so the new variant is the only one
                let template = self.create_id(
                    "product.template",
                    self.payload("product.template", &parent, Co::Shared),
                    &[("create_product_product", json!(false))],
                );
                json!({"product_tmpl_id": template})
            }
            "product.supplierinfo" => json!({
                "partner_id": base.partner,
                "product_tmpl_id": self.make("product.template", &parent, Co::Shared),
                "currency_id": base.currency,
                "company_id": company,
            }),
            "product.template" => json!({
                "name": tag, "uom_id": base.uom, "company_id": company
            }),
            "product.template.attribute.line" => {
                let (attribute, values) = self.attribute(&parent, 1);
                json!({
                    "product_tmpl_id": self.make("product.template", &parent, Co::Shared),
                    "attribute_id": attribute,
                    "value_ids": [[6, 0, values]],
                })
            }
            "product.template.attribute.value" => {
                let (attribute, values) = self.attribute(&parent, 2);
                let template = self.make("product.template", &parent, Co::Shared);
                let line = self.create_id(
                    "product.template.attribute.line",
                    json!({
                        "product_tmpl_id": template,
                        "attribute_id": attribute,
                        "value_ids": [[6, 0, [values[0]]]],
                    }),
                    &[],
                );
                // the line materialized the first value; the payload adds the second
                json!({
                    "product_tmpl_id": template,
                    "attribute_line_id": line,
                    "product_attribute_value_id": values[1],
                    "attribute_id": attribute,
                })
            }
            "product.uom" => json!({
                "uom_id": self.make("uom.uom", &parent, Co::Shared),
                "product_id": self.make("product.product", &parent, Co::Shared),
                "barcode": tag,
                "company_id": company,
            }),
            "uom.uom" => json!({"name": tag}),
            other => panic!("no payload is written for {other}"),
        };
        // A shared record sends `null`, which JSON-2 binds to the empty value. An explicit `false`
        // was refused for product.combo.company_id (422, finding MU-C01, task;
        // other models not tried). Leaving the key out is
        // no substitute: product.uom then defaults to the active company and is not shared.
        if co == Co::Shared
            && let Some(map) = vals.as_object_mut()
            && map.contains_key("company_id")
        {
            map.insert("company_id".to_owned(), Value::Null);
        }
        vals
    }

    /// One disposable record created by the provisioning session.
    fn make(&self, model: &str, tag: &str, co: Co) -> i64 {
        match model {
            // the template creates its only variant
            "product.product" => {
                let template = self.make("product.template", tag, co);
                self.one_id(model, json!([["product_tmpl_id", "=", template]]))
            }
            // the attribute line materializes its value records
            "product.template.attribute.value" => {
                let line = self.make("product.template.attribute.line", tag, co);
                self.one_id(model, json!([["attribute_line_id", "=", line]]))
            }
            _ => self.create_id(model, self.payload(model, tag, co), &[]),
        }
    }

    fn seed(&self, case: &ModelCase) -> Seeds {
        let tag = |suffix: &str| format!("crud {} seed {suffix}", case.model);
        match case.scope {
            Scope::Global => Seeds {
                a: None,
                b: None,
                shared: self.make(case.model, &tag("global"), Co::Shared),
            },
            Scope::Company => Seeds {
                a: Some(self.make(case.model, &tag("A"), Co::A)),
                b: Some(self.make(case.model, &tag("B"), Co::B)),
                shared: self.make(case.model, &tag("shared"), Co::Shared),
            },
        }
    }

    fn count(&self, model: &str, domain: Value) -> i64 {
        self.prov_with(
            model,
            "search_count",
            json!({"domain": domain}),
            &[("active_test", json!(false))],
        )
        .unwrap_or_else(|e| panic!("readback {e}"))
        .as_i64()
        .expect("search_count is an integer")
    }

    fn marker_value(&self, model: &str, id: i64, marker: &Marker) -> Value {
        let rows = self
            .prov(
                model,
                "read",
                json!({"ids": [id], "fields": [marker.field()]}),
            )
            .unwrap_or_else(|e| panic!("readback {e}"));
        let row = rows
            .as_array()
            .and_then(|rows| rows.first())
            .unwrap_or_else(|| panic!("readback {model}: record {id} is gone"));
        row.get(marker.field())
            .unwrap_or_else(|| panic!("readback {model}: field {} absent", marker.field()))
            .clone()
    }

    /// The subject a record-level denial names, and its company hint. Only the administrator's
    /// mixed batch (limited to company A, touching a company-B record) can switch company.
    fn record_subject(&self, who: Who) -> Option<(&str, i64, Hint<'static>)> {
        let hint = match who {
            Who::Anon => return None,
            Who::Admin => Hint::Switch("Multiuser Company B"),
            Who::A | Who::B => Hint::OutsideYourCompanies,
        };
        let user = self.fx.user(who.subject());
        Some((user.login.as_str(), user.id, hint))
    }

    fn fail(case: &ModelCase, who: Who, op: Op, detail: &str) -> String {
        format!(
            "[{}] {} {} (basis {}): {detail}",
            case.model,
            who.label(),
            op.method(),
            case.basis.join(", ")
        )
    }

    fn failures(case: &ModelCase, who: Who, op: Op, details: Vec<String>) -> Vec<String> {
        details
            .iter()
            .map(|detail| Self::fail(case, who, op, detail))
            .collect()
    }

    fn search_cell(&self, case: &ModelCase, who: Who, seeds: &Seeds) -> Vec<String> {
        let expect = expect(case, who, Op::Search);
        let domain = json!([["id", "in", seeds.all()]]);
        let out = self.call(who, case.model, "search", json!({"domain": domain}));
        let mut details: Vec<String> =
            judge(case, Op::Search, expect, &out, self.record_subject(who))
                .into_iter()
                .collect();
        if let (Expect::Visible(visible), true) = (expect, details.is_empty()) {
            let wanted = seeds.visible(who, visible);
            match out.ids() {
                Some(got) if got == wanted => {}
                Some(got) => details.push(format!("search returned {got:?}, expected {wanted:?}")),
                None => details.push("search result is not a list of identities".to_owned()),
            }
        }
        Self::failures(case, who, Op::Search, details)
    }

    fn read_targets(who: Who, expect: Expect, seeds: &Seeds) -> Vec<(i64, Expect)> {
        match expect {
            Expect::ReadByCompany => seeds
                .own(who)
                .map(|id| (id, Expect::Ok))
                .into_iter()
                .chain([(seeds.shared, Expect::Ok)])
                .chain(seeds.other(who).map(|id| (id, Expect::RecordDenied)))
                .collect(),
            other => seeds.all().into_iter().map(|id| (id, other)).collect(),
        }
    }

    fn read_cell(&self, case: &ModelCase, who: Who, seeds: &Seeds) -> Vec<String> {
        let mut details = Vec::new();
        for (id, expect) in Self::read_targets(who, expect(case, who, Op::Read), seeds) {
            let body = json!({"ids": [id], "fields": ["id"]});
            let out = self.call(who, case.model, "read", body);
            if let Some(detail) = judge(case, Op::Read, expect, &out, self.record_subject(who)) {
                details.push(format!("id {id}: {detail}"));
            } else if expect == Expect::Ok {
                let returned = out.value.as_ref().map(|v| v[0]["id"].clone());
                if returned != Some(json!(id)) {
                    details.push(format!("id {id}: read did not return the requested record"));
                }
            }
        }
        Self::failures(case, who, Op::Read, details)
    }

    fn create_cell(&self, case: &ModelCase, who: Who) -> Vec<String> {
        let expect = expect(case, who, Op::Create);
        let tag = format!("crud {} {} create", case.model, who.label());
        let vals = self.payload(case.model, &tag, Self::co_of(who));
        let before = self.count(case.model, json!([]));
        let out = self.call(who, case.model, "create", json!({"vals_list": [vals]}));
        let after = self.count(case.model, json!([]));
        let mut details: Vec<String> =
            judge(case, Op::Create, expect, &out, self.record_subject(who))
                .into_iter()
                .collect();
        if expect == Expect::Ok {
            let created = out.ids().and_then(|ids| ids.iter().next().copied());
            match created {
                Some(id) if self.count(case.model, json!([["id", "=", id]])) == 1 => {}
                _ => details.push("create returned no identity that reads back".to_owned()),
            }
            if after != before + 1 {
                details.push(format!(
                    "record count went {before} -> {after}, expected +1"
                ));
            }
        } else if after != before {
            details.push(format!(
                "record count went {before} -> {after} after a refusal"
            ));
        }
        Self::failures(case, who, Op::Create, details)
    }

    fn write_cell(&self, case: &ModelCase, who: Who) -> Vec<String> {
        let expect = expect(case, who, Op::Write);
        let tag = format!("crud {} {} write", case.model, who.label());
        let id = self.make(case.model, &tag, Self::co_of(who));
        let marker = Marker::of(case.model);
        let new = marker.new_value(&tag);
        let before = self.marker_value(case.model, id, &marker);
        let body = json!({"ids": [id], "vals": {marker.field(): new}});
        let out = self.call(who, case.model, "write", body);
        let after = self.marker_value(case.model, id, &marker);
        let mut details: Vec<String> =
            judge(case, Op::Write, expect, &out, self.record_subject(who))
                .into_iter()
                .collect();
        if expect == Expect::Ok {
            if !marker.same(&after, &new) {
                details.push(format!("{} did not change after a write", marker.field()));
            }
        } else if !marker.same(&after, &before) {
            details.push(format!(
                "{} changed although the write was refused",
                marker.field()
            ));
        }
        Self::failures(case, who, Op::Write, details)
    }

    fn unlink_cell(&self, case: &ModelCase, who: Who) -> Vec<String> {
        let expect = expect(case, who, Op::Unlink);
        let tag = format!("crud {} {} unlink", case.model, who.label());
        let id = self.make(case.model, &tag, Self::co_of(who));
        let out = self.call(who, case.model, "unlink", json!({"ids": [id]}));
        let left = self.count(case.model, json!([["id", "=", id]]));
        let mut details: Vec<String> =
            judge(case, Op::Unlink, expect, &out, self.record_subject(who))
                .into_iter()
                .collect();
        let wanted = i64::from(expect != Expect::Ok);
        if left != wanted {
            details.push(format!("{left} record(s) remain, expected {wanted}"));
        }
        Self::failures(case, who, Op::Unlink, details)
    }

    /// Every non-deferred cell of the manifest. Returns the mismatches and the deferred cell count.
    pub fn family_matrix(&self) -> (Vec<String>, usize) {
        let mut mismatches = Vec::new();
        let mut deferred_cells = 0;
        for case in MANIFEST {
            if case.is_deferred() {
                deferred_cells += 3 * OPS.len();
                continue;
            }
            let seeds = self.seed(case);
            for who in [Who::Admin, Who::A, Who::B] {
                for op in OPS {
                    mismatches.extend(match op {
                        Op::Search => self.search_cell(case, who, &seeds),
                        Op::Read => self.read_cell(case, who, &seeds),
                        Op::Create => self.create_cell(case, who),
                        Op::Write => self.write_cell(case, who),
                        Op::Unlink => self.unlink_cell(case, who),
                    });
                }
            }
        }
        (mismatches, deferred_cells)
    }

    /// One write over two templates of different companies is refused as a whole.
    pub fn mixed_batch(&self) -> Vec<String> {
        let case = MANIFEST
            .iter()
            .find(|case| case.model == "product.template")
            .expect("product.template is in the manifest");
        let marker = Marker::Int("sequence");
        let runs = [(
            Who::Admin,
            "admin limited to company A, record level",
            Expect::RecordDenied,
            vec![self.company(Subject::A)],
        )];
        let mut found = Vec::new();
        for (who, label, expect, allowed) in runs {
            let tag = format!("crud mixed batch {label}");
            let ids = [
                self.make(case.model, &format!("{tag} A"), Co::A),
                self.make(case.model, &format!("{tag} B"), Co::B),
            ];
            let before: Vec<Value> = ids
                .iter()
                .map(|id| self.marker_value(case.model, *id, &marker))
                .collect();
            let body = json!({"ids": ids, "vals": {"sequence": 9}});
            let out = self.call_in(who, &allowed, case.model, "write", body);
            let mut details: Vec<String> =
                judge(case, Op::Write, expect, &out, self.record_subject(who))
                    .into_iter()
                    .collect();
            for (id, old) in ids.iter().zip(&before) {
                if self.marker_value(case.model, *id, &marker) != *old {
                    details.push(format!("record {id} changed in a refused batch"));
                }
            }
            let details = details
                .into_iter()
                .map(|detail| format!("mixed batch ({label}): {detail}"))
                .collect();
            found.extend(Self::failures(case, who, Op::Write, details));
        }
        found
    }

    /// The anonymous column of one model (ids no fixture creates, so a regression of
    /// authentication cannot change a real row): every operation gets the same 401.
    pub fn anonymous_probe(&self, case: &ModelCase) -> Vec<String> {
        let bodies = [
            json!({"domain": []}),
            json!({"ids": [NO_SUCH_ID], "fields": ["id"]}),
            json!({"vals_list": [{}]}),
            json!({"ids": [NO_SUCH_ID], "vals": {}}),
            json!({"ids": [NO_SUCH_ID]}),
        ];
        let mut found = Vec::new();
        for (op, body) in OPS.into_iter().zip(bodies) {
            let out = self.call(Who::Anon, case.model, op.method(), body);
            if let Some(detail) = judge(case, op, Expect::Unauthenticated, &out, None) {
                found.push(Self::fail(case, Who::Anon, op, &detail));
            }
        }
        found
    }
}
